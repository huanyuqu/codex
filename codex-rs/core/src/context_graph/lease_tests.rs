use pretty_assertions::assert_eq;

use super::*;

#[tokio::test]
async fn one_turn_at_a_time_holds_a_workspace() {
    let key = ("graph-lease-test".to_string(), "/repo".to_string());
    let lease = lease_for(key.clone());
    let first = Arc::clone(&lease.permit)
        .try_acquire_owned()
        .unwrap_or_else(|err| panic!("first permit: {err}"));
    // The same conversation and directory share the lease; other directories do not.
    assert!(
        Arc::clone(&lease_for(key).permit)
            .try_acquire_owned()
            .is_err()
    );
    assert!(
        Arc::clone(&lease_for(("graph-lease-test".to_string(), "/worktree".to_string())).permit)
            .try_acquire_owned()
            .is_ok()
    );
    let held = WorkspaceLease {
        _permit: first,
        _file: None,
        lease: Arc::clone(&lease),
    };
    if let Ok(mut holder) = lease.holder.lock() {
        *holder = Some("main".to_string());
    }
    drop(held);
    assert_eq!(
        lease.holder.lock().ok().and_then(|holder| holder.clone()),
        None
    );
    assert!(Arc::clone(&lease.permit).try_acquire_owned().is_ok());
}

#[test]
fn lock_files_are_per_conversation_and_workspace() {
    let home = Path::new("/home/codex");
    let one = lock_path(home, "graph-1", "/repo");
    assert!(one.starts_with("/home/codex/context_graph/leases"));
    assert_eq!(one, lock_path(home, "graph-1", "/repo"));
    assert_ne!(one, lock_path(home, "graph-1", "/worktree"));
    assert_ne!(one, lock_path(home, "graph-2", "/repo"));
}

#[test]
fn a_locked_file_blocks_other_handles() {
    let dir = tempfile::TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let path = dir.path().join("lease.lock");
    let open = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap_or_else(|err| panic!("open: {err}"))
    };
    let first = open();
    assert!(first.try_lock().is_ok());
    assert!(matches!(open().try_lock(), Err(TryLockError::WouldBlock)));
    drop(first);
    assert!(open().try_lock().is_ok());
}
