//! Tool names the workflows ask the policy `Gate` for. They must match the registry in
//! `config/policy.yaml` exactly; an unregistered name is denied (fail closed). The
//! `registered_in_policy_config` test keeps the two in sync.

/// Write files inside the workspace (class `local_edit`; checked against `denied_paths`).
pub const FS_WRITE: &str = "fs.write";
/// Run an allowlisted executable inside the workspace (class `local_edit`).
pub const SHELL_EXEC: &str = "shell.exec";
/// Push a branch to a remote (class `external_write`, needs a hash-bound approval).
pub const GIT_PUSH: &str = "git.push";
/// Open a pull request (class `external_write`, needs a hash-bound approval).
pub const PR_CREATE: &str = "pr.create";
/// Query the research search service (class `read`; destination must be on the egress list).
pub const WEB_SEARCH: &str = "web.search";
/// Fetch a page for research (class `read`; destination must be on the egress list).
pub const WEB_FETCH: &str = "web.fetch";

#[cfg(test)]
mod tests {
    use super::*;
    use pair_policy::config::{ActionClass, PolicyConfig};

    #[test]
    fn registered_in_policy_config() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/policy.yaml");
        let text = std::fs::read_to_string(path).expect("read policy.yaml");
        let cfg = PolicyConfig::parse(&text).expect("parse policy.yaml");
        let expected = [
            (FS_WRITE, ActionClass::LocalEdit),
            (SHELL_EXEC, ActionClass::LocalEdit),
            (GIT_PUSH, ActionClass::ExternalWrite),
            (PR_CREATE, ActionClass::ExternalWrite),
            (WEB_SEARCH, ActionClass::Read),
            (WEB_FETCH, ActionClass::Read),
        ];
        for (tool, class) in expected {
            assert_eq!(cfg.tools.get(tool), Some(&class), "{tool}");
        }
    }
}
