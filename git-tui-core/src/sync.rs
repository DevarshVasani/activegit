//! Remote sync: upstream tracking, push/pull/publish.
//!
//! Reads (remotes, upstream, ahead/behind) use libgit2 directly and never
//! touch the network. Push/pull/publish shell out to the `git` CLI —
//! exactly like lazygit does — so authentication (ssh keys and agent,
//! credential helpers, `gh auth`) works the way the user's shell works.

use crate::error::GitError;
use std::path::Path;

/// Upstream tracking state for the current branch. Pure local reads, no
/// network: `ahead`/`behind` compare HEAD against the last-fetched
/// upstream commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    /// Upstream shorthand, e.g. `origin/main`. `None` when the branch has
    /// no upstream (or there are no commits yet).
    pub upstream: Option<String>,
    /// Commits on HEAD not on the upstream.
    pub ahead: usize,
    /// Upstream commits not on HEAD (as of the last fetch/pull).
    pub behind: usize,
    /// Configured remote names, sorted.
    pub remotes: Vec<String>,
}

/// Local tracking state for the current branch. Pure libgit2 reads, no
/// network: `ahead`/`behind` compare HEAD against the last-fetched
/// upstream commit, so they only move on fetch/pull/push.
pub fn sync_status(repo: &git2::Repository) -> Result<SyncStatus, GitError> {
    let remotes = list_remotes(repo)?;
    let none = || SyncStatus {
        upstream: None,
        ahead: 0,
        behind: 0,
        remotes: remotes.clone(),
    };
    let Some(name) = current_local_branch(repo) else {
        return Ok(none());
    };
    let branch = match repo.find_branch(&name, git2::BranchType::Local) {
        Ok(b) => b,
        Err(_) => return Ok(none()),
    };
    let upstream = match branch.upstream() {
        Ok(u) => u,
        Err(_) => return Ok(none()),
    };
    let upstream_name = upstream.get().shorthand().unwrap_or("upstream").to_string();
    let (ahead, behind) = match (
        branch.get().peel_to_commit(),
        upstream.get().peel_to_commit(),
    ) {
        (Ok(local), Ok(remote)) => repo
            .graph_ahead_behind(local.id(), remote.id())
            .unwrap_or((0, 0)),
        _ => (0, 0),
    };
    Ok(SyncStatus {
        upstream: Some(upstream_name),
        ahead,
        behind,
        remotes,
    })
}

/// Sorted remote names (`origin`, …).
pub fn list_remotes(repo: &git2::Repository) -> Result<Vec<String>, GitError> {
    let list = repo.remotes()?;
    let mut out: Vec<String> = list.iter().flatten().map(|s| s.to_string()).collect();
    out.sort();
    Ok(out)
}

/// Fetch URL of remote `name`, or `None` when it is not configured.
pub fn remote_url(repo: &git2::Repository, name: &str) -> Result<Option<String>, GitError> {
    match repo.find_remote(name) {
        Ok(r) => Ok(r.url().map(|s| s.to_string())),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(GitError::Git(e)),
    }
}

/// `git remote add <name> <url>`. Errors on empty input or an existing
/// remote of the same name.
pub fn add_remote(repo: &git2::Repository, name: &str, url: &str) -> Result<(), GitError> {
    if name.trim().is_empty() {
        return Err(GitError::Sync("remote name is empty".into()));
    }
    if url.trim().is_empty() {
        return Err(GitError::Sync("remote URL is empty".into()));
    }
    repo.remote(name, url)
        .map(|_| ())
        .map_err(|e| GitError::Sync(format!("cannot add remote {name}: {e}")))?;
    Ok(())
}

/// Push the branch, like lazygit's `P`: `git push [-u] <remote> <branch>`.
/// Shells out so ssh/credential-helper auth works exactly like the shell.
/// Returns the command output on success.
pub fn push(
    workdir: &Path,
    remote: &str,
    branch: &str,
    set_upstream: bool,
) -> Result<String, GitError> {
    if remote.trim().is_empty() || branch.trim().is_empty() {
        return Err(GitError::Sync("push needs a remote and a branch".into()));
    }
    let mut args = vec!["push"];
    if set_upstream {
        args.push("--set-upstream");
    }
    args.push(remote);
    args.push(branch);
    run_git(workdir, &args)
}

/// Pull the current branch, like lazygit's `p`: plain `git pull`, so the
/// user's `pull.rebase` / `pull.ff` config decides merge vs rebase.
pub fn pull(workdir: &Path) -> Result<String, GitError> {
    run_git(workdir, &["pull"])
}

/// Publish a repo that has no remote yet (the "not yet on GitHub" case):
/// point `remote` at `url` (refusing to clobber a different URL), then
/// push the branch with `-u` so later `p`/`P` just work.
pub fn publish(
    repo: &git2::Repository,
    remote: &str,
    url: &str,
    branch: &str,
) -> Result<String, GitError> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| GitError::Sync("bare repositories cannot be published".into()))?
        .to_path_buf();
    match remote_url(repo, remote)? {
        Some(existing) if existing == url => {}
        Some(existing) => {
            return Err(GitError::Sync(format!(
                "remote {remote} already points at {existing}"
            )))
        }
        None => add_remote(repo, remote, url)?,
    }
    push(&workdir, remote, branch, true)
}

/// Short name of the checked-out local branch, or `None` on detached or
/// unborn HEAD.
fn current_local_branch(repo: &git2::Repository) -> Option<String> {
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    head.shorthand().map(|s| s.to_string())
}

/// Run the `git` CLI in `workdir`. Prompts are disabled
/// (`GIT_TERMINAL_PROMPT=0`, ssh batch mode unless the user already set
/// `GIT_SSH_COMMAND`) so a missing credential fails fast with the remote's
/// message instead of hanging the worker thread forever.
fn run_git(workdir: &Path, args: &[&str]) -> Result<String, GitError> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .current_dir(workdir)
        .env("GIT_TERMINAL_PROMPT", "0");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let out = cmd
        .output()
        .map_err(|e| GitError::Sync(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut msg: String = stderr
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if msg.is_empty() {
            msg = format!("git {} failed with no output", args.join(" "));
        }
        if msg.len() > 400 {
            msg.truncate(msg.floor_char_boundary(400));
        }
        return Err(GitError::Sync(msg));
    }
    let mut combined = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if combined.is_empty() {
        combined = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    /// Local repo pushing to / pulling from a bare repo on disk. File
    /// remotes need no network, so push/pull tests run fully offline.
    fn repo_with_bare_origin() -> (tempfile::TempDir, git2::Repository, tempfile::TempDir) {
        let (dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let origin_path = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        add_remote(&repo, "origin", origin_path.to_str().unwrap()).unwrap();
        (dir, repo, origin_dir)
    }

    #[test]
    fn reports_no_upstream_without_remote() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let st = sync_status(&repo).unwrap();
        assert_eq!(st.upstream, None);
        assert_eq!((st.ahead, st.behind), (0, 0));
        assert!(st.remotes.is_empty());
    }

    #[test]
    fn push_records_upstream_and_clears_ahead() {
        let (_dir, repo, _origin) = repo_with_bare_origin();
        // One commit not yet pushed.
        testutil::commit_file(&repo, "a.txt", "a\nmore\n", "second");
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        let st = sync_status(&repo).unwrap();
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (0, 0));
    }

    #[test]
    fn pull_fast_forwards_to_remote_changes() {
        let (_dir, repo, origin) = repo_with_bare_origin();
        let bare = origin.path().join("origin.git");
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        // A second clone advances the remote (re-pointed at the bare repo:
        // cloning the workdir would leave `origin` on a non-bare repo).
        let (dir2, other) = testutil::clone_local(workdir.as_path());
        other
            .remote_set_url("origin", bare.to_str().unwrap())
            .unwrap();
        testutil::commit_file(&other, "b.txt", "b\n", "from other");
        let workdir2 = other.workdir().unwrap().to_path_buf();
        push(&workdir2, "origin", "main", false).unwrap();
        drop(dir2);
        // Pull brings the new commit in.
        pull(&workdir).unwrap();
        assert!(workdir.join("b.txt").exists());
        let st = sync_status(&repo).unwrap();
        assert_eq!((st.ahead, st.behind), (0, 0));
    }

    #[test]
    fn fetch_reveals_behind_count() {
        let (_dir, repo, origin) = repo_with_bare_origin();
        let bare = origin.path().join("origin.git");
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        let (dir2, other) = testutil::clone_local(workdir.as_path());
        other
            .remote_set_url("origin", bare.to_str().unwrap())
            .unwrap();
        testutil::commit_file(&other, "b.txt", "b\n", "from other");
        let workdir2 = other.workdir().unwrap().to_path_buf();
        push(&workdir2, "origin", "main", false).unwrap();
        drop(dir2);
        run_git(&workdir, &["fetch", "origin"]).unwrap();
        let st = sync_status(&repo).unwrap();
        assert_eq!((st.ahead, st.behind), (0, 1));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
    }

    #[test]
    fn publish_adds_remote_and_pushes_new_repo() {
        // Fresh `git init` with no remotes at all — the "not yet on
        // GitHub" case: point it at an (empty) remote and push.
        let (dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let origin_path = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        publish(&repo, "origin", origin_path.to_str().unwrap(), "main").unwrap();
        assert_eq!(
            remote_url(&repo, "origin").unwrap().as_deref(),
            Some(origin_path.to_str().unwrap())
        );
        // The remote actually received the branch.
        let remote = git2::Repository::open_bare(&origin_path).unwrap();
        assert!(remote.find_reference("refs/heads/main").is_ok());
        drop(dir);
    }

    #[test]
    fn local_commit_after_push_shows_ahead() {
        let (_dir, repo, _origin) = repo_with_bare_origin();
        let workdir = repo.workdir().unwrap().to_path_buf();
        push(&workdir, "origin", "main", true).unwrap();
        testutil::commit_file(&repo, "a.txt", "a\nlocal\n", "local work");
        let st = sync_status(&repo).unwrap();
        assert_eq!((st.ahead, st.behind), (1, 0));
    }

    #[test]
    fn pull_without_remote_is_an_error() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let workdir = repo.workdir().unwrap().to_path_buf();
        assert!(pull(&workdir).is_err());
    }

    #[test]
    fn push_with_no_commits_is_an_error() {
        let (dir, repo) = testutil::init_repo();
        let origin_path = dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        add_remote(&repo, "origin", origin_path.to_str().unwrap()).unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        assert!(push(&workdir, "origin", "main", true).is_err());
    }
}
