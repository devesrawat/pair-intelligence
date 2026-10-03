//! `pair-ops` command line: argument parsing and command execution (kept in the library so both
//! are testable; the binary only wires up the environment).
use crate::error::{OpsError, Result};
use crate::retention::RetentionPolicy;
use chrono::Duration;
use std::path::PathBuf;

/// Upper bound for the retention flags (100 years). Larger values are a typo, and `chrono`
/// panics on durations beyond its range.
pub const MAX_RETENTION_DAYS: i64 = 36_500;
pub const DEFAULT_STUCK_MINUTES: i64 = 60;
/// Exit code of `audit-stuck` when stuck rows exist, so cron and alerting can key on it.
pub const EXIT_STUCK_FOUND: u8 = 3;

pub const USAGE: &str = "\
usage: pair-ops <command> [options]

commands:
  purge   [--dry-run] [--payload-days N] [--source-days N] [--batch-size N] [--max-batches N]
          erase model/tool payloads and raw sources older than the retention window
  export  --out DIR [--include-config]
          write JSON-lines, a Markdown index and manifest.json into DIR (mode 0700/0600);
          DIR must be new, empty, or the output of an earlier export (which is replaced)
  audit-stuck [--older-than-minutes N]
          list tool_executions still `started` after N minutes (default 60); exit 3 if any

environment:
  DATABASE_URL  Postgres connection string (required, no default)
";

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Purge(RetentionPolicy),
    Export {
        out_dir: PathBuf,
        include_config: bool,
    },
    AuditStuck {
        older_than: Duration,
    },
    Help,
}

fn invalid(msg: impl Into<String>) -> OpsError {
    OpsError::InvalidArgument(msg.into())
}

fn positive<T: std::str::FromStr + PartialOrd + Default>(
    flag: &str,
    raw: Option<String>,
) -> Result<T> {
    let raw = raw.ok_or_else(|| invalid(format!("{flag} needs a value")))?;
    let n: T = raw
        .parse()
        .map_err(|_| invalid(format!("{flag} needs a positive integer, got {raw:?}")))?;
    if n > T::default() {
        Ok(n)
    } else {
        Err(invalid(format!("{flag} must be greater than zero")))
    }
}

/// A day count in `1..=MAX_RETENTION_DAYS` as a duration; never panics on absurd input.
fn days(flag: &str, raw: Option<String>) -> Result<Duration> {
    let n: i64 = positive(flag, raw)?;
    if n > MAX_RETENTION_DAYS {
        return Err(invalid(format!(
            "{flag} must be at most {MAX_RETENTION_DAYS} days"
        )));
    }
    Duration::try_days(n).ok_or_else(|| invalid(format!("{flag} is out of range")))
}

/// Parses arguments after the program name.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command> {
    let mut it = args.into_iter();
    match it.next().as_deref() {
        None | Some("-h" | "--help" | "help") => Ok(Command::Help),
        Some("purge") => parse_purge(it),
        Some("export") => parse_export(it),
        Some("audit-stuck") => parse_audit_stuck(it),
        Some(other) => Err(invalid(format!("unknown command {other:?}"))),
    }
}

fn parse_purge(mut it: impl Iterator<Item = String>) -> Result<Command> {
    let mut policy = RetentionPolicy::default();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--dry-run" => policy.dry_run = true,
            "--payload-days" => {
                policy.payload_retention = days("--payload-days", it.next())?;
            }
            "--source-days" => {
                policy.source_retention = days("--source-days", it.next())?;
            }
            "--batch-size" => policy.batch_size = positive("--batch-size", it.next())?,
            "--max-batches" => {
                policy.max_batches_per_target = positive("--max-batches", it.next())?
            }
            other => return Err(invalid(format!("unknown purge option {other:?}"))),
        }
    }
    Ok(Command::Purge(policy))
}

fn parse_audit_stuck(mut it: impl Iterator<Item = String>) -> Result<Command> {
    let mut minutes = DEFAULT_STUCK_MINUTES;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--older-than-minutes" => minutes = positive("--older-than-minutes", it.next())?,
            other => return Err(invalid(format!("unknown audit-stuck option {other:?}"))),
        }
    }
    let older_than = Duration::try_minutes(minutes)
        .ok_or_else(|| invalid("--older-than-minutes is out of range"))?;
    Ok(Command::AuditStuck { older_than })
}

fn parse_export(mut it: impl Iterator<Item = String>) -> Result<Command> {
    let mut out_dir = None;
    let mut include_config = false;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--include-config" => include_config = true,
            "--out" => {
                out_dir = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| invalid("--out needs a directory"))?,
                ))
            }
            other => return Err(invalid(format!("unknown export option {other:?}"))),
        }
    }
    let out_dir = out_dir.ok_or_else(|| invalid("export requires --out DIR"))?;
    Ok(Command::Export {
        out_dir,
        include_config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn test_parse_purge_defaults_and_dry_run() {
        assert_eq!(
            parse(args(&["purge"])).expect("parse"),
            Command::Purge(RetentionPolicy::default())
        );
        let Command::Purge(p) = parse(args(&[
            "purge",
            "--dry-run",
            "--batch-size",
            "10",
            "--payload-days",
            "7",
        ]))
        .expect("parse") else {
            panic!("expected purge");
        };
        assert!(p.dry_run);
        assert_eq!(p.batch_size, 10);
        assert_eq!(p.payload_retention, Duration::days(7));
    }

    #[test]
    fn test_parse_export_requires_out_and_reads_flags() {
        assert!(parse(args(&["export"])).is_err());
        assert_eq!(
            parse(args(&["export", "--out", "/tmp/x", "--include-config"])).expect("parse"),
            Command::Export {
                out_dir: PathBuf::from("/tmp/x"),
                include_config: true
            }
        );
    }

    #[test]
    fn test_parse_rejects_unknown_and_bad_values() {
        assert!(parse(args(&["frobnicate"])).is_err());
        assert!(parse(args(&["purge", "--nope"])).is_err());
        assert!(parse(args(&["purge", "--batch-size", "0"])).is_err());
        assert!(parse(args(&["purge", "--batch-size", "abc"])).is_err());
        assert!(parse(args(&["purge", "--batch-size"])).is_err());
        assert!(parse(args(&["export", "--out"])).is_err());
    }

    #[test]
    fn test_parse_huge_day_counts_are_an_error_not_a_panic() {
        for flag in ["--payload-days", "--source-days"] {
            let huge = i64::MAX.to_string();
            assert!(parse(args(&["purge", flag, &huge])).is_err(), "{flag}");
            let above_cap = (MAX_RETENTION_DAYS + 1).to_string();
            assert!(parse(args(&["purge", flag, &above_cap])).is_err(), "{flag}");
            let at_cap = MAX_RETENTION_DAYS.to_string();
            assert!(parse(args(&["purge", flag, &at_cap])).is_ok(), "{flag}");
        }
    }

    #[test]
    fn test_parse_audit_stuck_defaults_and_threshold() {
        assert_eq!(
            parse(args(&["audit-stuck"])).expect("parse"),
            Command::AuditStuck {
                older_than: Duration::minutes(DEFAULT_STUCK_MINUTES)
            }
        );
        assert_eq!(
            parse(args(&["audit-stuck", "--older-than-minutes", "5"])).expect("parse"),
            Command::AuditStuck {
                older_than: Duration::minutes(5)
            }
        );
        assert!(parse(args(&["audit-stuck", "--older-than-minutes", "0"])).is_err());
        assert!(parse(args(&["audit-stuck", "--older-than-minutes", "x"])).is_err());
        assert!(parse(args(&["audit-stuck", "--nope"])).is_err());
    }

    #[test]
    fn test_parse_no_args_and_help_show_usage() {
        assert_eq!(parse(args(&[])).expect("parse"), Command::Help);
        assert_eq!(parse(args(&["--help"])).expect("parse"), Command::Help);
    }
}
