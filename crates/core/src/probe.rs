//! 就绪探测：判断“core 真的起来了”，而不是“进程还在”。
//!
//! 与 Go 的 `engine.waitReady` 一致：每 500ms 查一次 `ShowNodeInfo`，最多 25s；
//! 每次查询单独 3s 超时（core 刚起来时 portal 可能还没开始监听）。判据见
//! [`crate::Expect`]。

use crate::{Expect, NodeInfo, Options};
use std::net::SocketAddr;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

pub(crate) enum Outcome {
    Ready(NodeInfo),
    /// 探测期间进程就退了；退出状态在 `try_wait` 里已经被回收，不能再 wait。
    Exited(Option<ExitStatus>),
    /// 到时限还没确认就绪，`reason` 是最后一次失败的原因。
    Timeout(String),
}

pub(crate) fn wait_ready(
    child: &mut Child,
    rpc: SocketAddr,
    expect: &Expect,
    options: &Options,
) -> Outcome {
    let deadline = Instant::now() + options.ready_timeout;
    let mut last = None;
    loop {
        // 时限判断放在最前面：这样 `last` 的初始值才会被读到（也才是“时限已到’
        // 那条路径的唯一出口）。
        let now = Instant::now();
        if now >= deadline {
            return Outcome::Timeout(last.unwrap_or_else(|| "超时".to_string()));
        }
        // 进程先退出就不必等满时限：装上就崩是最常见的一种失败。
        match child.try_wait() {
            Ok(Some(status)) => return Outcome::Exited(Some(status)),
            Ok(None) => {}
            Err(err) => {
                return Outcome::Timeout(format!("等待 core 进程失败: {err}"));
            }
        }
        match mesh_etrpc::show_node_info(rpc, options.probe_timeout) {
            Ok(node) => match check(&node, expect) {
                Ok(()) => return Outcome::Ready(node),
                Err(why) => last = Some(why),
            },
            Err(err) => last = Some(err.to_string()),
        }
        // 睡到下一次探测，但不要越过时限。
        let sleep = options.ready_interval.min(deadline - Instant::now());
        std::thread::sleep(sleep);
    }
}

/// “这份节点信息算不算我们要的 core”，与 Go 的 `identityMatches` 逐条对应。
pub(crate) fn check(node: &NodeInfo, expect: &Expect) -> Result<(), String> {
    if !expect.hostname.is_empty() {
        if node.hostname != expect.hostname {
            return Err(format!(
                "core 返回的节点与本机配置不匹配 (hostname={} ip={})",
                node.hostname, node.ipv4
            ));
        }
    } else if !expect.ipv4.is_empty() && !node.ipv4.starts_with(&expect.ipv4) {
        return Err(format!(
            "core 返回的节点与本机配置不匹配 (hostname={} ip={})",
            node.hostname, node.ipv4
        ));
    }
    // 地址还没分配下来（刚起来的 core 常见）：这时候定格会把“未连接”钉死在
    // 快照里，之后再没人改写，而 core 其实早就通了。继续等下一轮。
    if node.ipv4.is_empty() {
        return Err("节点已上线，等待分配虚拟 IP".to_string());
    }
    Ok(())
}

/// 把探测时限拼成人话，供 `StartFailed` 的 reason 用。
pub(crate) fn timeout_message(timeout: Duration, reason: &str) -> String {
    format!("{} 秒内未确认 core 就绪: {reason}", timeout.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_etrpc::StunInfo;

    fn node(hostname: &str, ipv4: &str) -> NodeInfo {
        NodeInfo {
            peer_id: 1,
            ipv4: ipv4.to_string(),
            hostname: hostname.to_string(),
            inst_id: String::new(),
            version: "2.7.1".to_string(),
            stun: StunInfo::default(),
        }
    }

    #[test]
    fn hostname_wins_when_it_is_configured() {
        let expect = Expect {
            hostname: "laptop".into(),
            ipv4: "10.0.0.9/24".into(),
        };
        assert!(check(&node("laptop", "10.0.0.1/24"), &expect).is_ok());
        let err = check(&node("other", "10.0.0.1/24"), &expect).unwrap_err();
        assert!(err.contains("不匹配"), "err = {err}");
    }

    #[test]
    fn ipv4_is_the_fallback_when_hostname_is_absent() {
        let expect = Expect {
            hostname: String::new(),
            ipv4: "10.0.0.9".into(),
        };
        assert!(check(&node("anything", "10.0.0.9/24"), &expect).is_ok());
        assert!(check(&node("anything", "10.0.0.10/24"), &expect).is_err());
    }

    #[test]
    fn an_empty_address_is_not_ready_yet() {
        // 地址没分配下来时不能算就绪：否则快照会一直显示未连接。
        let expect = Expect::default();
        assert!(check(&node("laptop", ""), &expect).is_err());
    }
}
