//! `pair-ops` command line: argument parsing and command execution (kept in the library so both
//! are testable; the binary only wires up the environment).
use crate::error::{OpsError, Result};
use crate::retention::RetentionPolicy;
use chrono::Duration;
use std::path::PathBuf;

pub const USAGE: &str = "\
usage: pair-ops <command> [options]

commands:
  purge   [--dry-run] [--payload-days N] [--source-days N] [--batch-size N] [--max-batches N]
          erase model/tool payloads and raw sources older than the retention window
  export  --out DIR [--include-config]
          write JSON-lines, a Markdown index and manifest.json into DIR

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

/// Parses arguments after the program name.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command> {
    let mut it = args.into_iter();
    match it.next().as_deref() {
        None | Some("-h" | "--help" | "help") => Ok(Command::Help),
        Some("purge") => parse_purge(it),
        Some("export") => parse_export(it),
        Some(other) => Err(invalid(format!("unknown command {other:?}"))),
    }
}

fn parse_purge(mut it: impl Iterator<Item = String>) -> Result<Command> {
    let mut policy = RetentionPolicy::default();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--dry-run" => policy.dry_run = true,
            "--payload-days" => {
                policy.payload_retention = Duration::days(positive("--payload-days", it.next())?)
            }
            "--source-days" => {
                policy.source_retention = Duration::days(positive("--source-days", it.next())?)
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
    fn test_parse_no_args_and_help_show_usage() {
        assert_eq!(parse(args(&[])).expect("parse"), Command::Help);
        assert_eq!(parse(args(&["--help"])).expect("parse"), Command::Help);
    }
}
