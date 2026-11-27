use std::path::Path;

/// 客户端可执行文件名两个平台不同，安装目录布局是同一套。
#[cfg(windows)]
pub const CLIENT_EXE: &str = "mesh-client.exe";
#[cfg(unix)]
pub const CLIENT_EXE: &str = "mesh-client";

/// 停止客户端时给它的时间。超过就强杀，再等这么久确认它真的退出。
const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 启动器拉起的客户端进程。
///
/// 拿着进程句柄是「确认它已经退出」的前提：启动确认超时之后必须先把这一版停掉，
/// 才能把 `bin/` 换回旧版本，否则换目录时它还可能占着 TUN、虚拟 IP 与文件。
/// 两个平台的差别只在怎么停它：Linux 先 `SIGTERM`，Windows 只有 `TerminateProcess`。
pub struct Client {
    inner: imp::Process,
}

impl Client {
    /// 已经退出时返回退出码，还在跑返回 `None`。
    pub fn exited(&mut self) -> Result<Option<i32>, String> {
        self.inner.exited()
    }

    /// 结束它，并在返回前确认它真的退出了。
    ///
    /// 返回 `Err` 表示「没能确认它退出」：调用方此时不得改动 `bin/`。
    pub fn stop(&mut self) -> Result<(), String> {
        self.inner.stop()
    }

    /// 等它自己退出，返回退出码。
    ///
    /// 只有 Linux 的常驻启动器用得到：Windows 的启动器确认启动成功就退出，
    /// 没有人等客户端结束。
    #[cfg(unix)]
    pub fn wait(&mut self) -> Result<i32, String> {
        self.inner.wait()
    }
}

/// start 启动客户端并返回进程句柄。
///
/// 启动确认不靠参数或标记：客户端就位之后自己提交 `bin/current`，启动器等的是
/// 那个文件变成正在跑的这个槽（见 `install.rs` 的 `confirm`）。
pub fn start(exe: &Path, root: &Path) -> Result<Client, String> {
    imp::start(exe, root)
}

/// wait_exit 等某个进程退出，仅用于 --wait-for（手动安装时等旧客户端让位）。
/// Linux 的常驻启动器走的是 [`Client::wait`]，不经过这里。
pub fn wait_exit(pid: u32, timeout: std::time::Duration) -> Result<(), String> {
    imp::wait_exit(pid, timeout)
}
#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::path::Path;
    use std::process::{Child, Command};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
    };
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

    pub struct Process {
        slot: Slot,
    }

    /// 两种来路的进程句柄：自己 spawn 出来的，和系统提权（UAC）交给我们的。
    /// 后者的 `HANDLE` 交给 `OwnedHandle` 关闭；前者由 `Child` 持有。
    enum Slot {
        Child(Child),
        Elevated(OwnedHandle),
    }

    impl Slot {
        fn raw(&self) -> HANDLE {
            match self {
                Slot::Child(child) => child.as_raw_handle() as HANDLE,
                Slot::Elevated(handle) => handle.as_raw_handle() as HANDLE,
            }
        }
    }

    pub fn start(exe: &Path, root: &Path) -> Result<super::Client, String> {
        let mut cmd = Command::new(exe);
        cmd.arg("--root").arg(root);
        cmd.current_dir(root);
        match cmd.spawn() {
            Ok(child) => Ok(super::Client {
                inner: Process {
                    slot: Slot::Child(child),
                },
            }),
            Err(e) if e.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED) => {
                let handle = runas(exe, root)?;
                Ok(super::Client {
                    inner: Process {
                        slot: Slot::Elevated(handle),
                    },
                })
            }
            Err(e) => Err(mesh_text::format(
                mesh_text::id::LAUNCHER_ERROR_START_CLIENT,
                &[&e],
            )),
        }
    }

    /// runas 走 UAC，并保留进程句柄：没有它，确认超时之后就无从知道新版退没退。
    fn runas(exe: &Path, root: &Path) -> Result<OwnedHandle, String> {
        let verb = wide(OsStr::new("runas"));
        let file = wide(exe.as_os_str());
        let directory = wide(root.as_os_str());
        let params = format!("--root {}", quote(&root.display().to_string()));
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
        if info.hProcess.is_null() {
            // 没有句柄就无从确认它什么时候退出：当作启动失败，比放一个追不上的
            // 进程在后台、回头去换 bin/ 安全。
            return Err(mesh_text::get(mesh_text::id::LAUNCHER_ERROR_NO_CHILD).to_string());
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(info.hProcess as RawHandle) })
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// quote 只处理空格与引号：路径里不会有裸引号，但 token 是可控输入。
    fn quote(s: &str) -> String {
        let escaped = s.replace('\"', "");
        format!("\"{escaped}\"")
    }

    impl Process {
        pub fn exited(&mut self) -> Result<Option<i32>, String> {
            let handle = self.slot.raw();
            match unsafe { WaitForSingleObject(handle, 0) } {
                WAIT_OBJECT_0 => Ok(Some(exit_code(handle)?)),
                WAIT_TIMEOUT => Ok(None),
                other => Err(mesh_text::format(
                    mesh_text::id::LAUNCHER_ERROR_WAIT_CLIENT,
                    &[&other],
                )),
            }
        }

        /// Windows 上没有 SIGTERM：停只有强杀一条路。杀不掉就返回错误，由调用方
        /// 决定「不换目录」——回滚一个还在跑的版本比停下来更危险。
        pub fn stop(&mut self) -> Result<(), String> {
            let handle = self.slot.raw();
            // 已经退出时 TerminateProcess 返回 0，直接看等待结果即可。
            unsafe { TerminateProcess(handle, 1) };
            match unsafe { WaitForSingleObject(handle, super::STOP_TIMEOUT.as_millis() as u32) } {
                WAIT_OBJECT_0 => Ok(()),
                _ => Err(mesh_text::format(
                    mesh_text::id::LAUNCHER_ERROR_WAIT_CLIENT_EXIT,
                    &[&mesh_text::get(mesh_text::id::LAUNCHER_TEXT_KILL_CLIENT)],
                )),
            }
        }
    }

    fn exit_code(handle: HANDLE) -> Result<i32, String> {
        let mut code = 0u32;
        if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
            let err = std::io::Error::last_os_error();
            return Err(mesh_text::format(
                mesh_text::id::LAUNCHER_ERROR_WAIT_CLIENT_EXIT,
                &[&err],
            ));
        }
        Ok(code as i32)
    }

    pub fn wait_exit(pid: u32, timeout: Duration) -> Result<(), String> {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
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
    use crate::log;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    pub struct Process {
        child: Child,
    }

    pub fn start(exe: &Path, root: &Path) -> Result<super::Client, String> {
        let mut cmd = Command::new(exe);
        cmd.arg("--root").arg(root);
        cmd.current_dir(root);
        let child = cmd
            .spawn()
            .map_err(|e| mesh_text::format(mesh_text::id::LAUNCHER_ERROR_START_CLIENT, &[&e]))?;
        Ok(super::Client {
            inner: Process { child },
        })
    }

    impl Process {
        pub fn exited(&mut self) -> Result<Option<i32>, String> {
            match self.child.try_wait() {
                Ok(Some(status)) => Ok(Some(status.code().unwrap_or(-1))),
                Ok(None) => Ok(None),
                Err(e) => Err(mesh_text::format(
                    mesh_text::id::LAUNCHER_ERROR_WAIT_CLIENT,
                    &[&e],
                )),
            }
        }

        pub fn wait(&mut self) -> Result<i32, String> {
            self.child
                .wait()
                .map(|status| status.code().unwrap_or(1))
                .map_err(|e| {
                    mesh_text::format(mesh_text::id::LAUNCHER_ERROR_WAIT_CLIENT_EXIT, &[&e])
                })
        }

        /// 先礼后兵。必须是 SIGTERM：客户端把 core 放进了独立会话，只有它自己的
        /// Shutdown 才会停 core；SIGKILL 留下的孤儿 core 会和回退后新起的那个抢
        /// 同一个 TUN 与虚拟 IP，等于没回退。
        pub fn stop(&mut self) -> Result<(), String> {
            let pid = self.child.id();
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
            if wait_within(self, super::STOP_TIMEOUT)? {
                return Ok(());
            }
            log::line(mesh_text::get(mesh_text::id::LAUNCHER_TEXT_KILL_CLIENT));
            let _ = self.child.kill();
            if wait_within(self, super::STOP_TIMEOUT)? {
                return Ok(());
            }
            Err(mesh_text::format(
                mesh_text::id::LAUNCHER_ERROR_WAIT_CLIENT_EXIT,
                &[&mesh_text::get(mesh_text::id::LAUNCHER_TEXT_KILL_CLIENT)],
            ))
        }
    }

    /// 等它退出，最多等 `timeout`。返回 false 表示还活着。
    fn wait_within(process: &mut Process, timeout: Duration) -> Result<bool, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if process.exited()?.is_some() {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(process.exited()?.is_some())
    }

    /// wait_exit 靠 /proc 判断存活，不引 libc：这里只需要“还在不在”，
    /// 退出状态由常驻启动器用 [`super::Client::wait`] 拿。pid 被复用时会误判，
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
