//! 从 `lang/chinese.lang` 生成 `id` 模块的常量表。
//!
//! 手写这张表意味着加一条文案要改四处（两份清单、常量、`ALL_IDS`），而漏掉
//! `ALL_IDS` 不会报错——偏偏它是唯一证明「id 与清单一一对应」的东西。清单是
//! 唯一事实来源，常量与那张表都由它派生，同步问题于是变成编译期问题。

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    let lang = PathBuf::from("lang/chinese.lang");
    println!("cargo:rerun-if-changed=lang/chinese.lang");
    println!("cargo:rerun-if-changed=lang/english.lang");
    let text = std::fs::read_to_string(&lang)
        .unwrap_or_else(|err| panic!("读不到 {}：{err}", lang.display()));

    // 常量名 → id。按常量名排序，生成出来的模块就是稳定顺序。
    let mut by_name: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.split('.').count() != 3 {
            continue;
        }
        let name = constant_name(key);
        if let Some(other) = by_name.insert(name.clone(), key.to_string()) {
            panic!("{key} 与 {other} 折成了同一个常量名 {name}，换一个 id");
        }
    }

    let mut out = String::new();
    out.push_str("// @generated：由 build.rs 从 lang/chinese.lang 生成，不要手改。\n");
    for (name, key) in &by_name {
        writeln!(out, "    pub const {name}: &str = {key:?};").unwrap();
    }
    out.push_str(
        "\n    /// 清单里的全部 id。只有测试用它证明 id 与清单一一对应，所以不参与\n    /// 正常构建。\n    #[cfg(test)]\n    pub(crate) const ALL_IDS: &[&str] = &[\n",
    );
    for key in by_name.values() {
        writeln!(out, "        {key:?},").unwrap();
    }
    out.push_str("    ];\n");

    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("ids.rs");
    std::fs::write(&dest, out).unwrap_or_else(|err| panic!("写不到 {}：{err}", dest.display()));
}

/// `engine.log.update_url_not_absolute` → `ENGINE_LOG_UPDATE_URL_NOT_ABSOLUTE`。
fn constant_name(key: &str) -> String {
    key.to_ascii_uppercase().replace('.', "_")
}
