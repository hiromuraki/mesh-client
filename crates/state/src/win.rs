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
    WinBuiltinAdministratorsSid, WinLocalSystemSid, ACL, ACL_REVISION, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION,
    SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 目录与文件的完全控制权限（FILE_ALL_ACCESS 的数值）。
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

/// 把目录的 DACL 换成「当前用户 + SYSTEM + Administrators 完全控制」，并切断继承。
///
/// 不带这一步时，`config/` 会继承安装根所在卷的默认 ACL——在数据盘或用户目录下
/// 就是 `Users: 读取` 与 `Authenticated Users: 修改`，两个私钥对所有本地账户可读。
pub fn harden_dir(dir: &Path) -> io::Result<()> {
    let user = current_user_sid()?;
    let system = well_known_sid(WinLocalSystemSid)?;
    let admins = well_known_sid(WinBuiltinAdministratorsSid)?;

    // 三条 ACE 远小于 1 KiB。缓冲区用 u64 数组而不是 Vec<u8>：ACL 要求 4 字节
    // 对齐，而 Vec<u8> 只保证 1 字节——今天能跑只是分配器恰好给了对齐的地址，
    // 换分配器或换个切片来源就会静默写坏内存。
    let mut acl_buf = [0u64; 64];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    let mut last_error: Option<io::Error> = None;
    let mut wide = wide_path(dir);
    unsafe {
        if InitializeAcl(acl, std::mem::size_of_val(&acl_buf) as u32, ACL_REVISION) == 0 {
            return Err(io::Error::last_os_error());
        }
        for sid in [&user, &system, &admins] {
            let ok = AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                FILE_ALL_ACCESS,
                sid.as_ptr() as *mut _,
            );
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let rc = SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        );
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        // 目录换了 DACL 不会回头改已有的子项：逐个再盖一次，否则旧安装里的
        // client.key / secure.key 仍然带着继承来的宽权限。
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let mut child = wide_path(&entry.path());
                let rc = SetNamedSecurityInfoW(
                    child.as_mut_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    acl,
                    std::ptr::null(),
                );
                if rc == ERROR_SUCCESS {
                    continue;
                }
                // 子项改不动不算致命：目录本身已经切断继承，改不动的那一项多半
                // 被别的账户占着或落在不支持的卷上。老板的密钥文件仍是旧 ACE，
                // 这一点由调用方按 harden_error 记日志，不拦着客户端启动。
                last_error = Some(io::Error::from_raw_os_error(rc as i32));
            }
        }
    }
    // 目录本身改成功、个别子项没改成功时仍然返回 Err：权限确实没收全，调用方
    // 要把它说出来。但那是「记一条日志」的事，不是「起不来」的事。
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
