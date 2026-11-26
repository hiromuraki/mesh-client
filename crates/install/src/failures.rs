use crate::layout;
use crate::version;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 记录的版本数上限。失败记录只用来挡住“同一个包装了又坏、坏了又装”的循环，
/// 留几个版本足够；不剪裁的话这台机器跑上一年会攒出几千条。
const KEEP_VERSIONS: usize = 5;
/// 同一个版本允许连续失败几次就停手。一次可能是瞬时问题（磁盘满、端口没释放），
/// 两次还是起不来就说明包本身有问题。
const LIMIT: u32 = 2;

/// `<root>/.update-failures.json` 的内存视图。
///
/// 写它的只有启动器（boot-check 超时的那一刻客户端已经崩了，只有启动器还活着）；
/// 读它的有两个：客户端在下载前读（省掉十几 MB 流量），启动器在安装前再读一次
/// （客户端可能拿着旧值）。
pub struct Failures {
    path: PathBuf,
    map: BTreeMap<String, u32>,
}

impl Failures {
    /// 读计数。文件不存在、内容坏了都按空表处理：这是一份“试过几次”的记账，
    /// 读不出来最多多试一次，不值得因此挡住安装。
    pub fn load(root: &Path) -> Self {
        let path = root.join(layout::FAILURES);
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, map }
    }

    /// 某个版本已经失败过几次。
    pub fn count(&self, version: &str) -> u32 {
        self.map.get(version).copied().unwrap_or(0)
    }

    /// 这个版本别再自动装了，等人手动干预。
    pub fn exhausted(&self, version: &str) -> bool {
        self.count(version) >= LIMIT
    }

    /// 记一次失败。写盘失败不是致命错误：计数丢了顶多多试一次。
    pub fn record(&mut self, version: &str) {
        let entry = self.map.entry(version.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        self.prune();
        self.save();
    }

    /// 某个版本确认启动成功后调用，把这个版本的记录抹掉。
    pub fn clear(&mut self, version: &str) {
        if self.map.remove(version).is_some() {
            self.save();
        }
    }

    /// 只留版本号最大的几个。按版本号排而不是按写入时间：版本号本身单调，
    /// 省掉时间戳字段，反复失败同一个版本也只占一格。
    fn prune(&mut self) {
        if self.map.len() <= KEEP_VERSIONS {
            return;
        }
        let mut keys: Vec<String> = self.map.keys().cloned().collect();
        keys.sort_by(|a, b| version::compare(b, a));
        for key in keys.into_iter().skip(KEEP_VERSIONS) {
            self.map.remove(&key);
        }
    }

    fn save(&self) {
        let Ok(text) = serde_json::to_string_pretty(&self.map) else {
            return;
        };
        let _ = crate::atomic_replace(&self.path, text.as_bytes(), 0o600);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;

    #[test]
    fn counts_until_exhausted() {
        let root = temp_dir("failures-count");
        let mut failures = Failures::load(&root);
        assert!(!failures.exhausted("2026.11.26-14"));
        failures.record("2026.11.26-14");
        assert_eq!(failures.count("2026.11.26-14"), 1);
        assert!(!failures.exhausted("2026.11.26-14"));
        failures.record("2026.11.26-14");
        assert!(failures.exhausted("2026.11.26-14"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn survives_a_reload() {
        let root = temp_dir("failures-reload");
        {
            let mut failures = Failures::load(&root);
            failures.record("2026.11.26-14");
        }
        assert_eq!(Failures::load(&root).count("2026.11.26-14"), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clearing_forgets_the_version() {
        let root = temp_dir("failures-clear");
        let mut failures = Failures::load(&root);
        failures.record("2026.11.26-14");
        failures.clear("2026.11.26-14");
        assert_eq!(failures.count("2026.11.26-14"), 0);
        assert_eq!(Failures::load(&root).count("2026.11.26-14"), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keeps_only_the_newest_versions() {
        let root = temp_dir("failures-prune");
        let mut failures = Failures::load(&root);
        for seq in 1..=8 {
            failures.record(&format!("2026.11.26-{seq}"));
        }
        assert_eq!(failures.map.len(), KEEP_VERSIONS);
        // 留下的是序号最大的 5 个；按字符串序会留下 -1 -2 -3 -4 -5。
        assert!(failures.map.contains_key("2026.11.26-8"));
        assert!(failures.map.contains_key("2026.11.26-4"));
        assert!(!failures.map.contains_key("2026.11.26-3"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
