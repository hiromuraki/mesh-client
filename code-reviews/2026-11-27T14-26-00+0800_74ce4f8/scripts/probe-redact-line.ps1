# 把 crates/engine/src/redact.rs 原样复制进一个临时 crate，直接跑 line()，
# 用来看脱敏后的真实字节（评审时用来核对「JSON 值整段替换、引号配平」）。
#
# 只写 %TEMP%，不改动仓库里的任何文件。
$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path
$src = Join-Path $repo "crates\engine\src\redact.rs"
if (-not (Test-Path $src)) { throw "找不到 $src" }

$dir = Join-Path ([System.IO.Path]::GetTempPath()) ("mesh-redact-probe-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path (Join-Path $dir "src") | Out-Null
Copy-Item $src (Join-Path $dir "src\redact.rs")

@'
mod redact;

fn main() {
    for text in [
        r#"{{"credential_secret":"xyz","n":1}}"#,
        r#"network_secret = "abc123""#,
        "ET_CREDENTIAL=deadbeef",
        "连接失败: token = abc，稍后重试",
        r#"secret = "abc"#,
    ] {
        println!("in : {text}");
        println!("out: {}", redact::line(text));
    }
}
'@ | Set-Content -Path (Join-Path $dir "src\main.rs") -Encoding utf8

@'
[package]
name = "mesh-redact-probe"
version = "0.1.0"
edition = "2021"

[workspace]

[[bin]]
name = "mesh-redact-probe"
path = "src/main.rs"
'@ | Set-Content -Path (Join-Path $dir "Cargo.toml") -Encoding utf8

$env:Path = "C:\Users\11717\.cargo\bin;" + $env:Path
Push-Location $dir
try { cargo run -q } finally { Pop-Location }
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
