//! DNS 解析超时：给 ureq 的默认 `ToSocketAddrs` 解析套一个硬上限。
//!
//! 为什么需要它：ureq 文档明确写了 DNS 解析**不可中断**（`stream.rs` 里那句
//! `TODO: Find a way to apply deadline to DNS lookup.`），`timeout` / `timeout_connect`
//! 只覆盖 TCP 连接与读写，**不覆盖解析**。校园网/运营商对境外域名做 UDP/53 静默丢包时，
//! 系统解析器会按自己的重试节奏卡住几十秒到几分钟，表现为同步一直停在「同步中」、
//! `sync-state.json` 的 `last_sync_at` 几小时不更新——上层那套 5 次退避重试根本没机会开始。
//!
//! 做法：把 `netloc.to_socket_addrs()` 丢到独立线程跑，主线程用
//! `mpsc::recv_timeout` 等最多 [`DNS_TIMEOUT`]。超时立刻返回
//! `io::ErrorKind::TimedOut`，ureq 会把它包成 `ErrorKind::Dns`，
//! 再经 [`super::transport_error`] 映射成**可重试**的 `Network` + 「域名解析失败（DNS）」文案。
//!
//! **线程泄漏是有意为之**：`std::thread` 无法被 kill，超时后那个仍在阻塞的解析线程会
//! 自己跑完（或永远卡在 `getaddrinfo`）再退出。它只在 DNS 丢包时出现，且一轮同步最多
//! 重试 5 次，也就是最多 5 个短命线程，代价可以忽略；换来的是"不会无限卡在同步中"。

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// DNS 解析上限 3s：正常网络下解析是毫秒级，3s 已经足够宽容；
/// 超过它基本就是丢包/污染，再等也只是把「同步中」拖长。
pub(crate) const DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// 带超时的 [`ureq::Resolver`] 实现。
///
/// 注意解析的是 **`netloc`（`host:port` / `[::1]:443`）而不是裸主机名**：
/// ureq 传入的就是这个形状，直接交给 `ToSocketAddrs` 即可。
/// 走代理时 ureq 传的是**代理服务器**的 `host:port`（通常 `127.0.0.1:7890`），
/// IP 字面量由 `to_socket_addrs` 立即返回，不会产生额外延迟。
pub struct TimeoutResolver {
    timeout: Duration,
}

impl TimeoutResolver {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl ureq::Resolver for TimeoutResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        // 线程需要 'static：把 netloc 拷进去（`&str` 不能跨线程借用）。
        let netloc = netloc.to_string();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            // 主线程超时后 rx 已被 drop，send 会失败——忽略即可，线程自行结束。
            let _ = tx.send(netloc.to_socket_addrs().map(|iter| iter.collect::<Vec<_>>()));
        });

        match rx.recv_timeout(self.timeout) {
            Ok(Ok(addrs)) if !addrs.is_empty() => Ok(addrs),
            // 解析"成功"但一个地址都没有：与 ureq 自身的判定保持一致（它也会报 Dns）。
            Ok(Ok(_)) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "DNS 解析无结果（0 个地址）",
            )),
            // 系统解析器的真实错误（NotFound / 临时失败等）原样上抛，
            // 由 ureq 统一包成 ErrorKind::Dns。
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "DNS 解析超时",
            )),
            // 解析线程 panic 了（不该发生）：当成临时失败，可重试。
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("DNS 解析线程异常退出"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `resolve` 来自 trait，测试里要显式引入。
    use ureq::Resolver;

    #[test]
    fn new_keeps_timeout() {
        let r = TimeoutResolver::new(Duration::from_secs(3));
        assert_eq!(r.timeout, Duration::from_secs(3));
        let r = TimeoutResolver::new(Duration::from_millis(250));
        assert_eq!(r.timeout, Duration::from_millis(250));
    }

    /// IP 字面量：不查 DNS，立即返回，端口保持不变。
    #[test]
    fn resolve_ip_literal_returns_same_addr() {
        let r = TimeoutResolver::new(DNS_TIMEOUT);
        let addrs = r.resolve("127.0.0.1:443").expect("IP 直连必须成功");
        assert_eq!(addrs, vec!["127.0.0.1:443".parse::<SocketAddr>().unwrap()]);
    }

    /// `localhost`：真实走一次解析，验证「正常路径」没被超时包装破坏。
    /// 极少数环境（无 hosts/无解析器）会解析失败，那种情况只要求是 io 错误而不是 panic。
    #[test]
    fn resolve_localhost_returns_at_least_one_addr() {
        let r = TimeoutResolver::new(DNS_TIMEOUT);
        match r.resolve("localhost:443") {
            Ok(addrs) => {
                assert!(!addrs.is_empty(), "localhost 应至少解析出一个地址");
                assert!(addrs.iter().all(|a| a.port() == 443), "{addrs:?}");
            }
            // 环境没有可用的 hosts/解析器：不算回归（本地开发容器常见）。
            Err(e) => assert!(
                matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::TimedOut | io::ErrorKind::Other
                ),
                "意外错误类型: {e:?}"
            ),
        }
    }

    /// 解析失败要如实上抛（不能吞成"空成功"）。
    #[test]
    fn resolve_invalid_netloc_is_error() {
        let r = TimeoutResolver::new(DNS_TIMEOUT);
        // 端口非法 → 立即 io 错误，不涉及网络
        assert!(r.resolve("127.0.0.1:not-a-port").is_err());
        assert!(r.resolve("").is_err());
    }
}
