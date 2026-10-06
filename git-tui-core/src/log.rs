//! Commit history (read-only).

use crate::error::GitError;

/// Owned one-line commit summary — sendable across the job channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// Short id (7 hex chars).
    pub id: String,
    /// Full commit oid (hex), used to join lanes of the graph.
    pub oid: String,
    pub summary: String,
    pub author: String,
    /// Full parent oids (hex), oldest-last as stored by git.
    pub parents: Vec<String>,
    /// Decorations pointing at this commit: `HEAD -> main`, branch names,
    /// `tag: v1`. Empty when nothing points here.
    pub refs: Vec<String>,
    /// True when the commit exists on the upstream (already pushed).
    /// False for local-only commits (no upstream, or ahead of it), which
    /// the TUI renders with an open dot.
    pub pushed: bool,
}

impl CommitInfo {
    /// True for merge commits (more than one parent).
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// One rendered graph row: which lane the commit sits on and how many
/// lanes are active on that row. Computed by [`compute_lanes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphRow {
    pub lane: usize,
    pub width: usize,
    pub is_merge: bool,
}

/// Assign lanes newest-first (the order [`log`] returns), like
/// `git log --graph`: the first parent keeps the commit's lane, extra
/// parents fork new lanes, and a parent that is already active closes
/// the current lane into it instead of duplicating it.
pub fn compute_lanes(entries: &[CommitInfo]) -> Vec<GraphRow> {
    let mut active: Vec<String> = Vec::new();
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let lane = active
            .iter()
            .position(|o| *o == entry.oid)
            .unwrap_or(active.len());
        let width = active.len().max(lane + 1).max(1);
        // Replace the commit's lane slot with its parents: first parent
        // stays on this lane, extra parents fork; parents already active
        // elsewhere just close this lane into them (no duplicate lanes).
        if lane < active.len() {
            active.remove(lane);
        }
        let mut insert_at = lane.min(active.len());
        let mut seen_here = false;
        for parent in &entry.parents {
            if active.contains(parent) {
                continue;
            }
            if !seen_here {
                active.insert(insert_at, parent.clone());
                insert_at += 1;
                seen_here = true;
            } else {
                active.insert(insert_at, parent.clone());
                insert_at += 1;
            }
        }
        rows.push(GraphRow {
            lane,
            width,
            is_merge: entry.is_merge(),
        });
    }
    rows
}

/// Newest-first history from HEAD, up to `limit` entries.
/// Empty repos yield an empty list (not an error).
/// Each entry carries its parents and ref decorations so the TUI can
/// render a `git log --graph` style lane view without touching git types.
pub fn log(repo: &git2::Repository, limit: usize) -> Result<Vec<CommitInfo>, GitError> {
    let mut walk = repo
        .revwalk()
        .map_err(|e| GitError::Log(format!("cannot walk history: {e}")))?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)
        .map_err(|e| GitError::Log(format!("cannot sort history: {e}")))?;
    // Unborn HEAD: no history yet.
    if walk.push_head().is_err() {
        return Ok(Vec::new());
    }
    let oids: Vec<git2::Oid> = walk
        .take(limit.max(1))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| GitError::Log(format!("cannot read commit: {e}")))?;

    // Local branches by tip oid, for `(side)` labels.
    let mut tips: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    if let Ok(branches) = repo.branches(Some(git2::BranchType::Local)) {
        for item in branches.flatten() {
            let (branch, _) = item;
            let name = branch
                .name()
                .ok()
                .flatten()
                .unwrap_or("(invalid)")
                .to_string();
            if let Ok(commit) = branch.get().peel_to_commit() {
                tips.entry(commit.id().to_string()).or_default().push(name);
            }
        }
    }
    // HEAD target: branch name, or detached oid.
    let head_branch: Option<String> = match repo.head() {
        Ok(head) if head.is_branch() => head.shorthand().map(|s| s.to_string()),
        _ => None,
    };
    let head_oid: Option<String> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| c.id().to_string());
    let head_detached = repo.head_detached().unwrap_or(false);

    // Tags by target oid, for `tag: v1` labels.
    let mut tags: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    if let Ok(tag_names) = repo.tag_names(None) {
        for name in tag_names.iter().flatten() {
            let target = repo
                .revparse_single(&format!("refs/tags/{name}"))
                .ok()
                .and_then(|o| o.peel_to_commit().ok())
                .map(|c| c.id().to_string());
            if let Some(oid) = target {
                tags.entry(oid).or_default().push(name.to_string());
            }
        }
    }

    let mut out = Vec::with_capacity(oids.len());
    // A commit counts as pushed when it exists on the remote: i.e. it is
    // an ancestor of (or equal to) any remote-tracking tip. This matches
    // `ahead` for a tracked branch, and still marks shared ancestors as
    // pushed on untracked branches (a fresh branch off pushed `main` keeps
    // its base filled, only the new commits render open). With no remotes,
    // nothing is pushed, so every dot renders open.
    let remote_tips = remote_tip_oids(repo);
    for oid in oids {
        let commit = repo
            .find_commit(oid)
            .map_err(|e| GitError::Log(format!("cannot read commit: {e}")))?;
        let id = format!("{oid:.7}");
        let summary = commit
            .message()
            .unwrap_or("(empty message)")
            .lines()
            .next()
            .unwrap_or("(empty message)")
            .trim()
            .to_string();
        let author = commit.author().name().unwrap_or("?").to_string();
        let parents: Vec<String> = commit.parent_ids().map(|p| p.to_string()).collect();
        let key = oid.to_string();
        let mut refs: Vec<String> = Vec::new();
        if let Some(names) = tips.get(&key) {
            let mut names = names.clone();
            names.sort();
            for name in names {
                if Some(name.as_str()) == head_branch.as_deref() {
                    refs.push(format!("HEAD -> {name}"));
                } else {
                    refs.push(name);
                }
            }
        }
        if head_detached && head_oid.as_deref() == Some(key.as_str()) {
            refs.insert(0, "HEAD".to_string());
        }
        if let Some(names) = tags.get(&key) {
            let mut names = names.clone();
            names.sort();
            for name in names {
                refs.push(format!("tag: {name}"));
            }
        }
        out.push(CommitInfo {
            id,
            oid: key.clone(),
            summary,
            author,
            parents,
            refs,
            pushed: is_pushed(repo, &remote_tips, &key),
        });
    }
    Ok(out)
}

/// Tip oids of all remote-tracking branches (`refs/remotes/*`).
fn remote_tip_oids(repo: &git2::Repository) -> Vec<git2::Oid> {
    let mut tips = Vec::new();
    if let Ok(branches) = repo.branches(Some(git2::BranchType::Remote)) {
        for item in branches.flatten() {
            let (branch, _) = item;
            if let Ok(commit) = branch.get().peel_to_commit() {
                tips.push(commit.id());
            }
        }
    }
    tips
}

/// True when `oid_hex` is reachable from any remote tip (ancestor or equal),
/// i.e. the commit already exists on the remote. Graph errors fall back to
/// pushed (filled) so a transient read failure never paints the whole
/// history as unpushed.
fn is_pushed(repo: &git2::Repository, remote_tips: &[git2::Oid], oid_hex: &str) -> bool {
    if remote_tips.is_empty() {
        return false;
    }
    let oid = match git2::Oid::from_str(oid_hex) {
        Ok(o) => o,
        Err(_) => return true,
    };
    for tip in remote_tips {
        if *tip == oid {
            return true;
        }
        match repo.graph_descendant_of(*tip, oid) {
            Ok(true) => return true,
            Ok(false) => continue,
            Err(_) => return true,
        }
    }
    false
}

/// One changed file within a commit, `git log --stat` style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFileStat {
    pub path: String,
    pub insertions: usize,
    pub deletions: usize,
    /// `A`/`D`/`M`/`R`/`C`/`T`, matching `git status --short`.
    pub status: char,
}

/// Full detail for one commit, for the commit overview panel: the
/// message split into summary/body, author identity, and a `--stat`
/// style file list against its first parent (or the empty tree, for a
/// root commit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOverview {
    pub id: String,
    pub oid: String,
    pub author: String,
    pub email: String,
    /// `YYYY-MM-DD HH:MM:SS +HHMM`, in the commit's own timezone.
    pub date: String,
    pub summary: String,
    /// Message body after the summary line, trimmed. Empty when the
    /// commit message is a single line.
    pub body: String,
    pub parents: usize,
    pub files: Vec<CommitFileStat>,
    pub insertions: usize,
    pub deletions: usize,
}

/// Look up one commit's full detail by its hex oid.
pub fn commit_overview(repo: &git2::Repository, oid: &str) -> Result<CommitOverview, GitError> {
    let parsed =
        git2::Oid::from_str(oid).map_err(|e| GitError::Log(format!("bad commit id: {e}")))?;
    let commit = repo
        .find_commit(parsed)
        .map_err(|e| GitError::Log(format!("cannot read commit: {e}")))?;
    let tree = commit
        .tree()
        .map_err(|e| GitError::Log(format!("cannot read tree: {e}")))?;
    let parent = commit.parents().next();
    let parent_tree = match &parent {
        Some(p) => Some(
            p.tree()
                .map_err(|e| GitError::Log(format!("cannot read parent tree: {e}")))?,
        ),
        None => None,
    };
    let diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
        .map_err(|e| GitError::Log(format!("cannot diff commit: {e}")))?;

    let mut files: Vec<CommitFileStat> = Vec::with_capacity(diff.deltas().len());
    for i in 0..diff.deltas().len() {
        let delta = diff.get_delta(i).expect("index within deltas().len()");
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let status = match delta.status() {
            git2::Delta::Added => 'A',
            git2::Delta::Deleted => 'D',
            git2::Delta::Renamed => 'R',
            git2::Delta::Copied => 'C',
            git2::Delta::Typechange => 'T',
            _ => 'M',
        };
        let (insertions, deletions) = git2::Patch::from_diff(&diff, i)
            .ok()
            .flatten()
            .and_then(|p| p.line_stats().ok())
            .map(|(_, ins, del)| (ins, del))
            .unwrap_or((0, 0));
        files.push(CommitFileStat {
            path,
            insertions,
            deletions,
            status,
        });
    }
    let stats = diff
        .stats()
        .map_err(|e| GitError::Log(format!("cannot stat diff: {e}")))?;

    let message = commit.message().unwrap_or("").to_string();
    let body = message
        .split_once("\n\n")
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .trim()
        .to_string();
    let author = commit.author();

    Ok(CommitOverview {
        id: oid.chars().take(7).collect(),
        oid: oid.to_string(),
        author: author.name().unwrap_or("?").to_string(),
        email: author.email().unwrap_or("").to_string(),
        date: format_commit_time(commit.time()),
        summary: commit.summary().unwrap_or("").to_string(),
        body,
        parents: commit.parent_count(),
        files,
        insertions: stats.insertions(),
        deletions: stats.deletions(),
    })
}

/// `YYYY-MM-DD HH:MM:SS +HHMM` in the commit's own timezone, with no
/// calendar dependency: just enough formatting for the overview panel.
fn format_commit_time(time: git2::Time) -> String {
    let offset_min = time.offset_minutes();
    let local_secs = time.seconds() + i64::from(offset_min) * 60;
    let days = local_secs.div_euclid(86400);
    let secs_of_day = local_secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    let sign = if offset_min >= 0 { '+' } else { '-' };
    let off = offset_min.unsigned_abs();
    format!(
        "{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} {sign}{:02}{:02}",
        off / 60,
        off % 60
    )
}

/// Days-since-epoch to a proleptic Gregorian (year, month, day), per
/// Howard Hinnant's `civil_from_days` — avoids pulling in a calendar
/// crate for one date label.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn lists_commits_newest_first() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "first");
        testutil::commit_file(&repo, "a.txt", "a\nb\n", "second");
        testutil::commit_file(&repo, "a.txt", "a\nb\nc\n", "third");
        let entries = log(&repo, 50).unwrap();
        let summaries: Vec<&str> = entries.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, ["third", "second", "first"], "got {summaries:?}");
        assert_eq!(entries[0].id.len(), 7);
        assert_eq!(entries[0].author, "Test User");
    }

    #[test]
    fn empty_repo_yields_empty_log() {
        let (_dir, repo) = testutil::init_repo();
        assert!(log(&repo, 50).unwrap().is_empty());
    }

    #[test]
    fn limit_is_respected() {
        let (_dir, repo) = testutil::init_repo();
        for i in 0..5 {
            testutil::commit_file(&repo, "a.txt", &format!("v{i}\n"), &format!("c{i}"));
        }
        assert_eq!(log(&repo, 2).unwrap().len(), 2);
    }

    #[test]
    fn linear_history_stays_on_lane_zero() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "first");
        testutil::commit_file(&repo, "a.txt", "a\nb\n", "second");
        let entries = log(&repo, 50).unwrap();
        assert_eq!(entries.len(), 2);
        // Linear chain: single parent each, newest first.
        assert_eq!(entries[0].parents.len(), 1);
        assert_eq!(entries[1].parents.len(), 0);
        let lanes = compute_lanes(&entries);
        assert!(lanes.iter().all(|r| r.lane == 0 && r.width == 1));
        assert!(lanes.iter().all(|r| !r.is_merge));
    }

    #[test]
    fn merge_commit_is_flagged_and_forks_lanes() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "base");
        // Side branch with one commit.
        testutil::new_branch(&repo, "side");
        testutil::commit_file(&repo, "a.txt", "a\nside\n", "side work");
        let side_oid = repo.head().unwrap().peel_to_commit().unwrap().id();
        // Back on main with one commit.
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        testutil::commit_file(&repo, "a.txt", "a\nmain\n", "main work");
        let main_oid = repo.head().unwrap().peel_to_commit().unwrap().id();
        // Merge side into main (no conflicts: different content lines).
        let sig = repo.signature().unwrap();
        let main_commit = repo.find_commit(main_oid).unwrap();
        let side_commit = repo.find_commit(side_oid).unwrap();
        // Build a merge tree from main's tree (content is irrelevant here).
        let tree = main_commit.tree().unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "merge side",
            &tree,
            &[&main_commit, &side_commit],
        )
        .unwrap();

        let entries = log(&repo, 50).unwrap();
        let merge = entries.iter().find(|e| e.summary == "merge side").unwrap();
        assert!(merge.is_merge(), "merge must be flagged: {merge:?}");
        assert_eq!(merge.parents.len(), 2);
        let lanes = compute_lanes(&entries);
        assert_eq!(lanes.len(), entries.len());
        // The merge row itself is a merge; lanes must widen past 1
        // somewhere to show the fork.
        let merge_row = lanes[entries
            .iter()
            .position(|e| e.summary == "merge side")
            .unwrap()];
        assert!(merge_row.is_merge);
        assert!(
            lanes.iter().any(|r| r.width > 1),
            "fork must widen lanes: {lanes:?}"
        );
    }

    #[test]
    fn branch_refs_decorate_tips() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "base");
        testutil::new_branch(&repo, "side");
        testutil::commit_file(&repo, "a.txt", "a\nside\n", "side work");
        let entries = log(&repo, 50).unwrap();
        // On side: tip carries `HEAD -> side`; base carries `main`.
        let tip_refs: Vec<String> = entries[0].refs.clone();
        assert!(
            tip_refs.iter().any(|r| r == "HEAD -> side"),
            "tip refs missing HEAD -> side: {tip_refs:?}"
        );
        let base_refs: Vec<String> = entries.last().unwrap().refs.clone();
        assert!(
            base_refs.iter().any(|r| r == "main"),
            "base refs missing main: {base_refs:?}"
        );
    }

    #[test]
    fn commit_overview_splits_message_and_stats_files() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "first");
        let oid = testutil::commit_file(
            &repo,
            "a.txt",
            "a\nb\nc\n",
            "second\n\nmore detail\nsecond line of body",
        );
        let overview = commit_overview(&repo, &oid.to_string()).unwrap();
        assert_eq!(overview.summary, "second");
        assert_eq!(overview.body, "more detail\nsecond line of body");
        assert_eq!(overview.author, "Test User");
        assert_eq!(overview.email, "test@example.com");
        assert_eq!(overview.parents, 1);
        assert_eq!(overview.files.len(), 1);
        assert_eq!(overview.files[0].path, "a.txt");
        assert_eq!(overview.files[0].status, 'M');
        assert_eq!(overview.files[0].insertions, 2);
        assert_eq!(overview.insertions, 2);
        assert_eq!(overview.deletions, 0);
    }

    #[test]
    fn commit_overview_root_commit_diffs_against_empty_tree() {
        let (_dir, repo) = testutil::init_repo();
        let oid = testutil::commit_file(&repo, "a.txt", "a\nb\n", "init");
        let overview = commit_overview(&repo, &oid.to_string()).unwrap();
        assert_eq!(overview.parents, 0);
        assert_eq!(overview.files.len(), 1);
        assert_eq!(overview.files[0].status, 'A');
        assert_eq!(overview.files[0].insertions, 2);
    }

    #[test]
    fn commit_overview_single_line_message_has_empty_body() {
        let (_dir, repo) = testutil::init_repo();
        let oid = testutil::commit_file(&repo, "a.txt", "a\n", "only a summary");
        let overview = commit_overview(&repo, &oid.to_string()).unwrap();
        assert_eq!(overview.summary, "only a summary");
        assert_eq!(overview.body, "");
    }

    #[test]
    fn commit_overview_rejects_bad_oid() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        assert!(commit_overview(&repo, "not-an-oid").is_err());
    }

    #[test]
    fn civil_from_days_round_trips_known_dates() {
        // Unix epoch.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // A known later date.
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
    }

    #[test]
    fn format_commit_time_applies_timezone_offset() {
        // 2024-01-01 00:00:00 UTC, +05:30 offset: local clock reads 05:30.
        let utc = format_commit_time(git2::Time::new(1704067200, 0));
        assert_eq!(utc, "2024-01-01 00:00:00 +0000");
        let plus = format_commit_time(git2::Time::new(1704067200, 330));
        assert_eq!(plus, "2024-01-01 05:30:00 +0530");
    }

    #[test]
    fn commits_without_remotes_are_unpushed() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "first");
        testutil::commit_file(&repo, "a.txt", "a\nb\n", "second");
        let entries = log(&repo, 50).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries.iter().all(|e| !e.pushed),
            "no remotes means nothing is pushed: {entries:?}"
        );
    }

    #[test]
    fn pushed_commits_are_marked_and_local_tip_is_not() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let origin_path = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        crate::sync::add_remote(&repo, "origin", origin_path.to_str().unwrap()).unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        crate::sync::push(&workdir, "origin", "main", true).unwrap();
        let pushed_entries = log(&repo, 50).unwrap();
        assert!(
            pushed_entries.iter().all(|e| e.pushed),
            "after push everything should be pushed: {pushed_entries:?}"
        );
        // One more local commit: only the tip is unpushed.
        testutil::commit_file(&repo, "a.txt", "a\nlocal\n", "local work");
        // Refresh the remote-tracking ref (fetch) so the comparison is
        // against the last-pushed state.
        std::process::Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(&workdir)
            .output()
            .unwrap();
        let entries = log(&repo, 50).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].summary, "local work");
        assert!(!entries[0].pushed, "tip should be unpushed: {entries:?}");
        assert!(entries[1].pushed, "base should stay pushed: {entries:?}");
    }

    #[test]
    fn untracked_branch_keeps_shared_base_pushed() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let origin_dir = tempfile::TempDir::new().unwrap();
        let origin_path = origin_dir.path().join("origin.git");
        git2::Repository::init_bare(&origin_path).unwrap();
        crate::sync::add_remote(&repo, "origin", origin_path.to_str().unwrap()).unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        crate::sync::push(&workdir, "origin", "main", true).unwrap();
        std::process::Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(&workdir)
            .output()
            .unwrap();
        // New local branch with no upstream of its own.
        testutil::new_branch(&repo, "feature");
        testutil::commit_file(&repo, "a.txt", "a\nfeature\n", "feature work");
        let entries = log(&repo, 50).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(!entries[0].pushed, "feature tip is local-only: {entries:?}");
        assert!(
            entries[1].pushed,
            "shared base is on the remote: {entries:?}"
        );
    }
}
