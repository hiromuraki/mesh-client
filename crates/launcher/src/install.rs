use crate::args::Source;
use crate::launch;
use crate::log;
use mesh_install::failures::Failures;
use mesh_install::layout;
use mesh_install::manifest::Manifest;
use mesh_install::signing;
use mesh_install::version;
use mesh_text::id;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// BIN_OLD 与 UPDATE_FILE 的形状归 mesh-install::layout 管：客户端也要用同一套
// 判据（下包前看失败计数、退出前留 update.tar.zst），各写一份迟早分叉。
// 这里再导出一次，supervisor 与 install 内部原有的 install::UPDATE_FILE 写法不必动。
//
// BIN_OLD 是上一个版本解包后的样子，只在一次安装的窗口里存在：装之前把 bin
// 改名到这里，启动确认成功后删掉，失败就改回来。所以「bin.old 存在」就等于
// 「上次安装没走完」——断电、解包到一半被杀、启动确认超时都落进同一个判据。
pub(crate) use mesh_install::layout::{BIN_OLD, UPDATE_FILE};
/// WAIT_CLIENT 是“等旧客户端退出”的上限，与客户端调用时的预期一致。
const WAIT_CLIENT: Duration = Duration::from_secs(60);
/// WAIT_BOOT 是“等新版确认启动”的上限。客户端在引擎与面板起来后立刻写标记，
/// 不等网络，所以正常情况下是秒级。
pub(crate) const WAIT_BOOT: Duration = Duration::from_secs(60);

/// run 是手动安装入口（--file / --url）。
///
/// Windows 上装完接着拉起新版本（客户端已经退出，没人启动了）；Linux 上装完就
/// 返回，由 systemd 拉起常驻启动器，否则手动装的这一份会和它抢同一个虚拟 IP。
pub fn run(
    root: &Path,
    source: Source,
    want_sha: Option<String>,
    wait_for: Option<u32>,
    force: bool,
) -> Result<(), String> {
    // 上次安装没走完（bin.old 还在）就先回滚。手动安装与启动路径必须同一个前提：
    // 不能拿唯一的回滚材料去换一份还没验过的新包。
    recover(root)?;
    if let Some(pid) = wait_for {
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_WAIT_PROCESS, &[&pid]));
        launch::wait_exit(pid, WAIT_CLIENT)?;
    }

    let pkg = match source {
        Source::File(p) => p,
        Source::Url(url) => {
            let dest = root.join(UPDATE_FILE);
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_DOWNLOAD, &[&url]));
            download(&url, &dest)?;
            dest
        }
    };

    // 这一条只防“下载完成到安装之间”包被换掉，与解包结果无关。
    let sum = sha256_file(&pkg)?;
    // 只有自己下载到 <root>/update.tar.zst 的那份才删：--file 给的是用户的文件，
    // 同样的路径成功安装时反而保留，两种结果不该由失败与否决定。
    let owned = pkg == root.join(UPDATE_FILE);
    if let Some(want) = want_sha {
        if !sum.eq_ignore_ascii_case(&want) {
            if owned {
                let _ = fs::remove_file(&pkg);
            }
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_DIGEST_MISMATCH,
                &[&sum, &want],
            ));
        }
    }
    log::line(&mesh_text::format(
        id::LAUNCHER_TEXT_PACKAGE_DIGEST,
        &[&sum],
    ));

    if let Err(e) = install_from_file(root, &pkg, force) {
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_INSTALL_FAILED, &[&e]));
        if owned {
            let _ = fs::remove_file(&pkg);
        }
        // 安装失败时 bin/ 已经被换回原来那一套。Windows 上客户端早已退出，
        // 必须把它拉回来——否则节点就此静默离线，直到有人去双击 UAC。
        #[cfg(windows)]
        {
            reload_current(root).map_err(|reload| {
                mesh_text::format(id::LAUNCHER_ERROR_INSTALL_AND_RELOAD, &[&e, &reload])
            })?;
            log::line(mesh_text::get(id::LAUNCHER_TEXT_RELOADED));
            return Ok(());
        }
        #[cfg(unix)]
        return Err(e);
    }

    #[cfg(windows)]
    {
        launch_and_check(root)?;
    }
    #[cfg(unix)]
    {
        log::line(mesh_text::get(id::LAUNCHER_TEXT_INSTALLED_FOR_SYSTEMD));
    }
    Ok(())
}

/// recover 在启动任何东西之前调用：bin.old 还在就说明上次安装没走完，
/// 无条件回到那一套。宁可退一版，也不要拿一份可能残缺的 bin 去启动。
pub(crate) fn recover(root: &Path) -> Result<(), String> {
    let bin = root.join("bin");
    let old = root.join(BIN_OLD);
    if !old.exists() {
        return Ok(());
    }
    log::line(mesh_text::get(id::LAUNCHER_TEXT_RECOVER_STALE));
    let _ = fs::remove_dir_all(&bin);
    fs::rename(&old, &bin).map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_ROLLBACK, &[&e]))?;
    Ok(())
}

/// install_from_file 把包装进 bin/：先把现有 bin 改名到 bin.old，再解包校验。
///
/// 失败时把 bin 删掉、把 bin.old 改回来——调用方看到的永远是「旧的」或「新的」，
/// 不会是一半。成功时 bin.old 留着，等启动确认之后再删。
pub(crate) fn install_from_file(
    root: &Path,
    pkg: &Path,
    force: bool,
) -> Result<Option<String>, String> {
    let bin = root.join("bin");
    let old_version = installed_version(&bin);

    let manifest = read_manifest(pkg)?;
    // 验签放在动 bin/ 之前：坏包不该换掉唯一的回滚材料，也不该走到「装完才发现
    // 起不来」那一步。降级检查与失败计数都在后面，因为它们只读、给出的错误更具体。
    check_manifest_signature(&manifest, signing::RELEASE_PUBLIC_KEY)?;
    log::line(&mesh_text::format(
        id::LAUNCHER_TEXT_INSTALLING,
        &[&manifest.version, &manifest.files.len()],
    ));

    // 降级防护：服务端把发布记录指回旧包时，客户端会当成“有新版本”下载下来。
    // 2026-11-26 两个种子就是这么被降级到 -3 的。 --force 留给手动修机器。
    if !force {
        if let Some(old) = old_version.as_deref() {
            if !version::is_newer(&manifest.version, old) {
                return Err(mesh_text::format(
                    id::LAUNCHER_ERROR_NOT_NEWER,
                    &[&manifest.version, &old],
                ));
            }
        }
        let failed = Failures::load(root);
        if failed.exhausted(&manifest.version) {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_FAILURES_EXHAUSTED,
                &[&manifest.version, &failed.count(&manifest.version)],
            ));
        }
    }

    // recover() 已在入口把残留的 bin.old 回滚回 bin/：这里再看到它，说明有人跳过了
    // 那一步。宁可失败，也不要删掉唯一的回滚材料。
    let old = root.join(BIN_OLD);
    if old.exists() {
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_BIN_OLD_REMAINS,
            &[&old.display()],
        ));
    }
    if bin.exists() {
        fs::rename(&bin, &old)
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_MOVE_OLD_BIN, &[&e]))?;
    }
    if let Err(e) = fs::create_dir_all(&bin) {
        restore(root);
        return Err(mesh_text::format(id::LAUNCHER_ERROR_CREATE_BIN, &[&e]));
    }

    if let Err(e) = unpack(pkg, &bin).and_then(|_| verify(&bin, &manifest)) {
        restore(root);
        // 装不上就是一次失败：不记下来的话客户端每轮都会重新下一遍、再试一遍。
        Failures::load(root).record(&manifest.version);
        return Err(e);
    }
    log::line(mesh_text::get(id::LAUNCHER_TEXT_VERIFIED));
    Ok(old_version)
}

/// commit 在启动确认成功之后调用：这一版验过了，旧的那份可以扔了。
pub(crate) fn commit(root: &Path) {
    let _ = fs::remove_dir_all(root.join(BIN_OLD));
    let _ = fs::remove_file(root.join(UPDATE_FILE));
}

/// restore 把 bin 删掉、bin.old 改回来。没有 bin.old 时什么都不做（首次安装失败）。
pub(crate) fn restore(root: &Path) {
    let bin = root.join("bin");
    let old = root.join(BIN_OLD);
    if !old.exists() {
        let _ = fs::remove_dir_all(&bin);
        return;
    }
    let _ = fs::remove_dir_all(&bin);
    if let Err(e) = fs::rename(&old, &bin) {
        log::line(&mesh_text::format(id::LAUNCHER_ERROR_ROLLBACK, &[&e]));
    }
}

/// launch_and_check 拉起 bin/ 里的版本并等它确认启动，失败就回到 bin.old。
/// 只有 Windows 用：Linux 的启动器是常驻的，等确认与回退都在 supervisor 里。
#[cfg(windows)]
fn launch_and_check(root: &Path) -> Result<(), String> {
    let bin = root.join("bin");
    let version = installed_version(&bin).unwrap_or_default();
    let token = token();
    launch::start(&bin.join(launch::CLIENT_EXE), root, Some(&token))?;
    log::line(mesh_text::get(id::LAUNCHER_TEXT_STARTED_NEW));

    if wait_boot_ok(root, &token, WAIT_BOOT) {
        log::line(mesh_text::get(id::LAUNCHER_TEXT_BOOT_OK));
        Failures::load(root).clear(&version);
        commit(root);
        return Ok(());
    }

    log::line(mesh_text::get(id::LAUNCHER_TEXT_BOOT_FAILED_ROLLBACK));
    Failures::load(root).record(&version);
    restore(root);
    reload_current(root).map_err(|r| mesh_text::format(id::LAUNCHER_ERROR_RELOAD_OLD, &[&r]))
}

/// reload_current 拉起 bin/ 里现有的版本（不等启动确认）。
#[cfg(windows)]
fn reload_current(root: &Path) -> Result<(), String> {
    let exe = root.join("bin").join(launch::CLIENT_EXE);
    if !exe.is_file() {
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_EXE_MISSING,
            &[&exe.display()],
        ));
    }
    launch::start(&exe, root, None)?;
    Ok(())
}

/// installed_version 读当前 bin/version.json 的版本号；还没有安装过就是 None。
///
/// 这里只取版本号、不校验字符集：值不合法时按“没有已安装版本”处理，
/// 与原来 serde 直接解析的行为一致（字符集校验在 layout::read_version 里，
/// 那是给“当前版本”这个判据用的）。
pub(crate) fn installed_version(bin: &Path) -> Option<String> {
    let text = fs::read_to_string(bin.join(layout::MANIFEST)).ok()?;
    Manifest::parse(&text).ok().map(|manifest| manifest.version)
}

/// package_version 只读包里的版本号，给常驻启动器做“这包还该不该装”的预检查用。
#[cfg(unix)]
pub(crate) fn package_version(pkg: &Path) -> Result<String, String> {
    Ok(read_manifest(pkg)?.version)
}

fn read_manifest(pkg: &Path) -> Result<Manifest, String> {
    let file = File::open(pkg)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&pkg.display(), &e]))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| {
        mesh_text::format(id::LAUNCHER_ERROR_DECOMPRESS_PACKAGE, &[&pkg.display(), &e])
    })?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE, &[&e]))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE_ENTRY, &[&e]))?;
        let path = entry
            .path()
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_ENTRY_NAME, &[&e]))?;
        if path.as_os_str() == "version.json" {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_VERSION_JSON, &[&e]))?;
            return Manifest::parse(&text)
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_PARSE_VERSION_JSON, &[&e]));
        }
    }
    Err(mesh_text::get(id::LAUNCHER_ERROR_NO_VERSION_JSON).to_string())
}

fn unpack(pkg: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(pkg)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&pkg.display(), &e]))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| {
        mesh_text::format(id::LAUNCHER_ERROR_DECOMPRESS_PACKAGE, &[&pkg.display(), &e])
    })?;
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_UNPACK, &[&dest.display(), &e]))
}

/// verify_current 在拉起客户端之前校验 `bin/` 与它自带的清单一致。
///
/// 两道闸，顺序有意义：先验清单的签名（谁能改 `bin/` 里的文件，却改不出发布私钥
/// 的签名），再逐文件比对摘要（解包只写了一半、磁盘坏了一块、有人换了二进制却
/// 没换清单，都在这里被拦住）。11 MiB 的包实测约 30 ms，与「加载一个 DLL」同一个量级。
pub fn verify_current(root: &Path) -> Result<(), String> {
    verify_current_signed_by(root, signing::RELEASE_PUBLIC_KEY)
}

/// 用给定公钥验当前 `bin/`。轮换公钥与测试走这一条。
pub fn verify_current_signed_by(root: &Path, public_hex: &str) -> Result<(), String> {
    let manifest = mesh_install::manifest::read(&layout::manifest_path(root))
        .map_err(|err| err.to_string())?;
    check_manifest_signature(&manifest, public_hex)?;
    verify(&root.join(layout::BIN), &manifest)
}

/// 清单必须由发布私钥签出。
///
/// 文件摘要只证明「这份 bin 与清单一致」：清单自己也是可以写在 `bin/` 里的一个
/// 文件，被换掉时那条判断就没有意义了。空签名单独报一条，因为它的成因不同
/// （手工造的包，而不是被改过的包）。
fn check_manifest_signature(manifest: &Manifest, public_hex: &str) -> Result<(), String> {
    if manifest.signature.is_empty() {
        return Err(mesh_text::get(id::LAUNCHER_ERROR_MANIFEST_UNSIGNED).to_string());
    }
    if !manifest.verify_with(public_hex) {
        return Err(mesh_text::get(id::LAUNCHER_ERROR_MANIFEST_SIGNATURE_INVALID).to_string());
    }
    Ok(())
}

/// verify 只认清单里列出的文件：多出来的忽略，少了或对不上就判包不可用。
fn verify(bin: &Path, manifest: &Manifest) -> Result<(), String> {
    for (name, want) in &manifest.files {
        if name.contains("..") || Path::new(name).is_absolute() {
            return Err(mesh_text::format(id::LAUNCHER_ERROR_ILLEGAL_PATH, &[&name]));
        }
        let path = bin.join(name);
        let got = sha256_file(&path)
            .map_err(|_| mesh_text::format(id::LAUNCHER_ERROR_MISSING_FILE, &[&name]))?;
        if !got.eq_ignore_ascii_case(want) {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_FILE_DIGEST_MISMATCH,
                &[&name],
            ));
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&path.display(), &e]))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_FILE, &[&path.display(), &e]))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// boot_marker 是客户端确认启动后写的文件。token 由启动器生成，保证“上一次启动
/// 留下的标记”不会被这一次误认。
pub(crate) fn boot_marker(root: &Path, token: &str) -> PathBuf {
    root.join(format!(".boot-ok-{token}"))
}

#[cfg(windows)]
fn wait_boot_ok(root: &Path, token: &str, timeout: Duration) -> bool {
    let marker = boot_marker(root, token);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if marker.is_file() {
            let _ = fs::remove_file(&marker);
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

pub(crate) fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos & 0xffff_ffff)
}

/// download 只在 --url 时用；Linux 版走 system curl：静态 musl 下把 openssl
/// 链进二进制不值得，而 Linux 上 curl 是标配。
#[cfg(windows)]
fn download(url: &str, dest: &Path) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_DOWNLOAD, &[&e]))?;
    let mut reader = response.into_reader();
    let mut file = File::create(dest)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_CREATE_FILE, &[&dest.display(), &e]))?;
    std::io::copy(&mut reader, &mut file)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_WRITE_FILE, &[&dest.display(), &e]))?;
    Ok(())
}

#[cfg(unix)]
fn download(url: &str, dest: &Path) -> Result<(), String> {
    let status = std::process::Command::new("curl")
        .args(["-fL", "--connect-timeout", "20", "--retry", "2"])
        .arg("-o")
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_CALL_CURL, &[&e]))?;
    if !status.success() {
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_DOWNLOAD_CURL,
            &[&status],
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mesh-launcher-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个 tar.zst 包：根上是清单，`entries` 是包里的文件。
    fn package(dir: &Path, manifest: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("pkg.tar.zst");
        let file = fs::File::create(&path).unwrap();
        let encoder = zstd::Encoder::new(file, 3).unwrap();
        let mut builder = tar::Builder::new(encoder);
        append(&mut builder, "version.json", manifest.as_bytes());
        for (name, body) in entries {
            append(&mut builder, name, body);
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        path
    }

    fn append<W: std::io::Write>(builder: &mut tar::Builder<W>, name: &str, body: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, body).unwrap();
    }

    /// 清单声明的摘要与实际内容不符：解包成功、校验失败。
    fn broken_package(dir: &Path) -> PathBuf {
        package(
            dir,
            r#"{"version":"2099.01.01-1","files":{"mesh-client.exe":"00"}}"#,
            &[("mesh-client.exe", b"not the bytes the manifest claims")],
        )
    }

    fn test_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[5u8; 32])
    }

    fn public_hex(key: &ed25519_dalek::SigningKey) -> String {
        hex(&key.verifying_key().to_bytes())
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex(&hasher.finalize())
    }

    /// 造一份签名正确的清单：摘要按文件内容算，签的是清单摘要。
    fn signed_manifest(
        key: &ed25519_dalek::SigningKey,
        version: &str,
        entries: &[(&str, &[u8])],
    ) -> String {
        use ed25519_dalek::Signer;
        let files: BTreeMap<String, String> = entries
            .iter()
            .map(|(name, body)| (name.to_string(), sha256_bytes(body)))
            .collect();
        let message = signing::manifest_string(
            signing::SLOT_CLIENT,
            "windows/amd64",
            version,
            &signing::files_digest(&files),
        );
        let signature = hex(&key.sign(message.as_bytes()).to_bytes());
        let pairs = files
            .iter()
            .map(|(name, sha)| format!("{name:?}:{sha:?}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"version":"{version}","slot":"client","platform":"windows/amd64","files":{{{pairs}}},"signature":"{signature}"}}"#
        )
    }

    /// 安装失败不能删用户给的文件：--file 收的是用户自己的包，客户端交接时那个
    /// 路径（<root>/update.tar.zst）才是下载下来的、可以删的那一份。
    #[test]
    fn a_tampered_bin_is_refused_before_launch() {
        let root = temp_root("verify-current");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("mesh-client.exe"), b"client").unwrap();
        let key = test_key();
        std::fs::write(
            layout::manifest_path(&root),
            signed_manifest(&key, "2026.11.27-1", &[("mesh-client.exe", b"client")]),
        )
        .unwrap();
        let public = public_hex(&key);

        assert!(
            verify_current_signed_by(&root, &public).is_ok(),
            "一份完整且签过名的 bin 应当通过"
        );
        // 换了二进制却没换清单：这正是「有人动过安装根」的形态。
        std::fs::write(bin.join("mesh-client.exe"), b"evil").unwrap();
        assert!(
            verify_current_signed_by(&root, &public).is_err(),
            "换掉 bin 里的文件就必须拒绝启动"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 清单与 bin/ 完全一致，但签名不是这个公钥签的：文件摘要那一关会全过，
    /// 只有验签能拦住——这正是加签名的理由。
    #[test]
    fn a_manifest_signed_by_another_key_is_refused() {
        let root = temp_root("verify-other-key");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("mesh-client.exe"), b"client").unwrap();
        let key = test_key();
        std::fs::write(
            layout::manifest_path(&root),
            signed_manifest(&key, "2026.11.27-1", &[("mesh-client.exe", b"client")]),
        )
        .unwrap();

        let stranger = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        assert!(
            verify_current_signed_by(&root, &public_hex(&stranger)).is_err(),
            "别的密钥签出来的清单不能过"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 没有签名的包在动 bin/ 之前就被拒：否则会先换掉唯一的回滚材料，再发现装不了。
    #[test]
    fn a_package_without_a_signature_is_refused_before_touching_bin() {
        let root = temp_root("unsigned-pkg");
        let src = temp_root("unsigned-pkg-src");
        std::fs::create_dir_all(root.join(layout::BIN)).unwrap();
        std::fs::write(root.join(layout::BIN).join("KEEP"), b"keep").unwrap();
        let pkg = package(
            &src,
            r#"{"version":"2099.01.01-1","slot":"client","platform":"windows/amd64","files":{},"signature":""}"#,
            &[],
        );

        assert!(
            install_from_file(&root, &pkg, true).is_err(),
            "空签名必须拒绝"
        );
        assert!(
            root.join(layout::BIN).join("KEEP").is_file(),
            "验签失败不该动 bin/"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn a_bin_without_a_manifest_is_refused() {
        let root = temp_root("verify-current-missing");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        assert!(verify_current(&root).is_err(), "没有清单就不能启动");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_manual_install_keeps_the_package_the_user_gave() {
        let root = temp_root("keep-package");
        let src = temp_root("keep-package-src");
        let pkg = broken_package(&src);

        assert!(
            run(&root, Source::File(pkg.clone()), None, None, true).is_err(),
            "坏包应当安装失败"
        );
        assert!(pkg.is_file(), "用户给的包被删掉了: {}", pkg.display());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 上次安装没走完时，残留的 bin.old 是唯一的回滚材料：手动安装必须先回滚它，
    /// 而不是把它删掉再拿坏 bin 顶上。
    #[test]
    fn a_manual_install_rolls_a_stale_bin_old_back_first() {
        let root = temp_root("stale-bin-old");
        let src = temp_root("stale-bin-old-src");
        fs::create_dir_all(root.join("bin.old")).unwrap();
        fs::write(root.join("bin.old").join("GOOD"), b"good").unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin").join("BROKEN"), b"broken").unwrap();
        let pkg = broken_package(&src);

        let _ = run(&root, Source::File(pkg), None, None, true);

        assert!(
            root.join("bin").join("GOOD").is_file(),
            "残留的 bin.old 没有先回滚：回滚材料被坏 bin 顶掉了"
        );
        assert!(!root.join("bin.old").exists(), "回滚之后不该再有 bin.old");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }
}
