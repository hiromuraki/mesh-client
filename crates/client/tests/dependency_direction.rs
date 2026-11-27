//! 依赖方向的闸门。
//!
//! MODULES.md 第 2 节的规则（text 是地板、叶子 crate 不许依赖引擎、engine 不认识
//! 面板与进程形态……）靠人记不住，也不该靠 review 时逐个 crate 翻 manifest。
//! 这里把“允许的边”写成一张表，多一条边就让 `cargo test` 红。
//!
//! 放在 mesh-client 里是因为它是装配点：树的根知道所有叶子该长什么样。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 每个成员允许依赖的 workspace 内 crate（只写短名，前缀 `mesh-` 自动补）。
const MEMBERS: &[(&str, &[&str])] = &[
    ("text", &[]),
    ("install", &["text"]),
    ("state", &["install", "text"]),
    ("remote", &["install", "text"]),
    ("etrpc", &["text"]),
    ("core", &["etrpc", "text"]),
    (
        "engine",
        &["install", "state", "remote", "core", "etrpc", "text"],
    ),
    ("panel", &["engine", "etrpc", "text"]),
    ("client", &["install", "state", "engine", "panel", "text"]),
    ("launcher", &["install", "text"]),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/client 一定在 workspace 根下")
        .to_path_buf()
}

fn read_manifest(root: &Path, member: &str) -> toml::Value {
    let path = root.join("crates").join(member).join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("读不到 {}: {err}", path.display()));
    text.parse::<toml::Value>()
        .unwrap_or_else(|err| panic!("{} 不是合法 TOML: {err}", path.display()))
}

fn collect_mesh_deps(table: &toml::Value, into: &mut BTreeSet<String>) {
    let Some(table) = table.as_table() else {
        return;
    };
    for (name, spec) in table {
        // 依赖可以写成 `package = "mesh-state"` 的别名，按真实 crate 名判断。
        let package = spec
            .get("package")
            .and_then(|value| value.as_str())
            .unwrap_or(name);
        if package.starts_with("mesh-") {
            into.insert(package.to_string());
        }
    }
}

fn declared_mesh_deps(manifest: &toml::Value) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(key) {
            collect_mesh_deps(table, &mut deps);
        }
    }
    // 平台专属依赖同样算数：没有人能靠 `[target.'cfg(windows)'.dependencies]`
    // 让 install 依赖 engine。
    if let Some(targets) = manifest.get("target").and_then(|value| value.as_table()) {
        for target in targets.values() {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(table) = target.get(key) {
                    collect_mesh_deps(table, &mut deps);
                }
            }
        }
    }
    deps
}

#[test]
fn no_member_depends_outside_the_allowed_edges() {
    let root = workspace_root();
    for (member, allowed) in MEMBERS {
        let manifest = read_manifest(&root, member);
        let package = manifest
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(|name| name.as_str())
            .unwrap_or_default();
        assert_eq!(
            package,
            format!("mesh-{member}"),
            "crates/{member} 的 package 名必须是 mesh-{member}"
        );
        let allowed: BTreeSet<String> = allowed.iter().map(|name| format!("mesh-{name}")).collect();
        let forbidden: Vec<String> = declared_mesh_deps(&manifest)
            .difference(&allowed)
            .cloned()
            .collect();
        assert!(
            forbidden.is_empty(),
            "crates/{member} 依赖了不允许的 crate: {forbidden:?}（MODULES.md 第 2 节）"
        );
    }
}

#[test]
fn workspace_lists_exactly_the_ten_members() {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let manifest: toml::Value = manifest.parse().unwrap();
    let members: BTreeSet<String> = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(|members| members.as_array())
        .expect("workspace.members 必须是数组")
        .iter()
        .filter_map(|member| member.as_str())
        .map(str::to_string)
        .collect();
    let expected: BTreeSet<String> = MEMBERS
        .iter()
        .map(|(member, _)| format!("crates/{member}"))
        .collect();
    assert_eq!(
        members, expected,
        "workspace 成员与 MODULES.md 第 1 节不一致"
    );
}
