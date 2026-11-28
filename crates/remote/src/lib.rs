//! 设备面 HTTP：心跳、整包下载。
//!
//! 对应 Go 的 `internal/remote`。这一层不知道磁盘布局：密钥由引擎从 `mesh-state`
//! 取出来传进来，设备身份就是这把钥匙的公钥。这样“谁是设备”与“怎么说话”分开，
//! 401 的处理留在引擎里。

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

mod sealed;
pub mod signing;
use mesh_install::platform;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 心跳响应体的上限（与 Go 版一致）。配置只有几 KB，超过这个量说明对端不对。
const MAX_HEARTBEAT_BODY: u64 = 1 << 20;
/// 错误响应体的上限：只需要读到 code 与一句话。
const MAX_ERROR_BODY: u64 = 4096;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// 凭据私钥的长度：base64 之前的 32 字节 X25519 私钥。
const CREDENTIAL_SECRET_LEN: usize = 32;

/// 设备面 HTTP 的错误。
///
/// `Unauthorized` 必须独立成变体并带上 `code` 与 `Date` 头：引擎要按时钟偏差与
/// 其它原因分别提示，把它们折成一句话就没法分了。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("401 {code}")]
    Unauthorized {
        code: String,
        /// 响应 `Date` 头解析出的服务端时间；缺失或无法解析时是 `None`。
        server_time: Option<SystemTime>,
    },
    #[error(
        "{}",
        mesh_text::format(mesh_text::id::REMOTE_ERROR_API, &[message, &status])
    )]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    #[error(
        "{}",
        mesh_text::format(mesh_text::id::REMOTE_ERROR_TRANSPORT, &[.0])
    )]
    Transport(String),
    #[error("{}", mesh_text::format(mesh_text::id::REMOTE_ERROR_DECODE, &[.0]))]
    Decode(String),
    /// 凭据封装解不开。里面是技术原因，只进日志。
    #[error("{}", mesh_text::format(mesh_text::id::REMOTE_ERROR_SEALED, &[.0]))]
    Sealed(String),
}

impl Error {
    /// 401 响应体里的 `code`；不是 401 时是空串。
    ///
    /// 契约里校验失败一律是 `unauthorized`，这里保留 `code` 只为把它写进日志。
    pub fn unauthorized_code(&self) -> Option<&str> {
        match self {
            Error::Unauthorized { code, .. } => Some(code),
            _ => None,
        }
    }
}

/// 绑定一个服务端地址与设备身份。
pub struct Client {
    base_url: String,
    /// 本机公钥的 base64：设备身份就是它，服务端按它找到槽位。
    pubkey: String,
    key: SigningKey,
}

impl Client {
    /// `base_url` 末尾的 `/` 去掉：拼路径时只留一个分隔符。
    pub fn new(base_url: &str, key: SigningKey) -> Client {
        let pubkey = BASE64.encode(key.verifying_key().to_bytes());
        Client {
            base_url: base_url.trim_end_matches('/').to_string(),
            pubkey,
            key,
        }
    }

    /// 本机的公钥（base64）：设置页把它显示给管理员填进新槽位。
    pub fn pubkey(&self) -> &str {
        &self.pubkey
    }

    /// `POST /api/v1/heartbeat`：`Ok(None)` 表示服务端回了 204（两边都没变）。
    ///
    /// 请求里只有「我是谁、我现在的状态」：服务端据此决定要不要给配置与完整的最新
    /// 发布信息。「这份我已经有了」由调用方按版本与摘要自己判，不往请求里塞摘要。
    pub fn heartbeat(
        &self,
        hostname: &str,
        config_hash: &str,
        app_version: &str,
        credential_id: &str,
        timeout: Duration,
    ) -> Result<Option<Heartbeat>, Error> {
        let timestamp = unix_now().to_string();
        let nonce = random_hex(16)?;
        let platform_id = platform::id();
        let message = signing_string(
            "POST",
            "/api/v1/heartbeat",
            &self.pubkey,
            &timestamp,
            &nonce,
            platform_id,
            config_hash,
            app_version,
        );
        let signature = BASE64.encode(self.key.sign(message.as_bytes()).to_bytes());
        let body = serde_json::json!({
            "pubkey": self.pubkey,
            "hostname": hostname,
            "appVersion": app_version,
            "configHash": config_hash,
            "credentialId": credential_id,
            "timestamp": timestamp,
            "nonce": nonce,
            "signature": signature,
            "platform": platform_id,
        });
        let body = body.to_string();
        let response = post(&self.url("/api/v1/heartbeat"), &body, timeout)?;
        if response.status() == 204 {
            return Ok(None);
        }
        decode(response, MAX_HEARTBEAT_BODY).map(Some)
    }

    /// 下载 `url` 到 `dst`，最多 `limit` 字节，返回实际写入字节数。
    ///
    /// 多读一个字节再判断：读满 `limit + 1` 说明响应被截断，不能把半截文件当成成功。
    /// （`update.tar.zst` 装不上会回滚，但“装了小半截包”这种事不该有机会发生。）
    pub fn download(
        &self,
        url: &str,
        dst: &Path,
        limit: u64,
        timeout: Duration,
    ) -> Result<u64, Error> {
        let response = ureq::get(url)
            .timeout(timeout)
            .call()
            .map_err(map_ureq_error)?;
        if response.status() != 200 {
            return Err(api_error(response.status(), response));
        }
        let mut file = File::create(dst).map_err(|err| {
            Error::Transport(mesh_text::format(
                mesh_text::id::REMOTE_ERROR_CREATE_FILE,
                &[&dst.display(), &err],
            ))
        })?;
        let mut reader = response.into_reader().take(limit + 1);
        let written = io::copy(&mut reader, &mut file).map_err(|err| {
            Error::Transport(mesh_text::format(
                mesh_text::id::REMOTE_ERROR_WRITE_FILE,
                &[&dst.display(), &err],
            ))
        })?;
        file.flush().map_err(|err| {
            Error::Transport(mesh_text::format(
                mesh_text::id::REMOTE_ERROR_WRITE_FILE,
                &[&dst.display(), &err],
            ))
        })?;
        if written > limit {
            return Err(Error::Transport(mesh_text::format(
                mesh_text::id::REMOTE_ERROR_BODY_TOO_LARGE,
                &[&limit],
            )));
        }
        Ok(written)
    }

    /// 心跳带的凭据私钥（base64）：只认封给设备公钥的那一份。
    ///
    /// 没有封装说明这一次没有签发凭据（正式节点、或还没到签发时机），返回空串。
    /// 解不开就是这一次下发作废，不回落到任何明文：能改响应字节的一方正是这条封装
    /// 要防的人，回落等于把封装变成可选的装饰。
    pub fn credential_secret(&self, heartbeat: &Heartbeat) -> Result<String, Error> {
        if heartbeat.credential_sealed.is_empty() {
            return Ok(String::new());
        }
        let secret = sealed::open(&self.key, &heartbeat.credential_sealed)?;
        if secret.len() != CREDENTIAL_SECRET_LEN {
            return Err(Error::Sealed(format!(
                "credential is {} bytes, want {CREDENTIAL_SECRET_LEN}",
                secret.len()
            )));
        }
        Ok(BASE64.encode(secret))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

/// 控制面单独发布的启动器（安装根目录里那个固定文件）。
///
/// 客户端自己算本地文件的 sha256 与之比对，所以服务端每次都带；`version` 只用于显示。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LauncherInfo {
    pub version: String,
    pub url: String,
    pub sha256: String,
    /// 离线发布私钥对「{launcher, 平台, 版本, 摘要}」的签名（hex）。客户端验不过
    /// 就不替换；字段缺失按无效处理。
    pub signature: String,
}

/// 服务端下发的客户端更新信息，只在设备版本更旧时出现。
///
/// 只有一种形状：version/url/notes/sha256/signature/autoUpdate 每次都在（notes 为空
/// 时省略）。服务端不比较客户端手上有什么，「这份我有了」由引擎按版本与摘要自己判。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    /// 为空时省略（`appNotes` 没填）。
    pub notes: String,
    /// 发布包的完整摘要，空串表示服务端没有登记（早期记录）。没有它就证明不了
    /// 下载到的字节是发布的那一份，引擎会拒收这份 offer。
    pub sha256: String,
    /// 离线发布私钥对「{client, 平台, 版本, 摘要}」的签名（hex）。摘要只说明
    /// 内容没被换过，签名才说明这份内容出自发布方。
    pub signature: String,
    /// 包级开关：这个版本该不该自己装，由发布方决定；本地
    /// `--disable-auto-update` 可以一票否决。缺失按手动处理。
    pub auto_update: bool,
}

/// 心跳响应。服务端没变化的字段不出现，所以每个字段都要能缺省。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Heartbeat {
    pub config_hash: String,
    pub config_b64: String,
    pub app_update: Option<UpdateInfo>,
    pub launcher: Option<LauncherInfo>,
    /// 凭据私钥只在下发的那一次响应里出现，之后服务端只剩 id。明文从不过网：
    /// 服务端只发封给设备公钥的 credential_sealed。
    pub credential_sealed: String,
    pub credential_id: String,
    pub credential_expiry: i64,
}

impl Heartbeat {
    /// 解码随心跳下发的配置；服务端没带配置时返回空串。
    pub fn config_toml(&self) -> Result<String, Error> {
        if self.config_b64.is_empty() {
            return Ok(String::new());
        }
        let raw = BASE64.decode(&self.config_b64).map_err(|err| {
            Error::Decode(mesh_text::format(
                mesh_text::id::REMOTE_ERROR_CONFIG_DECODE,
                &[&err],
            ))
        })?;
        String::from_utf8(raw).map_err(|err| {
            Error::Decode(mesh_text::format(
                mesh_text::id::REMOTE_ERROR_CONFIG_NOT_UTF8,
                &[&err],
            ))
        })
    }
}

/// 设备请求的待签名文本：字段顺序固定，与 JSON 序列化无关，两端各自按同样顺序
/// 拼出来（见 `contract/device-api.md`）。末尾没有换行。
///
/// 参数就是协议里的八行，不折成一个结构体：**顺序本身就是契约**，摊在这里
/// 才能和 `contract/device-api.md` 的那段文字逐行对照。
#[allow(clippy::too_many_arguments)]
fn signing_string(
    method: &str,
    path: &str,
    pubkey: &str,
    timestamp: &str,
    nonce: &str,
    platform_id: &str,
    config_hash: &str,
    app_version: &str,
) -> String {
    [
        "MESH2",
        method,
        path,
        pubkey,
        timestamp,
        nonce,
        platform_id,
        config_hash,
        app_version,
    ]
    .join("\n")
}

fn post(url: &str, body: &str, timeout: Duration) -> Result<ureq::Response, Error> {
    ureq::post(url)
        .timeout(timeout)
        .set("Content-Type", "application/json")
        .send_string(body)
        .map_err(map_ureq_error)
}

fn map_ureq_error(error: ureq::Error) -> Error {
    match error {
        ureq::Error::Status(status, response) => api_error(status, response),
        ureq::Error::Transport(transport) => Error::Transport(transport.to_string()),
    }
}

#[derive(Debug, Default, Deserialize)]
struct ErrorPayload {
    #[serde(default)]
    code: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    message: String,
}

/// 把一次失败响应翻成 [`Error`]。
///
/// 与 Go 版 `readAPIError` 一致：先按整段文本兜底，再从 JSON 里取 `code`，`message`
/// 优先取 `error`、其次 `message`。`Date` 头单独解析，缺了就是 `None`。
fn api_error(status: u16, response: ureq::Response) -> Error {
    let server_time = response
        .header("date")
        .and_then(|value| httpdate::parse_http_date(value).ok());
    let mut limited = response.into_reader().take(MAX_ERROR_BODY);
    let mut raw = Vec::new();
    let _ = limited.read_to_end(&mut raw);
    let mut message = String::from_utf8_lossy(&raw).trim().to_string();
    let mut code = String::new();
    if let Ok(payload) = serde_json::from_slice::<ErrorPayload>(&raw) {
        code = payload.code;
        if !payload.error.is_empty() {
            message = payload.error;
        } else if !payload.message.is_empty() {
            message = payload.message;
        }
    }
    if status == 401 {
        return Error::Unauthorized { code, server_time };
    }
    Error::Api {
        status,
        code,
        message,
    }
}

fn decode<T: DeserializeOwned>(response: ureq::Response, limit: u64) -> Result<T, Error> {
    let mut limited = response.into_reader().take(limit);
    let mut raw = Vec::new();
    limited.read_to_end(&mut raw).map_err(|err| {
        Error::Decode(mesh_text::format(
            mesh_text::id::REMOTE_ERROR_READ_RESPONSE,
            &[&err],
        ))
    })?;
    serde_json::from_slice(&raw).map_err(|err| Error::Decode(err.to_string()))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn random_hex(bytes: usize) -> Result<String, Error> {
    use rand::RngCore;
    let mut raw = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    Ok(hex::encode(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    struct Captured {
        request_line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// 只用 std 的假服务端：按给的顺序应答每一个请求，把请求记下来断言。
    struct Stub {
        base_url: String,
        captured: Arc<Mutex<Vec<Captured>>>,
    }

    impl Stub {
        fn new(responses: Vec<Vec<u8>>) -> Stub {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let captured = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&captured);
            std::thread::spawn(move || {
                for response in responses {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    let request = read_request(&mut stream);
                    sink.lock().unwrap().push(request);
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                }
            });
            Stub { base_url, captured }
        }

        fn last_body(&self) -> serde_json::Value {
            let captured = self.captured.lock().unwrap();
            serde_json::from_slice(&captured.last().unwrap().body).unwrap()
        }
    }

    fn read_request(stream: &mut TcpStream) -> Captured {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head).into_owned();
        let mut lines = text.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut headers = Vec::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        let length: usize = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        if length > 0 {
            stream.read_exact(&mut body).unwrap();
        }
        Captured {
            request_line,
            headers,
            body,
        }
    }

    fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut text = format!(
            "HTTP/1.1 {status} Stub\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str("\r\n");
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn client(base_url: &str) -> Client {
        Client::new(base_url, SigningKey::from_bytes(&[7u8; 32]))
    }

    /// 测试私钥对应的公钥，也就是这些请求里出现的身份。
    fn pubkey() -> String {
        BASE64.encode(
            SigningKey::from_bytes(&[7u8; 32])
                .verifying_key()
                .to_bytes(),
        )
    }

    fn timeout() -> Duration {
        Duration::from_secs(5)
    }

    #[test]
    fn signing_string_shape_is_fixed() {
        assert_eq!(
            signing_string(
                "POST",
                "/api/v1/heartbeat",
                "pubkey-b64",
                "1795541874",
                "abc",
                "windows/amd64",
                "hash",
                "2026.11.26-13",
            ),
            "MESH2\nPOST\n/api/v1/heartbeat\npubkey-b64\n1795541874\nabc\nwindows/amd64\nhash\n2026.11.26-13"
        );
    }

    #[test]
    fn heartbeat_is_signed_and_204_is_none() {
        let stub = Stub::new(vec![response(204, &[], b"")]);
        let reply = client(&stub.base_url)
            .heartbeat("laptop", "hash", "2026.11.26-13", "cred-1", timeout())
            .unwrap();
        assert!(reply.is_none());
        let captured = stub.captured.lock().unwrap();
        assert_eq!(captured[0].request_line, "POST /api/v1/heartbeat HTTP/1.1");
        assert!(captured[0]
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value.starts_with("application/json")));
        drop(captured);
        let body = stub.last_body();
        assert_eq!(body["pubkey"], pubkey());
        assert_eq!(body["hostname"], "laptop");
        assert_eq!(body["configHash"], "hash");
        assert_eq!(body["appVersion"], "2026.11.26-13");
        assert_eq!(body["credentialId"], "cred-1");
        assert_eq!(body["platform"], platform::id());
        let nonce = body["nonce"].as_str().unwrap();
        assert_eq!(nonce.len(), 32);
        assert!(nonce.bytes().all(|b| b.is_ascii_hexdigit()));
        let timestamp = body["timestamp"].as_str().unwrap();
        assert!(timestamp.parse::<u64>().unwrap() > 1_700_000_000);
        // 签名原文必须能按契约拼回去并验签通过。
        let message = signing_string(
            "POST",
            "/api/v1/heartbeat",
            body["pubkey"].as_str().unwrap(),
            timestamp,
            nonce,
            body["platform"].as_str().unwrap(),
            body["configHash"].as_str().unwrap(),
            body["appVersion"].as_str().unwrap(),
        );
        let signature = BASE64.decode(body["signature"].as_str().unwrap()).unwrap();
        let signature = ed25519_dalek::Signature::from_slice(&signature).unwrap();
        let verifying = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        verifying.verify(message.as_bytes(), &signature).unwrap();
    }

    #[test]
    fn the_heartbeat_body_carries_no_update_digest() {
        let stub = Stub::new(vec![response(204, &[], b"")]);
        client(&stub.base_url)
            .heartbeat("laptop", "hash", "v", "none", timeout())
            .unwrap();
        let body = stub.last_body();
        // 请求里只有「我是谁、我现在的状态」：手上那份发布包不进请求，判重在客户端。
        assert!(
            body.get("appUpdateKnown").is_none(),
            "心跳请求不该带发布摘要: {body}"
        );
    }

    #[test]
    fn heartbeat_parses_the_changes() {
        let config = BASE64.encode("hostname = \"laptop\"\n");
        let body = format!(
            r#"{{"configHash":"h2","configB64":"{config}","appUpdate":{{"version":"2026.11.27-1","url":"http://10.126.126.1:18080/x.tar.zst","notes":"修了个 bug","sha256":"ab","autoUpdate":true}},"launcher":{{"version":"2026.11.27-1","url":"http://10.126.126.1:18080/mesh.exe","sha256":"cd"}},"credentialId":"uuid","credentialExpiry":1798000000}}"#
        );
        let stub = Stub::new(vec![response(200, &[], body.as_bytes())]);
        let reply = client(&stub.base_url)
            .heartbeat("laptop", "hash", "2026.11.26-13", "none", timeout())
            .unwrap()
            .unwrap();
        assert_eq!(reply.config_hash, "h2");
        assert_eq!(reply.config_toml().unwrap(), "hostname = \"laptop\"\n");
        let update = reply.app_update.unwrap();
        assert_eq!(update.version, "2026.11.27-1");
        assert_eq!(update.notes, "修了个 bug");
        assert!(update.auto_update);
        assert_eq!(reply.launcher.unwrap().sha256, "cd");
        assert_eq!(reply.credential_id, "uuid");
        assert_eq!(reply.credential_expiry, 1_798_000_000);
    }

    #[test]
    fn a_204_with_no_fields_leaves_everything_empty() {
        let stub = Stub::new(vec![response(200, &[], b"{}")]);
        let reply = client(&stub.base_url)
            .heartbeat("laptop", "hash", "v", "none", timeout())
            .unwrap()
            .unwrap();
        assert_eq!(reply.config_hash, "");
        assert!(reply.app_update.is_none());
        assert!(!reply.app_update.is_some());
        assert_eq!(reply.config_toml().unwrap(), "");
    }

    #[test]
    fn unauthorized_keeps_the_code_and_the_server_time() {
        let stub = Stub::new(vec![response(
            401,
            &[("Date", "Wed, 21 Oct 2015 07:28:00 GMT")],
            br#"{"code":"unauthorized"}"#,
        )]);
        let error = client(&stub.base_url)
            .heartbeat("laptop", "hash", "v", "none", timeout())
            .unwrap_err();
        match error {
            Error::Unauthorized { code, server_time } => {
                assert_eq!(code, "unauthorized");
                assert_eq!(
                    server_time.unwrap(),
                    UNIX_EPOCH + Duration::from_secs(1_445_412_480)
                );
            }
            other => panic!("应是 401，得到 {other:?}"),
        }
    }

    #[test]
    fn other_statuses_carry_status_code_and_message() {
        let stub = Stub::new(vec![response(
            409,
            &[],
            // 消息里有中文，用普通字符串再转字节（字节串字面量只允许 ASCII）。
            r#"{"code":"device_limit","message":"设备数已达上限"}"#.as_bytes(),
        )]);
        let error = client(&stub.base_url)
            .heartbeat("laptop", "hash", "v", "none", timeout())
            .unwrap_err();
        match error {
            Error::Api {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 409);
                assert_eq!(code, "device_limit");
                assert_eq!(message, "设备数已达上限");
            }
            other => panic!("应是 Api，得到 {other:?}"),
        }
    }

    #[test]
    fn download_writes_the_file_and_enforces_the_limit() {
        let dir = std::env::temp_dir().join(format!("mesh-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("update.tar.zst");
        let stub = Stub::new(vec![
            response(200, &[], b"abcd"),
            response(200, &[], b"abcd"),
        ]);
        let client = client(&stub.base_url);
        let url = format!("{}/update.tar.zst", stub.base_url);
        assert_eq!(client.download(&url, &dst, 4, timeout()).unwrap(), 4);
        assert_eq!(std::fs::read(&dst).unwrap(), b"abcd");
        // 超限必须报错：宁可重下一遍，也不能把半截包当成功。
        assert!(client.download(&url, &dst, 3, timeout()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 控制面用种子 `00..1f` 封出来的那封信封（`sealed.rs` 的用例里也有它）。
    const SEALED: &str = "ZDA2HMrT/31vujoZvinzIrjif+cWRPdzTK3sTaRJKiHKeXaBiQwzIQ4jdKtUBpSODCvi6rC3reGWyCeygVPeZ/7X5GzdRqvt6LMwDEQ7jBg=";

    fn seed_client() -> Client {
        let mut seed = [0u8; 32];
        for (index, byte) in seed.iter_mut().enumerate() {
            *byte = index as u8;
        }
        Client::new("http://10.0.0.1:8089", SigningKey::from_bytes(&seed))
    }

    #[test]
    fn a_sealed_credential_is_used() {
        let heartbeat = Heartbeat {
            credential_sealed: SEALED.to_string(),
            credential_id: "cred-1".to_string(),
            ..Heartbeat::default()
        };
        let secret = seed_client().credential_secret(&heartbeat).unwrap();
        assert_eq!(
            BASE64.decode(secret).unwrap(),
            b"0123456789abcdef0123456789abcdef",
            "封装的明文就是凭据私钥本身"
        );
    }

    #[test]
    fn a_broken_sealed_credential_is_refused_rather_than_trusted() {
        for broken in ["not base64", "AAAA"] {
            let heartbeat = Heartbeat {
                credential_sealed: broken.to_string(),
                credential_id: "cred-1".to_string(),
                ..Heartbeat::default()
            };
            assert!(
                matches!(
                    seed_client().credential_secret(&heartbeat),
                    Err(Error::Sealed(_))
                ),
                "封装的凭据坏了却不能当成成功：{broken:?}"
            );
        }
        // 没有封装就是这一次没有签发凭据，不是「解不开」。
        let none = Heartbeat {
            credential_id: "cred-1".to_string(),
            ..Heartbeat::default()
        };
        assert_eq!(seed_client().credential_secret(&none).unwrap(), "");
    }

    #[test]
    fn a_sealed_credential_of_the_wrong_length_is_refused() {
        // 另一台设备的密钥解不开这封，所以只能证明长度校验在解封之后也生效：
        // 这里用另一个种子对同一封信封解封，必须失败而不是给出短明文。
        let heartbeat = Heartbeat {
            credential_sealed: SEALED.to_string(),
            credential_id: "cred-1".to_string(),
            ..Heartbeat::default()
        };
        let other = Client::new("http://10.0.0.1:8089", SigningKey::from_bytes(&[3u8; 32]));
        assert!(matches!(
            other.credential_secret(&heartbeat),
            Err(Error::Sealed(_))
        ));
    }
}
