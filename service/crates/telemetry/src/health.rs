//! Pure operational health evaluation: disk space and queue backlog.
//! Thresholds mirror `config/alerts.yaml`; I/O is injected so checks are testable.

use std::io;
use std::path::Path;

use serde::Serialize;

pub const BYTES_PER_KIB: u64 = 1024;
pub const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

/// Disk: warn below 15% free, critical below 5% free or under 2 GiB.
pub const DISK_WARN_FREE_PERCENT: f64 = 15.0;
pub const DISK_CRITICAL_FREE_PERCENT: f64 = 5.0;
pub const DISK_CRITICAL_FREE_BYTES: u64 = 2 * BYTES_PER_GIB;

/// Queue: warn at 50 queued jobs or 5 min oldest age; critical at 200 or 30 min.
pub const BACKLOG_WARN_DEPTH: u64 = 50;
pub const BACKLOG_CRITICAL_DEPTH: u64 = 200;
pub const BACKLOG_WARN_OLDEST_SECS: u64 = 300;
pub const BACKLOG_CRITICAL_OLDEST_SECS: u64 = 1800;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthLevel {
    Ok,
    Warn,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub level: HealthLevel,
    pub detail: String,
}

impl CheckResult {
    pub fn new(name: &str, level: HealthLevel, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            level,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskStats {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Classify disk usage. Zero-size filesystems are critical (cannot be trusted).
pub fn evaluate_disk(stats: DiskStats) -> CheckResult {
    if stats.total_bytes == 0 {
        return CheckResult::new(
            "disk",
            HealthLevel::Critical,
            "filesystem reports zero size",
        );
    }
    let free_percent = stats.free_bytes as f64 * 100.0 / stats.total_bytes as f64;
    let level = if free_percent < DISK_CRITICAL_FREE_PERCENT
        || stats.free_bytes < DISK_CRITICAL_FREE_BYTES
    {
        HealthLevel::Critical
    } else if free_percent < DISK_WARN_FREE_PERCENT {
        HealthLevel::Warn
    } else {
        HealthLevel::Ok
    };
    CheckResult::new(
        "disk",
        level,
        format!("{free_percent:.1}% free ({} bytes)", stats.free_bytes),
    )
}

/// Disk check with injected filesystem probe (real probe: [`statfs_via_df`]).
pub fn check_disk_free<F>(path: &Path, statfs: F) -> CheckResult
where
    F: FnOnce(&Path) -> io::Result<DiskStats>,
{
    match statfs(path) {
        Ok(stats) => evaluate_disk(stats),
        Err(err) => CheckResult::new("disk", HealthLevel::Warn, format!("statfs failed: {err}")),
    }
}

/// Parse `df -Pk <path>` output (POSIX format; works on Linux and macOS).
pub fn parse_df_output(output: &str) -> io::Result<DiskStats> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_owned());
    let line = output
        .lines()
        .nth(1)
        .ok_or_else(|| bad("df output has no data line"))?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    let kib = |i: usize| -> io::Result<u64> {
        cols.get(i)
            .ok_or_else(|| bad("df line too short"))?
            .parse::<u64>()
            .map_err(|_| bad("df column not numeric"))
    };
    Ok(DiskStats {
        total_bytes: kib(1)? * BYTES_PER_KIB,
        free_bytes: kib(3)? * BYTES_PER_KIB,
    })
}

/// Real probe backed by `df -Pk`; avoids `unsafe` libc calls.
pub fn statfs_via_df(path: &Path) -> io::Result<DiskStats> {
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other("df exited non-zero"));
    }
    parse_df_output(&String::from_utf8_lossy(&out.stdout))
}

/// Classify queue backlog by depth and age of the oldest queued job.
pub fn evaluate_backlog(depth: u64, oldest_age_secs: u64) -> CheckResult {
    let level =
        if depth >= BACKLOG_CRITICAL_DEPTH || oldest_age_secs >= BACKLOG_CRITICAL_OLDEST_SECS {
            HealthLevel::Critical
        } else if depth >= BACKLOG_WARN_DEPTH || oldest_age_secs >= BACKLOG_WARN_OLDEST_SECS {
            HealthLevel::Warn
        } else {
            HealthLevel::Ok
        };
    CheckResult::new(
        "queue_backlog",
        level,
        format!("depth={depth} oldest_age_secs={oldest_age_secs}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOTAL: u64 = 100 * BYTES_PER_GIB;

    fn disk(free_gib: u64) -> DiskStats {
        DiskStats {
            total_bytes: TOTAL,
            free_bytes: free_gib * BYTES_PER_GIB,
        }
    }

    #[test]
    fn test_evaluate_disk_plenty_free_is_ok() {
        assert_eq!(evaluate_disk(disk(50)).level, HealthLevel::Ok);
    }

    #[test]
    fn test_evaluate_disk_ten_percent_free_is_warn() {
        assert_eq!(evaluate_disk(disk(10)).level, HealthLevel::Warn);
    }

    #[test]
    fn test_evaluate_disk_four_percent_free_is_critical() {
        assert_eq!(evaluate_disk(disk(4)).level, HealthLevel::Critical);
    }

    #[test]
    fn test_evaluate_disk_small_volume_under_floor_is_critical() {
        let stats = DiskStats {
            total_bytes: 10 * BYTES_PER_GIB,
            free_bytes: BYTES_PER_GIB,
        };
        assert_eq!(evaluate_disk(stats).level, HealthLevel::Critical);
    }

    #[test]
    fn test_evaluate_disk_zero_total_is_critical() {
        let stats = DiskStats {
            total_bytes: 0,
            free_bytes: 0,
        };
        assert_eq!(evaluate_disk(stats).level, HealthLevel::Critical);
    }

    #[test]
    fn test_check_disk_free_uses_injected_statfs() {
        let r = check_disk_free(Path::new("/data"), |p| {
            assert_eq!(p, Path::new("/data"));
            Ok(disk(3))
        });
        assert_eq!(r.level, HealthLevel::Critical);
    }

    #[test]
    fn test_check_disk_free_probe_error_is_warn() {
        let r = check_disk_free(Path::new("/x"), |_| Err(io::Error::other("boom")));
        assert_eq!(r.level, HealthLevel::Warn);
        assert!(r.detail.contains("boom"));
    }

    #[test]
    fn test_parse_df_output_valid_returns_bytes() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 1000 400 600 40% /\n";
        let s = parse_df_output(out).expect("valid df output");
        assert_eq!(
            s,
            DiskStats {
                total_bytes: 1000 * 1024,
                free_bytes: 600 * 1024
            }
        );
    }

    #[test]
    fn test_parse_df_output_garbage_is_error() {
        assert!(parse_df_output("nope").is_err());
        assert!(parse_df_output("h\n/dev/x a b c").is_err());
    }

    #[test]
    fn test_evaluate_backlog_thresholds() {
        assert_eq!(evaluate_backlog(0, 0).level, HealthLevel::Ok);
        assert_eq!(
            evaluate_backlog(BACKLOG_WARN_DEPTH - 1, 0).level,
            HealthLevel::Ok
        );
        assert_eq!(
            evaluate_backlog(BACKLOG_WARN_DEPTH, 0).level,
            HealthLevel::Warn
        );
        assert_eq!(
            evaluate_backlog(1, BACKLOG_WARN_OLDEST_SECS).level,
            HealthLevel::Warn
        );
        assert_eq!(
            evaluate_backlog(BACKLOG_CRITICAL_DEPTH, 0).level,
            HealthLevel::Critical
        );
        assert_eq!(
            evaluate_backlog(1, BACKLOG_CRITICAL_OLDEST_SECS).level,
            HealthLevel::Critical
        );
    }
}
