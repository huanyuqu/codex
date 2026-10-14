//! Merging the code of a branch that works in its own git worktree, along with its context.
//!
//! The branch's uncommitted work is first committed in its worktree, so the merge carries
//! everything the branch did. The merge itself runs in the target's worktree with
//! `git merge --no-ff`; when it stops at conflicts, the merge is left in progress for the user or
//! the model to resolve, and the conflicting files are reported.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use codex_context_graph_store::CodeMerge;
use tokio::process::Command;

/// The result of one git command.
struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        // Merges and commits never wait for an editor.
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .output()
        .await
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

async fn git_ok(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = git(cwd, args).await?;
    anyhow::ensure!(
        output.success,
        "git {} failed: {}",
        args.join(" "),
        if output.stderr.is_empty() {
            output.stdout
        } else {
            output.stderr
        }
    );
    Ok(output.stdout)
}

/// The git directory a worktree shares with the repository's other worktrees.
async fn common_dir(root: &str) -> anyhow::Result<PathBuf> {
    let dir = git_ok(Path::new(root), &["rev-parse", "--git-common-dir"]).await?;
    let dir = Path::new(root).join(dir);
    Ok(tokio::fs::canonicalize(&dir).await.unwrap_or(dir))
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Checks that the code of the branch `branch_name`, whose worktree is `source`, can be merged
/// into the worktree `target`: both are worktrees of one git repository, and not the same one.
pub(crate) async fn check(target: &Path, source: &Path, branch_name: &str) -> anyhow::Result<()> {
    let target_root = git_ok(target, &["rev-parse", "--show-toplevel"])
        .await
        .context("this conversation does not work in a git repository")?;
    let source_root = git_ok(source, &["rev-parse", "--show-toplevel"])
        .await
        .with_context(|| format!("branch {branch_name} does not work in a git repository"))?;
    anyhow::ensure!(
        target_root != source_root,
        "branch {branch_name} works in this conversation's workspace, so its changes are \
         already here; merge it without --code"
    );
    anyhow::ensure!(
        common_dir(&target_root).await? == common_dir(&source_root).await?,
        "branch {branch_name} works in another repository; its code cannot be merged here"
    );
    Ok(())
}

/// Merges the code of the branch `branch_name`, whose worktree is `source`, into the worktree
/// `target`.
pub(crate) async fn merge_code(
    target: &Path,
    source: &Path,
    branch_name: &str,
) -> anyhow::Result<CodeMerge> {
    check(target, source, branch_name).await?;
    let mut merge = CodeMerge::default();
    let head = git_ok(source, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    merge.git_branch = if head == "HEAD" {
        git_ok(source, &["rev-parse", "--short", "HEAD"]).await?
    } else {
        head
    };
    if !git_ok(source, &["status", "--porcelain"]).await?.is_empty() {
        git_ok(source, &["add", "--all"]).await?;
        let message = format!("Work of branch {branch_name}, saved by Codex before merging");
        git_ok(
            source,
            &["commit", "--no-verify", "--quiet", "-m", &message],
        )
        .await?;
        merge.snapshot_commit = Some(git_ok(source, &["rev-parse", "HEAD"]).await?);
    }
    let commit = git_ok(source, &["rev-parse", "HEAD"]).await?;

    let before = git_ok(target, &["rev-parse", "HEAD"]).await?;
    let message = format!("Merge branch {branch_name} of the conversation");
    let merged = git(
        target,
        &["merge", "--no-ff", "--no-edit", "-m", &message, &commit],
    )
    .await?;
    if merged.success {
        let after = git_ok(target, &["rev-parse", "HEAD"]).await?;
        if after != before {
            merge.files = lines(&git_ok(target, &["diff", "--name-only", &before, &after]).await?);
            merge.merge_commit = Some(after);
        }
        return Ok(merge);
    }
    let conflicts = lines(&git_ok(target, &["diff", "--name-only", "--diff-filter=U"]).await?);
    anyhow::ensure!(
        !conflicts.is_empty(),
        "git merge failed: {}",
        if merged.stderr.is_empty() {
            merged.stdout
        } else {
            merged.stderr
        }
    );
    merge.files = lines(&git_ok(target, &["diff", "--name-only", "--cached"]).await?);
    merge.conflicts = conflicts;
    Ok(merge)
}

/// What a merge note says about the code merged with a branch.
pub(crate) fn describe(merge: &CodeMerge) -> String {
    let files = |files: &[String]| {
        const SHOWN: usize = 20;
        let mut text = files
            .iter()
            .take(SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if files.len() > SHOWN {
            text.push_str(&format!(" and {} more", files.len() - SHOWN));
        }
        text
    };
    let mut text = format!(
        "Its code (git {}) was merged into this workspace",
        merge.git_branch
    );
    match (&merge.merge_commit, merge.conflicts.is_empty()) {
        (_, false) => text.push_str(&format!(
            ", and the merge stopped at conflicts in {}. Resolve them and commit the merge \
             before relying on these files.",
            files(&merge.conflicts)
        )),
        (Some(commit), true) => {
            let short: String = commit.chars().take(12).collect();
            text.push_str(&format!(" in commit {short}"));
            if merge.files.is_empty() {
                text.push('.');
            } else {
                text.push_str(&format!(", changing {}.", files(&merge.files)));
            }
        }
        (None, true) => {
            text = format!(
                "Its code (git {}) had nothing this workspace lacks.",
                merge.git_branch
            )
        }
    }
    text
}

#[cfg(test)]
#[path = "code_merge_tests.rs"]
mod tests;
