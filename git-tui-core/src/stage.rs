//! Staging operations.

use crate::diff::LineKind;
use crate::error::GitError;
use std::path::Path;

/// Stage a whole file (workdir -> index).
pub fn stage_file(repo: &git2::Repository, path: &str) -> Result<(), GitError> {
    let mut index = repo.index()?;
    index
        .add_path(Path::new(path))
        .map_err(|e| GitError::HunkStaging(format!("cannot stage file {path}: {e}")))?;
    index.write()?;
    Ok(())
}

/// Unstage a whole file (index -> HEAD). New files are removed from the index.
pub fn unstage_file(repo: &git2::Repository, path: &str) -> Result<(), GitError> {
    // If HEAD exists, reset this path in the index to HEAD.
    if let Ok(head) = repo.head() {
        if let Ok(target) = head.peel_to_commit() {
            let obj = target.as_object().clone();
            repo.reset_default(Some(&obj), [path])?;
            // reset_default updates the index on disk; ensure it is written.
            return Ok(());
        }
    }
    // No HEAD (or unborn): drop the path from the index if present.
    let mut index = repo.index()?;
    // remove_path errors if absent; treat as success (already unstaged).
    let _ = index.remove_path(Path::new(path));
    index.write()?;
    Ok(())
}

/// Discard all changes in a file (staged and unstaged), restoring it to
/// HEAD. Untracked files are deleted from the workdir. Staged-new files
/// (added but not in HEAD) are unindexed and deleted. Clean files and
/// conflicted files are errors.
pub fn discard_file(repo: &git2::Repository, path: &str) -> Result<(), GitError> {
    use git2::Status as S;
    let rel = Path::new(path);
    let flags = repo
        .status_file(rel)
        .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
    if flags.contains(S::CONFLICTED) {
        return Err(GitError::Discard(format!(
            "conflicted: resolve markers in {path} first"
        )));
    }
    let index_bits = S::INDEX_NEW
        | S::INDEX_MODIFIED
        | S::INDEX_DELETED
        | S::INDEX_RENAMED
        | S::INDEX_TYPECHANGE;
    let workdir_bits = S::WT_MODIFIED | S::WT_DELETED | S::WT_RENAMED | S::WT_TYPECHANGE;
    let staged = flags.intersects(index_bits);
    let unstaged = flags.intersects(workdir_bits);
    let wt_new = flags.contains(S::WT_NEW);

    // Untracked: only WT_NEW, nothing in the index. Deleting restores a
    // clean tree; resetting/checkout would be a no-op.
    if !staged && !unstaged && wt_new {
        return remove_workdir_path(repo, path);
    }
    if !staged && !unstaged {
        return Err(GitError::Discard(format!(
            "{path} is unchanged — nothing to discard"
        )));
    }

    match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(commit) => {
            let in_head = commit
                .tree()
                .ok()
                .and_then(|t| t.get_path(rel).ok())
                .is_some();
            let obj = commit.as_object().clone();
            repo.reset_default(Some(&obj), [path])
                .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
            if in_head {
                // Tracked in HEAD: restore the workdir copy (also brings
                // back files deleted from the workdir).
                let mut builder = git2::build::CheckoutBuilder::new();
                builder.force().path(path);
                repo.checkout_head(Some(&mut builder))
                    .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
            } else {
                // Added (staged-new): unstage left it untracked, so delete
                // the workdir copy to actually discard it.
                remove_workdir_path(repo, path)?;
            }
            Ok(())
        }
        None => {
            // Unborn HEAD: everything is new, so drop the index entry and
            // delete the workdir copy.
            let mut index = repo.index()?;
            let _ = index.remove_path(rel);
            index.write()?;
            remove_workdir_path(repo, path)?;
            Ok(())
        }
    }
}

/// Delete `path` from the workdir (file or directory). Missing paths are
/// success (already discarded).
fn remove_workdir_path(repo: &git2::Repository, path: &str) -> Result<(), GitError> {
    let Some(workdir) = repo.workdir() else {
        return Err(GitError::Discard("bare repo".into()));
    };
    let full = workdir.join(Path::new(path));
    if !full.exists() && !full.is_symlink() {
        return Ok(());
    }
    if full.is_dir() && !full.is_symlink() {
        std::fs::remove_dir_all(&full)
            .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
    } else {
        std::fs::remove_file(&full)
            .map_err(|e| GitError::Discard(format!("cannot discard {path}: {e}")))?;
    }
    Ok(())
}
/// Stage a single hunk (by index into the unstaged diff) via partial-index
/// application: only that hunk's changes are written to the index.
pub fn stage_hunk(repo: &git2::Repository, path: &str, hunk_index: usize) -> Result<(), GitError> {
    let diff = crate::diff::unstaged_diff(repo, path)?;
    if hunk_index >= diff.hunks.len() {
        return Err(GitError::HunkStaging(format!(
            "hunk index {hunk_index} out of range ({} hunks in {path})",
            diff.hunks.len()
        )));
    }
    if diff.hunks.is_empty() {
        return Err(GitError::HunkStaging(format!(
            "no hunks to stage in {path}"
        )));
    }
    let hunk = &diff.hunks[hunk_index];
    let (old_start, old_lines) = parse_hunk_header(&hunk.header).ok_or_else(|| {
        GitError::HunkStaging(format!("cannot parse hunk header: {}", hunk.header))
    })?;

    let base = index_blob_content(repo, path)?;
    let base_ends_newline = base.ends_with('\n') || base.is_empty();
    let mut base_lines: Vec<String> = if base.is_empty() {
        Vec::new()
    } else {
        base.lines().map(|l| l.to_string()).collect()
    };

    // New lines contributed by this hunk (context + additions).
    let new_segment: Vec<String> = hunk
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Context || l.kind == LineKind::Add)
        .map(|l| l.text.clone())
        .collect();

    let start = if old_lines == 0 {
        old_start
    } else if old_start == 0 {
        0
    } else {
        old_start.saturating_sub(1)
    };
    let start = start.min(base_lines.len());
    let end = (start + old_lines).min(base_lines.len());
    base_lines.splice(start..end, new_segment);

    let mut new_content = base_lines.join("\n");
    if !base_lines.is_empty() && base_ends_newline {
        new_content.push('\n');
    } else if !new_content.is_empty() {
        // Preserve workdir trailing-newline convention when base lacked one.
        if let Ok(wd) = workdir_content(repo, path) {
            if wd.ends_with('\n') {
                new_content.push('\n');
            }
        }
    }

    write_index_content(repo, path, new_content.as_bytes())?;
    Ok(())
}

fn index_blob_content(repo: &git2::Repository, path: &str) -> Result<String, GitError> {
    let index = repo.index()?;
    match index.get_path(Path::new(path), 0) {
        Some(entry) => {
            let blob = repo.find_blob(entry.id)?;
            Ok(String::from_utf8_lossy(blob.content()).into_owned())
        }
        None => Ok(String::new()),
    }
}

fn workdir_content(repo: &git2::Repository, path: &str) -> Result<String, GitError> {
    let full = repo
        .workdir()
        .ok_or_else(|| GitError::HunkStaging("bare repo".into()))?
        .join(Path::new(path));
    std::fs::read_to_string(&full)
        .map_err(|e| GitError::HunkStaging(format!("cannot read {path}: {e}")))
}

fn write_index_content(repo: &git2::Repository, path: &str, data: &[u8]) -> Result<(), GitError> {
    let mut index = repo.index()?;
    let entry = match index.get_path(Path::new(path), 0) {
        Some(e) => e,
        None => git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: data.len() as u32,
            id: git2::Oid::zero(),
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        },
    };
    index
        .add_frombuffer(&entry, data)
        .map_err(|e| GitError::HunkStaging(format!("cannot write partial hunk to index: {e}")))?;
    index.write()?;
    Ok(())
}

/// Parse `"@@ -old_start[,old_lines] +new_start[,new_lines] @@ ..."`
/// into `(old_start, old_lines)`.
fn parse_hunk_header(header: &str) -> Option<(usize, usize)> {
    parse_hunk_header_full(header).map(|(old_start, old_lines, _, _)| (old_start, old_lines))
}

/// Full hunk header parse: `(old_start, old_lines, new_start, new_lines)`.
/// A missing `,N` means 1 (git convention for single-line hunks).
fn parse_hunk_header_full(header: &str) -> Option<(usize, usize, usize, usize)> {
    let header = header.trim();
    let inner = header.strip_prefix("@@")?.split("@@").next()?.trim();
    let mut parts = inner.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let parse_side = |s: &str| -> Option<(usize, usize)> {
        match s.split_once(',') {
            Some((start, lines)) => Some((start.parse().ok()?, lines.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = parse_side(old)?;
    let (new_start, new_lines) = parse_side(new)?;
    Some((old_start, old_lines, new_start, new_lines))
}

/// Whether HEAD contains `path` (i.e. the file is tracked in HEAD).
fn head_has_path(repo: &git2::Repository, path: &str) -> bool {
    let rel = Path::new(path);
    repo.head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.tree().ok())
        .and_then(|t| t.get_path(rel).ok())
        .is_some()
}

/// Whether the index contains `path`.
fn index_has_path(repo: &git2::Repository, path: &str) -> bool {
    repo.index()
        .ok()
        .and_then(|idx| idx.get_path(Path::new(path), 0))
        .is_some()
}

/// Restore (discard) a single hunk: revert that hunk's region to the
/// "old" side while leaving every other hunk untouched.
///
/// - `staged = false`: the unstaged diff (index -> workdir). The workdir
///   file's hunk region (`new` side) is replaced with the hunk's old
///   segment (context + deletions). An untracked file whose only hunk is
///   discarded is deleted, matching [`discard_file`].
/// - `staged = true`: the staged diff (HEAD -> index). The index blob's
///   hunk region is replaced with the HEAD side. A staged-new file whose
///   only hunk is discarded is removed from the index (leaving the
///   workdir file untracked).
pub fn discard_hunk(
    repo: &git2::Repository,
    path: &str,
    hunk_index: usize,
    staged: bool,
) -> Result<(), GitError> {
    let fail = |msg: String| GitError::Discard(msg);
    let diff = if staged {
        crate::diff::staged_diff(repo, path)?
    } else {
        crate::diff::unstaged_diff(repo, path)?
    };
    if diff.binary {
        return Err(fail(format!("cannot restore hunk of binary file {path}")));
    }
    if hunk_index >= diff.hunks.len() {
        return Err(fail(format!(
            "hunk index {hunk_index} out of range ({} hunks in {path})",
            diff.hunks.len()
        )));
    }
    let hunk = &diff.hunks[hunk_index];
    let has_changes =
        hunk.lines.iter().any(|l| {
            l.kind == LineKind::Context || l.kind == LineKind::Add || l.kind == LineKind::Del
        }) && hunk
            .lines
            .iter()
            .any(|l| l.kind == LineKind::Add || l.kind == LineKind::Del);
    if !has_changes {
        return Err(fail(format!(
            "nothing to restore in hunk {hunk_index} of {path}"
        )));
    }
    let (_, _, new_start, new_lines) = parse_hunk_header_full(&hunk.header)
        .ok_or_else(|| fail(format!("cannot parse hunk header: {}", hunk.header)))?;
    // Old-side content for this hunk (what the region reverts to).
    let old_segment: Vec<String> = hunk
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Context || l.kind == LineKind::Del)
        .map(|l| l.text.clone())
        .collect();

    if staged {
        let base = index_blob_content(repo, path)?;
        let base_ends_newline = base.ends_with('\n') || base.is_empty();
        let mut base_lines: Vec<String> = if base.is_empty() {
            Vec::new()
        } else {
            base.lines().map(|l| l.to_string()).collect()
        };
        let start = hunk_new_offset(new_start, new_lines, base_lines.len());
        let end = (start + new_lines).min(base_lines.len());
        base_lines.splice(start..end, old_segment);
        if base_lines.is_empty() {
            if !head_has_path(repo, path) {
                // Staged-new file fully restored: drop it from the index so
                // the workdir copy becomes untracked again.
                let mut index = repo.index()?;
                let _ = index.remove_path(Path::new(path));
                index.write()?;
                return Ok(());
            }
            write_index_content(repo, path, b"")?;
            return Ok(());
        }
        let mut new_content = base_lines.join("\n");
        if base_ends_newline {
            new_content.push('\n');
        }
        write_index_content(repo, path, new_content.as_bytes())?;
        return Ok(());
    }

    // Unstaged: rewrite the workdir file.
    let full = repo
        .workdir()
        .ok_or_else(|| fail("bare repo".to_string()))?
        .join(Path::new(path));
    let bytes = std::fs::read(&full).map_err(|e| fail(format!("cannot read {path}: {e}")))?;
    let base = String::from_utf8_lossy(&bytes);
    let base_ends_newline = base.ends_with('\n') || base.is_empty();
    let mut base_lines: Vec<String> = if base.is_empty() {
        Vec::new()
    } else {
        base.lines().map(|l| l.to_string()).collect()
    };
    let start = hunk_new_offset(new_start, new_lines, base_lines.len());
    let end = (start + new_lines).min(base_lines.len());
    base_lines.splice(start..end, old_segment);
    if base_lines.is_empty() {
        if !head_has_path(repo, path) && !index_has_path(repo, path) {
            // Untracked file fully restored: delete it like discard_file.
            remove_workdir_path(repo, path).map_err(|e| fail(e.to_string()))?;
            return Ok(());
        }
        std::fs::write(&full, b"").map_err(|e| fail(format!("cannot write {path}: {e}")))?;
        return Ok(());
    }
    let mut new_content = base_lines.join("\n");
    if base_ends_newline {
        new_content.push('\n');
    }
    std::fs::write(&full, new_content.as_bytes())
        .map_err(|e| fail(format!("cannot write {path}: {e}")))?;
    Ok(())
}

/// 0-based workdir/index offset for a hunk's `new` side, clamped into the
/// file. Mirrors the `old`-side offset math in [`stage_hunk`].
fn hunk_new_offset(new_start: usize, new_lines: usize, file_len: usize) -> usize {
    let start = if new_lines == 0 {
        new_start
    } else if new_start == 0 {
        0
    } else {
        new_start.saturating_sub(1)
    };
    start.min(file_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::fs;

    fn two_hunk_repo() -> (tempfile::TempDir, git2::Repository) {
        let (dir, repo) = testutil::init_repo();
        let base = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        testutil::commit_file(&repo, "a.txt", &base, "init");
        let dirty = base.replacen("line 5\n", "line 5 CHANGED\n", 1).replacen(
            "line 35\n",
            "line 35 CHANGED\n",
            1,
        );
        fs::write(repo.workdir().unwrap().join("a.txt"), &dirty).unwrap();
        (dir, repo)
    }

    #[test]
    fn stage_single_hunk_leaves_other_hunks_unstaged() {
        let (_dir, repo) = two_hunk_repo();
        let before = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert!(before.hunks.len() >= 2);

        stage_hunk(&repo, "a.txt", 0).unwrap();

        // Staged diff now has exactly one hunk; unstaged has the remainder.
        let staged = crate::diff::staged_diff(&repo, "a.txt").unwrap();
        let unstaged = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert_eq!(staged.hunks.len(), 1, "staged: {staged:?}");
        assert_eq!(unstaged.hunks.len(), before.hunks.len() - 1);

        let status = crate::status::repo_status(&repo).unwrap();
        let entry = status.files.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(entry.state, crate::status::FileState::BothStagedAndUnstaged);
    }

    #[test]
    fn unstage_file_restores_index_to_head() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        stage_file(&repo, "a.txt").unwrap();
        assert!(!crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
        unstage_file(&repo, "a.txt").unwrap();
        assert!(crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
        assert!(!crate::diff::unstaged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
    }

    #[test]
    fn stage_out_of_range_hunk_index_errors() {
        let (_dir, repo) = two_hunk_repo();
        let err = stage_hunk(&repo, "a.txt", 99).unwrap_err();
        match err {
            GitError::HunkStaging(_) => {}
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn discard_unstaged_file_restores_head_content() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        discard_file(&repo, "a.txt").unwrap();
        let content = fs::read_to_string(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert_eq!(content, "a\n");
        let status = crate::status::repo_status(&repo).unwrap();
        assert!(status.files.iter().all(|e| e.path != "a.txt"));
    }

    #[test]
    fn discard_staged_and_unstaged_changes_restores_head() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "staged\n");
        stage_file(&repo, "a.txt").unwrap();
        testutil::dirty_file(&repo, "a.txt", "unstaged\n");
        discard_file(&repo, "a.txt").unwrap();
        let content = fs::read_to_string(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert_eq!(content, "a\n");
        assert!(crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
        let status = crate::status::repo_status(&repo).unwrap();
        assert!(status.files.iter().all(|e| e.path != "a.txt"));
    }

    #[test]
    fn discard_untracked_file_deletes_it() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::write(repo.workdir().unwrap().join("new.txt"), "new\n").unwrap();
        discard_file(&repo, "new.txt").unwrap();
        assert!(!repo.workdir().unwrap().join("new.txt").exists());
    }

    #[test]
    fn discard_staged_new_file_deletes_it() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::write(repo.workdir().unwrap().join("new.txt"), "new\n").unwrap();
        stage_file(&repo, "new.txt").unwrap();
        discard_file(&repo, "new.txt").unwrap();
        assert!(!repo.workdir().unwrap().join("new.txt").exists());
        let status = crate::status::repo_status(&repo).unwrap();
        assert!(status.files.iter().all(|e| e.path != "new.txt"));
    }

    #[test]
    fn discard_clean_file_errors() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let err = discard_file(&repo, "a.txt").unwrap_err();
        match err {
            GitError::Discard(msg) => assert!(msg.contains("nothing to discard"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn discard_unstaged_hunk_leaves_other_hunks() {
        let (_dir, repo) = two_hunk_repo();
        let before = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert!(before.hunks.len() >= 2);
        discard_hunk(&repo, "a.txt", 0, false).unwrap();
        let after = crate::diff::unstaged_diff(&repo, "a.txt").unwrap();
        assert_eq!(after.hunks.len(), before.hunks.len() - 1);
        let content = fs::read_to_string(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert!(
            content.contains("line 5\n"),
            "first hunk restored: {content:?}"
        );
        assert!(
            content.contains("line 35 CHANGED"),
            "second hunk kept: {content:?}"
        );
    }

    #[test]
    fn discard_staged_hunk_reverts_index_only() {
        let (_dir, repo) = two_hunk_repo();
        stage_hunk(&repo, "a.txt", 0).unwrap();
        assert_eq!(
            crate::diff::staged_diff(&repo, "a.txt")
                .unwrap()
                .hunks
                .len(),
            1
        );
        discard_hunk(&repo, "a.txt", 0, true).unwrap();
        assert!(crate::diff::staged_diff(&repo, "a.txt")
            .unwrap()
            .hunks
            .is_empty());
        // Workdir still holds both changes; only the index was restored.
        let content = fs::read_to_string(repo.workdir().unwrap().join("a.txt")).unwrap();
        assert!(content.contains("line 5 CHANGED"));
    }

    #[test]
    fn discard_untracked_single_hunk_deletes_file() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        fs::write(repo.workdir().unwrap().join("new.txt"), "one\ntwo\n").unwrap();
        discard_hunk(&repo, "new.txt", 0, false).unwrap();
        assert!(!repo.workdir().unwrap().join("new.txt").exists());
    }

    #[test]
    fn discard_out_of_range_hunk_errors() {
        let (_dir, repo) = two_hunk_repo();
        let err = discard_hunk(&repo, "a.txt", 99, false).unwrap_err();
        match err {
            GitError::Discard(msg) => assert!(msg.contains("out of range"), "got: {msg}"),
            other => panic!("wrong error: {other:?}"),
        }
    }
}
