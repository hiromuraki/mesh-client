//! 前端构建产物：编译期嵌入，不复制、不改名、不在运行期读文件。
//!
//! 路径相对 crate 根解析（crates/panel -> client-rust -> 仓库根），直接吃
//! client/frontend/dist。目录不存在时编译直接失败——这是有意的：前端没构建就
//! 不该产出一个「面板打开是白屏」的二进制。

use crate::router::Response;
use include_dir::{include_dir, Dir};

static DIST: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../frontend/dist");

/// 控制面板只有单页：不存在的路径一律回 index.html（Go 的 handleStatic 同样把
/// 未知路径改写成 /）。
pub(crate) fn serve(path: &str) -> Response {
    let file = relative_path(path)
        .and_then(|relative| DIST.get_file(relative))
        .or_else(|| DIST.get_file("index.html"));
    match file {
        Some(file) => Response::asset(content_type(file.path()), file.contents().to_vec()),
        // include_dir 在编译期就会检查目录，index.html 一定在；真缺了也不该把
        // 请求挂住。
        None => Response::error(404, "not_found"),
    }
}

/// 把请求路径变成嵌入目录里的相对路径。
///
/// 只接受简单的相对路径：出现 ..、反斜杠或空段的请求一律当作不存在（回 SPA
/// 兜底），避免把路径拼接交给文件系统。
fn relative_path(path: &str) -> Option<&str> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let simple = trimmed
        .split('/')
        .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\\'));
    if simple {
        Some(trimmed)
    } else {
        None
    }
}

/// 按扩展名给出 Content-Type，与 Go 的 http.FileServer 对同一批文件的选择一致。
pub(crate) fn content_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "png" => "image/png",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}
