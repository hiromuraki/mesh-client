//! 安装工件的完整性：本地字节要证明它就是登记的那一份。
//!
//! 下载客户端包与替换启动器走同一套判据：完整的 sha256 只回答「是不是同一份」，
//! 发布签名回答「是不是发布方给的」。这个模块只做前一半——形状与摘要——因为它是
//! 两个二进制都要用的那一半。

use std::io::{self, Read};
use std::path::Path;

/// 完整摘要的形状：64 位十六进制。大小写不敏感的比较留给调用方，形状是唯一的。
pub fn is_full_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 文件的完整 sha256（小写十六进制）。读不出来时把 IO 错误原样交回：调用方要能
/// 说清是哪个文件、为什么读不动。
pub fn digest(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 同 [`digest`]，读不出来按「拿不到摘要」处理：只关心「是不是同一份」的调用方
/// 用它，能给出原因的那条路径用 [`digest`]。
pub fn digest_file(path: &Path) -> Option<String> {
    digest(path).ok()
}

/// 文件是否就是 `want` 那一份。摘要缺失、文件读不出来都算不匹配。
pub fn matches(path: &Path, want: &str) -> bool {
    is_full_digest(want)
        && digest_file(path)
            .map(|sum| sum.eq_ignore_ascii_case(want))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;

    #[test]
    fn only_64_hex_digits_are_full_digests() {
        assert!(is_full_digest(&"ab".repeat(32)));
        assert!(!is_full_digest(""));
        assert!(!is_full_digest(&"ab".repeat(31)));
        assert!(!is_full_digest(&"gg".repeat(32)));
    }

    #[test]
    fn a_file_matches_its_own_digest_only() {
        let dir = temp_dir("artifact");
        let path = dir.join("payload");
        std::fs::write(&path, b"package-bytes").unwrap();
        let sum = digest_file(&path).unwrap();
        assert_eq!(sum.len(), 64);
        assert!(matches(&path, &sum));
        assert!(matches(&path, &sum.to_uppercase()), "摘要比较不分大小写");
        assert!(!matches(&path, &"0".repeat(64)));
        assert!(!matches(&dir.join("missing"), &sum));
        // 读不动的文件：能给出原因的路径拿得到 IO 错误，摘要为空的路径拿到 None。
        assert!(digest(&dir.join("missing")).is_err());
        assert_eq!(digest_file(&dir.join("missing")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
