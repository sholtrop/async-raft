use std::io::SeekFrom;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::{AppData, AppDataResponse, AppError, RaftNetwork, RaftStorage};
use crate::error::RaftResult;
use crate::raft::{InstallSnapshotRequest, InstallSnapshotResponse};
use crate::core::{State, RaftCore, SnapshotState, UpdateCurrentLeader};

impl<D: AppData, R: AppDataResponse, E: AppError, N: RaftNetwork<D, E>, S: RaftStorage<D, R, E>> RaftCore<D, R, E, N, S> {
    /// Invoked by leader to send chunks of a snapshot to a follower (§7).
    ///
    /// Leaders always send chunks in order. It is important to note that, according to the Raft spec,
    /// a log may only have one snapshot at any time. As snapshot contents are application specific,
    /// the Raft log will only store a pointer to the snapshot file along with the index & term.
    #[tracing::instrument(level="trace", skip(self, req))]
    pub(super) async fn handle_install_snapshot_request(&mut self, req: InstallSnapshotRequest) -> RaftResult<InstallSnapshotResponse, E> {
        // If message's term is less than most recent term, then we do not honor the request.
        if &req.term < &self.current_term {
            return Ok(InstallSnapshotResponse{term: self.current_term});
        }

        // Update election timeout.
        self.update_next_election_timeout();

        // Update current term if needed.
        let mut report_metrics = false;
        if &self.current_term != &req.term {
            self.update_current_term(req.term, None);
            self.save_hard_state().await?;
            report_metrics = true;
        }

        // Update current leader if needed.
        if self.current_leader.as_ref() != Some(&req.leader_id) {
            self.update_current_leader(UpdateCurrentLeader::OtherNode(req.leader_id));
            report_metrics = true;
        }

        // If not follower, become follower.
        if !self.target_state.is_follower() && !self.target_state.is_non_voter() {
            self.set_target_state(State::Follower); // State update will emit metrics.
        }

        if report_metrics {
            self.report_metrics();
        }

        // Compare current snapshot state with received RPC and handle as needed.
        match self.snapshot_state.take() {
            None => Ok(self.begin_installing_snapshot(req).await?),
            Some(SnapshotState::Snapshotting{handle, ..}) => {
                handle.abort(); // Abort the current compaction in favor of installation from leader.
                Ok(self.begin_installing_snapshot(req).await?)
            }
            Some(SnapshotState::Streaming{snapshot, id, offset}) => Ok(self.continue_installing_snapshot(req, offset, id, snapshot).await?),
        }
    }

    #[tracing::instrument(level="trace", skip(self, req))]
    async fn begin_installing_snapshot(&mut self, req: InstallSnapshotRequest) -> RaftResult<InstallSnapshotResponse, E> {
        // Create a new snapshot and begin writing its contents.
        let (id, mut snapshot) = self.storage.create_snapshot().await
            .map_err(|err| self.map_fatal_storage_result(err))?;
        snapshot.as_mut().write_all(&req.data).await?;

        // If this was a small snapshot, and it is already done, then finish up.
        if req.done {
            self.finalize_snapshot_installation(req, id, snapshot).await?;
            return Ok(InstallSnapshotResponse{term: self.current_term});
        }

        // Else, retain snapshot components for later segments & respod.
        self.snapshot_state = Some(SnapshotState::Streaming{
            offset: req.data.len() as u64,
            id, snapshot,
        });
        return Ok(InstallSnapshotResponse{term: self.current_term});
    }

    #[tracing::instrument(level="trace", skip(self, req, offset, snapshot))]
    async fn continue_installing_snapshot(
        &mut self, req: InstallSnapshotRequest, mut offset: u64, id: String, mut snapshot: Box<S::Snapshot>,
    ) -> RaftResult<InstallSnapshotResponse, E> {
        // Always seek to the target offset if not an exact match.
        if &req.offset != &offset {
            if let Err(err) = snapshot.as_mut().seek(SeekFrom::Start(req.offset)).await {
                self.snapshot_state = Some(SnapshotState::Streaming{offset, id, snapshot});
                return Err(err.into());
            }
            offset = req.offset;
        }

        // Write the next segment & update offset.
        if let Err(err) = snapshot.as_mut().write_all(&req.data).await {
            self.snapshot_state = Some(SnapshotState::Streaming{offset, id, snapshot});
            return Err(err.into());
        }
        offset += req.data.len() as u64;

        // If the snapshot stream is done, then finalize.
        if req.done {
            self.finalize_snapshot_installation(req, id, snapshot).await?;
        } else {
            self.snapshot_state = Some(SnapshotState::Streaming{offset, id, snapshot});
        }
        return Ok(InstallSnapshotResponse{term: self.current_term});
    }

    /// Finalize the installation of a new snapshot.
    ///
    /// Any errors which come up from this routine will cause the Raft node to go into shutdown.
    #[tracing::instrument(level="trace", skip(self, req, snapshot))]
    async fn finalize_snapshot_installation(&mut self, req: InstallSnapshotRequest, id: String, mut snapshot: Box<S::Snapshot>) -> RaftResult<(), E> {
        snapshot.as_mut().shutdown().await.map_err(|err| self.map_fatal_storage_result(err))?;
        let delete_through = if &self.last_log_index > &req.last_included_index {
            Some(req.last_included_index)
        } else {
            None
        };
        self.storage.finalize_snapshot_installation(req.last_included_index, req.last_included_term, delete_through, id, snapshot).await
            .map_err(|err| self.map_fatal_storage_result(err))?;
        Ok(())
    }
}
