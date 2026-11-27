//! 与平台绑定的三件事：子进程的创建方式、把它纳入本进程的生命周期、以及终止它。
//!
//! 对应 Go 的 `coreproc/{platform_windows,platform_other,job_windows,job_other}.go`。

use std::process::Command;

/// 创建前调整子进程：Windows 上不弹控制台窗口，Unix 上让它成为新会话/进程组的组长。
pub(crate) fn configure(cmd: &mut Command) {
    imp::configure(cmd)
}

/// 把子进程纳入本进程的生命周期：本进程被强制结束时，core 必须跟着死。
///
/// 否则客户端崩溃/被任务管理器结束时，core 会留下来占着 TUN 适配器与虚拟 IP，
/// 下一个客户端起来时 core 抢不到同一份资源，表现为“重启后一直起不来”。
pub(crate) fn assign_to_job(pid: u32) -> Result<(), String> {
    imp::assign_to_job(pid)
}

/// 终止 core。`force == false` 是“先礼”：Windows 上是 taskkill 不带 /F（只能发
/// 关闭消息），Unix 上是 SIGTERM 到整个进程组。
pub(crate) fn terminate(pid: u32, force: bool) -> Result<(), String> {
    imp::terminate(pid, force)
}

#[cfg(windows)]
mod imp {
    use std::process::Command;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// `CREATE_NO_WINDOW`：core 是 console 程序，客户端是 windowsgui 子系统，
    /// 不设这一位会在桌面形态下闪一个黑窗口。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub(crate) fn configure(cmd: &mut Command) {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    /// Job 句柄进程内共享一份：Go 也是懒创建一个全局 job 之后不再关，
    /// 句柄随进程退出由内核关闭，那时 KILL_ON_JOB_CLOSE 才生效。
    struct Job(HANDLE);
    // HANDLE 是裸指针，但 Job 只在本进程内独占地持有它，且不跨线程访问内部数据。
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    static JOB: OnceLock<Result<Job, String>> = OnceLock::new();

    pub(crate) fn assign_to_job(pid: u32) -> Result<(), String> {
        let job = match JOB.get_or_init(create_job) {
            Ok(job) => job,
            Err(err) => return Err(err.clone()),
        };
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            return Err(mesh_text::format(
                mesh_text::id::CORE_ERROR_OPEN_PROCESS,
                &[&pid, &std::io::Error::last_os_error()],
            ));
        }
        let assigned = unsafe { AssignProcessToJobObject(job.0, process) };
        unsafe { CloseHandle(process) };
        if assigned == 0 {
            return Err(mesh_text::format(
                mesh_text::id::CORE_ERROR_ASSIGN_JOB,
                &[&std::io::Error::last_os_error()],
            ));
        }
        Ok(())
    }

    fn create_job() -> Result<Job, String> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(mesh_text::format(
                mesh_text::id::CORE_ERROR_CREATE_JOB,
                &[&std::io::Error::last_os_error()],
            ));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let err = std::io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(mesh_text::format(
                mesh_text::id::CORE_ERROR_SET_JOB,
                &[&err],
            ));
        }
        Ok(Job(handle))
    }

    pub(crate) fn terminate(pid: u32, force: bool) -> Result<(), String> {
        // taskkill /T 结束整棵进程树：core 会派生自己的子进程，只杀父进程会把它们
        // 留成孤儿。不带 /F 时它只能给顶层窗口发关闭消息，而 core 没有窗口，于是
        // 必然失败——调用方看到这个错误就立刻改用 /F，不再空等。
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T"]);
        if force {
            cmd.arg("/F");
        }
        configure(&mut cmd);
        // 用 output 而不是 status：taskkill 的提示是本地化的多行文本，直接继承
        // 控制台会把它打进客户端/启动器的输出里；真正失败时并进错误信息更有用。
        let output = cmd
            .output()
            .map_err(|err| mesh_text::format(mesh_text::id::CORE_ERROR_TASKKILL_CALL, &[&err]))?;
        if output.status.success() {
            Ok(())
        } else {
            let detail = String::from_utf8_lossy(&output.stderr);
            Err(mesh_text::format(
                mesh_text::id::CORE_ERROR_TASKKILL_STATUS,
                &[&output.status, &detail.trim()],
            ))
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::process::Command;

    pub(crate) fn configure(cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        // setsid 让 core 成为新会话与进程组的组长：之后按组发信号才能带上它派生的
        // 子进程，终端/Ctrl+C 也不会只带走一个。不能用 process_group(0)：那只改
        // PGID、不换会话，systemd 停客户端时信号仍会打到 core。
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    pub(crate) fn assign_to_job(_pid: u32) -> Result<(), String> {
        Ok(())
    }

    pub(crate) fn terminate(pid: u32, force: bool) -> Result<(), String> {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let pid = pid as i32;
        // 负 pid 表示整个进程组；core 是组长，正常情况一次就够。
        if unsafe { libc::kill(-pid, signal) } == 0 {
            return Ok(());
        }
        let group_error = std::io::Error::last_os_error();
        if unsafe { libc::kill(pid, signal) } == 0 {
            return Ok(());
        }
        Err(mesh_text::format(
            mesh_text::id::CORE_ERROR_SIGNAL,
            &[&pid, &group_error],
        ))
    }
}
