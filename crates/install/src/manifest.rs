use crate::Error;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// `bin/version.json`：版本号、槽位、平台加每个文件的 sha256，整体由发布私钥签名。
///
/// 四个字段都没有 `#[serde(default)]`：缺了 `files` 等于“零个文件要校验”，
/// 缺了 `signature` 等于“不验签”。两者都会让坏包直接过闸，必须解析失败。
///
/// `files` 里不含 `version.json` 自己（自己的哈希写不进去），所以清单本身靠
/// `signature` 保护：改 `version`、`slot`、`platform`、`files` 任何一处，验签都不过。
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub slot: String,
    pub platform: String,
    pub files: BTreeMap<String, String>,
    pub signature: String,
}

impl Manifest {
    /// 解析清单文本。错误就是 serde 那一句话，前缀由调用方按场景加：
    /// 启动器读的是包里的临时成员，客户端读的是 `bin/version.json`，两者措辞不同。
    pub fn parse(text: &str) -> Result<Manifest, String> {
        serde_json::from_str(text).map_err(|err| err.to_string())
    }

    /// 清单是否由发布私钥签出。
    pub fn verify(&self) -> bool {
        self.verify_with(crate::signing::RELEASE_PUBLIC_KEY)
    }

    /// 用给定的公钥（hex）验。轮换公钥与测试走这一条。
    pub fn verify_with(&self, public_hex: &str) -> bool {
        crate::signing::verify_manifest_with(
            public_hex,
            &self.slot,
            &self.platform,
            &self.version,
            &self.files,
            &self.signature,
        )
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

    fn complete() -> &'static str {
        r#"{"version":"2026.11.26-13","slot":"client","platform":"windows/amd64","files":{"mesh-client.exe":"ab","version.json":"cd"},"signature":"00"}"#
    }

    #[test]
    fn parses_a_complete_manifest() {
        let manifest = Manifest::parse(complete()).unwrap();
        assert_eq!(manifest.version, "2026.11.26-13");
        assert_eq!(manifest.slot, "client");
        assert_eq!(manifest.platform, "windows/amd64");
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

    /// 没有签名的清单必须解析失败，而不是“解析成功、验签为空串所以不过”——
    /// 后者依赖调用方记得检查，前者不依赖。
    #[test]
    fn missing_signature_is_an_error() {
        assert!(Manifest::parse(
            r#"{"version":"2026.11.26-13","slot":"client","platform":"windows/amd64","files":{}}"#
        )
        .is_err());
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
        std::fs::write(&path, complete()).unwrap();
        assert_eq!(read(&path).unwrap().version, "2026.11.26-13");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清单的 `verify` 必须真的看签名：同一份内容换掉一个字段就不该通过。
    #[test]
    fn verify_covers_every_field() {
        use ed25519_dalek::Signer;
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let public = hex::encode(key.verifying_key().to_bytes());
        let mut files = BTreeMap::new();
        files.insert("mesh-client.exe".to_string(), "ab".to_string());
        let message = crate::signing::manifest_string(
            "client",
            "windows/amd64",
            "2026.11.26-13",
            &crate::signing::files_digest(&files),
        );
        let signature = hex::encode(key.sign(message.as_bytes()).to_bytes());
        let mut manifest = Manifest {
            version: "2026.11.26-13".into(),
            slot: "client".into(),
            platform: "windows/amd64".into(),
            files,
            signature,
        };
        assert!(manifest.verify_with(&public));
        assert!(!manifest.verify_with(crate::signing::RELEASE_PUBLIC_KEY));
        manifest.version = "2026.11.26-14".into();
        assert!(!manifest.verify_with(&public));
    }
}
