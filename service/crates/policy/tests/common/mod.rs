#![allow(dead_code)]
use pair_core::ids::{TaskId, TraceId};
use pair_core::types::{ActionRequest, DataClass, PolicyContext};
use pair_policy::PolicyEngine;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub const CONFIG_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/policy.yaml");

pub struct Fixture {
    pub _dir: TempDir,
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub engine: PolicyEngine,
    pub ctx: PolicyContext,
}

pub fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize tempdir");
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(home.join(".ssh")).expect("mkdir ssh");
    std::fs::create_dir_all(home.join(".aws")).expect("mkdir aws");
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");
    std::fs::write(home.join(".ssh/id_rsa"), "secret").expect("write key");
    let engine = PolicyEngine::from_config_file(Path::new(CONFIG_PATH), &home).expect("load policy config");
    let ctx = PolicyContext {
        workspace_root: workspace.to_string_lossy().into_owned(),
        approvals: vec![],
        policy_version: engine.version().to_owned(),
    };
    Fixture { _dir: dir, home, workspace, engine, ctx }
}

pub fn request(tool: &str) -> ActionRequest {
    ActionRequest {
        tool: tool.to_owned(),
        executable: None,
        args: vec![],
        paths: vec![],
        destination: None,
        data_class: DataClass::Public,
        task: TaskId::new(),
        trace: TraceId::new(),
    }
}
