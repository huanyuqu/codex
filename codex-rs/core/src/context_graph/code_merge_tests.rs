use std::path::Path;

use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

fn run(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
    assert!(status.success(), "git {args:?} failed");
}

/// A repository with one commit and a second worktree on its own branch.
fn repo_with_worktree() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("tempdir: {err}"));
    let main = dir.path().join("main");
    let branch = dir.path().join("branch");
    std::fs::create_dir(&main).unwrap_or_else(|err| panic!("mkdir: {err}"));
    run(&main, &["init", "--quiet", "--initial-branch", "main"]);
    run(&main, &["config", "user.name", "t"]);
    run(&main, &["config", "user.email", "t@example.com"]);
    std::fs::write(main.join("a.txt"), "one\n").unwrap_or_else(|err| panic!("write: {err}"));
    run(&main, &["add", "a.txt"]);
    run(&main, &["commit", "--quiet", "-m", "start"]);
    run(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "try-b",
            branch.to_str().unwrap_or_default(),
        ],
    );
    (dir, main, branch)
}

#[tokio::test]
async fn uncommitted_branch_work_is_saved_and_merged() {
    let (_dir, main, branch) = repo_with_worktree();
    std::fs::write(branch.join("b.txt"), "new\n").unwrap_or_else(|err| panic!("write: {err}"));

    let merge = merge_code(&main, &branch, "try-b")
        .await
        .unwrap_or_else(|err| panic!("merge: {err:#}"));
    assert_eq!(merge.git_branch, "try-b");
    assert!(merge.snapshot_commit.is_some());
    assert!(merge.merge_commit.is_some());
    assert_eq!(merge.files, vec!["b.txt".to_string()]);
    assert!(merge.conflicts.is_empty());
    assert!(main.join("b.txt").exists());
    assert!(describe(&merge).contains(", changing b.txt."));

    // Merging again finds nothing new.
    let again = merge_code(&main, &branch, "try-b")
        .await
        .unwrap_or_else(|err| panic!("merge: {err:#}"));
    assert_eq!(again.merge_commit, None);
    assert_eq!(
        describe(&again),
        "Its code (git try-b) had nothing this workspace lacks."
    );
}

#[tokio::test]
async fn conflicts_are_reported_and_left_to_resolve() {
    let (_dir, main, branch) = repo_with_worktree();
    std::fs::write(branch.join("a.txt"), "two\n").unwrap_or_else(|err| panic!("write: {err}"));
    std::fs::write(main.join("a.txt"), "three\n").unwrap_or_else(|err| panic!("write: {err}"));
    run(&main, &["commit", "--quiet", "-am", "main edit"]);

    let merge = merge_code(&main, &branch, "try-b")
        .await
        .unwrap_or_else(|err| panic!("merge: {err:#}"));
    assert_eq!(merge.conflicts, vec!["a.txt".to_string()]);
    assert_eq!(merge.merge_commit, None);
    assert!(describe(&merge).contains("stopped at conflicts in a.txt"));
}

#[tokio::test]
async fn a_shared_workspace_has_nothing_to_merge() {
    let (_dir, main, _branch) = repo_with_worktree();
    let err = merge_code(&main, &main, "same")
        .await
        .err()
        .map(|err| err.to_string())
        .unwrap_or_default();
    assert!(err.contains("already here"), "{err}");
}
