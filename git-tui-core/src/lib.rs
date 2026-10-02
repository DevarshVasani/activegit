//! git-tui-core: git operations and the async job engine behind activegit.
//!
//! No TUI dependencies in this crate.

pub mod branch;
pub mod commit;
pub mod diff;
pub mod error;
pub mod jobqueue;
pub mod llm;
pub mod log;
pub mod repo;
pub mod stage;
pub mod stash;
pub mod status;
pub mod sync;
#[cfg(test)]
pub mod testutil;

#[cfg(test)]
mod gate {
    use crate::{commit, diff, stage, status, testutil};

    /// Phase 1 gate: init, two commits, dirty with two regions, stage one
    /// hunk, commit, assert log + remaining unstaged hunk.
    #[test]
    fn end_to_end_init_stage_hunk_and_commit() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "first\n", "first");
        let base = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        testutil::commit_file(&repo, "a.txt", &base, "second");
        let dirty = base.replacen("line 5\n", "line 5 CHANGED\n", 1).replacen(
            "line 35\n",
            "line 35 CHANGED\n",
            1,
        );
        std::fs::write(repo.workdir().unwrap().join("a.txt"), &dirty).unwrap();

        let before = diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert!(before.hunks.len() >= 2);

        stage::stage_hunk(&repo, "a.txt", 0).unwrap();
        commit::commit(&repo, "partial").unwrap();

        // Log shows the commit.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.message().unwrap().lines().next().unwrap(), "partial");

        // Status still shows the file as unstaged with only the remaining hunk.
        let st = status::repo_status(&repo).unwrap();
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, status::FileState::Unstaged);
        let remaining = diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert_eq!(remaining.hunks.len(), before.hunks.len() - 1);
        assert!(diff::staged_diff(&repo, "a.txt").unwrap().hunks.is_empty());
    }
}
