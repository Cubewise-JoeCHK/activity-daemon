//! Screen frame watcher for activity-daemon (issue #5).
//!
//! This crate currently implements the rolling-retention planner that
//! bounds the local frame store (client-side rolling retention per
//! docs/needs.md). The capture pipeline (Windows API JPEG capture every
//! `screen.interval_s`, content-hash dedup via `sha16`, AFK/lock pause)
//! arrives with #5 and will feed captured files into
//! [`plan_deletions`] on a schedule.
//!
//! Rules (ADR rev 2): oldest-first auto-delete keeping a rolling budget of
//! `max_age_days` and `max_total_mb`; both limits configurable; the planner
//! never deletes everything while at least one in-budget file remains.

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
}
