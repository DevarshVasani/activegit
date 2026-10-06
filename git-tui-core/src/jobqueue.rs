//! Phase 2: async job engine.
//!
//! Single-writer rule: only the worker thread mutates the repo.
//! The TUI submits [`AsyncJob`]s and reads owned [`AsyncResult`]s.
//! See `docs/architecture.md`.

use crate::branch::BranchInfo;
use crate::diff::FileDiff;
use crate::error::GitError;
use crate::log::{CommitInfo, CommitOverview};
use crate::repo::Repo;
use crate::stash::StashEntry;
use crate::status::RepoStatus;
use crate::sync::SyncStatus;
use crossbeam_channel::{Receiver, Sender};
use std::path::Path;
use std::time::Duration;

/// Work requests. All data owned so it can cross threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncJob {
    RefreshStatus,
    LoadDiff {
        path: String,
        staged: bool,
    },
    /// Whole file content (for clean files, whose diffs are empty).
    LoadFile {
        path: String,
    },
    /// Full new-version text for Markdown preview.
    LoadMarkdown {
        path: String,
        staged: bool,
    },
    StageHunk {
        path: String,
        hunk_index: usize,
    },
    /// Restore a single hunk (`staged = false` reverts the workdir hunk,
    /// `staged = true` reverts the index hunk toward HEAD).
    DiscardHunk {
        path: String,
        hunk_index: usize,
        staged: bool,
    },
    StageFile {
        path: String,
    },
    UnstageFile {
        path: String,
    },
    DiscardFile {
        path: String,
    },
    Commit {
        message: String,
    },
    ListBranches,
    CreateBranch {
        name: String,
    },
    CheckoutBranch {
        name: String,
    },
    DeleteBranch {
        name: String,
    },
    ListLog {
        limit: usize,
    },
    /// Full detail for one commit (message, author, `--stat` file list),
    /// for the commit overview panel.
    LoadCommitOverview {
        oid: String,
    },
    ListStash,
    StashPush {
        message: String,
    },
    StashPop {
        index: usize,
    },
    StashDrop {
        index: usize,
    },
    /// Push the branch to a remote (`git push [-u]`, shells out so auth
    /// works like the user's shell).
    Push {
        remote: String,
        branch: String,
        set_upstream: bool,
    },
    /// Pull the current branch (`git pull`, honoring pull.rebase/pull.ff).
    Pull,
    /// Point `remote` at `url` (unless already set) and push with `-u`.
    /// The "not yet on GitHub" flow: create the empty remote, then publish.
    Publish {
        remote: String,
        url: String,
        branch: String,
    },
    /// Local upstream tracking state (no network).
    LoadSync,
    /// Generate a commit message from staged diffs via the LLM provider.
    /// Carries a snapshot of [`crate::llm::LlmConfig`] so the worker — the
    /// only thread holding the repo — can build the prompt + call the API.
    GenerateCommitMessage {
        llm: crate::llm::LlmConfig,
    },
    /// Ask the provider which models it offers, for the setup form's
    /// model row. Carries a config snapshot so the worker can build the
    /// request (key and base URL are part of what the form is editing),
    /// and the submitting form's request id so a late answer for an
    /// abandoned config can be told apart from the current one.
    FetchModels {
        id: u64,
        llm: crate::llm::LlmConfig,
    },
}

/// Work results. All data owned.
#[derive(Debug)]
pub enum AsyncResult {
    Status(RepoStatus),
    Diff(FileDiff),
    Markdown {
        path: String,
        staged: bool,
        text: String,
    },
    Branches(Vec<BranchInfo>),
    Log(Vec<CommitInfo>),
    CommitOverview(CommitOverview),
    Stash(Vec<StashEntry>),
    SyncStatus(SyncStatus),
    /// LLM-generated commit message draft (commit box fills `draft`).
    GeneratedMessage(String),
    /// Model ids the provider offers, for the setup form's model row,
    /// tagged with the [`AsyncJob::FetchModels`] request id that asked.
    Models {
        id: u64,
        models: Vec<String>,
    },
    /// A model fetch failed (same id); the message goes next to the model
    /// row instead of the global error line, since an unreachable provider
    /// is not the user's mistake and the form stays usable.
    ModelsError {
        id: u64,
        error: String,
    },
    MutationDone,
    Error(GitError),
}

/// Single-writer job queue: a background thread owns the [`Repo`] and is the
/// only place that touches git state. The TUI submits [`AsyncJob`]s (never
/// blocking — the channel is unbounded) and drains [`AsyncResult`]s without
/// blocking the render loop via [`JobQueue::try_recv`].
pub struct JobQueue {
    job_tx: Sender<AsyncJob>,
    result_rx: Receiver<AsyncResult>,
}

impl JobQueue {
    /// Open the repo at `path` and move it into the worker thread.
    pub fn spawn(path: impl AsRef<Path>) -> Result<Self, GitError> {
        let repo = Repo::open(path.as_ref())?;
        Ok(Self::spawn_from_repo(repo))
    }

    /// Move an already-opened [`Repo`] into the worker thread.
    pub fn spawn_from_repo(repo: Repo) -> Self {
        let (job_tx, job_rx) = crossbeam_channel::unbounded::<AsyncJob>();
        let (result_tx, result_rx) = crossbeam_channel::unbounded::<AsyncResult>();
        std::thread::spawn(move || worker_loop(repo, job_rx, result_tx));
        Self { job_tx, result_rx }
    }

    /// Submit a job without blocking. Errors only if the worker is gone.
    pub fn submit(&self, job: AsyncJob) -> Result<(), GitError> {
        self.job_tx
            .send(job)
            .map_err(|_| GitError::ChannelDisconnected)
    }

    /// Non-blocking drain for the render loop.
    pub fn try_recv(&self) -> Option<AsyncResult> {
        self.result_rx.try_recv().ok()
    }

    /// Blocking receive (tests / simple drivers).
    pub fn recv(&self) -> Result<AsyncResult, GitError> {
        self.result_rx
            .recv()
            .map_err(|_| GitError::ChannelDisconnected)
    }

    /// Blocking receive with timeout (tests).
    pub fn recv_timeout(&self, timeout: Duration) -> Option<AsyncResult> {
        self.result_rx.recv_timeout(timeout).ok()
    }
}

fn worker_loop(mut repo: Repo, jobs: Receiver<AsyncJob>, results: Sender<AsyncResult>) {
    while let Ok(job) = jobs.recv() {
        let result = execute(&mut repo, job);
        if results.send(result).is_err() {
            // TUI is gone; shut down.
            break;
        }
    }
}

fn execute(repo: &mut Repo, job: AsyncJob) -> AsyncResult {
    match job {
        AsyncJob::RefreshStatus => match repo.status() {
            Ok(st) => AsyncResult::Status(st),
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::LoadDiff { path, staged } => {
            let r = if staged {
                repo.staged_diff(&path)
            } else {
                repo.unstaged_diff(&path)
            };
            match r {
                Ok(d) => AsyncResult::Diff(d),
                Err(e) => AsyncResult::Error(e),
            }
        }
        AsyncJob::LoadFile { path } => match repo.whole_file(&path) {
            Ok(d) => AsyncResult::Diff(d),
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::LoadMarkdown { path, staged } => match repo.new_content(&path, staged) {
            Ok(text) => AsyncResult::Markdown { path, staged, text },
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::StageFile { path } => match repo.stage_file(&path) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::UnstageFile { path } => match repo.unstage_file(&path) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::DiscardFile { path } => match repo.discard_file(&path) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::StageHunk { path, hunk_index } => match repo.stage_hunk(&path, hunk_index) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::DiscardHunk {
            path,
            hunk_index,
            staged,
        } => match repo.discard_hunk(&path, hunk_index, staged) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::Commit { message } => match repo.commit(&message) {
            Ok(_) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::ListBranches => match repo.list_branches() {
            Ok(b) => AsyncResult::Branches(b),
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::CreateBranch { name } => match repo.create_branch(&name) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::CheckoutBranch { name } => match repo.checkout_branch(&name) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::DeleteBranch { name } => match repo.delete_branch(&name) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::ListLog { limit } => match repo.log(limit) {
            Ok(entries) => AsyncResult::Log(entries),
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::LoadCommitOverview { oid } => match repo.commit_overview(&oid) {
            Ok(overview) => AsyncResult::CommitOverview(overview),
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::ListStash => match repo.list_stash() {
            Ok(entries) => AsyncResult::Stash(entries),
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::StashPush { message } => match repo.stash_push(&message) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::StashPop { index } => match repo.stash_pop(index) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::StashDrop { index } => match repo.stash_drop(index) {
            Ok(()) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::Push {
            remote,
            branch,
            set_upstream,
        } => match repo.workdir() {
            Some(wd) => match crate::sync::push(&wd, &remote, &branch, set_upstream) {
                Ok(_) => AsyncResult::MutationDone,
                Err(e) => AsyncResult::Error(e),
            },
            None => AsyncResult::Error(GitError::Sync("bare repositories cannot push".into())),
        },
        AsyncJob::Pull => match repo.workdir() {
            Some(wd) => match crate::sync::pull(&wd) {
                Ok(_) => AsyncResult::MutationDone,
                Err(e) => AsyncResult::Error(e),
            },
            None => AsyncResult::Error(GitError::Sync("bare repositories cannot pull".into())),
        },
        AsyncJob::Publish {
            remote,
            url,
            branch,
        } => match crate::sync::publish(repo.inner(), &remote, &url, &branch) {
            Ok(_) => AsyncResult::MutationDone,
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::LoadSync => match crate::sync::sync_status(repo.inner()) {
            Ok(st) => AsyncResult::SyncStatus(st),
            Err(e) => AsyncResult::Error(e),
        },
        AsyncJob::GenerateCommitMessage { llm } => {
            match crate::llm::generate_commit_message(repo.inner(), &llm) {
                Ok(msg) => AsyncResult::GeneratedMessage(msg),
                Err(e) => AsyncResult::Error(e),
            }
        }
        AsyncJob::FetchModels { id, llm } => match crate::llm::fetch_models(&llm) {
            Ok(models) => AsyncResult::Models { id, models },
            Err(e) => AsyncResult::ModelsError {
                id,
                error: e.to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::FileState;
    use crate::testutil;
    use std::time::Duration;

    fn recv_next(queue: &JobQueue) -> AsyncResult {
        queue
            .recv_timeout(Duration::from_secs(5))
            .expect("worker should answer within 5s")
    }

    fn expect_status(result: AsyncResult) -> RepoStatus {
        match result {
            AsyncResult::Status(st) => st,
            other => panic!("expected Status, got {other:?}"),
        }
    }

    fn expect_diff(result: AsyncResult) -> FileDiff {
        match result {
            AsyncResult::Diff(d) => d,
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    fn expect_mutation_done(result: AsyncResult) {
        match result {
            AsyncResult::MutationDone => {}
            other => panic!("expected MutationDone, got {other:?}"),
        }
    }

    #[test]
    fn refresh_status_returns_current_branch_and_files() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue.submit(AsyncJob::RefreshStatus).unwrap();
        let st = expect_status(recv_next(&queue));
        assert_eq!(st.branch, "main");
        assert!(st.files.iter().any(|e| e.path == "a.txt"));
    }

    #[test]
    fn load_unstaged_diff_returns_hunks() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "hello\n", "init");
        testutil::dirty_file(&repo, "a.txt", "world\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::LoadDiff {
                path: "a.txt".into(),
                staged: false,
            })
            .unwrap();
        let d = expect_diff(recv_next(&queue));
        assert_eq!(d.path, "a.txt");
        assert!(!d.hunks.is_empty());
    }

    #[test]
    fn load_file_returns_whole_clean_file_as_context() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "one\ntwo\n", "init");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::LoadFile {
                path: "a.txt".into(),
            })
            .unwrap();
        let d = expect_diff(recv_next(&queue));
        assert_eq!(d.path, "a.txt");
        assert_eq!(d.hunks.len(), 1);
        assert!(
            d.hunks[0]
                .lines
                .iter()
                .all(|l| l.kind == crate::diff::LineKind::Context),
            "expected all context: {d:?}"
        );
        let texts: Vec<&str> = d.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["one", "two"]);
    }

    #[test]
    fn load_staged_diff_empty_then_nonempty_after_stage() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();

        queue
            .submit(AsyncJob::LoadDiff {
                path: "a.txt".into(),
                staged: true,
            })
            .unwrap();
        assert!(expect_diff(recv_next(&queue)).hunks.is_empty());

        queue
            .submit(AsyncJob::StageFile {
                path: "a.txt".into(),
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));

        queue
            .submit(AsyncJob::LoadDiff {
                path: "a.txt".into(),
                staged: true,
            })
            .unwrap();
        assert!(!expect_diff(recv_next(&queue)).hunks.is_empty());
    }

    #[test]
    fn stage_file_then_status_shows_staged() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::StageFile {
                path: "a.txt".into(),
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));
        queue.submit(AsyncJob::RefreshStatus).unwrap();
        let st = expect_status(recv_next(&queue));
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, FileState::Staged);
    }

    #[test]
    fn stage_hunk_partially_stages_then_commit() {
        let (_dir, repo) = testutil::init_repo();
        let base = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        testutil::commit_file(&repo, "a.txt", &base, "init");
        let workdir = repo.workdir().unwrap().to_path_buf();
        let dirty = base.replacen("line 5\n", "line 5 CHANGED\n", 1).replacen(
            "line 35\n",
            "line 35 CHANGED\n",
            1,
        );
        std::fs::write(workdir.join("a.txt"), &dirty).unwrap();
        drop(repo);
        let queue = JobQueue::spawn(&workdir).unwrap();

        queue
            .submit(AsyncJob::StageHunk {
                path: "a.txt".into(),
                hunk_index: 0,
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));

        // Mutation invalidation: refresh shows partially staged file.
        queue.submit(AsyncJob::RefreshStatus).unwrap();
        let st = expect_status(recv_next(&queue));
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, FileState::BothStagedAndUnstaged);

        queue
            .submit(AsyncJob::Commit {
                message: "partial via queue".into(),
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));

        queue.submit(AsyncJob::RefreshStatus).unwrap();
        let st = expect_status(recv_next(&queue));
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, FileState::Unstaged);
        assert_eq!(st.head_summary, "partial via queue");
    }

    #[test]
    fn unstage_file_restores_unstaged_state() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::StageFile {
                path: "a.txt".into(),
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));
        queue
            .submit(AsyncJob::UnstageFile {
                path: "a.txt".into(),
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));
        queue.submit(AsyncJob::RefreshStatus).unwrap();
        let st = expect_status(recv_next(&queue));
        let entry = st.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, FileState::Unstaged);
    }

    #[test]
    fn discard_file_via_queue_restores_clean_tree() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::DiscardFile {
                path: "a.txt".into(),
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));
        queue.submit(AsyncJob::RefreshStatus).unwrap();
        let st = expect_status(recv_next(&queue));
        assert!(st.files.iter().all(|e| e.path != "a.txt"));
    }

    #[test]
    fn out_of_range_hunk_returns_error_and_worker_survives() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::StageHunk {
                path: "a.txt".into(),
                hunk_index: 99,
            })
            .unwrap();
        match recv_next(&queue) {
            AsyncResult::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
        // Worker still alive for the next job.
        queue.submit(AsyncJob::RefreshStatus).unwrap();
        let st = expect_status(recv_next(&queue));
        assert!(st.files.iter().any(|e| e.path == "a.txt"));
    }

    #[test]
    fn empty_commit_returns_error() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::Commit {
                message: "nothing staged".into(),
            })
            .unwrap();
        match recv_next(&queue) {
            AsyncResult::Error(crate::error::GitError::EmptyCommit) => {}
            other => panic!("expected EmptyCommit error, got {other:?}"),
        }
    }

    #[test]
    fn jobs_are_processed_in_fifo_order() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        // Submit three jobs back-to-back; results must arrive in order.
        queue.submit(AsyncJob::RefreshStatus).unwrap();
        queue
            .submit(AsyncJob::LoadDiff {
                path: "a.txt".into(),
                staged: false,
            })
            .unwrap();
        queue.submit(AsyncJob::RefreshStatus).unwrap();
        let first = recv_next(&queue);
        let second = recv_next(&queue);
        let third = recv_next(&queue);
        assert!(matches!(first, AsyncResult::Status(_)), "got {first:?}");
        assert!(matches!(second, AsyncResult::Diff(_)), "got {second:?}");
        assert!(matches!(third, AsyncResult::Status(_)), "got {third:?}");
    }

    #[test]
    fn generate_commit_message_with_nothing_staged_errors_without_network() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::GenerateCommitMessage {
                llm: crate::llm::LlmConfig::default(),
            })
            .unwrap();
        match recv_next(&queue) {
            AsyncResult::Error(crate::error::GitError::Llm(msg)) => {
                assert!(msg.contains("nothing staged"), "got: {msg}");
            }
            other => panic!("expected Llm error, got {other:?}"),
        }
    }

    #[test]
    fn generate_commit_message_without_key_errors_with_hint() {
        // Env vars are process-global: hold the shared lock and restore
        // afterwards so parallel tests never observe our removals.
        let _guard = testutil::ENV_LOCK.lock().unwrap();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        let prev_llm = std::env::var("LLM_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("LLM_API_KEY");
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        crate::stage::stage_file(&repo, "a.txt").unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue
            .submit(AsyncJob::GenerateCommitMessage {
                llm: crate::llm::LlmConfig::default(),
            })
            .unwrap();
        let result = recv_next(&queue);
        match prev_openai {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        match prev_llm {
            Some(v) => std::env::set_var("LLM_API_KEY", v),
            None => std::env::remove_var("LLM_API_KEY"),
        }
        match result {
            AsyncResult::Error(crate::error::GitError::Llm(msg)) => {
                assert!(msg.contains("API key"), "got: {msg}");
            }
            other => panic!("expected missing-key Llm error, got {other:?}"),
        }
    }

    #[test]
    fn spawn_with_bad_path_errors() {
        match JobQueue::spawn("/nonexistent-path-xyz-123") {
            Err(crate::error::GitError::NotARepo(_)) => {}
            Err(other) => panic!("wrong error: {other:?}"),
            Ok(_) => panic!("expected NotARepo error"),
        }
    }

    fn branch_names(result: AsyncResult) -> Vec<String> {
        match result {
            AsyncResult::Branches(b) => b.into_iter().map(|x| x.name).collect(),
            other => panic!("expected Branches, got {other:?}"),
        }
    }

    #[test]
    fn list_branches_via_queue() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::new_branch(&repo, "side");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue.submit(AsyncJob::ListBranches).unwrap();
        let names = branch_names(recv_next(&queue));
        assert!(names.contains(&"main".to_string()), "got {names:?}");
        assert!(names.contains(&"side".to_string()), "got {names:?}");
    }

    #[test]
    fn create_checkout_delete_branch_via_queue() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();

        queue
            .submit(AsyncJob::CreateBranch {
                name: "feat".into(),
            })
            .unwrap();
        assert!(matches!(recv_next(&queue), AsyncResult::MutationDone));

        queue
            .submit(AsyncJob::CheckoutBranch {
                name: "feat".into(),
            })
            .unwrap();
        assert!(matches!(recv_next(&queue), AsyncResult::MutationDone));

        // Cannot delete the checked-out branch.
        queue
            .submit(AsyncJob::DeleteBranch {
                name: "feat".into(),
            })
            .unwrap();
        assert!(matches!(recv_next(&queue), AsyncResult::Error(_)));

        queue
            .submit(AsyncJob::CheckoutBranch {
                name: "main".into(),
            })
            .unwrap();
        assert!(matches!(recv_next(&queue), AsyncResult::MutationDone));

        queue
            .submit(AsyncJob::DeleteBranch {
                name: "feat".into(),
            })
            .unwrap();
        assert!(matches!(recv_next(&queue), AsyncResult::MutationDone));

        queue.submit(AsyncJob::ListBranches).unwrap();
        let names = branch_names(recv_next(&queue));
        assert!(!names.contains(&"feat".to_string()), "got {names:?}");
    }

    #[test]
    fn list_log_via_queue() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "one");
        testutil::commit_file(&repo, "a.txt", "a\nb\n", "two");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();
        queue.submit(AsyncJob::ListLog { limit: 50 }).unwrap();
        match recv_next(&queue) {
            AsyncResult::Log(entries) => {
                let summaries: Vec<&str> = entries.iter().map(|e| e.summary.as_str()).collect();
                assert_eq!(summaries, ["two", "one"]);
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn stash_push_pop_drop_via_queue() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "work\n");
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();

        queue
            .submit(AsyncJob::StashPush {
                message: "wip".into(),
            })
            .unwrap();
        assert!(matches!(recv_next(&queue), AsyncResult::MutationDone));

        queue.submit(AsyncJob::ListStash).unwrap();
        match recv_next(&queue) {
            AsyncResult::Stash(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(entries[0].message.contains("wip"));
            }
            other => panic!("expected Stash, got {other:?}"),
        }

        queue.submit(AsyncJob::StashPop { index: 0 }).unwrap();
        assert!(matches!(recv_next(&queue), AsyncResult::MutationDone));

        queue.submit(AsyncJob::ListStash).unwrap();
        match recv_next(&queue) {
            AsyncResult::Stash(entries) => assert!(entries.is_empty()),
            other => panic!("expected Stash, got {other:?}"),
        }

        // Nothing left to drop.
        queue.submit(AsyncJob::StashDrop { index: 0 }).unwrap();
        assert!(matches!(recv_next(&queue), AsyncResult::Error(_)));
    }

    fn expect_sync(result: AsyncResult) -> crate::sync::SyncStatus {
        match result {
            AsyncResult::SyncStatus(st) => st,
            other => panic!("expected SyncStatus, got {other:?}"),
        }
    }

    #[test]
    fn push_pull_and_sync_status_via_queue() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let origin_path = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        crate::sync::add_remote(&repo, "origin", origin_path.to_str().unwrap()).unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();

        queue
            .submit(AsyncJob::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: true,
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));

        queue.submit(AsyncJob::LoadSync).unwrap();
        let st = expect_sync(recv_next(&queue));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (0, 0));

        // Advance the remote from a second clone, then pull it back.
        // The bare repo's HEAD still points at unborn master (init_bare
        // default), so aim it at main before cloning — like a real host.
        git2::Repository::open_bare(&origin_path)
            .unwrap()
            .set_head("refs/heads/main")
            .unwrap();
        let (dir2, other) = testutil::clone_local(&origin_path);
        testutil::commit_file(&other, "b.txt", "b\n", "from other");
        let other_path = other.workdir().unwrap().to_path_buf();
        drop(other);
        let push_queue = JobQueue::spawn(other_path).unwrap();
        push_queue
            .submit(AsyncJob::Push {
                remote: "origin".into(),
                branch: "main".into(),
                set_upstream: false,
            })
            .unwrap();
        expect_mutation_done(recv_next(&push_queue));
        drop(dir2);

        queue.submit(AsyncJob::Pull).unwrap();
        expect_mutation_done(recv_next(&queue));

        queue.submit(AsyncJob::LoadSync).unwrap();
        let st = expect_sync(recv_next(&queue));
        assert_eq!((st.ahead, st.behind), (0, 0));
    }

    #[test]
    fn publish_new_repo_via_queue() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let origin_path = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        let path = repo.workdir().unwrap().to_path_buf();
        drop(repo);
        let queue = JobQueue::spawn(path).unwrap();

        queue
            .submit(AsyncJob::Publish {
                remote: "origin".into(),
                url: origin_path.to_str().unwrap().into(),
                branch: "main".into(),
            })
            .unwrap();
        expect_mutation_done(recv_next(&queue));

        queue.submit(AsyncJob::LoadSync).unwrap();
        let st = expect_sync(recv_next(&queue));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        let remote = git2::Repository::open_bare(&origin_path).unwrap();
        assert!(remote.find_reference("refs/heads/main").is_ok());
    }
}
