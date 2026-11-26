//! 单实例：instance.lock 与「唤起已有实例」。
//!
//! 锁是操作系统的文件锁（mesh-install::lock），不是「文件存在即运行中」：客户端
//! 崩溃时内核会释放，不会留下一个要人工删的文件。拿不到锁说明已经有一个实例在跑，
//! 这时不该报错退出，而是把它的面板唤到前台（desktop）或把地址打出来（headless）。

use crate::APP_TITLE;
use std::path::Path;

/// 已有实例时用 config/ui.json 里的地址唤起面板。
///
/// 成功打开浏览器、或只能把地址写到 stderr，都算正常结束；连地址都没有才报错。
/// open 是注入的：单测不需要真的起浏览器。
pub(crate) fn wake_existing(
    ui_path: &Path,
    open: impl Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(url) = mesh_panel::read_url(ui_path) {
        if open(&url).is_ok() {
            return Ok(());
        }
        crate::platform::say_err(&format!("{APP_TITLE}: 已有实例在运行，面板地址: {url}"));
        return Ok(());
    }
    Err("已有实例在运行".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_ui(name: &str, content: Option<&str>) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mesh-client-wake-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.json");
        if let Some(content) = content {
            fs::write(&path, content).unwrap();
        }
        path
    }

    #[test]
    fn opens_the_saved_panel_url() {
        let path = temp_ui("ok", Some(r#"{"url":"http://127.0.0.1:11020/"}"#));
        let opened = std::cell::Cell::new(String::new());
        wake_existing(&path, |url| {
            opened.set(url.to_string());
            Ok(())
        })
        .expect("有地址就该唤起成功");
        assert_eq!(opened.into_inner(), "http://127.0.0.1:11020/");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn falls_back_to_printing_the_url_when_no_browser_is_available() {
        let path = temp_ui("nobrowser", Some(r#"{"url":"http://127.0.0.1:11021/"}"#));
        wake_existing(&path, |_| Err("没有桌面".to_string())).expect("打不出浏览器也不算错误");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn without_a_record_the_second_instance_fails() {
        let path = temp_ui("missing", None);
        assert_eq!(
            wake_existing(&path, |_| Ok(())).unwrap_err(),
            "已有实例在运行"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
