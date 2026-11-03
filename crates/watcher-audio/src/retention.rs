//! Rolling retention for meeting audio (issue #6, mirroring watcher-screen's
//! planner rules per ADR rev 2).
//!
//! The planner is **duplicated locally on purpose**: plugins must not depend
//! on other plugins (ADR "plugins must not depend on other plugins"), so this
//! module carries a copy of the same budget rules tuned for audio files:
//!
//! 1. Every file strictly older than `max_age_days` is deleted.
//! 2. Then, oldest-first, files are deleted until the total size of the
//!    remaining files is within `max_total_mb` - except that at least one
//!    file is always kept if any in-budget file remains.
//!
//! Sync-aware guard (ADR rev 5): "retention only prunes already-synced
//! files". Candidates whose `media` index row is not yet synced (per the
//! local DB) are spared; both rules apply. A crash-leftover `*.part` file
//! older than a day is removed regardless (it was never indexed).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Bytes per MB used by the budget math (1 MiB).
pub const MB_BYTES: u64 = 1024 * 1024;

/// One candidate file for retention planning.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    /// Path as stored on disk.
    pub path: String,
    /// Age of the file in days (now - modified time).
    pub age_days: f64,
    /// File size in bytes.
    pub size_bytes: u64,
}

/// Retention budget: keep nothing older than `max_age_days` and keep the
/// total under `max_total_mb`.
#[derive(Debug, Clone, PartialEq)]
pub struct RetentionBudget {
    pub max_age_days: u64,
    pub max_total_mb: u64,
}

/// Compute the deletion list enforcing the budget.
///
/// The returned list is ordered oldest-first. Duplicated from
/// watcher-screen per the plugin-independence ADR rule (do not unify by
/// adding a cross-plugin dependency; keep both copies in sync manually).
pub fn plan_deletions(files: &[FileInfo], budget: &RetentionBudget) -> Vec<String> {
    // Oldest-first view (stable sort keeps input order on equal ages).
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| {
        files[b]
            .age_days
            .partial_cmp(&files[a].age_days)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let budget_bytes = budget.max_total_mb.saturating_mul(MB_BYTES);
    let mut deletions: Vec<usize> = Vec::new();
    let mut kept: Vec<usize> = Vec::new();
    for &i in &order {
        if files[i].age_days > budget.max_age_days as f64 {
            deletions.push(i);
        } else {
            kept.push(i);
        }
    }

    let mut total: u64 = kept.iter().map(|&i| files[i].size_bytes).sum();
    let mut next = 0;
    while total > budget_bytes && kept.len() > next + 1 {
        total -= files[kept[next]].size_bytes;
        deletions.push(kept[next]);
        next += 1;
    }

    deletions
        .into_iter()
        .map(|i| files[i].path.clone())
        .collect()
}

/// Recursively collect regular files under `root` with age/size info.
/// Returns (FileInfo with the full path string, PathBuf). Non-files and
/// unreadable entries are skipped.
pub fn collect_files(root: &Path, now: SystemTime) -> std::io::Result<Vec<(FileInfo, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue, // vanished or unreadable: skip
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let age_days = now
                .duration_since(meta.modified().unwrap_or(now))
                .unwrap_or_default()
                .as_secs_f64()
                / 86_400.0;
            out.push((
                FileInfo {
                    path: path.to_string_lossy().to_string(),
                    age_days,
                    size_bytes: meta.len(),
                },
                path,
            ));
        }
    }
    Ok(out)
}

/// Result of a sweep.
#[derive(Debug, Default, Clone)]
pub struct SweepOutcome {
    /// Files actually deleted (finalized recordings).
    pub deleted: Vec<PathBuf>,
    /// Stale `.part` files removed (never indexed; older than a day).
    pub removed_part_files: Vec<PathBuf>,
    /// Candidates spared because their index row is not synced yet.
    pub protected_unsynced: usize,
    /// Delete failures (logged by the caller).
    pub errors: Vec<String>,
}

/// The `media/audio/...` relative path (forward slashes) of `path`, given the
/// data dir root. Used to match the sync-protection set from the DB.
pub fn relative_to_data_dir(data_dir: &Path, path: &Path) -> String {
    path.strip_prefix(data_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Run one retention sweep over `media_root` (which must live under
/// `data_dir`): walk files, plan deletions within the budget, spare unsynced
/// files, delete, and prune now-empty year/month/day directories.
///
/// `protected_rel_paths` holds the `media`-bucket relative paths (forward
/// slashes) whose index rows are not synced yet; they are never deleted
/// (ADR rev 5).
pub fn sweep(
    data_dir: &Path,
    media_root: &Path,
    budget: &RetentionBudget,
    protected_rel_paths: &HashSet<String>,
) -> SweepOutcome {
    let mut outcome = SweepOutcome::default();
    let now = SystemTime::now();
    let Ok(candidates) = collect_files(media_root, now) else {
        return outcome;
    };

    // Stale .part files: crash leftovers, never indexed, deleted when older
    // than a day; they are excluded from the budget accounting.
    let mut files: Vec<FileInfo> = Vec::with_capacity(candidates.len());
    for (info, path) in candidates {
        let is_part = path.extension().map(|e| e == "part").unwrap_or(false);
        if is_part {
            if info.age_days > 1.0 {
                match std::fs::remove_file(&path) {
                    Ok(()) => outcome.removed_part_files.push(path),
                    Err(e) => outcome.errors.push(format!("{}: {e}", path.display())),
                }
            }
            continue;
        }
        files.push(info);
    }

    // Plan, then apply the sync-protection filter (compare on the
    // data-dir-relative, forward-slash path form used in `media` rows).
    for full in plan_deletions(&files, budget) {
        let p = PathBuf::from(&full);
        let rel = relative_to_data_dir(data_dir, &p);
        if protected_rel_paths.contains(&rel) {
            outcome.protected_unsynced += 1;
            continue;
        }
        match std::fs::remove_file(&p) {
            Ok(()) => outcome.deleted.push(p),
            Err(e) => outcome.errors.push(format!("{}: {e}", p.display())),
        }
    }

    prune_empty_dirs(media_root);
    outcome
}

/// Remove empty directories under `root` bottom-up (never removes `root`).
pub fn prune_empty_dirs(root: &Path) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    stack.push(entry.path());
                }
            }
        }
        if dir != root {
            dirs.push(dir);
        }
    }
    // Deepest first: sort by length descending is a good approximation of
    // depth for the Y/M/D tree.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in dirs {
        let _ = std::fs::remove_dir(&dir); // fails if non-empty: fine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, age_days: f64, size_bytes: u64) -> FileInfo {
        FileInfo {
            path: path.to_string(),
            age_days,
            size_bytes,
        }
    }

    fn mb(n: u64) -> u64 {
        n * MB_BYTES
    }

    // --- planner (duplicated rules, same test matrix as watcher-screen) ---

    #[test]
    fn age_limit_deletes_every_expired_file_and_only_those() {
        let files: Vec<FileInfo> = (1..=100)
            .map(|d| file(&format!("f{d:03}.wav"), d as f64, mb(1)))
            .collect();
        let budget = RetentionBudget {
            max_age_days: 30,
            max_total_mb: u64::MAX / MB_BYTES,
        };
        let deletions = plan_deletions(&files, &budget);
        assert_eq!(deletions.len(), 70);
        for path in &deletions {
            let day: f64 = path[1..4].parse().expect("day from filename");
            assert!(day > 30.0, "deleted file must be older than limit");
        }
        for f in &files {
            if f.age_days <= 30.0 {
                assert!(!deletions.contains(&f.path));
            }
        }
    }

    #[test]
    fn size_limit_deletes_oldest_first_until_under_budget() {
        let files: Vec<FileInfo> = (1..=100)
            .map(|d| file(&format!("f{d:03}.wav"), d as f64, mb(1)))
            .collect();
        let budget = RetentionBudget {
            max_age_days: 365,
            max_total_mb: 10,
        };
        let deletions = plan_deletions(&files, &budget);
        assert_eq!(deletions.len(), 90);
        let kept_total: u64 = files
            .iter()
            .filter(|f| !deletions.contains(&f.path))
            .map(|f| f.size_bytes)
            .sum();
        assert!(kept_total <= mb(10));
        for pair in deletions.windows(2) {
            let a: f64 = pair[0][1..4].parse().expect("day");
            let b: f64 = pair[1][1..4].parse().expect("day");
            assert!(a >= b, "deletion list must be oldest-first");
        }
    }

    #[test]
    fn under_budget_means_no_deletions() {
        let files: Vec<FileInfo> = (0..5)
            .map(|d| file(&format!("f{d}.wav"), d as f64 + 1.0, mb(1)))
            .collect();
        let budget = RetentionBudget {
            max_age_days: 365,
            max_total_mb: 100,
        };
        assert!(plan_deletions(&files, &budget).is_empty());
    }

    #[test]
    fn single_oversized_file_is_never_deleted_by_size_rule() {
        let files = vec![file("only.wav", 1.0, mb(500))];
        let budget = RetentionBudget {
            max_age_days: 365,
            max_total_mb: 10,
        };
        assert!(plan_deletions(&files, &budget).is_empty());
    }

    #[test]
    fn mixed_age_and_size_rules() {
        let files = vec![
            file("a.wav", 400.0, mb(1)), // too old
            file("b.wav", 100.0, mb(8)), // in age, over size once summed
            file("c.wav", 50.0, mb(8)),  // in age
            file("d.wav", 2.0, mb(2)),   // in age, newest
        ];
        let budget = RetentionBudget {
            max_age_days: 365,
            max_total_mb: 12,
        };
        let deletions = plan_deletions(&files, &budget);
        assert_eq!(deletions, vec!["a.wav".to_string(), "b.wav".to_string()]);
    }

    // --- sweep (filesystem level) ---

    fn temp_media(tag: &str) -> (PathBuf, PathBuf) {
        let data =
            std::env::temp_dir().join(format!("watcher-audio-sweep-{tag}-{}", std::process::id()));
        let media = data.join("media").join("audio");
        std::fs::create_dir_all(media.join("2026").join("01").join("01")).expect("mkdir");
        (data, media)
    }

    fn set_mtime(path: &Path, age_days: f64) {
        let mtime = SystemTime::now() - std::time::Duration::from_secs_f64(age_days * 86_400.0);
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(mtime).expect("set mtime");
    }

    #[test]
    fn sweep_deletes_old_files_and_prunes_empty_dirs() {
        let (data, media) = temp_media("old");
        let old = media.join("2026").join("01").join("01").join("old_mic.wav");
        std::fs::write(&old, vec![0u8; 1000]).expect("write old");
        set_mtime(&old, 120.0); // > 60 day default
        let new = media.join("2026").join("01").join("01").join("new_mic.wav");
        std::fs::write(&new, vec![0u8; 1000]).expect("write new");

        let budget = RetentionBudget {
            max_age_days: 60,
            max_total_mb: 10240,
        };
        let outcome = sweep(&data, &media, &budget, &HashSet::new());
        assert_eq!(outcome.deleted, vec![old.clone()]);
        assert!(outcome.errors.is_empty());
        assert!(!old.exists());
        assert!(new.exists(), "fresh file must survive");

        // After a follow-up sweep removing `new`, empty dirs are pruned.
        std::fs::remove_file(&new).expect("remove");
        let _ = sweep(&data, &media, &budget, &HashSet::new());
        assert!(
            !media.join("2026").exists(),
            "empty year dir must be pruned"
        );
        assert!(media.exists(), "media root itself must stay");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn sweep_spares_unsynced_files() {
        let (data, media) = temp_media("unsynced");
        let old = media.join("2026").join("01").join("01").join("old_mic.wav");
        std::fs::write(&old, vec![0u8; 1000]).expect("write");
        set_mtime(&old, 120.0);
        let mut protected = HashSet::new();
        protected.insert("media/audio/2026/01/01/old_mic.wav".to_string());

        let budget = RetentionBudget {
            max_age_days: 60,
            max_total_mb: 10240,
        };
        let outcome = sweep(&data, &media, &budget, &protected);
        assert!(outcome.deleted.is_empty());
        assert_eq!(outcome.protected_unsynced, 1);
        assert!(old.exists(), "unsynced file must never be pruned");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn sweep_removes_stale_part_files_and_excludes_them_from_budget() {
        let (data, media) = temp_media("parts");
        let stale_part = media
            .join("2026")
            .join("01")
            .join("01")
            .join("x_mic.wav.part");
        std::fs::write(&stale_part, vec![0u8; 1000]).expect("write");
        set_mtime(&stale_part, 2.0);
        let fresh_part = media
            .join("2026")
            .join("01")
            .join("01")
            .join("y_mic.wav.part");
        std::fs::write(&fresh_part, vec![0u8; 1000]).expect("write"); // age 0: live recording

        let budget = RetentionBudget {
            max_age_days: 60,
            max_total_mb: 10240,
        };
        let outcome = sweep(&data, &media, &budget, &HashSet::new());
        assert_eq!(outcome.removed_part_files, vec![stale_part.clone()]);
        assert!(!stale_part.exists());
        assert!(fresh_part.exists(), "recent .part must survive");
        assert!(
            outcome.deleted.is_empty(),
            ".part files must not count as media"
        );
        let _ = std::fs::remove_dir_all(&data);
    }
}
