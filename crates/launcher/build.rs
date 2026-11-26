// 编译 Windows 资源：图标。启动器自己是 asInvoker，不需要额外 manifest。
//
// Linux 目标下 winresource 不在依赖里（Cargo.toml 按 target 分），所以整个资源
// 编译必须跟着 cfg 一起消失；判断也写在 cfg 块里，别让非 Windows 的 main 以
// `return` 收尾——clippy 会判它 needless_return，而 CI 是 -D warnings。
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    {
        // 只有 release 产物需要资源：把 manifest 嵌进测试二进制会让它在 SxS 里
        // 启动失败（测试进程没有对应的 assembly 上下文）。
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
            && std::env::var("PROFILE").as_deref() == Ok("release")
        {
            compile_resources();
        }
    }
}

#[cfg(windows)]
fn compile_resources() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("FileDescription", "Mesh");
    res.set("ProductName", "Mesh");
    res.set("OriginalFilename", "mesh.exe");
    if let Err(e) = res.compile() {
        panic!("编译 Windows 资源失败: {e}");
    }
}
