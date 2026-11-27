//! Windows 上两件只能靠 API 做的事：收紧 `config/` 的 DACL、用 DPAPI 加密凭据私钥。
//!
//! 两件事都是「对本地其它账户」的防线：客户端以提权身份运行，同账户进程本来就能读
//! 它读得到的一切，这两个 API 挡不住那一档。

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::Security::{
    AddAccessAllowedAceEx, CreateWellKnownSid, GetTokenInformation, InitializeAcl, TokenUser,
    WinBuiltinAdministratorsSid, WinBuiltinUsersSid, WinLocalSystemSid, ACL, ACL_REVISION,
    CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
    PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 目录与文件的完全控制权限（FILE_ALL_ACCESS 的数值）。
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
/// 读 + 执行：安装根里的文件要能被任何本地账户读取与运行，但不能被改写。
const FILE_READ_EXECUTE: u32 = 0x0012_00A9;

/// 用给定的 (SID, 访问掩码) 序列初始化一块 ACL，然后交给 `body` 去套用。
///
/// 缓冲区用 u64 数组而不是 Vec<u8>：ACL 要求 4 字节对齐，而 Vec<u8> 只保证 1 字节，
/// 今天能跑只是分配器恰好给了对齐的地址。
fn with_acl(
    entries: &[(&[u8], u32)],
    body: impl FnOnce(*mut ACL) -> io::Result<()>,
) -> io::Result<()> {
    let mut acl_buf = [0u64; 64];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    // SAFETY: 缓冲区至少 512 字节且 8 字节对齐；SID 由 CreateWellKnownSid /
    // GetTokenInformation 分配，活得比 acl 长。
    unsafe {
        if InitializeAcl(acl, std::mem::size_of_val(&acl_buf) as u32, ACL_REVISION) == 0 {
            return Err(io::Error::last_os_error());
        }
        for (sid, access) in entries {
            let ok = AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                *access,
                sid.as_ptr() as *mut _,
            );
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    body(acl)
}

/// 把 acl 套到 path 上并切断继承。
fn set_dacl(path: &Path, acl: *mut ACL) -> io::Result<()> {
    let mut wide = wide_path(path);
    // SAFETY: wide 是 NUL 结尾的 UTF-16 缓冲，acl 由 with_acl 保证在调用期间有效。
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// 把 `config/` 的 DACL 换成「当前用户 + SYSTEM + Administrators 完全控制」，并切断继承。
///
/// 不带这一步时，`config/` 会继承安装根所在卷的默认 ACL——在数据盘或用户目录下
/// 就是 `Users: 读取` 与 `Authenticated Users: 修改`，两个私钥对所有本地账户可读。
pub fn harden_dir(dir: &Path) -> io::Result<()> {
    let user = current_user_sid()?;
    let system = well_known_sid(WinLocalSystemSid)?;
    let admins = well_known_sid(WinBuiltinAdministratorsSid)?;
    let mut last_error: Option<io::Error> = None;
    with_acl(
        &[
            (&user, FILE_ALL_ACCESS),
            (&system, FILE_ALL_ACCESS),
            (&admins, FILE_ALL_ACCESS),
        ],
        |acl| {
            set_dacl(dir, acl)?;
            // 目录换了 DACL 不会回头改已有的子项：逐个再盖一次，否则旧安装里的
            // client.key / secure.key 仍然带着继承来的宽权限。
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if let Err(err) = set_dacl(&entry.path(), acl) {
                        // 改不动不算致命：目录本身已经切断继承。由调用方记日志。
                        last_error = Some(err);
                    }
                }
            }
            Ok(())
        },
    )?;
    match last_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// 把安装根本身的 DACL 换成「Administrators + SYSTEM 完全控制、Users 读+执行」。
///
/// 不收紧这一层时，安装根继承卷的默认 ACL（数据盘上常常是 `Authenticated Users:
/// 修改`），任何本地账户都能替换 `mesh.exe` 或往 `bin/` 里塞东西——而开机自启的
/// 计划任务会以最高权限拉起 `mesh.exe`。那是一条不需要任何漏洞的提权链。
///
/// `config/` 不在这一层处理：它有自己更严的 DACL，用这里的 ACE 盖上去反而会放宽。
pub fn harden_root(root: &Path) -> io::Result<()> {
    let system = well_known_sid(WinLocalSystemSid)?;
    let admins = well_known_sid(WinBuiltinAdministratorsSid)?;
    let users = well_known_sid(WinBuiltinUsersSid)?;
    let mut last_error: Option<io::Error> = None;
    with_acl(
        &[
            (&admins, FILE_ALL_ACCESS),
            (&system, FILE_ALL_ACCESS),
            (&users, FILE_READ_EXECUTE),
        ],
        |acl| {
            set_dacl(root, acl)?;
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    if entry.file_name().to_str() == Some(mesh_install::layout::CONFIG) {
                        continue;
                    }
                    if let Err(err) = set_dacl(&entry.path(), acl) {
                        last_error = Some(err);
                    }
                }
            }
            Ok(())
        },
    )?;
    match last_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// 用当前用户的 DPAPI 密钥加密一段字节。解密的只有同一台机器上的同一用户。
pub fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let blob =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as *mut _) };
    Ok(blob)
}

/// protect 的逆操作。blob 不是本机本账户产生的（或已被改过）时报错，不返回垃圾。
pub fn unprotect(blob: &[u8]) -> io::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let plain =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as *mut _) };
    Ok(plain)
}

/// 进程令牌里的用户 SID。
fn current_user_sid() -> io::Result<Vec<u8>> {
    let mut token = std::ptr::null_mut();
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        )
    };
    unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = buf.as_ptr() as *const TOKEN_USER;
    copy_sid(unsafe { (*user).User.Sid })
}

fn well_known_sid(kind: i32) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(len as usize);
    Ok(buf)
}

/// SID 自带长度字段：第二字节是 SubAuthorityCount，总长 = 8 + 4×n。
fn copy_sid(sid: *mut core::ffi::c_void) -> io::Result<Vec<u8>> {
    if sid.is_null() {
        return Err(io::Error::other("令牌里没有用户 SID"));
    }
    let bytes = unsafe { std::slice::from_raw_parts(sid as *const u8, 8) };
    let len = 8 + 4 * bytes[1] as usize;
    Ok(unsafe { std::slice::from_raw_parts(sid as *const u8, len) }.to_vec())
}

fn wide_path(path: &Path) -> Vec<u16> {
    OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
