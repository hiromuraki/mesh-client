//! 测试用的假 `easytier-core`。
//!
//! 只做三件事：按参数打印几行、在 `--rpc-portal` 上应答 `ShowNodeInfo`、按配置
//! 选择行为（正常/崩溃/不起 portal/派生一个子进程/**忽略 SIGTERM**）。
//!
//! 行为从 `-c <config>` 那份文件里读（`mode=` 等 key=value 行），不用环境变量：
//! 集成测试并行跑在同一个进程里，`set_var` 会互相串。
//!
//! 这不是生产代码：`client/` 下的 `coreproc` 单测用 `ping`/`sleep` 顶替子进程，
//! 但我们要验证的是“参数拼装、输出泵、就绪探测、进程组回收”，必须有一个能被
//! 托管、又能在指定时刻以指定方式死掉的替身。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config = String::new();
    let mut portal = String::new();
    let mut hostname = String::new();
    let mut secure_mode = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-c" | "--config" | "--config-file" => {
                config = args.get(index + 1).cloned().unwrap_or_default();
                index += 2;
            }
            "--rpc-portal" => {
                portal = args.get(index + 1).cloned().unwrap_or_default();
                index += 2;
            }
            "--hostname" => {
                hostname = args.get(index + 1).cloned().unwrap_or_default();
                index += 2;
            }
            "--rpc-portal-whitelist" => index += 2,
            "--secure-mode" => {
                secure_mode = true;
                index += 1;
            }
            _ => index += 1,
        }
    }

    let settings = read_settings(&config);
    let mode = settings
        .get("mode")
        .cloned()
        .unwrap_or_else(|| "ok".to_string());
    if let Some(marker) = settings.get("marker") {
        let _ = std::fs::write(marker, std::process::id().to_string());
    }
    // 打印收到的关键参数：测试据此断言“参数拼装真的生效了”。
    println!("fake-core booting mode={mode}");
    println!("config={config}");
    println!("portal={portal}");
    println!("hostname={hostname} secure_mode={secure_mode}");
    println!("credential_env={}", std::env::var("ET_CREDENTIAL").is_ok());
    println!(
        "private_key_env={}",
        std::env::var("ET_LOCAL_PRIVATE_KEY").is_ok()
    );

    match mode.as_str() {
        "crash" => {
            eprintln!("fatal: fake core crashing");
            std::process::exit(3);
        }
        "lines" => {
            println!("line one");
            // 行尾故意留三个空格：托管方应当把它们去掉。
            println!("trailing spaces   ");
            println!();
            eprintln!("line two");
        }
        "child" => {
            spawn_child();
        }
        "ignore-term" => ignore_term(),
        _ => {}
    }

    if mode == "no-portal" {
        println!("not starting the rpc portal");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    // 其余模式都起 portal：只有应答了 ShowNodeInfo，托管方才会认为“就绪”。
    let Ok(listener) = TcpListener::bind(&portal) else {
        eprintln!("cannot bind portal {portal}");
        std::process::exit(4);
    };
    println!("portal listening on {portal}");
    let reported_hostname = settings
        .get("report_hostname")
        .cloned()
        .unwrap_or_else(|| hostname.clone());
    let ipv4 = settings
        .get("ipv4")
        .cloned()
        .unwrap_or_else(|| "10.126.126.100/24".to_string());
    let exit_after_first = mode == "exit-after-start";
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        if read_request(&mut stream).is_none() {
            continue;
        }
        let _ = stream.write_all(&response_frame(&reported_hostname, &ipv4));
        let _ = stream.flush();
        if exit_after_first {
            std::thread::sleep(Duration::from_millis(300));
            std::process::exit(7);
        }
    }
}

fn read_settings(path: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            // 配置文件按 TOML 写（`ipv4 = "10.0.0.1/24"`），值两边的引号要剥掉。
            let value = value.trim().trim_matches('"').to_string();
            out.insert(key.trim().to_string(), value);
        }
    }
    out
}

fn spawn_child() {
    #[cfg(windows)]
    let child = std::process::Command::new("ping")
        .args(["-t", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn();
    #[cfg(not(windows))]
    let child = std::process::Command::new("sleep").arg("300").spawn();
    match child {
        Ok(child) => println!("child {}", child.id()),
        Err(err) => eprintln!("cannot spawn child: {err}"),
    }
}

#[cfg(unix)]
fn ignore_term() {
    // 用来验证“软停失败就立刻强杀”：这一版会无视 SIGTERM。
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
}

#[cfg(not(unix))]
fn ignore_term() {}

/// 读掉一个请求帧（4B 长度 + 16B 头 + 载荷），返回载荷长度。
fn read_request(stream: &mut std::net::TcpStream) -> Option<usize> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).ok()?;
    let total = u32::from_le_bytes(head) as usize;
    let mut rest = vec![0u8; total];
    stream.read_exact(&mut rest).ok()?;
    Some(total)
}

fn response_frame(hostname: &str, ipv4: &str) -> Vec<u8> {
    let mut node = Vec::new();
    put_uint(&mut node, 1, 42); // peer_id
    put_str(&mut node, 2, ipv4);
    put_str(&mut node, 4, hostname);
    put_str(&mut node, 9, "2.7.1-fake");

    let mut show = Vec::new();
    put_bytes(&mut show, 1, &node);
    let mut response = Vec::new();
    put_bytes(&mut response, 1, &show);
    let mut packet = Vec::new();
    put_uint(&mut packet, 3, 1); // transaction_id
    put_bytes(&mut packet, 5, &response);

    let mut frame = Vec::new();
    frame.extend_from_slice(&((16 + packet.len()) as u32).to_le_bytes());
    frame.extend_from_slice(&1u32.to_le_bytes()); // from_peer
    frame.extend_from_slice(&1u32.to_le_bytes()); // to_peer
    frame.push(9); // packet_type = rpc response
    frame.extend_from_slice(&[0, 0, 0]);
    frame.extend_from_slice(&(packet.len() as u32).to_le_bytes());
    frame.extend_from_slice(&packet);
    frame
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_uint(out: &mut Vec<u8>, num: u32, value: u64) {
    if value == 0 {
        return;
    }
    put_varint(out, u64::from(num) << 3);
    put_varint(out, value);
}

fn put_bytes(out: &mut Vec<u8>, num: u32, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    put_varint(out, (u64::from(num) << 3) | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_str(out: &mut Vec<u8>, num: u32, text: &str) {
    put_bytes(out, num, text.as_bytes());
}
