//! `config/` 下的身份与配置。
//!
//! 这里管五个文件（外加一个只读的 `config.toml` 提示解析）：
//! `client.json`、`client.key`、`secure.key`、`credential.json`、`config.toml`。
//! 对应 Go 的 `internal/state` 与 `engine/toml.go`。
//!
//! 磁盘格式是 snake_case、线上是 camelCase，两套独立格式（见 `contract/README.md`）。
//! 所以这里的 serde 结构体不写 `rename_all`：字段名就是磁盘上的名字。

pub mod hints;

#[cfg(windows)]
mod win;

/// 收紧 `config/` 的 DACL：只留当前用户、SYSTEM、Administrators。
///
/// Unix 上不需要：那里的权限就是 0600，`atomic_replace` 已经按它建文件。
#[cfg(windows)]
fn harden_dir(dir: &Path) -> io::Result<()> {
    win::harden_dir(dir)
}

#[cfg(not(windows))]
fn harden_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

use ed25519_dalek::SigningKey;
use mesh_install::atomic_replace;
use mesh_install::layout;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Ed25519 seed 的长度。`client.key` 只有这一种合法长度。
const SEED_LEN: usize = 32;
/// `secure.key` 里那 32 字节 X25519 私钥的长度。
const SECURE_KEY_LEN: usize = 32;

/// 本 crate 的错误。
///
/// 只分三类，因为上层能做的判断只有这三类：文件读写失败、内容不合格式、
/// 密钥长度不对。最后一条必须独立：长度异常时绝不重建密钥（重建等于换掉设备
/// 身份，用户会被迫重新注册），要人工看一眼那个文件。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{}",
        mesh_text::format(mesh_text::id::STATE_ERROR_IO, &[&path.display(), &source])
    )]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "{}",
        mesh_text::format(mesh_text::id::STATE_ERROR_PARSE, &[&path.display(), &reason])
    )]
    Parse { path: PathBuf, reason: String },
    #[error(
        "{}",
        mesh_text::format(
            mesh_text::id::STATE_ERROR_KEY_LENGTH,
            &[&path.display(), &len, &expected]
        )
    )]
    KeyLength {
        path: PathBuf,
        len: usize,
        expected: usize,
    },
    #[error("{}", mesh_text::get(mesh_text::id::STATE_ERROR_EMPTY_ROOT))]
    EmptyRoot,
}

impl Error {
    fn io(path: &Path, source: io::Error) -> Self {
        Error::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    fn parse(path: &Path, reason: impl Into<String>) -> Self {
        Error::Parse {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }
}

/// `config/client.json`。不记录 `config_hash`：配置的摘要每次都从磁盘上的
/// `config.toml` 现算，这样文件被外部改坏时也能被服务端发现并纠正。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub server_url: String,
    pub device_id: String,
    pub hostname: String,
    pub created_at: i64,
    /// 用户显式指定的 core 路径；空串表示按自动查找顺序找。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub core_path: String,
}

/// `config/credential.json`：控制面签发的凭据，取代 `secure.key` 成为这个节点
/// 在 mesh 里的身份。私钥只在这个文件里，不上报到任何地方。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Credential {
    pub id: String,
    pub secret: String,
    /// 私钥的保护方式。Windows 上写 `dpapi`，表示 `secret` 是 DPAPI 密文的 base64；
    /// Unix 上不写（靠 0600）。跨平台搬 config 目录时靠它报错，而不是解出一把错密钥。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protect: String,
    /// 控制面给的过期时刻（unix 秒）。只在签发的那一次心跳响应里出现，
    /// 记下来只为面板显示；0 表示没有记录（早期版本写下的文件）。
    #[serde(skip_serializing_if = "is_zero")]
    pub expires_at: i64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// `config/` 的入口。
pub struct Store {
    root: PathBuf,
    keys: Mutex<Keys>,
}

#[derive(Default)]
struct Keys {
    signing: Option<SigningKey>,
    secure: Option<String>,
}

impl Store {
    /// 打开安装根：`config/` 放在根下、跨版本共享，打开时确保它存在。
    pub fn open(root: &Path) -> Result<Store, Error> {
        if root.as_os_str().is_empty() {
            return Err(Error::EmptyRoot);
        }
        let dir = root.join(layout::CONFIG);
        fs::create_dir_all(&dir).map_err(|err| Error::io(&dir, err))?;
        // 目录一建好就收权限，赶在写任何密钥之前：继承来的 ACL 可能对本地所有账户
        // 开放，晚一步就等于先把私钥暴露过一次。
        harden_dir(&dir).map_err(|err| Error::io(&dir, err))?;
        Ok(Store {
            root: root.to_path_buf(),
            keys: Mutex::new(Keys::default()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `config/` 目录。
    pub fn dir(&self) -> PathBuf {
        self.root.join(layout::CONFIG)
    }

    /// `log/` 目录（日志由引擎写，这里只给出与 `config/` 并列的位置）。
    pub fn log_dir(&self) -> PathBuf {
        self.root.join(layout::LOG)
    }

    pub fn client_path(&self) -> PathBuf {
        self.dir().join("client.json")
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir().join("client.key")
    }

    pub fn secure_key_path(&self) -> PathBuf {
        self.dir().join("secure.key")
    }

    pub fn credential_path(&self) -> PathBuf {
        self.dir().join("credential.json")
    }

    /// 当前生效的 EasyTier 配置。
    pub fn config_path(&self) -> PathBuf {
        self.dir().join("config.toml")
    }

    /// 读 `client.json`；文件不存在返回 `None`（首次启动的样子）。
    pub fn load_client(&self) -> Result<Option<ClientConfig>, Error> {
        let path = self.client_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::io(&path, err)),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| Error::parse(&path, err.to_string()))
    }

    /// 原子写 `client.json`（0600，含身份私钥的目录）。
    pub fn save_client(&self, config: &ClientConfig) -> Result<(), Error> {
        let path = self.client_path();
        let mut text = serde_json::to_string_pretty(config)
            .map_err(|err| Error::parse(&path, err.to_string()))?;
        text.push('\n');
        atomic_replace(&path, text.as_bytes(), 0o600).map_err(|err| Error::io(&path, err))
    }

    /// 清空 `device_id`（服务端吊销后回设置页），其余字段原样保留。
    pub fn clear_device(&self) -> Result<(), Error> {
        let Some(mut config) = self.load_client()? else {
            return Ok(());
        };
        config.device_id.clear();
        self.save_client(&config)
    }

    /// 读或生成 Ed25519 私钥（32 字节 seed，权限 0600）。
    ///
    /// 生成必须与读取互斥：两个线程同时走到生成分支会各写一份，内存里的私钥与
    /// 磁盘上的就不是同一把，之后所有签名请求 401、用户被迫重新注册。
    pub fn signing_key(&self) -> Result<SigningKey, Error> {
        let path = self.key_path();
        let mut keys = self.lock_keys();
        if let Some(key) = keys.signing.as_ref() {
            return Ok(key.clone());
        }
        let key = match fs::read(&path) {
            Ok(stored) => {
                let bytes = open_key(&path, &stored)?;
                if bytes.len() != SEED_LEN {
                    return Err(Error::KeyLength {
                        path,
                        len: bytes.len(),
                        expected: SEED_LEN,
                    });
                }
                let mut seed = [0u8; SEED_LEN];
                seed.copy_from_slice(&bytes);
                SigningKey::from_bytes(&seed)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut seed = [0u8; SEED_LEN];
                rand::rngs::OsRng.fill_bytes(&mut seed);
                let sealed = seal_key(&path, &seed)?;
                atomic_replace(&path, &sealed, 0o600).map_err(|err| Error::io(&path, err))?;
                SigningKey::from_bytes(&seed)
            }
            Err(err) => return Err(Error::io(&path, err)),
        };
        keys.signing = Some(key.clone());
        Ok(key)
    }

    /// 读或生成安全模式节点的私钥（base64 的 32 字节 X25519 私钥）。
    ///
    /// core 在 `local_private_key` 为空时每次启动都随机生成一把，对端看到的节点
    /// 身份于是每重启一次就换一个；由客户端固定下来之后公钥始终不变。
    pub fn secure_key(&self) -> Result<String, Error> {
        let path = self.secure_key_path();
        let mut keys = self.lock_keys();
        if let Some(key) = keys.secure.as_ref() {
            return Ok(key.clone());
        }
        let key = match fs::read(&path) {
            Ok(stored) => {
                let bytes = open_key(&path, &stored)?;
                if bytes.len() != SECURE_KEY_LEN {
                    return Err(Error::KeyLength {
                        path,
                        len: bytes.len(),
                        expected: SECURE_KEY_LEN,
                    });
                }
                BASE64.encode(bytes)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut secret = [0u8; SECURE_KEY_LEN];
                rand::rngs::OsRng.fill_bytes(&mut secret);
                let sealed = seal_key(&path, &secret)?;
                atomic_replace(&path, &sealed, 0o600).map_err(|err| Error::io(&path, err))?;
                BASE64.encode(secret)
            }
            Err(err) => return Err(Error::io(&path, err)),
        };
        keys.secure = Some(key.clone());
        Ok(key)
    }

    /// 读 `credential.json`；文件不存在、或里面没有私钥，都按“没有凭据”处理。
    pub fn load_credential(&self) -> Result<Option<Credential>, Error> {
        let path = self.credential_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::io(&path, err)),
        };
        let mut credential: Credential =
            serde_json::from_str(&text).map_err(|err| Error::parse(&path, err.to_string()))?;
        if credential.secret.is_empty() {
            return Ok(None);
        }
        credential.secret = open_secret(&path, &credential.secret, &credential.protect)?;
        credential.protect.clear();
        Ok(Some(credential))
    }

    /// 原子写凭据（0600）。
    pub fn save_credential(&self, credential: &Credential) -> Result<(), Error> {
        let path = self.credential_path();
        if credential.secret.is_empty() {
            return Err(Error::parse(
                &path,
                mesh_text::get(mesh_text::id::STATE_ERROR_EMPTY_CREDENTIAL_SECRET),
            ));
        }
        let (secret, protect) = seal_secret(&path, &credential.secret)?;
        let stored = Credential {
            id: credential.id.clone(),
            secret,
            expires_at: credential.expires_at,
            protect,
        };
        let mut text = serde_json::to_string_pretty(&stored)
            .map_err(|err| Error::parse(&path, err.to_string()))?;
        text.push('\n');
        atomic_replace(&path, text.as_bytes(), 0o600).map_err(|err| Error::io(&path, err))
    }

    /// 删除本地凭据（「删除本地配置」）。
    pub fn remove_credential(&self) -> Result<(), Error> {
        let path = self.credential_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::io(&path, err)),
        }
    }

    /// 删除下发的 `config.toml`（「删除本地配置」）。
    pub fn remove_config(&self) -> Result<(), Error> {
        let path = self.config_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::io(&path, err)),
        }
    }

    /// 读磁盘上的 `config.toml`；不存在返回 `None`。
    pub fn read_config(&self) -> Result<Option<String>, Error> {
        let path = self.config_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::io(&path, err)),
        };
        // 下发的配置一定是 UTF-8；不是就别猜（猜测会把摘要也算错）。
        String::from_utf8(bytes).map(Some).map_err(|err| {
            Error::parse(
                &path,
                mesh_text::format(mesh_text::id::STATE_ERROR_NOT_UTF8, &[&err]),
            )
        })
    }

    /// 原子写 `config.toml`：先写临时文件再改名，core 永远不会读到半份配置。
    pub fn write_config(&self, toml: &str) -> Result<(), Error> {
        let path = self.config_path();
        atomic_replace(&path, toml.as_bytes(), 0o600).map_err(|err| Error::io(&path, err))
    }

    fn lock_keys(&self) -> std::sync::MutexGuard<'_, Keys> {
        // 持锁期间只做文件读写，不回调任何外部代码：中毒只会发生在同线程 panic 时，
        // 那种情况下继续用这把锁里的缓存与“重新读一遍磁盘”没有区别。
        self.keys.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// 密钥文件的封装头。Windows 上是 DPAPI 密文的标记；Unix 上不写（靠 0600）。
const SEALED_MAGIC: &[u8; 8] = b"MESHKEY1";

/// 私钥落盘前的封装。Windows 上包一层 DPAPI：文件被拷到别的机器或别的账户名下
/// 就解不开；Unix 上原样落盘，权限由 0600 保证。
#[cfg(windows)]
fn seal_key(path: &Path, raw: &[u8]) -> Result<Vec<u8>, Error> {
    let blob = win::protect(raw).map_err(|err| Error::io(path, err))?;
    let mut out = Vec::with_capacity(SEALED_MAGIC.len() + blob.len());
    out.extend_from_slice(SEALED_MAGIC);
    out.extend_from_slice(&blob);
    Ok(out)
}

#[cfg(not(windows))]
fn seal_key(_path: &Path, raw: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(raw.to_vec())
}

/// seal_key 的逆操作。没有封装头就报错，不去猜「这大概是一把裸密钥」——
/// 旧格式只会出现在手抄或跨平台搬运的目录里，猜错的后果是换掉设备身份。
#[cfg(windows)]
fn open_key(path: &Path, stored: &[u8]) -> Result<Vec<u8>, Error> {
    let Some(blob) = stored.strip_prefix(SEALED_MAGIC.as_slice()) else {
        return Err(Error::parse(
            path,
            mesh_text::get(mesh_text::id::STATE_ERROR_KEY_NOT_SEALED),
        ));
    };
    win::unprotect(blob).map_err(|err| Error::io(path, err))
}

#[cfg(not(windows))]
fn open_key(_path: &Path, stored: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(stored.to_vec())
}
/// 凭据私钥落盘前的编码。返回 (写进 secret 的值, protect 标记)。
///
/// 入参出参都是 base64 的 32 字节私钥（线上格式），这里只负责「落盘形态」。
#[cfg(windows)]
fn seal_secret(path: &Path, secret: &str) -> Result<(String, String), Error> {
    let raw = BASE64.decode(secret).map_err(|err| {
        Error::parse(
            path,
            mesh_text::format(mesh_text::id::STATE_ERROR_CREDENTIAL_NOT_BASE64, &[&err]),
        )
    })?;
    let blob = win::protect(&raw).map_err(|err| Error::io(path, err))?;
    Ok((BASE64.encode(blob), "dpapi".to_string()))
}

#[cfg(not(windows))]
fn seal_secret(_path: &Path, secret: &str) -> Result<(String, String), Error> {
    Ok((secret.to_string(), String::new()))
}

/// seal_secret 的逆操作：把磁盘上的形态解回 base64 私钥。
#[cfg(windows)]
fn open_secret(path: &Path, stored: &str, protect: &str) -> Result<String, Error> {
    if protect != "dpapi" {
        return Err(Error::parse(
            path,
            mesh_text::get(mesh_text::id::STATE_ERROR_CREDENTIAL_PLAINTEXT),
        ));
    }
    let blob = BASE64.decode(stored).map_err(|err| {
        Error::parse(
            path,
            mesh_text::format(
                mesh_text::id::STATE_ERROR_CREDENTIAL_CIPHER_NOT_BASE64,
                &[&err],
            ),
        )
    })?;
    let raw = win::unprotect(&blob).map_err(|err| Error::io(path, err))?;
    Ok(BASE64.encode(raw))
}

#[cfg(not(windows))]
fn open_secret(path: &Path, stored: &str, protect: &str) -> Result<String, Error> {
    if !protect.is_empty() {
        return Err(Error::parse(
            path,
            mesh_text::get(mesh_text::id::STATE_ERROR_CREDENTIAL_DPAPI_UNREADABLE),
        ));
    }
    Ok(stored.to_string())
}

/// 注册用的 base64 公钥。
pub fn public_key_base64(key: &SigningKey) -> String {
    BASE64.encode(key.verifying_key().as_bytes())
}

use base64::Engine as _;
const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn temp_root(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mesh-state-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_creates_the_config_dir() {
        let root = temp_root("open");
        Store::open(&root).unwrap();
        assert!(root.join(layout::CONFIG).is_dir());
        assert!(matches!(Store::open(Path::new("")), Err(Error::EmptyRoot)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn client_json_round_trips_as_snake_case() {
        let root = temp_root("client");
        let store = Store::open(&root).unwrap();
        assert!(store.load_client().unwrap().is_none());
        let config = ClientConfig {
            server_url: "http://10.126.126.1:8088".into(),
            device_id: "0123456789abcdef0123456789abcdef".into(),
            hostname: "laptop".into(),
            created_at: 1795541874,
            core_path: String::new(),
        };
        store.save_client(&config).unwrap();
        assert_eq!(store.load_client().unwrap(), Some(config.clone()));
        let text = fs::read_to_string(store.client_path()).unwrap();
        // 磁盘格式是 snake_case，且 core_path 为空时整个键不出现（Go 的 omitempty）。
        assert!(text.contains("\"server_url\""));
        assert!(text.contains("\"device_id\""));
        assert!(text.contains("\"created_at\""));
        assert!(!text.contains("core_path"));
        store.clear_device().unwrap();
        let cleared = store.load_client().unwrap().unwrap();
        assert_eq!(cleared.device_id, "");
        assert_eq!(cleared.server_url, config.server_url);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn signing_key_is_generated_once_and_persisted() {
        let root = temp_root("key");
        let store = Arc::new(Store::open(&root).unwrap());
        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                store.signing_key().unwrap().to_bytes()
            }));
        }
        let seeds: Vec<[u8; 32]> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for seed in &seeds[1..] {
            assert_eq!(seed, &seeds[0], "并发调用必须拿到同一把私钥");
        }
        let reopened = Store::open(&root).unwrap();
        assert_eq!(reopened.signing_key().unwrap().to_bytes(), seeds[0]);
        // Unix 上就是 32 字节裸 seed；Windows 上多一层 DPAPI 封装。
        let stored = fs::read(store.key_path()).unwrap().len();
        #[cfg(not(windows))]
        assert_eq!(stored, SEED_LEN);
        #[cfg(windows)]
        assert!(stored > SEED_LEN, "封装后的文件应当比 seed 长");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_wrong_length_key_fails_instead_of_being_replaced() {
        let root = temp_root("key-bad");
        let store = Store::open(&root).unwrap();
        fs::write(store.key_path(), b"too short").unwrap();
        // 裸字节在 Windows 上先被判成「没封装」，长度检查因此轮不到；两种都算拒绝。
        assert!(matches!(
            store.signing_key(),
            Err(Error::KeyLength { len: 9, .. }) | Err(Error::Parse { .. })
        ));
        assert_eq!(fs::read(store.key_path()).unwrap(), b"too short");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn secure_key_is_base64_of_32_bytes() {
        let root = temp_root("secure");
        let store = Store::open(&root).unwrap();
        let key = store.secure_key().unwrap();
        let raw = BASE64.decode(&key).unwrap();
        assert_eq!(raw.len(), SECURE_KEY_LEN);
        assert_eq!(Store::open(&root).unwrap().secure_key().unwrap(), key);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn credential_round_trips_and_needs_a_secret() {
        let root = temp_root("credential");
        let store = Store::open(&root).unwrap();
        assert!(store.load_credential().unwrap().is_none());
        assert!(store
            .save_credential(&Credential {
                id: "id".into(),
                secret: String::new(),
                expires_at: 0,
                protect: String::new(),
            })
            .is_err());
        let credential = Credential {
            id: "9b2f".into(),
            secret: "c2VjcmV0".into(),
            expires_at: 1_798_000_000,
            protect: String::new(),
        };
        store.save_credential(&credential).unwrap();
        assert_eq!(store.load_credential().unwrap(), Some(credential));
        store.remove_credential().unwrap();
        assert!(store.load_credential().unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn config_toml_is_written_atomically_and_read_back() {
        let root = temp_root("config");
        let store = Store::open(&root).unwrap();
        assert!(store.read_config().unwrap().is_none());
        store.write_config("hostname = \"laptop\"\n").unwrap();
        assert_eq!(
            store.read_config().unwrap().as_deref(),
            Some("hostname = \"laptop\"\n")
        );
        let _ = fs::remove_dir_all(&root);
    }
    /// 收紧之后，身份目录只留当前用户、SYSTEM、Administrators 三条 ACE，
    /// 而且不再继承安装根所在卷的默认 ACL。
    #[cfg(windows)]
    #[test]
    fn hardening_keeps_only_three_principals() {
        let dir = temp_root("acl");
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        // 先放一个文件：它继承的是临时目录的 ACL，收紧目录后必须一并被改写。
        let key = config.join("client.key");
        std::fs::write(&key, b"seed").unwrap();
        win::harden_dir(&config).unwrap();
        let output = std::process::Command::new("icacls")
            .arg(&config)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("SYSTEM"), "{text}");
        assert!(text.contains("Administrators"), "{text}");
        assert!(
            !text.contains("BUILTIN\\Users"),
            "继承来的 Users 权限还在: {text}"
        );
        assert!(
            !text.contains("Authenticated Users"),
            "继承来的 Authenticated Users 权限还在: {text}"
        );
        // 目录里已有的密钥文件也要跟着收紧，否则收紧目录只是收了个空壳。
        let file_acl = std::process::Command::new("icacls")
            .arg(&key)
            .output()
            .unwrap();
        let file_text = String::from_utf8_lossy(&file_acl.stdout);
        assert!(
            !file_text.contains("Authenticated Users"),
            "密钥文件仍带着继承权限: {file_text}"
        );
        assert!(file_text.contains("SYSTEM"), "{file_text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 凭据落盘是 DPAPI 密文：文件里看不到私钥，读回来还是原来那把。
    #[cfg(windows)]
    #[test]
    fn a_credential_is_encrypted_at_rest() {
        let dir = temp_root("dpapi");
        let store = Store::open(&dir).unwrap();
        let secret = BASE64.encode([7u8; 32]);
        let credential = Credential {
            id: "cred-1".to_string(),
            secret: secret.clone(),
            expires_at: 42,
            protect: String::new(),
        };
        store.save_credential(&credential).unwrap();

        let text = std::fs::read_to_string(store.credential_path()).unwrap();
        assert!(text.contains("\"dpapi\""), "{text}");
        assert!(!text.contains(&secret), "私钥明文落盘了: {text}");

        let loaded = store.load_credential().unwrap().unwrap();
        assert_eq!(loaded.secret, secret);
        assert!(loaded.protect.is_empty(), "内存里不该留下保护标记");
        assert_eq!(loaded.id, "cred-1");
        assert_eq!(loaded.expires_at, 42);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 两把密钥落盘都是 DPAPI 密文：文件里看不到私钥明文，读回来还是同一把。
    #[cfg(windows)]
    #[test]
    fn both_keys_are_encrypted_at_rest() {
        let root = temp_root("sealed-keys");
        let store = Store::open(&root).unwrap();
        let signing = store.signing_key().unwrap();
        let secure = store.secure_key().unwrap();

        for path in [store.key_path(), store.secure_key_path()] {
            let bytes = fs::read(&path).unwrap();
            assert!(
                bytes.starts_with(b"MESHKEY1"),
                "{} 缺少封装头",
                path.display()
            );
        }
        let raw_seed = fs::read(store.key_path()).unwrap();
        assert!(
            !raw_seed[8..].windows(32).any(|w| w == signing.to_bytes()),
            "client.key 里出现了明文 seed"
        );

        // 重新打开之后还是同一把：封装是幂等的。
        let reopened = Store::open(&root).unwrap();
        assert_eq!(
            reopened.signing_key().unwrap().to_bytes(),
            signing.to_bytes()
        );
        assert_eq!(reopened.secure_key().unwrap(), secure);
        let _ = fs::remove_dir_all(&root);
    }
}
