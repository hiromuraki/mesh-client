//! Windows 实现：用 `schtasks.exe` 管任务计划程序里的 `Mesh Client`。
//!
//! 不直接调 Task Scheduler 的 COM 接口：一共三条命令（建、删、查），`schtasks` 是
//! 系统自带、行为稳定的那个入口，接 COM 要多一整层 vtable 声明和内存管理。

use std::ffi::OsStr;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_ACP};

/// 任务计划程序里的名字。放在根任务目录下，不建子目录。
const TASK_NAME: &str = "Mesh Client";

/// 客户端是 windowsgui 子系统：不隐藏的话 spawn 控制台程序会闪一个黑框。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(crate) const SUPPORTED: bool = true;

/// 任务是否存在。
///
/// 只看任务在不在，不比对它指向的路径：安装目录被挪走之后任务还在、只是指向了旧目录，
/// 那时用户再勾一次就会被覆盖成新路径；这里报 false 反而会让复选框自己跳回未选中，
/// 看着像设置丢了。
pub(crate) fn enabled() -> Result<bool, String> {
    match run(["/Query", "/TN", TASK_NAME]) {
        Ok(()) => Ok(true),
        // 任务不存在时 schtasks 返回 1；其它退出码（服务没起来、参数错）是真错误。
        Err(Failure::Exit { code: 1, .. }) => Ok(false),
        Err(err) => Err(err.to_string()),
    }
}

/// 以 `launcher`（安装根目录的启动器）为目标启用开机自启，已有任务覆盖。
///
/// 目标始终是启动器而不是 `bin/` 里的主程序：开机时该跑的是负责回滚与拉起的那一个。
pub(crate) fn enable(launcher: &Path) -> Result<(), String> {
    let absolute =
        std::path::absolute(launcher).map_err(|err| format!("解析启动器路径失败: {err}"))?;
    if !absolute.is_file() {
        return Err(format!("启动器不存在: {}", absolute.display()));
    }
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建临时目录失败: {err}"))?;
    // schtasks /XML 只收 UTF-16：写成 UTF-8 会被判成 XML 格式错误。
    let file = dir.join("task.xml");
    let written = std::fs::write(&file, utf16le(&task_xml(&absolute, &account_name())))
        .map_err(|err| format!("写任务定义失败: {err}"));
    let result = written.and_then(|()| {
        run([
            OsStr::new("/Create"),
            OsStr::new("/F"),
            OsStr::new("/TN"),
            OsStr::new(TASK_NAME),
            OsStr::new("/XML"),
            file.as_os_str(),
        ])
        .map_err(|err| format!("创建开机自启任务失败: {err}"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// 删除自启任务；本来就没有时直接返回。
pub(crate) fn disable() -> Result<(), String> {
    if !enabled()? {
        return Ok(());
    }
    run(["/Delete", "/F", "/TN", TASK_NAME]).map_err(|err| format!("删除开机自启任务失败: {err}"))
}

/// schtasks 的一次失败。分两种：进程起不来，和跑完以后非零退出。
///
/// 调用方要靠退出码区分「任务不存在」（1）与「查询本身失败」，所以错误不能只有一句话。
enum Failure {
    Spawn(String),
    Exit { code: i32, output: String },
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Spawn(message) => write!(f, "{message}"),
            Failure::Exit { code, output } if output.is_empty() => {
                write!(f, "schtasks 退出码 {code}")
            }
            Failure::Exit { code, output } => write!(f, "schtasks 退出码 {code}: {output}"),
        }
    }
}

/// 执行一条 schtasks 命令。失败时把它的输出一起带出来：那几句话是唯一能说明为什么
/// 没建成的线索，光有退出码没法排查。
fn run<I, S>(args: I) -> Result<(), Failure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|err| Failure::Spawn(format!("无法执行 schtasks.exe: {err}")))?;
    // 被强制终止时没有退出码，按 -1 处理（与 Go 的 ExitCode() 一致）。
    let code = output.status.code().unwrap_or(-1);
    if code == 0 {
        return Ok(());
    }
    let mut text = decode(&output.stdout);
    let stderr = decode(&output.stderr);
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&stderr);
    }
    Err(Failure::Exit { code, output: text })
}

/// schtasks 按控制台代码页输出（中文系统上是 GBK），当 UTF-8 解会整段变成替换符，
/// 报错就又没有线索了。
fn decode(bytes: &[u8]) -> String {
    let Ok(length) = i32::try_from(bytes.len()) else {
        return String::new();
    };
    if length == 0 {
        return String::new();
    }
    unsafe {
        let needed =
            MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), length, std::ptr::null_mut(), 0);
        let Ok(needed) = usize::try_from(needed) else {
            return String::new();
        };
        let mut buffer = vec![0u16; needed];
        let Ok(capacity) = i32::try_from(needed) else {
            return String::new();
        };
        let written = MultiByteToWideChar(
            CP_ACP,
            0,
            bytes.as_ptr(),
            length,
            buffer.as_mut_ptr(),
            capacity,
        );
        let Ok(written) = usize::try_from(written) else {
            return String::new();
        };
        buffer.truncate(written);
        String::from_utf16_lossy(&buffer).trim().to_string()
    }
}

/// 「计算机或域\用户名」。拿不到就返回空串，XML 里不写 UserId，任务退化成
/// 「任意用户登录时运行」。
fn account_name() -> String {
    let user = std::env::var("USERNAME")
        .unwrap_or_default()
        .trim()
        .to_string();
    if user.is_empty() {
        return String::new();
    }
    let mut domain = std::env::var("USERDOMAIN")
        .unwrap_or_default()
        .trim()
        .to_string();
    if domain.is_empty() {
        domain = std::env::var("COMPUTERNAME")
            .unwrap_or_default()
            .trim()
            .to_string();
    }
    if domain.is_empty() {
        return user;
    }
    format!("{domain}\\{user}")
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("mesh-autostart-{}", std::process::id()))
}

/// 任务定义。只写必要的元素：schtasks 对 Task Scheduler 的 XSD 校验很严，多写一个
/// 顺序不对的节点就整份拒绝。
///
/// `ExecutionTimeLimit` 必须是 `PT0S`：默认值是 72 小时，到点任务被强制终止，客户端
/// 会在开机第三天无声无息地消失。
fn task_xml(launcher: &Path, account: &str) -> String {
    // 两个 UserId 的缩进一样（都是 6 个空格），所以只拼一次。
    let user_id = if account.is_empty() {
        String::new()
    } else {
        format!("      <UserId>{}</UserId>\n", esc(account))
    };
    let command = esc(&launcher.display().to_string());
    let directory = esc(&launcher
        .parent()
        .unwrap_or(Path::new(""))
        .display()
        .to_string());
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n");
    xml.push_str(
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n",
    );
    xml.push_str("  <RegistrationInfo>\n    <Description>");
    xml.push_str(&esc("Mesh Client：登录后自动启动客户端"));
    xml.push_str("</Description>\n  </RegistrationInfo>\n");
    xml.push_str("  <Triggers>\n    <LogonTrigger>\n      <Enabled>true</Enabled>\n");
    xml.push_str(&user_id);
    xml.push_str("    </LogonTrigger>\n  </Triggers>\n");
    xml.push_str("  <Principals>\n    <Principal id=\"Author\">\n");
    xml.push_str(&user_id);
    xml.push_str("      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>HighestAvailable</RunLevel>\n    </Principal>\n  </Principals>\n");
    xml.push_str("  <Settings>\n");
    xml.push_str("    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n");
    xml.push_str("    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n");
    xml.push_str("    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n");
    xml.push_str("    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n");
    xml.push_str("    <Enabled>true</Enabled>\n  </Settings>\n");
    xml.push_str("  <Actions Context=\"Author\">\n    <Exec>\n");
    xml.push_str("      <Command>");
    xml.push_str(&command);
    xml.push_str("</Command>\n      <WorkingDirectory>");
    xml.push_str(&directory);
    xml.push_str("</WorkingDirectory>\n    </Exec>\n  </Actions>\n</Task>\n");
    xml
}

/// XML 文本转义。路径里的 `&` 不转义会让整份任务定义被拒绝。
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// schtasks 要 UTF-16 小端且带 BOM。
fn utf16le(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + text.len() * 2);
    out.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 任务定义直接决定开机能不能起来，这里钉住几个关键字段：少一个 HighestAvailable
    // 就会弹 UAC，少一个 PT0S 客户端会在三天后被系统杀掉。
    #[test]
    fn task_xml_keeps_the_fields_that_decide_whether_it_runs() {
        let xml = task_xml(Path::new("C:\\Program Files\\Mesh\\mesh.exe"), "PC\\user");
        for want in [
            "<LogonTrigger>",
            "<UserId>PC\\user</UserId>",
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>HighestAvailable</RunLevel>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<Command>C:\\Program Files\\Mesh\\mesh.exe</Command>",
            "<WorkingDirectory>C:\\Program Files\\Mesh</WorkingDirectory>",
        ] {
            assert!(xml.contains(want), "任务 XML 缺少 {want:?}:\n{xml}");
        }
    }

    #[test]
    fn task_xml_without_account_has_no_user_id() {
        let xml = task_xml(Path::new("C:\\mesh\\mesh.exe"), "");
        assert!(
            !xml.contains("<UserId>"),
            "拿不到用户名时不该写 UserId:\n{xml}"
        );
    }

    #[test]
    fn task_xml_escapes_the_path() {
        let xml = task_xml(Path::new("C:\\a&b\\mesh.exe"), "PC\\user");
        assert!(
            xml.contains("C:\\a&amp;b\\mesh.exe"),
            "路径里的 & 没有转义:\n{xml}"
        );
    }

    #[test]
    fn utf16le_writes_a_bom_and_little_endian_units() {
        let bytes = utf16le("Aé");
        assert_eq!(bytes.len(), 6);
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "缺少 UTF-16LE BOM");
        assert_eq!(&bytes[2..4], &[b'A', 0], "ASCII 字符编码不对");
        assert_eq!(&bytes[4..6], &[0xE9, 0], "é 的编码不对");
    }

    // 自启状态只读真实系统，测试不能碰它：真建一条 `Mesh Client` 任务会把这台机器的
    // 开机行为改掉。所以这里只覆盖纯函数，enabled/enable/disable 由真机验收。
}
