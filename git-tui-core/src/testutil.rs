//! Test fixtures for Phase 1. Only compiled for tests.
#![cfg(test)]

use git2::{Oid, Repository};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

/// Env vars are process-global and `cargo test` runs threads in parallel:
/// every test that reads or writes LLM key vars must hold this first.
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Init a new repo in a temp dir, default branch `main`, local user config set.
pub fn init_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("create tempdir");
    let repo = Repository::init(dir.path()).expect("init repo");
    // Point unborn HEAD at main so the first commit lands on `main`.
    repo.set_head("refs/heads/main").expect("set head to main");
    {
        let mut cfg = repo.config().expect("open config");
        cfg.set_str("user.name", "Test User")
            .expect("set user.name");
        cfg.set_str("user.email", "test@example.com")
            .expect("set user.email");
        cfg.set_str("init.defaultBranch", "main").ok();
        cfg.set_str("commit.gpgsign", "false").ok();
        // Git for Windows enables autocrlf system-wide; keep checkouts byte-exact.
        cfg.set_str("core.autocrlf", "false").ok();
    }
    (dir, repo)
}

/// Write file, stage it, commit. Returns the new commit id.
pub fn commit_file(repo: &Repository, path: &str, contents: &str, msg: &str) -> Oid {
    write_workdir_file(repo, Path::new(path), contents);
    let mut index = repo.index().expect("open index");
    index
        .add_path(Path::new(path))
        .unwrap_or_else(|e| panic!("stage {path}: {e}"));
    index.write().expect("write index");
    write_index_as_commit(repo, msg)
}

/// Modify a workdir file without staging (append).
pub fn dirty_file(repo: &Repository, path: &str, append_contents: &str) {
    let full = repo.workdir().expect("workdir").join(path);
    let mut f = OpenOptions::new()
        .append(true)
        .open(&full)
        .unwrap_or_else(|e| panic!("open {path} for append: {e}"));
    f.write_all(append_contents.as_bytes())
        .expect("append to file");
}

/// Clone a local repo (file path) into a fresh temp dir. The clone keeps
/// the `origin` remote pointing at `src`, so push/pull tests run offline.
pub fn clone_local(src: &Path) -> (TempDir, Repository) {
    let dir = TempDir::new().expect("create tempdir");
    let repo = Repository::clone(src.to_str().expect("src is utf-8"), dir.path())
        .expect("clone local repo");
    {
        let mut cfg = repo.config().expect("open config");
        cfg.set_str("user.name", "Test User")
            .expect("set user.name");
        cfg.set_str("user.email", "test@example.com")
            .expect("set user.email");
        cfg.set_str("commit.gpgsign", "false").ok();
        // Git for Windows enables autocrlf system-wide; keep checkouts byte-exact.
        cfg.set_str("core.autocrlf", "false").ok();
    }
    (dir, repo)
}
/// Create branch `name` at HEAD and check it out.
pub fn new_branch(repo: &Repository, name: &str) {
    let head = repo.head().expect("HEAD exists for new_branch");
    let commit = head.peel_to_commit().expect("peel to commit");
    repo.branch(name, &commit, false).expect("create branch");
    repo.set_head(&format!("refs/heads/{name}"))
        .expect("set head to new branch");
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .expect("checkout new branch");
}

fn write_workdir_file(repo: &Repository, rel: &Path, contents: &str) {
    let full = repo.workdir().expect("workdir").join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(&full, contents).expect("write file");
}

fn write_index_as_commit(repo: &Repository, msg: &str) -> Oid {
    let mut index = repo.index().expect("open index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let sig = repo.signature().expect("signature");
    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(head) if head.peel_to_commit().is_ok() => {
            vec![head.peel_to_commit().expect("head commit")]
        }
        _ => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
        .expect("create commit")
}
