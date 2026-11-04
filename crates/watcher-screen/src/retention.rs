//! Rolling retention for the local frame store (issue #5, ADR rev 2).
//!
//! [`plan_deletions`] is the pure planner (age + size budget, oldest-first).
//! [`sweep_frames`] applies it to the on-disk frame tree
//! (`media/frames/<YYYY>/<MM>/<DD>/`), deleting the files the planner
//! selects. Index rows in the `media` bucket are never touched here: they
//! are the historical record of what was captured.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use log::{debug, info, warn};

/// Bytes per MB used by the budget math (1 MiB).
pub const MB_BYTES: u64 = 1024 * 1024;

/// Seconds between automatic retention sweeps (1 hour).
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

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
/// 1. Every file strictly older than `max_age_days` is deleted.
/// 2. Then, oldest-first, files are deleted until the total size of the
///    remaining files is within `max_total_mb` - except that at least one
///    file is always kept if any in-age file exists (a single oversized
///    file stays and can be handled by the age rule later).
///
/// The returned list is ordered oldest-first.
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

/// What one sweep pass observed and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Frame files found under the frame tree.
    pub scanned: usize,
    /// Files deleted (oldest-first, per the planner).
    pub deleted: usize,
    /// Bytes reclaimed by the deletions.
    pub freed_bytes: u64,
    /// Per-file failures (metadata read or unlink); the sweep continues.
    pub errors: usize,
}

/// Apply the retention planner to the frame tree under `<root>/frames`.
///
/// Walks every regular file below the frame root (the writer's
/// `<YYYY>/<MM>/<DD>/` layout and anything else that ends up there),
/// plans deletions for `budget` and unlinks them oldest-first. A missing
/// frame root is not an error (nothing captured yet). Empty day/month
/// directories are pruned after deletions. Index rows in the database are
/// left alone - they stay as the historical record.
pub fn sweep_frames(media_root: &Path, budget: &RetentionBudget) -> std::io::Result<SweepReport> {
    let frame_root = media_root.join("frames");
    let mut report = SweepReport::default();
    if !frame_root.exists() {
        debug!(
            "retention sweep: no frame tree at {} yet",
            frame_root.display()
        );
        return Ok(report);
    }

    let mut files: Vec<FileInfo> = Vec::new();
    let mut raw: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    walk(
        &frame_root,
        &mut files,
        &mut raw,
        &mut dirs,
        &mut report.errors,
    );
    report.scanned = files.len();

    for path in plan_deletions(&files, budget) {
        let Some(idx) = files.iter().position(|f| f.path == path) else {
            continue;
        };
        let size = files[idx].size_bytes;
        let disk_path = &raw[idx].1;
        match fs::remove_file(disk_path) {
            Ok(()) => {
                report.deleted += 1;
                report.freed_bytes += size;
                debug!("retention sweep: deleted {}", disk_path.display());
            }
            Err(e) => {
                report.errors += 1;
                warn!(
                    "retention sweep: failed to delete {}: {e}",
                    disk_path.display()
                );
            }
        }
    }

    if report.deleted > 0 {
        prune_empty_dirs(&dirs);
    }

    info!(
        "retention sweep: scanned {} frame file(s), deleted {}, freed {:.1} MB, {} error(s)",
        report.scanned,
        report.deleted,
        report.freed_bytes as f64 / MB_BYTES as f64,
        report.errors,
    );
    Ok(report)
}

/// Recursively collect candidate files below `dir` (and the dirs seen).
fn walk(
    dir: &Path,
    files: &mut Vec<FileInfo>,
    raw: &mut Vec<(String, PathBuf, u64)>,
    dirs: &mut Vec<PathBuf>,
    errors: &mut usize,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        *errors += 1;
        warn!("retention sweep: cannot read {}", dir.display());
        return;
    };
    dirs.push(dir.to_path_buf());
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, files, raw, dirs, errors),
            Ok(t) if t.is_file() => {
                let Ok(meta) = entry.metadata() else {
                    *errors += 1;
                    continue;
                };
                let modified = meta.modified().unwrap_or(SystemTime::now());
                let now = SystemTime::now();
                let age_days = match now.duration_since(modified) {
                    Ok(age) => age.as_secs_f64() / 86_400.0,
                    // Clock skew: a future mtime is the youngest possible.
                    Err(_) => 0.0,
                };
                let text = path.to_string_lossy().into_owned();
                files.push(FileInfo {
                    path: text.clone(),
                    age_days,
                    size_bytes: meta.len(),
                });
                raw.push((text, path, meta.len()));
            }
            Ok(_) => {}
            Err(_) => *errors += 1,
        }
    }
}

/// Try to remove now-empty directories, deepest first. Failures are fine.
fn prune_empty_dirs(dirs: &[PathBuf]) {
    let mut dirs: Vec<&PathBuf> = dirs.iter().collect();
    // Deeper paths sort first (day before month before year).
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in dirs {
        if fs::remove_dir(dir).is_ok() {
            debug!("retention sweep: pruned empty dir {}", dir.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

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

    #[test]
    fn age_limit_deletes_every_expired_file_and_only_those() {
        let files: Vec<FileInfo> = (1..=100)
            .map(|d| file(&format!("f{d:03}.jpg"), d as f64, mb(1)))
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
        // Everything at or under the age limit survives (budget is huge).
        for f in &files {
            if f.age_days <= 30.0 {
                assert!(!deletions.contains(&f.path));
            }
        }
    }

    #[test]
    fn size_limit_deletes_oldest_first_until_under_budget() {
        let files: Vec<FileInfo> = (1..=100)
            .map(|d| file(&format!("f{d:03}.jpg"), d as f64, mb(1)))
            .collect();
        let budget = RetentionBudget {
            max_age_days: 365,
            max_total_mb: 10,
        };
        let deletions = plan_deletions(&files, &budget);
        assert_eq!(deletions.len(), 90);
        // The 10 newest survive; kept total is within budget.
        let kept_total: u64 = files
            .iter()
            .filter(|f| !deletions.contains(&f.path))
            .map(|f| f.size_bytes)
            .sum();
        assert!(kept_total <= mb(10));
        // Oldest-first ordering of the deletion list.
        for pair in deletions.windows(2) {
            let a: f64 = pair[0][1..4].parse().expect("day");
            let b: f64 = pair[1][1..4].parse().expect("day");
            assert!(a >= b, "deletion list must be oldest-first");
        }
    }

    #[test]
    fn under_budget_means_no_deletions() {
        let files: Vec<FileInfo> = (0..5)
            .map(|d| file(&format!("f{d}.jpg"), d as f64 + 1.0, mb(1)))
            .collect();
        let budget = RetentionBudget {
            max_age_days: 365,
            max_total_mb: 100,
        };
        assert!(plan_deletions(&files, &budget).is_empty());
    }

    #[test]
    fn single_oversized_file_is_never_deleted_by_size_rule() {
        let files = vec![file("only.jpg", 1.0, mb(500))];
        let budget = RetentionBudget {
            max_age_days: 365,
            max_total_mb: 10,
        };
        assert!(plan_deletions(&files, &budget).is_empty());
    }

    #[test]
    fn mixed_age_and_size_rules() {
        let files = vec![
            file("a.jpg", 400.0, mb(1)), // too old
            file("b.jpg", 100.0, mb(8)), // in age, over size once a/b/c summed
            file("c.jpg", 50.0, mb(8)),  // in age
            file("d.jpg", 2.0, mb(2)),   // in age, newest
        ];
        let budget = RetentionBudget {
            max_age_days: 365,
            max_total_mb: 12,
        };
        let deletions = plan_deletions(&files, &budget);
        // a is age-expired; then 8+8+2 = 18 MB > 12 MB, so b goes (oldest
        // in-age first); 8+2 = 10 MB fits. d is never dropped (keep 1+).
        assert_eq!(deletions, vec!["a.jpg".to_string(), "b.jpg".to_string()]);
    }

    // ---- sweep wiring tests (temp dirs, all platforms) ----

    /// Unique temp dir for one test.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock before unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("watcher-screen-test-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Write a file with a modified time `age_days` in the past.
    fn write_frame(path: &Path, len: usize, age_days: f64) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
        let mut f = fs::File::create(path).expect("create file");
        f.write_all(&vec![0u8; len]).expect("write");
        let mtime = SystemTime::now() - Duration::from_secs_f64(age_days * 86_400.0);
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .expect("set mtime");
    }

    #[test]
    fn sweep_deletes_expired_and_overbudget_files_oldest_first() {
        let media = temp_dir("sweep-budget");
        let frames = media.join("frames");
        let a = frames.join("2026").join("05").join("01").join("old.jpg");
        let b = frames.join("2026").join("10").join("01").join("mid.jpg");
        let c = frames.join("2026").join("10").join("20").join("new.jpg");
        write_frame(&a, 3 * MB_BYTES as usize, 120.0);
        write_frame(&b, 8 * MB_BYTES as usize, 10.0);
        write_frame(&c, 8 * MB_BYTES as usize, 5.0);

        let budget = RetentionBudget {
            max_age_days: 90,
            max_total_mb: 12,
        };
        let report = sweep_frames(&media, &budget).expect("sweep");

        // a is age-expired; b+c = 16 MB > 12 MB so b goes oldest-first;
        // c (8 MB) stays. b is 10 days old, within the age budget.
        assert_eq!(
            report,
            SweepReport {
                scanned: 3,
                deleted: 2,
                freed_bytes: 11 * MB_BYTES,
                errors: 0,
            }
        );
        assert!(!a.exists());
        assert!(!b.exists());
        assert!(c.exists());
        let _ = fs::remove_dir_all(media);
    }

    #[test]
    fn sweep_missing_frame_tree_is_not_an_error() {
        let media = temp_dir("sweep-empty");
        let report = sweep_frames(
            &media,
            &RetentionBudget {
                max_age_days: 90,
                max_total_mb: 20 * 1024,
            },
        )
        .expect("sweep without frames dir");
        assert_eq!(report, SweepReport::default());
        let _ = fs::remove_dir_all(media);
    }

    #[test]
    fn sweep_prunes_empty_day_dirs_after_deletion() {
        let media = temp_dir("sweep-prune");
        let lonely = media
            .join("frames")
            .join("2026")
            .join("01")
            .join("02")
            .join("gone.jpg");
        let survivor = media
            .join("frames")
            .join("2026")
            .join("01")
            .join("03")
            .join("kept.jpg");
        write_frame(&lonely, 1024, 400.0);
        write_frame(&survivor, 1024, 1.0);

        let budget = RetentionBudget {
            max_age_days: 90,
            max_total_mb: 20 * 1024,
        };
        let report = sweep_frames(&media, &budget).expect("sweep");
        assert_eq!(report.deleted, 1);
        assert!(!lonely.exists());
        // The emptied 02 day dir is pruned; the 03 dir survives.
        assert!(!lonely.parent().expect("day dir").exists());
        assert!(survivor.parent().expect("day dir").exists());
        let _ = fs::remove_dir_all(media);
    }

    #[test]
    fn sweep_handles_monitor_named_and_legacy_frame_names() {
        // Multi-monitor capture writes `HHMMSS_<monitor>_<sha16>.jpg`
        // names; pre-multi-monitor files lack the monitor part. The sweep
        // is name-agnostic: the shared budget covers both, oldest-first.
        let media = temp_dir("sweep-monitors");
        let day = media.join("frames").join("2026").join("11").join("04");
        let old_display1 = day.join("090000_DISPLAY1_1111111111111111.jpg");
        let old_legacy = day.join("090000_2222222222222222.jpg");
        let fresh_display2 = day.join("100000_DISPLAY2_3333333333333333.jpg");
        write_frame(&old_display1, 5 * MB_BYTES as usize, 100.0);
        write_frame(&old_legacy, 2 * MB_BYTES as usize, 100.0);
        write_frame(&fresh_display2, 5 * MB_BYTES as usize, 1.0);

        let budget = RetentionBudget {
            max_age_days: 90,
            max_total_mb: 20 * 1024,
        };
        let report = sweep_frames(&media, &budget).expect("sweep");

        // Only the two age-expired files go, monitor-labeled or not.
        assert_eq!(
            report,
            SweepReport {
                scanned: 3,
                deleted: 2,
                freed_bytes: 7 * MB_BYTES,
                errors: 0,
            }
        );
        assert!(!old_display1.exists());
        assert!(!old_legacy.exists());
        assert!(fresh_display2.exists());
        let _ = fs::remove_dir_all(media);
    }
}
