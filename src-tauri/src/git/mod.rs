//! Local version control: one git repository per model folder.
//!
//! Uses embedded libgit2 (`git2`) rather than shelling out to `git`, because a
//! user who installs a `.dmg` has no Xcode Command Line Tools and macOS would
//! otherwise pop an install prompt on first run. Repositories are ordinary
//! `.git` directories, so they still open in any git client.
//!
//! Model folders are fully self-contained: `metadata.json`, `files/`, and
//! `images/` are all tracked. App bookkeeping never lives inside them.

use crate::error::{AppError, AppResult};
use git2::{DiffOptions, IndexAddOption, Repository, StatusOptions};
use serde::Serialize;
use std::fs;
use std::path::Path;

const DEFAULT_AUTHOR_NAME: &str = "Printables Offline";
const DEFAULT_AUTHOR_EMAIL: &str = "sync@printablesoffline.local";

#[derive(Debug, Clone, Serialize, Default)]
pub struct GitStatus {
    pub is_repo: bool,
    pub clean: bool,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub untracked: Vec<String>,
}

impl GitStatus {
    pub fn change_count(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len() + self.untracked.len()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub time: String,
    pub author: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: String,
    pub status: String,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitDetail {
    pub info: CommitInfo,
    pub files: Vec<FileChange>,
    pub insertions: usize,
    pub deletions: usize,
}

pub fn is_repo(model_dir: &Path) -> bool {
    model_dir.join(".git").exists()
}

/// Open an existing repository, or create one if `create` is true.
pub fn open(model_dir: &Path, create: bool) -> AppResult<Repository> {
    if is_repo(model_dir) {
        return Repository::open(model_dir).map_err(|e| {
            AppError::git(format!(
                "Cannot open git repository at {}: {}",
                model_dir.display(),
                e.message()
            ))
        });
    }
    if !create {
        return Err(AppError::git(format!(
            "{} is not a git repository",
            model_dir.display()
        )));
    }
    fs::create_dir_all(model_dir)
        .map_err(|e| AppError::io(format!("Cannot create {}: {e}", model_dir.display())))?;
    let repo = Repository::init(model_dir)?;
    ensure_signature(&repo)?;
    Ok(repo)
}

fn ensure_signature(repo: &Repository) -> AppResult<()> {
    let mut config = repo.config()?;
    if config.get_string("user.name").is_err() {
        config.set_str("user.name", DEFAULT_AUTHOR_NAME)?;
    }
    if config.get_string("user.email").is_err() {
        config.set_str("user.email", DEFAULT_AUTHOR_EMAIL)?;
    }
    // Keep libgit2's filemode handling predictable on APFS.
    let _ = config.set_bool("core.filemode", true);
    Ok(())
}

pub fn signature(repo: &Repository) -> AppResult<git2::Signature<'static>> {
    ensure_signature(repo)?;
    git2::Signature::now(DEFAULT_AUTHOR_NAME, DEFAULT_AUTHOR_EMAIL)
        .map_err(|e| AppError::git(format!("Cannot create git signature: {}", e.message())))
}

/// Contents of `path` as committed at HEAD, if the repository has it.
pub fn head_file(model_dir: &Path, path: &str) -> Option<Vec<u8>> {
    let repo = Repository::open(model_dir).ok()?;
    let head = repo.head().ok()?;
    let tree = head.peel_to_tree().ok()?;
    let entry = tree.get_path(Path::new(path)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    Some(blob.content().to_vec())
}

/// Stage everything (including deletions) and commit if the tree changed.
/// Returns `Some(commit_id)` when a commit was created.
pub fn commit_all(model_dir: &Path, message: &str) -> AppResult<Option<String>> {
    let repo = open(model_dir, true)?;
    let sig = signature(&repo)?;

    let mut index = repo.index()?;
    index.add_all(["*"], IndexAddOption::DEFAULT | IndexAddOption::FORCE, None)?;
    index.update_all(["*"], None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let parent_commit = match repo.head() {
        Ok(head) => {
            let oid = head
                .target()
                .ok_or_else(|| AppError::git("HEAD has no target"))?;
            Some(repo.find_commit(oid)?)
        }
        Err(_) => None,
    };

    // Nothing changed since the last commit?
    if let Some(parent) = parent_commit.as_ref() {
        if parent.tree_id() == tree_id {
            return Ok(None);
        }
    }

    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
    let oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;
    Ok(Some(oid.to_string()))
}

pub fn status(model_dir: &Path) -> AppResult<GitStatus> {
    if !is_repo(model_dir) {
        return Ok(GitStatus::default());
    }
    let repo = Repository::open(model_dir)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .exclude_submodules(true);

    let statuses = repo.statuses(Some(&mut opts))?;
    let mut out = GitStatus {
        is_repo: true,
        clean: true,
        ..Default::default()
    };

    for entry in statuses.iter() {
        let Some(path) = entry.path() else { continue };
        let s = entry.status();
        let path = path.to_string();
        if s.is_index_new() || s.is_wt_new() {
            if s.is_index_new() {
                out.added.push(path);
            } else {
                out.untracked.push(path);
            }
        } else if s.is_index_modified()
            || s.is_wt_modified()
            || s.is_wt_renamed()
            || s.is_index_renamed()
        {
            out.modified.push(path);
        } else if s.is_index_deleted() || s.is_wt_deleted() {
            out.deleted.push(path);
        }
    }

    out.added.sort();
    out.modified.sort();
    out.deleted.sort();
    out.untracked.sort();
    out.clean = out.change_count() == 0;
    Ok(out)
}

pub fn log(model_dir: &Path, limit: usize) -> AppResult<Vec<CommitInfo>> {
    if !is_repo(model_dir) {
        return Ok(Vec::new());
    }
    let repo = Repository::open(model_dir)?;
    let mut walk = repo.revwalk()?;
    if walk.push_head().is_err() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (i, oid) in walk.enumerate() {
        if i >= limit {
            break;
        }
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        out.push(to_commit_info(&commit));
    }
    Ok(out)
}

fn to_commit_info(commit: &git2::Commit) -> CommitInfo {
    let id = commit.id().to_string();
    CommitInfo {
        short_id: id.chars().take(8).collect(),
        summary: commit.summary().unwrap_or("(no message)").to_string(),
        time: chrono::DateTime::<chrono::Utc>::from_timestamp(commit.time().seconds(), 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        author: commit.author().name().unwrap_or("unknown").to_string(),
        id,
    }
}

pub fn show(model_dir: &Path, commit_id: &str) -> AppResult<CommitDetail> {
    let repo = Repository::open(model_dir)?;
    let oid = git2::Oid::from_str(commit_id)
        .map_err(|e| AppError::invalid(format!("Invalid commit id: {}", e.message())))?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

    let mut opts = DiffOptions::new();
    opts.context_lines(0);
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;

    let mut files = Vec::new();
    for (idx, delta) in diff.deltas().enumerate() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let status = match delta.status() {
            git2::Delta::Added => "added",
            git2::Delta::Deleted => "deleted",
            git2::Delta::Renamed => "renamed",
            _ => "modified",
        };
        let (added, removed) =
            patch_line_counts(&git2::Patch::from_diff(&diff, idx).ok().flatten());
        files.push(FileChange {
            path,
            status: status.to_string(),
            added,
            removed,
        });
    }

    let stats = diff.stats()?;
    Ok(CommitDetail {
        info: to_commit_info(&commit),
        files,
        insertions: stats.insertions(),
        deletions: stats.deletions(),
    })
}

fn patch_line_counts(patch: &Option<git2::Patch>) -> (usize, usize) {
    let Some(patch) = patch else { return (0, 0) };
    let (mut added, mut removed) = (0, 0);
    for hunk_idx in 0..patch.num_hunks() {
        let Ok((_hunk, lines)) = patch.hunk(hunk_idx) else {
            continue;
        };
        for line_idx in 0..lines {
            let Ok(line) = patch.line_in_hunk(hunk_idx, line_idx) else {
                continue;
            };
            match line.origin() {
                '+' => added += 1,
                '-' => removed += 1,
                _ => {}
            }
        }
    }
    (added, removed)
}

/// Restore the working tree to `commit_id` and record that as a new commit,
/// so the restore itself is revertable.
pub fn restore(model_dir: &Path, commit_id: &str) -> AppResult<Option<String>> {
    let repo = Repository::open(model_dir)?;
    let oid = git2::Oid::from_str(commit_id)
        .map_err(|e| AppError::invalid(format!("Invalid commit id: {}", e.message())))?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;

    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force().remove_untracked(true);
    repo.checkout_tree(tree.as_object(), Some(&mut checkout))?;

    commit_all(
        model_dir,
        &format!(
            "Restored revision {}",
            oid.to_string().chars().take(8).collect::<String>()
        ),
    )
}

/// Collapse history to a single commit holding the current tree.
/// A tag is created first so the previous history is recoverable.
pub fn compact(model_dir: &Path) -> AppResult<usize> {
    let repo = Repository::open(model_dir)?;
    let removed = log(model_dir, usize::MAX)?.len().saturating_sub(1);
    if removed == 0 {
        return Ok(0);
    }
    let head = repo.head()?.peel_to_commit()?;
    let tag_name = format!("pre-compact-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    let _ = repo.tag_lightweight(&tag_name, head.as_object(), false);

    let sig = signature(&repo)?;
    let tree = head.tree()?;
    let message = format!(
        "History compacted: {removed} earlier revision(s) collapsed; previous history at tag {tag_name}"
    );
    let new_oid = repo.commit(None, &sig, &sig, &message, &tree, &[])?;

    // Point the current branch at the new root commit.
    if let Ok(head_ref) = repo.head() {
        if let Some(name) = head_ref.name() {
            repo.reference(name, new_oid, true, "compact history")?;
        }
    }
    repo.set_head("HEAD")?;
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
    Ok(removed)
}

/// Initialise a repository for a model that was cloned before 2.0 existed.
pub fn ensure_initialised(model_dir: &Path) -> AppResult<Option<String>> {
    if is_repo(model_dir) {
        return Ok(None);
    }
    open(model_dir, true)?;
    commit_all(
        model_dir,
        "Initial import (adopted into version control by Printables Offline 2.0)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_model_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("po-git-test-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn commits_only_when_something_changed() {
        let dir = temp_model_dir("commit");
        fs::write(dir.join("metadata.json"), "{}").unwrap();

        let first = commit_all(&dir, "Initial").unwrap();
        assert!(first.is_some(), "first commit should be created");

        let second = commit_all(&dir, "No-op").unwrap();
        assert!(second.is_none(), "unchanged tree must not create a commit");

        fs::write(dir.join("metadata.json"), "{\"name\":\"x\"}").unwrap();
        let third = commit_all(&dir, "Edit").unwrap();
        assert!(third.is_some(), "modified file should commit");

        let status = status(&dir).unwrap();
        assert!(status.clean, "working tree should be clean after commit");
        assert_eq!(log(&dir, 10).unwrap().len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tracks_deletions_and_restores_them() {
        let dir = temp_model_dir("restore");
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::write(dir.join("files/a.stl"), "solid a").unwrap();
        let first = commit_all(&dir, "Initial").unwrap().unwrap();

        fs::remove_file(dir.join("files/a.stl")).unwrap();
        commit_all(&dir, "Delete a.stl").unwrap();
        assert!(!dir.join("files/a.stl").exists());

        restore(&dir, &first).unwrap();
        assert!(
            dir.join("files/a.stl").exists(),
            "restoring the first commit must bring the file back"
        );
        assert_eq!(log(&dir, 100).unwrap().len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_reports_untracked_files() {
        let dir = temp_model_dir("status");
        fs::write(dir.join("metadata.json"), "{}").unwrap();
        commit_all(&dir, "Initial").unwrap();

        fs::write(dir.join("notes.txt"), "hello").unwrap();
        let status = status(&dir).unwrap();
        assert!(!status.clean);
        assert_eq!(status.untracked, vec!["notes.txt".to_string()]);

        let _ = fs::remove_dir_all(&dir);
    }
}
