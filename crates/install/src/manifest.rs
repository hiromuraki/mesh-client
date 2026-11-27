use crate::{layout, signing, Error};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// `bin/<槽>/version.json`：版本号、槽位、平台加每个文件的 sha256，整体由发布私钥签名。
///
/// 四个字段都没有 `#[serde(default)]`：缺了 `files` 等于“零个文件要校验”，
/// 缺了 `signature` 等于“不验签”。两者都会让坏包直接过闸，必须解析失败。
///
/// `files` 里不含 `version.json` 自己（自己的哈希写不进去），所以清单本身靠
/// `signature` 保护：改 `version`、`slot`、`platform`、`files` 任何一处，验签都不过。
///
/// 字段是私有的：能读它的地方只需要这几个访问器，而构造只能经过 [`Manifest::parse`]——
/// 一个「解析成功但没验过 `validate`」的清单不该能被造出来。
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    version: String,
    slot: String,
    platform: String,
    files: BTreeMap<String, String>,
    signature: String,
}

impl Manifest {
    /// 发布版本号（`YYYY.MM.DD-N`）。
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 发布槽位：`client` 或 `launcher`。
    pub fn slot(&self) -> &str {
        &self.slot
    }

    /// `GOOS/GOARCH` 形式的平台标识。
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// 文件名 → sha256。不含 `version.json` 自己。
    pub fn files(&self) -> &BTreeMap<String, String> {
        &self.files
    }
    /// 解析清单文本。前缀由调用方按场景加：启动器读的是包里的临时成员，客户端
    /// 读的是槽目录里的 `version.json`，两者措辞不同。
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let manifest: Manifest = serde_json::from_str(text).map_err(|err| err.to_string())?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// 清单必须落在发布契约的合法域里。
    ///
    /// 签名只回答「这些字节是不是发布私钥签的」，不回答「这些内容能不能装」。
    /// 两件事分开：形状在解析时定死，免得签错的东西一路走到解包才炸；本机是否适用
    /// （槽位、平台）留给验签方，因为那取决于运行环境而不是清单自己。
    ///
    /// 摘要的长度与大小写也必须定死：`FilesDigest` 按「名字 :: 摘要」拼行，
    /// 长度或大小写不固定就等于同一组文件有几种合法写法，签名覆盖的范围随之模糊。
    fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.slot.as_str(),
            signing::SLOT_CLIENT | signing::SLOT_LAUNCHER
        ) {
            return Err(format!("槽位 {:?} 不在允许范围", self.slot));
        }
        if !crate::version::valid(&self.version) {
            return Err(format!("版本号 {:?} 不合法", self.version));
        }
        if !platform_ok(&self.platform) {
            return Err(format!("平台 {:?} 不是 GOOS/GOARCH 形状", self.platform));
        }
        if self.files.is_empty() {
            return Err("清单没有列任何文件".to_string());
        }
        for (name, digest) in &self.files {
            if !file_name_ok(name) {
                return Err(format!("文件名 {name:?} 不合法"));
            }
            if !hex_ok(digest, 64) {
                return Err(format!("{name} 的摘要不是 64 位小写十六进制：{digest:?}"));
            }
        }
        if !hex_ok(&self.signature, 128) {
            return Err(format!("签名不是 128 位小写十六进制：{:?}", self.signature));
        }
        Ok(())
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

/// 读某个清单文件（通常是 `<root>/bin/<槽>/version.json`）。
///
/// 与 [`Manifest::parse`] 的分工：这里负责把“文件不存在”“不是 JSON”收成一个
/// [`Error::ManifestRead`]，好让上层用 `?` 往上抛；只关心版本号本身的地方
/// （比如只想知道版本号的地方）用它自己的宽松处理。
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

/// 平台是 `GOOS/GOARCH`：小写字母数字加一个斜杠。
fn platform_ok(platform: &str) -> bool {
    let Some((os, arch)) = platform.split_once('/') else {
        return false;
    };
    let lower_alnum = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    };
    !os.is_empty()
        && !arch.is_empty()
        && platform.len() <= 32
        && lower_alnum(os)
        && lower_alnum(arch)
}

/// 定长小写十六进制。
fn hex_ok(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

/// 包内文件名：只能是 `bin/` 下的一个普通文件名。
///
/// `version.json` 不在 `files` 里——它自己写不进自己的摘要。
fn file_name_ok(name: &str) -> bool {
    is_plain_file_name(name) && name != layout::MANIFEST
}

/// 单层普通文件名：非空、≤128 字节、不含路径分隔符与控制字符、不是 `.`/`..`。
///
/// 解包那一侧用同一条判据：清单的合法域只允许这个形状，解包再按另一套规则收，
/// 就等于给路径穿越留第二种解释。
pub fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;

    const DIGEST: &str = "abababababababababababababababababababababababababababababababab";
    const SIGNATURE: &str = concat!(
        "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
        "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
    );

    fn complete() -> String {
        format!(
            r#"{{"version":"2026.11.26-13","slot":"client","platform":"windows/amd64","files":{{"mesh-client.exe":"{DIGEST}"}},"signature":"{SIGNATURE}"}}"#
        )
    }

    fn with_files(files: &str) -> String {
        format!(
            r#"{{"version":"2026.11.26-13","slot":"client","platform":"windows/amd64","files":{{{files}}},"signature":"{SIGNATURE}"}}"#
        )
    }

    #[test]
    fn parses_a_complete_manifest() {
        let manifest = Manifest::parse(&complete()).unwrap();
        assert_eq!(manifest.version(), "2026.11.26-13");
        assert_eq!(manifest.slot(), "client");
        assert_eq!(manifest.platform(), "windows/amd64");
        assert_eq!(manifest.files().len(), 1);
        assert_eq!(manifest.files()["mesh-client.exe"], DIGEST);
    }

    /// 契约之外的东西一律在解析时拒绝：签错的东西不该走到解包才发现。
    #[test]
    fn a_manifest_outside_the_contract_is_an_error() {
        for (case, text) in [
            (
                "unknown slot",
                r#"{"version":"2026.11.26-13","slot":"server","platform":"windows/amd64","files":{"a.exe":"abababababababababababababababababababababababababababababababab"},"signature":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}"#.to_string()),
            (
                "empty file set",
                with_files(""),
            ),
            (
                "short digest",
                with_files(r#""a.exe":"ab""#),
            ),
            (
                "uppercase digest",
                with_files(
                    r#""a.exe":"ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB""#,
                ),
            ),
            (
                "non-hex digest",
                with_files(
                    r#""a.exe":"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz""#,
                ),
            ),
            (
                "path separator in name",
                with_files(r#""sub/a.exe":"abababababababababababababababababababababababababababababababab""#),
            ),
            (
                "parent segment in name",
                with_files(r#""..":"abababababababababababababababababababababababababababababababab""#),
            ),
            (
                "manifest lists itself",
                with_files(r#""version.json":"abababababababababababababababababababababababababababababababab""#),
            ),
            (
                "bad platform",
                r#"{"version":"2026.11.26-13","slot":"client","platform":"Windows","files":{"a.exe":"abababababababababababababababababababababababababababababababab"},"signature":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}"#.to_string(),
            ),
        ] {
            assert!(
                Manifest::parse(&text).is_err(),
                "{case} 应当被拒绝：{text}"
            );
        }
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

    /// 清单的 `verify` 必须真的看签名：被签的四个字段各改一处，每一处都不该通过。
    #[test]
    fn verify_covers_every_field() {
        use ed25519_dalek::Signer;
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let public = hex::encode(key.verifying_key().to_bytes());
        let files = BTreeMap::from([("mesh-client.exe".to_string(), DIGEST.to_string())]);
        let message = crate::signing::manifest_string(
            "client",
            "windows/amd64",
            "2026.11.26-13",
            &crate::signing::files_digest(&files),
        );
        let signature = hex::encode(key.sign(message.as_bytes()).to_bytes());
        let manifest = format!(
            r#"{{"version":"2026.11.26-13","slot":"client","platform":"windows/amd64","files":{{"mesh-client.exe":"{DIGEST}"}},"signature":"{signature}"}}"#
        );
        assert!(Manifest::parse(&manifest).unwrap().verify_with(&public));
        assert!(!Manifest::parse(&manifest)
            .unwrap()
            .verify_with(crate::signing::RELEASE_PUBLIC_KEY));

        let other_digest = "1f".repeat(32);
        for (case, tampered) in [
            (
                "version",
                manifest.replacen("2026.11.26-13", "2026.11.26-14", 1),
            ),
            ("slot", manifest.replacen("\"client\"", "\"launcher\"", 1)),
            (
                "platform",
                manifest.replacen("windows/amd64", "linux/amd64", 1),
            ),
            ("files", manifest.replacen(DIGEST, &other_digest, 1)),
        ] {
            let parsed = Manifest::parse(&tampered).unwrap();
            assert!(!parsed.verify_with(&public), "{case} 被改过，签名不该通过");
        }
    }
}
