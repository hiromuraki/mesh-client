//! 发布包签名。
//!
//! 签名串是 `MESH3`、槽位、平台、版本、摘要五行，末尾没有换行，与 Go 侧的
//! `signing.String` 逐字节一致。公钥编在这里而不是来自控制面：控制面被拿下后
//! 仍能改下发内容，只有编进二进制的公钥挡得住它。

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// 离线发布私钥对应的公钥（hex）。轮换它意味着发一次新客户端。
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

/// 发布签名覆盖的那段文本。
pub fn signing_string(slot: &str, platform: &str, version: &str, sha256: &str) -> String {
    ["MESH3", slot, platform, version, sha256].join("\n")
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

/// 用给定的公钥（hex）验签。解析失败一律算不通过：空串、非 hex、长度不对、
/// 公钥写错。
pub fn verify_release_with(
    public_hex: &str,
    slot: &str,
    platform: &str,
    version: &str,
    sha256: &str,
    signature_hex: &str,
) -> bool {
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
    key.verify(
        signing_string(slot, platform, version, sha256).as_bytes(),
        &Signature::from_bytes(&raw),
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_string_is_the_go_side_layout() {
        assert_eq!(
            signing_string(SLOT_CLIENT, "windows/amd64", "2026.11.27-103", "ab"),
            "MESH3\nclient\nwindows/amd64\n2026.11.27-103\nab"
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
        // 版本、平台、摘要、槽位任一不同都不该通过。
        assert!(!verify_release(
            SLOT_LAUNCHER,
            "windows/amd64",
            "2026.11.27-103",
            "d0f9ebec1e56c81baf2a6b81512b9afa44c1b8635cc3bbc49d012d10d92cbd9c",
            SIGNATURE
        ));
    }

    #[test]
    fn a_malformed_signature_never_verifies() {
        for signature in ["", "00", "zz", &"00".repeat(63)[..]] {
            assert!(
                !verify_release(
                    SLOT_CLIENT,
                    "windows/amd64",
                    "2026.11.27-103",
                    "ab",
                    signature
                ),
                "{signature}"
            );
        }
    }
}
