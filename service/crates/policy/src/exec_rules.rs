//! Per-executable validation. The executable allowlist is NOT a code-execution boundary:
//! interpreters and build tools run arbitrary code by design, and even `git` and `sed` can
//! be steered into running commands. This module classifies a command line as fixed-purpose
//! (`Safe`) or `CodeExec`, and refuses outright the git invocations that inject config.

const GIT_SUBCOMMANDS: [&str; 10] = [
    "status",
    "diff",
    "log",
    "show",
    "add",
    "commit",
    "worktree",
    "rev-parse",
    "ls-files",
    "update-index",
];
const GIT_SAFE_GLOBAL_FLAGS: [&str; 2] = ["--no-pager", "--no-optional-locks"];
const GIT_FORBIDDEN_FLAGS: [&str; 9] = [
    "--upload-pack",
    "--receive-pack",
    "--exec-path",
    "--config-env",
    "--config",
    "--ext-diff",
    "--textconv",
    "--output",
    "--open-files-in-pager",
];
const GIT_FORBIDDEN_CONFIG_KEYS: [&str; 10] = [
    "alias.",
    "core.sshcommand",
    "core.fsmonitor",
    "core.hookspath",
    "core.pager",
    "core.editor",
    "core.askpass",
    "diff.external",
    "credential.helper",
    "gpg.program",
];
const GIT_DIR_FLAG: &str = "-C";
const GIT_CONFIG_FLAG: &str = "-c";
const SED_SAFE_FLAGS: [&str; 6] = ["-n", "-E", "-r", "--quiet", "--silent", "--regexp-extended"];
const SED_SUBST_FLAGS: &str = "gpiImM";
const RG_PRE_FLAG: &str = "--pre";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecClass {
    Safe,
    CodeExec,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ExecAnalysis {
    pub class: ExecClass,
    /// Values that are paths regardless of how they look (e.g. the operand of `git -C`).
    pub extra_paths: Vec<String>,
}

impl ExecAnalysis {
    fn of(class: ExecClass) -> Self {
        Self {
            class,
            extra_paths: Vec::new(),
        }
    }
}

/// `Err` means the command line is refused in every environment.
pub fn analyze(exe: &str, args: &[String], code_exec: &[String]) -> Result<ExecAnalysis, String> {
    if code_exec.iter().any(|c| c == exe) {
        return Ok(ExecAnalysis::of(ExecClass::CodeExec));
    }
    match exe {
        "git" => analyze_git(args),
        "sed" => Ok(analyze_sed(args)),
        "rg" if args.iter().any(|a| is_flag(a, RG_PRE_FLAG)) => {
            Ok(ExecAnalysis::of(ExecClass::CodeExec))
        }
        _ => Ok(ExecAnalysis::of(ExecClass::Safe)),
    }
}

fn is_flag(arg: &str, flag: &str) -> bool {
    arg == flag
        || arg
            .strip_prefix(flag)
            .is_some_and(|rest| rest.starts_with('='))
}

fn analyze_git(args: &[String]) -> Result<ExecAnalysis, String> {
    let mut extra_paths = Vec::new();
    let mut rest = args.iter().peekable();
    while let Some(flag) = rest.next_if(|a| a.starts_with('-')) {
        if flag == GIT_DIR_FLAG {
            let dir = rest
                .next()
                .ok_or_else(|| "git -C needs a directory".to_owned())?;
            extra_paths.push(dir.clone());
        } else if flag.starts_with(GIT_CONFIG_FLAG) {
            return Err("git -c config injection is not allowed".to_owned());
        } else if !GIT_SAFE_GLOBAL_FLAGS.contains(&flag.as_str()) {
            return Err(format!(
                "git option {flag:?} is not allowed before the subcommand"
            ));
        }
    }
    let subcommand = rest
        .next()
        .ok_or_else(|| "git needs a subcommand".to_owned())?;
    if !GIT_SUBCOMMANDS.contains(&subcommand.as_str()) {
        return Err(format!("git subcommand {subcommand:?} is not allowed"));
    }
    for arg in rest {
        if let Some(flag) = GIT_FORBIDDEN_FLAGS.iter().find(|f| is_flag(arg, f)) {
            return Err(format!("git flag {flag} is not allowed"));
        }
    }
    reject_config_keys(args)?;
    Ok(ExecAnalysis {
        class: ExecClass::Safe,
        extra_paths,
    })
}

/// Rejects `key=value` pieces that set a code-running git config key.
fn reject_config_keys(args: &[String]) -> Result<(), String> {
    for piece in args
        .iter()
        .flat_map(|a| a.split(|c: char| c.is_whitespace() || c == '\'' || c == '"'))
    {
        let lower = piece.to_ascii_lowercase();
        if let Some(key) = GIT_FORBIDDEN_CONFIG_KEYS
            .iter()
            .find(|k| lower.starts_with(**k) && lower.contains('='))
        {
            return Err(format!("git config key {key} is not allowed"));
        }
    }
    Ok(())
}

fn analyze_sed(args: &[String]) -> ExecAnalysis {
    let mut script_seen = false;
    for arg in args {
        let safe = if arg.starts_with('-') && arg.len() > 1 {
            SED_SAFE_FLAGS.contains(&arg.as_str())
        } else if script_seen {
            true
        } else {
            script_seen = true;
            is_plain_script(arg)
        };
        if !safe {
            return ExecAnalysis::of(ExecClass::CodeExec);
        }
    }
    ExecAnalysis::of(ExecClass::Safe)
}

/// Only print/delete/quit and `s` without the `e`/`w` flags; everything else may execute or write.
fn is_plain_script(script: &str) -> bool {
    script
        .split(['\n', ';'])
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .all(is_plain_command)
}

fn is_plain_command(cmd: &str) -> bool {
    let body = skip_address(cmd).trim_start();
    match body.chars().next() {
        Some('p' | 'd' | 'q' | '=') => body.len() == 1,
        Some('s') => is_plain_substitution(&body[1..]),
        _ => false,
    }
}

fn skip_address(cmd: &str) -> &str {
    if let Some(rest) = cmd.strip_prefix('/') {
        let mut escaped = false;
        for (i, c) in rest.char_indices() {
            match c {
                '\\' if !escaped => escaped = true,
                '/' if !escaped => return &rest[i + 1..],
                _ => escaped = false,
            }
        }
        return cmd;
    }
    cmd.trim_start_matches(|c: char| c.is_ascii_digit() || c == ',' || c == '$')
}

fn is_plain_substitution(rest: &str) -> bool {
    let Some(delim) = rest
        .chars()
        .next()
        .filter(|d| !d.is_alphanumeric() && !"\\ \n".contains(*d))
    else {
        return false;
    };
    let mut delimiters_seen = 0;
    let mut escaped = false;
    let mut flags_start = None;
    for (i, c) in rest.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == delim {
            delimiters_seen += 1;
            if delimiters_seen == 2 {
                flags_start = Some(i + c.len_utf8());
                break;
            }
        }
    }
    flags_start.map(|i| &rest[i..]).is_some_and(|flags| {
        flags
            .chars()
            .all(|f| SED_SUBST_FLAGS.contains(f) || f.is_ascii_digit())
    })
}
