use std::path::Path;
use std::process::Child;

/// 客户端可执行文件名两个平台不同，安装目录布局是同一套。
#[cfg(windows)]
pub const CLIENT_EXE: &str = "mesh-client.exe";
#[cfg(unix)]
pub const CLIENT_EXE: &str = "mesh-client";

/// start 启动客户端并返回子进程句柄。
///
/// Windows 返回 None：客户端是独立进程，启动器确认它起来之后就退出，
/// 没有谁会去等它（提权走 ShellExecute，连 Child 都拿不到）。
/// Linux 返回 Some：启动器是常驻的，客户端是它的子进程，退出码要透传给 systemd。
pub fn start(exe: &Path, root: &Path, boot_check: Option<&str>) -> Result<Option<Child>, String> {
    imp::start(exe, root, boot_check)
}

/// wait_exit 等某个进程退出，仅用于 --wait-for（手动安装时等旧客户端让位）。
/// Linux 的常驻启动器走的是 Child::wait，不经过这里。
pub fn wait_exit(pid: u32, timeout: std::time::Duration) -> Result<(), String> {
    imp::wait_exit(pid, timeout)
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
        SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    /// 客户端清单要求管理员权限（建 TUN 必须），启动器自己是 asInvoker。
    /// 未提权时 CreateProcess 会以 ERROR_ELEVATION_REQUIRED(740) 直接失败，那种情况
    /// 改走 ShellExecuteEx 的 runas，由系统弹 UAC。
    const ERROR_ELEVATION_REQUIRED: i32 = 740;
    const ERROR_CANCELLED: i32 = 1223;

    pub fn start(
        exe: &Path,
        root: &Path,
        boot_check: Option<&str>,
    ) -> Result<Option<Child>, String> {
        let mut cmd = Command::new(exe);
        cmd.arg("--root").arg(root);
        if let Some(token) = boot_check {
            cmd.arg("--boot-check").arg(token);
        }
        cmd.current_dir(root);
        match cmd.spawn() {
            Ok(_child) => Ok(None),
            Err(e) if e.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED) => {
                runas(exe, root, boot_check)?;
                Ok(None)
            }
            Err(e) => Err(mesh_text::format(
                mesh_text::id::LAUNCHER_ERROR_START_CLIENT,
                &[&e],
            )),
        }
    }

    fn runas(exe: &Path, root: &Path, boot_check: Option<&str>) -> Result<(), String> {
        let verb = wide(OsStr::new("runas"));
        let file = wide(exe.as_os_str());
        let directory = wide(root.as_os_str());
        let mut params = format!("--root {}", quote(&root.display().to_string()));
        if let Some(token) = boot_check {
            params.push_str(&format!(" --boot-check {}", quote(token)));
        }
        let parameters = wide(OsStr::new(&params));

        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = parameters.as_ptr();
        info.lpDirectory = directory.as_ptr();
        info.nShow = SW_SHOWNORMAL;

        let ok = unsafe { ShellExecuteExW(&mut info) };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_CANCELLED) {
                return Err(mesh_text::get(mesh_text::id::LAUNCHER_TEXT_UAC_CANCELLED).to_string());
            }
            return Err(mesh_text::format(
                mesh_text::id::LAUNCHER_ERROR_START_CLIENT_ADMIN,
                &[&err],
            ));
        }
        if !info.hProcess.is_null() {
            unsafe { CloseHandle(info.hProcess) };
        }
        Ok(())
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// quote 只处理空格与引号：路径里不会有裸引号，但 token 是可控输入。
    fn quote(s: &str) -> String {
        format!("\"{}\"", s.replace('"', ""))
    }

    pub fn wait_exit(pid: u32, timeout: Duration) -> Result<(), String> {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                // 打开失败通常意味着它已经退了。
                return Ok(());
            }
            let code = WaitForSingleObject(handle, timeout.as_millis() as u32);
            CloseHandle(handle);
            match code {
                WAIT_OBJECT_0 => Ok(()),
                WAIT_TIMEOUT => Err(mesh_text::format(
                    mesh_text::id::LAUNCHER_ERROR_WAIT_EXIT_TIMEOUT,
                    &[&pid, &timeout.as_secs()],
                )),
                other => Err(mesh_text::format(
                    mesh_text::id::LAUNCHER_ERROR_WAIT_EXIT,
                    &[&pid, &other],
                )),
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::path::Path;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    pub fn start(
        exe: &Path,
        root: &Path,
        boot_check: Option<&str>,
    ) -> Result<Option<Child>, String> {
        let mut cmd = Command::new(exe);
        cmd.arg("--root").arg(root);
        if let Some(token) = boot_check {
            cmd.arg("--boot-check").arg(token);
        }
        cmd.current_dir(root);
        let child = cmd
            .spawn()
            .map_err(|e| mesh_text::format(mesh_text::id::LAUNCHER_ERROR_START_CLIENT, &[&e]))?;
        Ok(Some(child))
    }

    /// wait_exit 靠 /proc 判断存活，不引 libc：这里只需要“还在不在”，
    /// 退出状态由常驻启动器用 Child::wait 拿。pid 被复用时会误判，
    /// 但 --wait-for 只用在手动安装，能接受。
    pub fn wait_exit(pid: u32, timeout: Duration) -> Result<(), String> {
        let dir = Path::new("/proc").join(pid.to_string());
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !dir.exists() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(mesh_text::format(
            mesh_text::id::LAUNCHER_ERROR_WAIT_EXIT_TIMEOUT,
            &[&pid, &timeout.as_secs()],
        ))
    }
}
