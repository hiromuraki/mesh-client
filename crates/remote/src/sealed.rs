//! 凭据封装：控制面把凭据私钥封给设备公钥，心跳里只出现密文。
//!
//! 格式与派生规则见 `contract/device-api.md`「凭据封装」。收件人公钥就是设备注册
//! 的那把 Ed25519 公钥换算出来的 X25519 公钥：客户端没有第二份密钥，也不用多存
//! 一个字段。

use crate::Error;
use base64::Engine as _;
use blake2::digest::consts::{U24, U32};
use blake2::{Blake2b, Digest};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305};
use ed25519_dalek::SigningKey;
use x25519_dalek::x25519;

/// 域分离标签：以后用同一对密钥封别的东西（例如 enrollment token）时换一个，
/// 两边的密文不能互换。
const LABEL: &[u8] = b"mesh/credential-sealed/v1";
/// 信封开头的临时 X25519 公钥。
const EPHEMERAL_LEN: usize = 32;
/// 认证标签长度：比它短的信封连一段空明文都不够。
const TAG_LEN: usize = 16;

/// 解一封凭据。信封是 base64 的 `临时公钥 || 密文`，接收方只能是 `recipient`
/// 对应的私钥持有者。
///
/// 失败就是失败：调用方不得回落到响应里的明文字段——那字段正是这条封装要取代的。
pub fn open(recipient: &SigningKey, envelope_b64: &str) -> Result<Vec<u8>, Error> {
    let envelope = crate::BASE64
        .decode(envelope_b64)
        .map_err(|err| seal_error(format!("envelope is not base64: {err}")))?;
    if envelope.len() < EPHEMERAL_LEN + TAG_LEN {
        return Err(seal_error(format!("envelope is {} bytes", envelope.len())));
    }
    let (ephemeral, ciphertext) = envelope.split_at(EPHEMERAL_LEN);
    let mut ephemeral_key = [0u8; EPHEMERAL_LEN];
    ephemeral_key.copy_from_slice(ephemeral);
    // 收件人公钥取 Ed25519 公钥的 Montgomery 形式；私钥是签名私钥展开后的标量，
    // 两者是同一把密钥的两种写法（`to_scalar_bytes` 的文档写明它可作为 X25519
    // 私钥使用）。
    let recipient_public = recipient.verifying_key().to_montgomery().to_bytes();
    let shared = x25519(recipient.to_scalar_bytes(), ephemeral_key);
    if shared == [0u8; 32] {
        return Err(seal_error("shared secret is zero".to_string()));
    }
    let mut key = Blake2b::<U32>::new();
    key.update(LABEL);
    key.update(ephemeral);
    key.update(recipient_public);
    key.update(shared);
    let cipher = XChaCha20Poly1305::new((&key.finalize()[..]).into());
    let mut nonce = Blake2b::<U24>::new();
    nonce.update(ephemeral);
    nonce.update(recipient_public);
    let nonce = nonce.finalize();
    cipher
        .decrypt((&nonce[..]).into(), ciphertext)
        .map_err(|_| seal_error("ciphertext does not authenticate".to_string()))
}

/// 把技术原因包成 [`Error::Sealed`]：外层文案在语言清单里，原因只进日志。
fn seal_error(reason: String) -> Error {
    Error::Sealed(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    /// 控制面用同一个种子封出来的一封信封（`server/app/seal_test.go` 里也有这一
    /// 对常量）：明文是 `0123456789abcdef` 反复两遍，收件人是种子 `00..1f` 的
    /// Ed25519 公钥。任一侧改了格式，这条用例失败。
    const SEALED: &str = "ZDA2HMrT/31vujoZvinzIrjif+cWRPdzTK3sTaRJKiHKeXaBiQwzIQ4jdKtUBpSODCvi6rC3reGWyCeygVPeZ/7X5GzdRqvt6LMwDEQ7jBg=";

    fn seed_key() -> SigningKey {
        let mut seed = [0u8; 32];
        for (index, byte) in seed.iter_mut().enumerate() {
            *byte = index as u8;
        }
        SigningKey::from_bytes(&seed)
    }

    #[test]
    fn the_control_plane_envelope_opens() {
        let plain = open(&seed_key(), SEALED).expect("envelope must open");
        assert_eq!(
            String::from_utf8(plain).unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn another_device_cannot_open_it() {
        let mut seed = [7u8; 32];
        seed[0] = 9;
        let other = SigningKey::from_bytes(&seed);
        assert!(
            open(&other, SEALED).is_err(),
            "另一个设备的私钥解开了这封信封"
        );
    }

    #[test]
    fn a_tampered_envelope_is_refused() {
        let raw = crate::BASE64.decode(SEALED).unwrap();
        let mut broken = raw.clone();
        let last = broken.len() - 1;
        broken[last] ^= 1;
        let broken = crate::BASE64.encode(&broken);
        assert!(open(&seed_key(), &broken).is_err(), "改过的密文被接受了");

        // 临时公钥被换掉同样要失败：密钥与 nonce 都由它派生。
        let mut stray = raw;
        stray[0] ^= 1;
        let stray = crate::BASE64.encode(&stray);
        assert!(
            open(&seed_key(), &stray).is_err(),
            "换掉临时公钥的信封被接受了"
        );
    }

    #[test]
    fn a_short_or_unencoded_envelope_is_refused() {
        assert!(open(&seed_key(), "not base64").is_err());
        assert!(open(&seed_key(), "").is_err());
        assert!(open(&seed_key(), &crate::BASE64.encode([0u8; 8])).is_err());
    }
}
