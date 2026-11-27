//! 发布产物的签名：包摘要（`MESH3`）与清单（`MESH4`）。
//!
//! 两段签名串的形状一致：标签、槽位、平台、版本、摘要，以 `\n` 连接，末尾没有换行，
//! 与 Go 侧 `signing.String` / `signing.ManifestString` 逐字节相同。公钥编在这里而不是
//! 来自控制面：控制面被拿下后仍能改下发内容，只有编进二进制的公钥挡得住它。
//!
//! `MESH3` 签的是整包（tar.zst）的 sha256，由客户端在下载前验；`MESH4` 签的是清单
//! 里每个文件的摘要汇总，由启动器在拉起之前验。两者不合并成一个前缀：`MESH3` 已有
//! 一批签名落在发布记录里，改它的行格式等于让那些记录全部作废。

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// 离线发布私钥对应的公钥（hex）。轮换它意味着发一次新客户端与一个新启动器。
///
/// 引擎把它当作一个可替换的常量：生产就是这一把，测试换成自己生成的密钥对，
/// 这样「有效签名能过」这条路径才测得到——那份私钥不在这台机器上。
pub const RELEASE_PUBLIC_KEY: &str =
    "d0f9ebec1e56c81baf2a6b81512b9afa44c1b8635cc3bbc49d012d10d92cbd9c";

/// 签名串里的槽位名。它与管理接口的字段名前缀无关：签名覆盖的文本要比一次
/// 请求字段改名活得久。
pub const SLOT_CLIENT: &str = "client";
/// 启动器槽位的签名串槽位名。
pub const SLOT_LAUNCHER: &str = "launcher";

/// 发布包签名覆盖的那段文本。
pub fn signing_string(slot: &str, platform: &str, version: &str, sha256: &str) -> String {
    ["MESH3", slot, platform, version, sha256].join("\n")
}

/// 清单签名覆盖的那段文本。
pub fn manifest_string(slot: &str, platform: &str, version: &str, files_digest: &str) -> String {
    ["MESH4", slot, platform, version, files_digest].join("\n")
}

/// 清单里全部文件摘要的汇总，签的就是它。
///
/// 每行 `<名字>::<摘要小写>\n`，按名字的字节序升序——`BTreeMap` 的迭代顺序正是这个，
/// Go 侧显式排序。不用 JSON 重编码：键序、转义、空白在两端的序列化器里默认行为不同，
/// 而这类分叉的表现是「本地全绿、线上每台都拒绝启动」。
///
/// 分隔符用 `::` 而不是制表符：摘要是固定的 64 位十六进制，从行尾剥掉 `\n` 与摘要之后
/// 分隔符的位置唯一，两种写法都不会产生歧义；选前者只是因为它在日志与 diff 里能读。
pub fn files_digest(files: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (name, sha) in files {
        hasher.update(name.as_bytes());
        hasher.update(b"::");
        // 构建脚本写的是小写，但两端都归一：万一混进大写，签名与验签不该因此分叉。
        hasher.update(sha.to_ascii_lowercase().as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// 这份签名是否由发布私钥对同一组字段签出。
pub fn verify_release(
    slot: &str,
    platform: &str,
    version: &str,
    sha256: &str,
    signature_hex: &str,
) -> bool {
    verify_release_with(
        RELEASE_PUBLIC_KEY,
        slot,
        platform,
        version,
        sha256,
        signature_hex,
    )
}

/// 用给定的公钥（hex）验包签名。
pub fn verify_release_with(
    public_hex: &str,
    slot: &str,
    platform: &str,
    version: &str,
    sha256: &str,
    signature_hex: &str,
) -> bool {
    verify_bytes(
        public_hex,
        &signing_string(slot, platform, version, sha256),
        signature_hex,
    )
}

/// 这份清单是否由发布私钥签出。
pub fn verify_manifest(
    slot: &str,
    platform: &str,
    version: &str,
    files: &BTreeMap<String, String>,
    signature_hex: &str,
) -> bool {
    verify_manifest_with(
        RELEASE_PUBLIC_KEY,
        slot,
        platform,
        version,
        files,
        signature_hex,
    )
}

/// 用给定的公钥（hex）验清单签名。
pub fn verify_manifest_with(
    public_hex: &str,
    slot: &str,
    platform: &str,
    version: &str,
    files: &BTreeMap<String, String>,
    signature_hex: &str,
) -> bool {
    verify_bytes(
        public_hex,
        &manifest_string(slot, platform, version, &files_digest(files)),
        signature_hex,
    )
}

/// 解析失败一律算不通过：空串、非 hex、长度不对、公钥写错。
fn verify_bytes(public_hex: &str, message: &str, signature_hex: &str) -> bool {
    let Ok(key_bytes) = hex::decode(public_hex) else {
        return false;
    };
    let Ok(key_bytes) = <[u8; 32]>::try_from(key_bytes.as_slice()) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes) else {
        return false;
    };
    let Ok(raw) = hex::decode(signature_hex) else {
        return false;
    };
    let Ok(raw) = <[u8; 64]>::try_from(raw.as_slice()) else {
        return false;
    };
    key.verify(message.as_bytes(), &Signature::from_bytes(&raw))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, sha)| (name.to_string(), sha.to_string()))
            .collect()
    }

    #[test]
    fn signing_string_is_the_go_side_layout() {
        assert_eq!(
            signing_string(SLOT_CLIENT, "windows/amd64", "2026.11.27-103", "ab"),
            "MESH3\nclient\nwindows/amd64\n2026.11.27-103\nab"
        );
    }

    #[test]
    fn manifest_string_is_the_go_side_layout() {
        assert_eq!(
            manifest_string(SLOT_CLIENT, "windows/amd64", "2026.11.27-110", "cd"),
            "MESH4\nclient\nwindows/amd64\n2026.11.27-110\ncd"
        );
    }

    /// 汇总只取决于「名字 + 摘要」的集合：插入顺序、摘要大小写都不该改变它。
    #[test]
    fn files_digest_ignores_insertion_order_and_case() {
        let a = files_digest(&files(&[("b.dll", "0A"), ("a.exe", "ff")]));
        let b = files_digest(&files(&[("a.exe", "ff"), ("b.dll", "0a")]));
        assert_eq!(a, b);
        // 名字进汇总：改一个字符就不再相同。
        assert_ne!(a, files_digest(&files(&[("c.dll", "0a"), ("a.exe", "ff")])));
        // 摘要进汇总：改一个字节也不再相同。
        assert_ne!(a, files_digest(&files(&[("b.dll", "0b"), ("a.exe", "ff")])));
    }

    /// 与 Go 的 `signing.FilesDigest` 钉在一起：两边算出的字节必须相同。
    /// 这条加上 `manifest_string_is_the_go_side_layout`，把跨语言格式锁死。
    #[test]
    fn files_digest_matches_the_go_side() {
        let map = files(&[("wintun.dll", "cd"), ("mesh-client.exe", "ab")]);
        assert_eq!(
            files_digest(&map),
            "442a109466d320336abdcbad56bdb6cf6fdbf262a50232c599000820f85f31c0"
        );
    }

    #[test]
    fn an_empty_manifest_still_has_a_digest() {
        assert_eq!(
            files_digest(&BTreeMap::new()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_signature_by_the_matching_key_verifies() {
        use ed25519_dalek::Signer;
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let public = hex::encode(key.verifying_key().to_bytes());
        let message = signing_string(SLOT_CLIENT, "windows/amd64", "2026.11.27-103", "ab");
        let signature = hex::encode(key.sign(message.as_bytes()).to_bytes());
        assert!(verify_release_with(
            &public,
            SLOT_CLIENT,
            "windows/amd64",
            "2026.11.27-103",
            "ab",
            &signature
        ));
        // 槽位进签名串：换个槽位就不该通过。
        assert!(!verify_release_with(
            &public,
            SLOT_LAUNCHER,
            "windows/amd64",
            "2026.11.27-103",
            "ab",
            &signature
        ));
    }

    /// 清单签名与包签名是两个前缀：拿包签名当清单签名用不该通过。
    #[test]
    fn a_package_signature_is_not_a_manifest_signature() {
        use ed25519_dalek::Signer;
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let public = hex::encode(key.verifying_key().to_bytes());
        let map = files(&[("mesh-client.exe", "ab")]);
        let digest = files_digest(&map);
        let package = signing_string(SLOT_CLIENT, "windows/amd64", "2026.11.27-110", &digest);
        let signature = hex::encode(key.sign(package.as_bytes()).to_bytes());
        assert!(!verify_manifest_with(
            &public,
            SLOT_CLIENT,
            "windows/amd64",
            "2026.11.27-110",
            &map,
            &signature
        ));
    }

    #[test]
    fn a_manifest_signature_covers_every_field() {
        use ed25519_dalek::Signer;
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let public = hex::encode(key.verifying_key().to_bytes());
        let map = files(&[("mesh-client.exe", "ab")]);
        let message = manifest_string(
            SLOT_CLIENT,
            "windows/amd64",
            "2026.11.27-110",
            &files_digest(&map),
        );
        let signature = hex::encode(key.sign(message.as_bytes()).to_bytes());
        assert!(verify_manifest_with(
            &public,
            SLOT_CLIENT,
            "windows/amd64",
            "2026.11.27-110",
            &map,
            &signature
        ));
        // 版本、平台、槽位、文件集合任一不同都不该通过。
        for (slot, platform, version, other) in [
            (SLOT_CLIENT, "windows/amd64", "2026.11.27-111", map.clone()),
            (SLOT_CLIENT, "linux/amd64", "2026.11.27-110", map.clone()),
            (
                SLOT_LAUNCHER,
                "windows/amd64",
                "2026.11.27-110",
                map.clone(),
            ),
            (
                SLOT_CLIENT,
                "windows/amd64",
                "2026.11.27-110",
                files(&[("mesh-client.exe", "ac")]),
            ),
        ] {
            assert!(
                !verify_manifest_with(&public, slot, platform, version, &other, &signature),
                "{slot} {platform} {version}"
            );
        }
    }

    /// 真实发布私钥签出的固定向量：`server/tools/signpkg -slot client
    /// -platform windows/amd64 -version 2026.11.27-103 -sha256 d0f9…`。
    /// 它把 Go 那边签出来的字节钉在 Rust 这边的验证上——行串格式一漂移、公钥
    /// 一被改错，这条立刻失败。
    #[test]
    fn a_signature_from_the_release_key_verifies() {
        const SIGNATURE: &str = concat!(
            "9b940038e5cde63b3da308c94bc7f4dee90cabb9cbe61683982e6ae1b6847f1f",
            "6cc68eb4e85af969d44cfb2e687ff46077f5351ba6028f7d07a6365b50b2d400",
        );
        assert!(verify_release(
            SLOT_CLIENT,
            "windows/amd64",
            "2026.11.27-103",
            "d0f9ebec1e56c81baf2a6b81512b9afa44c1b8635cc3bbc49d012d10d92cbd9c",
            SIGNATURE
        ));
        assert!(!verify_release(
            SLOT_LAUNCHER,
            "windows/amd64",
            "2026.11.27-103",
            "d0f9ebec1e56c81baf2a6b81512b9afa44c1b8635cc3bbc49d012d10d92cbd9c",
            SIGNATURE
        ));
    }

    /// 真实发布私钥签出的清单向量：`signpkg -manifest` 签的就是这份内容。
    /// 它证明 Go 的 `FilesDigest`/`ManifestString` 与这里的验证逐字节一致——
    /// 跨语言格式一分叉这条就红，而不是等每台设备都起不来才发现。
    #[test]
    fn a_manifest_signature_from_the_release_key_verifies() {
        const SIGNATURE: &str = concat!(
            "d3c064d70098d40a7d9d9c7a6ad859f1429a5ccbbe837839c6f4e8056d510ddb",
            "41f2e3b37b810160df718b99199d719e1271a4f75608851b006474d7ec74ef02",
        );
        let mut map = files(&[("mesh-client.exe", "ab"), ("wintun.dll", "cd")]);
        assert!(verify_manifest(
            SLOT_CLIENT,
            "windows/amd64",
            "2026.11.27-110",
            &map,
            SIGNATURE
        ));
        // 改一个摘要就不该通过。
        map.insert("wintun.dll".to_string(), "ce".to_string());
        assert!(!verify_manifest(
            SLOT_CLIENT,
            "windows/amd64",
            "2026.11.27-110",
            &map,
            SIGNATURE
        ));
    }

    #[test]
    fn a_malformed_signature_never_verifies() {
        for signature in ["", "00", "zz", &"00".repeat(63)[..]] {
            assert!(!verify_release(
                SLOT_CLIENT,
                "windows/amd64",
                "2026.11.27-103",
                "ab",
                signature
            ));
        }
    }
}
