//! DNS 白名单代理（FR-05 / ADR-006）。
//!
//! UDP DNS 服务器，监听在沙盒网络中的指定 IP:53。
//! 解析请求域名，与白名单比对，允许的转发到上游 DNS，拒绝的返回 NXDOMAIN。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::{Name, RecordType};
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::TokioAsyncResolver;

/// 错误类型。
#[derive(Debug)]
pub enum DnsError {
    Io(std::io::Error),
    Proto(String),
}

impl std::fmt::Display for DnsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DnsError::Io(e) => write!(f, "io: {e}"),
            DnsError::Proto(s) => write!(f, "proto: {s}"),
        }
    }
}

impl std::error::Error for DnsError {}

/// DNS 代理核心。
#[derive(Clone)]
pub struct DnsProxy {
    allowed: Arc<RwLock<HashSet<String>>>,
    upstream: TokioAsyncResolver,
}

impl std::fmt::Debug for DnsProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DnsProxy").finish_non_exhaustive()
    }
}

impl DnsProxy {
    /// 创建 DNS 代理。
    pub fn new(domains: Vec<String>) -> Self {
        let upstream = TokioAsyncResolver::tokio(ResolverConfig::google(), ResolverOpts::default());
        Self {
            allowed: Arc::new(RwLock::new(domains.into_iter().collect())),
            upstream,
        }
    }

    /// 检查域名是否在白名单中。
    pub async fn is_allowed(&self, domain: &str) -> bool {
        let allowed = self.allowed.read().await;
        allowed
            .iter()
            .any(|d| domain == d.as_str() || domain.ends_with(&format!(".{d}")))
    }

    /// 启动 UDP DNS 服务器，监听 addr:53。
    ///
    /// 收到查询后，解析域名、检查白名单、转发或拒绝。
    pub async fn serve(&self, addr: &str) -> Result<(), DnsError> {
        let bind = format!("{addr}:53");
        let sock = Arc::new(UdpSocket::bind(&bind).await.map_err(DnsError::Io)?);
        tracing::info!(bind = %bind, "dns proxy listening");
        let mut buf = [0u8; 4096];

        loop {
            let (len, src) = sock.recv_from(&mut buf).await.map_err(DnsError::Io)?;
            let query = buf[..len].to_vec();
            let proxy = self.clone();
            let sock = sock.clone();

            tokio::spawn(async move {
                if let Err(e) = proxy.handle_query(&sock, src, &query).await {
                    tracing::warn!(from = %src, error = %e, "dns query failed");
                }
            });
        }
    }

    /// 处理单条 DNS 查询。
    async fn handle_query(
        &self,
        sock: &Arc<UdpSocket>,
        src: SocketAddr,
        query: &[u8],
    ) -> Result<(), DnsError> {
        // 解析 DNS 查询
        let request = Message::from_vec(query)
            .map_err(|e| DnsError::Proto(format!("parse query: {e}")))?;

        if request.op_code() != OpCode::Query || request.message_type() != MessageType::Query {
            return Ok(());
        }

        let question = match request.queries().first() {
            Some(q) => q,
            None => return Ok(()),
        };

        let domain = question.name().to_ascii().to_lowercase();
        let qtype = question.query_type();

        // 检查白名单
        if !self.is_allowed(&domain).await {
            tracing::debug!(domain = %domain, from = %src, "dns blocked");
            let nx = Self::make_response(&request, ResponseCode::NXDomain);
            let resp = nx
                .to_vec()
                .map_err(|e| DnsError::Proto(e.to_string()))?;
            sock.send_to(&resp, src).await.map_err(DnsError::Io)?;
            return Ok(());
        }

        tracing::debug!(domain = %domain, qtype = ?qtype, from = %src, "dns allowed, forwarding");

        let resp = self.forward(&request, &domain, qtype).await;
        let bytes = resp.to_vec().map_err(|e| DnsError::Proto(e.to_string()))?;
        sock.send_to(&bytes, src).await.map_err(DnsError::Io)?;
        Ok(())
    }

    /// 转发 DNS 查询到上游解析器。
    async fn forward(&self, request: &Message, domain: &str, qtype: RecordType) -> Message {
        let mut response = Self::make_response(request, ResponseCode::NoError);

        match qtype {
            RecordType::A => {
                if let Ok(addrs) = self.upstream.lookup_ip(domain).await {
                    for addr in addrs.iter() {
                        if addr.is_ipv4() {
                            response.add_answer(hickory_proto::rr::Record::from_rdata(
                                Name::from_ascii(domain).unwrap(),
                                60,
                                hickory_proto::rr::RData::A(addr.into()),
                            ));
                        }
                    }
                } else {
                    response.set_response_code(ResponseCode::NXDomain);
                }
            }
            RecordType::AAAA => {
                if let Ok(addrs) = self.upstream.lookup_ip(domain).await {
                    for addr in addrs.iter() {
                        if addr.is_ipv6() {
                            response.add_answer(hickory_proto::rr::Record::from_rdata(
                                Name::from_ascii(domain).unwrap(),
                                60,
                                hickory_proto::rr::RData::AAAA(addr.into()),
                            ));
                        }
                    }
                } else {
                    response.set_response_code(ResponseCode::NXDomain);
                }
            }
            _ => {}
        }
        response
    }

    /// 构建 DNS 响应头。
    fn make_response(request: &Message, rcode: ResponseCode) -> Message {
        let mut response = Message::new();
        response.set_id(request.id());
        response.set_message_type(MessageType::Response);
        response.set_op_code(OpCode::Query);
        response.set_response_code(rcode);
        response.set_recursion_desired(true);
        response.set_recursion_available(true);
        response.add_queries(request.queries().to_vec());
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_match() {
        let proxy = DnsProxy::new(vec!["pypi.org".into()]);
        assert!(proxy.is_allowed("pypi.org").await);
        assert!(!proxy.is_allowed("google.com").await);
    }

    #[tokio::test]
    async fn subdomain_match() {
        let proxy = DnsProxy::new(vec!["python.org".into()]);
        assert!(proxy.is_allowed("files.python.org").await);
        assert!(!proxy.is_allowed("example.com").await);
    }

    #[tokio::test]
    async fn empty_list_denies_all() {
        let proxy = DnsProxy::new(vec![]);
        assert!(!proxy.is_allowed("anything.com").await);
    }
}