//! Extracts everything an argument could name as a filesystem location or a remote.
//!
//! Arguments are not trusted to be one value: they are split at whitespace, `=`, `;` and
//! quotes (a sed script, `--out=/x`), then at `:` (`KEY:/x`), and short-flag clusters
//! (`-xf/etc/x`) contribute every suffix that starts like a path. Over-extraction only ever
//! produces extra checks, never fewer.

use crate::egress::parse_host;

const PIECE_SEPARATORS: [char; 5] = ['=', ';', '\'', '"', '\n'];
const SCHEME_SEPARATOR: &str = "://";
const PARENT: &str = "..";

/// Locations referenced by a command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ArgRefs {
    pub paths: Vec<String>,
    /// Anything the egress allowlist must approve: URLs and scp-style remotes (as `ssh://host`).
    pub destinations: Vec<String>,
}

pub fn extract(args: &[String]) -> ArgRefs {
    let mut refs = ArgRefs::default();
    for arg in args {
        for piece in arg
            .split(|c: char| c.is_whitespace() || PIECE_SEPARATORS.contains(&c))
            .filter(|p| !p.is_empty())
        {
            if let Some(url) = url_candidate(piece) {
                refs.destinations.push(url.to_owned());
            } else if let Some(remote) = scp_remote(piece) {
                refs.destinations.push(format!("ssh://{remote}"));
            } else {
                refs.paths
                    .extend(piece.split(':').flat_map(path_candidates));
            }
        }
    }
    refs
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "+.-".contains(c)
}

/// The URL inside a piece. A single-dash flag letter glued to the scheme (`-ohttps://x`) is
/// stripped so the real scheme is what gets checked.
fn url_candidate(piece: &str) -> Option<&str> {
    let idx = piece.find(SCHEME_SEPARATOR)?;
    let start = piece[..idx]
        .rfind(|c: char| !is_scheme_char(c))
        .map_or(0, |i| i + 1);
    let candidate = &piece[start..];
    let stripped = match candidate.strip_prefix("--") {
        Some(rest) => rest,
        None => match candidate.strip_prefix('-') {
            Some(rest) => rest.char_indices().nth(1).map_or(rest, |(i, _)| &rest[i..]),
            None => candidate,
        },
    };
    Some(stripped)
}

fn is_host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '-'
}

/// `user@host:path` or `host:path`, as the host. A bare `word:path` counts as a remote only
/// when it cannot be a local `KEY:value`, i.e. the host has a dot or the path looks like a path.
fn scp_remote(piece: &str) -> Option<String> {
    let (left, right) = piece.split_once(':')?;
    if left.is_empty() || right.is_empty() || right.starts_with("//") {
        return None;
    }
    let (user, host) = match left.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, left),
    };
    let user_ok = user.is_none_or(|u| {
        !u.is_empty() && u.chars().all(|c| is_host_char(c) || c == '_' || c == '+')
    });
    let looks_remote =
        user.is_some() || host.contains('.') || right.contains('/') || right.starts_with('~');
    let host_ok = host.chars().all(is_host_char) && !host.starts_with(['-', '.']);
    (user_ok && host_ok && looks_remote && parse_host(host).is_ok())
        .then(|| host.to_ascii_lowercase())
}

fn is_path_like(value: &str) -> bool {
    value.contains('/') || value.starts_with('~') || value == PARENT
}

fn starts_like_path(value: &str) -> bool {
    value.starts_with(['/', '~']) || value == PARENT || value.starts_with("../")
}

fn path_candidates(fragment: &str) -> Vec<String> {
    let mut out = Vec::new();
    if is_path_like(fragment) {
        out.push(fragment.to_owned());
    }
    if let Some(body) = fragment
        .strip_prefix('-')
        .filter(|rest| !rest.starts_with('-'))
    {
        out.extend(
            body.char_indices()
                .map(|(i, _)| &body[i..])
                .filter(|suffix| starts_like_path(suffix))
                .map(str::to_owned),
        );
    }
    out
}
