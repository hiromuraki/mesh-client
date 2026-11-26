use crate::Error;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// `bin/version.json`：版本号加每个文件的 sha256。
///
/// `files` 没有 `#[serde(default)]`：缺了它等于“零个文件要校验”，坏包会被直接放过。
/// 这一点与启动器原实现一致，是有意的。
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub files: BTreeMap<String, String>,
}

impl Manifest {
    /// 解析清单文本。错误就是 serde 那一句话，前缀由调用方按场景加：
    /// 启动器读的是包里的临时成员，客户端读的是 `bin/version.json`，两者措辞不同。
    pub fn parse(text: &str) -> Result<Manifest, String> {
        serde_json::from_str(text).map_err(|err| err.to_string())
    }
}

/// 读某个清单文件（通常是 `<root>/bin/version.json`）。
///
/// 与 [`Manifest::parse`] 的分工：这里负责把“文件不存在”“不是 JSON”收成一个
/// [`Error::ManifestRead`]，好让上层用 `?` 往上抛；只关心版本号本身的地方
/// （比如启动器的 `installed_version`）用它自己的宽松处理。
pub fn read(path: &Path) -> Result<Manifest, Error> {
    let text = std::fs::read_to_string(path).map_err(|err| Error::ManifestRead {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    Manifest::parse(&text).map_err(|reason| Error::ManifestRead {
        path: path.to_path_buf(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;

    #[test]
    fn parses_a_complete_manifest() {
        let manifest = Manifest::parse(
            r#"{"version":"2026.11.26-13","files":{"mesh-client.exe":"ab","version.json":"cd"}}"#,
        )
        .unwrap();
        assert_eq!(manifest.version, "2026.11.26-13");
        assert_eq!(manifest.files.len(), 2);
        assert_eq!(manifest.files["mesh-client.exe"], "ab");
    }

    #[test]
    fn missing_files_is_an_error() {
        // 缺 files 的清单必须失败：否则“校验零个文件”会让坏包直接过闸。
        assert!(Manifest::parse(r#"{"version":"2026.11.26-13"}"#).is_err());
    }

    #[test]
    fn missing_version_is_an_error() {
        assert!(Manifest::parse(r#"{"files":{}}"#).is_err());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(Manifest::parse("{").is_err());
    }

    #[test]
    fn read_reports_a_missing_file() {
        let dir = temp_dir("manifest-read");
        let err = read(&dir.join("version.json")).unwrap_err();
        assert!(matches!(err, Error::ManifestRead { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_parses_an_existing_file() {
        let dir = temp_dir("manifest-ok");
        let path = dir.join("version.json");
        std::fs::write(&path, r#"{"version":"2026.11.26-13","files":{}}"#).unwrap();
        assert_eq!(read(&path).unwrap().version, "2026.11.26-13");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
