//! The write lease of a shared workspace.
//!
//! Branches of one conversation that work in the same directory take turns: a branch holds the
//! workspace for the whole of a turn, so no branch's files change in the middle of another
//! branch's turn. Branches forked into their own worktrees have their own directories and run
//! side by side. Within a process a semaphore orders the branches; across processes, such as
//! two Codex instances, a lock file under the Codex home does.

use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use sha1::Digest as _;
use sha1::Sha1;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// A conversation's graph and a working directory.
type LeaseKey = (String, String);

/// Who may write to one workspace of one conversation.
struct Lease {
    permit: Arc<Semaphore>,
    /// Name of the branch whose turn holds the lease.
    holder: StdMutex<Option<String>>,
}

static LEASES: LazyLock<StdMutex<HashMap<LeaseKey, Arc<Lease>>>> = LazyLock::new(StdMutex::default);

/// How often a turn waiting for another process's lease checks again.
const LOCK_POLL: Duration = Duration::from_millis(500);

/// The lease a turn holds until it ends.
pub(crate) struct WorkspaceLease {
    _permit: OwnedSemaphorePermit,
    /// The lock file, locked while this lease lives; closing it releases the lock.
    _file: Option<File>,
    lease: Arc<Lease>,
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if let Ok(mut holder) = self.lease.holder.lock() {
            *holder = None;
        }
    }
}

fn lease_for(key: LeaseKey) -> Arc<Lease> {
    let Ok(mut leases) = LEASES.lock() else {
        return Arc::new(Lease {
            permit: Arc::new(Semaphore::new(1)),
            holder: StdMutex::default(),
        });
    };
    Arc::clone(leases.entry(key).or_insert_with(|| {
        Arc::new(Lease {
            permit: Arc::new(Semaphore::new(1)),
            holder: StdMutex::default(),
        })
    }))
}

/// Takes the write lease of the turn's workspace for the turn, waiting while another branch of
/// the conversation holds it. Sessions without a context graph need no lease.
pub(crate) async fn acquire_workspace_lease(
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
) -> CodexResult<Option<WorkspaceLease>> {
    let Some(service) = sess.services.context_graph.as_ref() else {
        return Ok(None);
    };
    let membership = service.live.membership();
    let workspace = turn_context.config.cwd.display().to_string();
    let lease = lease_for((membership.graph_id.clone(), workspace.clone()));
    let permit = match Arc::clone(&lease.permit).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            let holder = lease
                .holder
                .lock()
                .ok()
                .and_then(|holder| holder.clone())
                .unwrap_or_else(|| "another branch".to_string());
            let message = format!(
                "Waiting for branch {holder} to finish its turn in {workspace}. Branches that \
                 share a workspace take turns; fork into a worktree to run them side by side."
            );
            sess.send_event(turn_context, EventMsg::Warning(WarningEvent { message }))
                .await;
            tokio::select! {
                permit = Arc::clone(&lease.permit).acquire_owned() => {
                    permit.map_err(|_| CodexErr::TurnAborted)?
                }
                () = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            }
        }
    };
    if let Ok(mut holder) = lease.holder.lock() {
        *holder = Some(membership.name.clone());
    }
    let file = lock_across_processes(
        sess,
        turn_context,
        cancellation_token,
        &membership.graph_id,
        &workspace,
        &membership.name,
    )
    .await?;
    Ok(Some(WorkspaceLease {
        _permit: permit,
        _file: file,
        lease,
    }))
}

/// The lock file of a conversation's workspace.
fn lock_path(codex_home: &Path, graph_id: &str, workspace: &str) -> PathBuf {
    let mut hasher = Sha1::new();
    hasher.update(graph_id.as_bytes());
    hasher.update([0]);
    hasher.update(workspace.as_bytes());
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    codex_home
        .join("context_graph")
        .join("leases")
        .join(format!("{digest}.lock"))
}

/// Locks the workspace's lock file, waiting while another process holds it. A lock file that
/// cannot be created leaves the lease to this process alone.
async fn lock_across_processes(
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
    graph_id: &str,
    workspace: &str,
    branch: &str,
) -> CodexResult<Option<File>> {
    let path = lock_path(
        turn_context.config.codex_home.as_path(),
        graph_id,
        workspace,
    );
    let opened = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
        });
    let mut file = match opened {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!("workspace lease file unavailable: {err}");
            return Ok(None);
        }
    };
    let mut warned = false;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                if !warned {
                    warned = true;
                    let mut holder = String::new();
                    let _ = file.read_to_string(&mut holder);
                    let holder = holder.trim();
                    let holder = if holder.is_empty() {
                        "another Codex process"
                    } else {
                        holder
                    };
                    let message = format!(
                        "Waiting for branch {holder} to finish its turn in {workspace}. Branches \
                         that share a workspace take turns; fork into a worktree to run them \
                         side by side."
                    );
                    sess.send_event(turn_context, EventMsg::Warning(WarningEvent { message }))
                        .await;
                }
                tokio::select! {
                    () = tokio::time::sleep(LOCK_POLL) => {}
                    () = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                }
            }
            Err(TryLockError::Error(err)) => {
                tracing::warn!("workspace lease file could not be locked: {err}");
                return Ok(None);
            }
        }
    }
    // Name the holder for processes that wait.
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(branch.as_bytes());
    Ok(Some(file))
}

#[cfg(test)]
#[path = "lease_tests.rs"]
mod tests;
