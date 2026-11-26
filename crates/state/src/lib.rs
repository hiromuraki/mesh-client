//! `config/` 下的身份与配置。
//!
//! 这里管五个文件（外加一个只读的 `config.toml` 提示解析）：
//! `client.json`、`client.key`、`secure.key`、`credential.json`、`config.toml`。
//! 对应 Go 的 `internal/state` 与 `engine/toml.go`。
//!
//! 磁盘格式是 snake_case、线上是 camelCase，两套独立格式（见 `contract/README.md`）。
//! 所以这里的 serde 结构体不写 `rename_all`：字段名就是磁盘上的名字。

pub mod hints;

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
    #[error("{} 读写失败: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} 内容不合格式: {reason}", path.display())]
    Parse { path: PathBuf, reason: String },
    #[error("{} 长度异常: {len}（应为 {expected}）", path.display())]
    KeyLength {
        path: PathBuf,
        len: usize,
        expected: usize,
    },
    #[error("状态目录不能为空")]
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
            Ok(bytes) => {
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
                atomic_replace(&path, &seed, 0o600).map_err(|err| Error::io(&path, err))?;
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
        let key = match fs::read_to_string(&path) {
            Ok(text) => {
                let key = text.trim().to_string();
                if key.is_empty() {
                    return Err(Error::parse(&path, "文件为空"));
                }
                key
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut secret = [0u8; SECURE_KEY_LEN];
                rand::rngs::OsRng.fill_bytes(&mut secret);
                let key = BASE64.encode(secret);
                atomic_replace(&path, key.as_bytes(), 0o600)
                    .map_err(|err| Error::io(&path, err))?;
                key
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
        let credential: Credential =
            serde_json::from_str(&text).map_err(|err| Error::parse(&path, err.to_string()))?;
        if credential.secret.is_empty() {
            return Ok(None);
        }
        Ok(Some(credential))
    }

    /// 原子写凭据（0600）。
    pub fn save_credential(&self, credential: &Credential) -> Result<(), Error> {
        let path = self.credential_path();
        if credential.secret.is_empty() {
            return Err(Error::parse(&path, "凭据私钥为空"));
        }
        let mut text = serde_json::to_string_pretty(credential)
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
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|err| Error::parse(&path, format!("不是 UTF-8: {err}")))
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
        assert_eq!(fs::read(store.key_path()).unwrap().len(), SEED_LEN);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_wrong_length_key_fails_instead_of_being_replaced() {
        let root = temp_root("key-bad");
        let store = Store::open(&root).unwrap();
        fs::write(store.key_path(), b"too short").unwrap();
        assert!(matches!(
            store.signing_key(),
            Err(Error::KeyLength { len: 9, .. })
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
            })
            .is_err());
        let credential = Credential {
            id: "9b2f".into(),
            secret: "c2VjcmV0".into(),
            expires_at: 1_798_000_000,
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
}
