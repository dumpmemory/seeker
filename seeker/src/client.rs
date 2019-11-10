use async_std::io::copy;
use async_std::net::{TcpStream, UdpSocket};
use async_std::task::JoinHandle;
use async_std::{future, task};
use config::rule::{Action, ProxyRules};
use config::{Address, Config};
use futures::io::Error;
use hermesdns::{DnsClient, DnsNetworkClient, QueryType};
use ssclient::SSClient;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::io::Result;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use sysconfig::{list_user_proc_socks, SocketInfo};
use tracing::{trace, trace_span};
use tracing_futures::Instrument;
use tun::socket::{TunTcpSocket, TunUdpSocket};

#[async_trait::async_trait]
pub trait Client {
    async fn handle_tcp(&self, socket: TunTcpSocket, addr: Address) -> Result<()>;
    async fn handle_udp(&self, socket: TunUdpSocket, addr: Address) -> Result<()>;
}

#[async_trait::async_trait]
impl Client for SSClient {
    async fn handle_tcp(&self, socket: TunTcpSocket, addr: Address) -> Result<()> {
        self.handle_tcp_connection(socket, addr).await
    }

    async fn handle_udp(&self, socket: TunUdpSocket, addr: Address) -> Result<()> {
        self.handle_udp_connection(socket, addr).await
    }
}

struct DirectClient {
    resolver: DnsNetworkClient,
    dns_server: (String, u16),
}

impl DirectClient {
    pub async fn new(dns_server: (String, u16)) -> Self {
        DirectClient {
            resolver: DnsNetworkClient::new(0).await,
            dns_server,
        }
    }

    fn dns_server(&self) -> (&str, u16) {
        (&self.dns_server.0, self.dns_server.1)
    }

    async fn lookup_ip(&self, domain: &str) -> Result<Option<String>> {
        let dns_server = self.dns_server();
        let now = Instant::now();
        let packet = self
            .resolver
            .send_query(domain, QueryType::A, dns_server, true)
            .await?;
        let ip = packet.get_random_a();
        let duration = now.elapsed();
        trace!(duration = ?duration, domain = domain, dns_server = ?dns_server, ip = ?ip, "lookup ip");
        Ok(ip)
    }
}

#[async_trait::async_trait]
impl Client for DirectClient {
    async fn handle_tcp(&self, mut socket: TunTcpSocket, addr: Address) -> Result<()> {
        let sock_addr = match addr {
            Address::SocketAddress(addr) => addr,
            Address::DomainNameAddress(domain, port) => {
                let ip = self.lookup_ip(&domain).await?;
                match ip {
                    None => {
                        return Err(Error::new(
                            ErrorKind::NotFound,
                            format!("domain {} not found", &domain),
                        ))
                    }
                    Some(ip) => SocketAddr::new(ip.parse().expect("not valid ip addr"), port),
                }
            }
        };
        let now = Instant::now();
        let conn = TcpStream::connect(sock_addr).await?;
        let elapsed = now.elapsed();
        trace!(duration = ?elapsed, "TcpStream::connect");
        let mut socket_clone = socket.clone();
        let mut ref_conn = &conn;
        let mut ref_conn2 = &conn;
        let a = copy(&mut socket_clone, &mut ref_conn);
        let b = copy(&mut ref_conn2, &mut socket);
        let (ret_a, ret_b) = future::join!(a, b).await;
        ret_a?;
        ret_b?;
        Ok(())
    }

    #[allow(unreachable_code)]
    async fn handle_udp(&self, socket: TunUdpSocket, addr: Address) -> Result<()> {
        let sock_addr = match addr.clone() {
            Address::SocketAddress(addr) => addr,
            Address::DomainNameAddress(domain, port) => {
                let ip = self.lookup_ip(&domain).await?;
                match ip {
                    None => {
                        return Err(Error::new(
                            ErrorKind::NotFound,
                            format!("domain {} not found", &domain),
                        ))
                    }
                    Some(ip) => SocketAddr::new(ip.parse().expect("not valid ip addr"), port),
                }
            }
        };

        let mut buf = vec![0; 1024];
        let mut udp_map = HashMap::new();

        loop {
            let now = Instant::now();
            let (recv_from_local_size, local_src) = socket.recv_from(&mut buf).await?;
            let duration = now.elapsed();
            let udp_socket = match udp_map.get(&local_src).cloned() {
                Some(socket) => socket,
                None => {
                    let new_udp = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
                    let bind_addr = new_udp.local_addr()?;
                    trace!(addr = %bind_addr, "bind new udp socket");
                    udp_map.insert(local_src, new_udp.clone());

                    let cloned_socket = socket.clone();
                    let cloned_new_udp = new_udp.clone();
                    let _handle: JoinHandle<Result<_>> = task::spawn(async move {
                        let mut recv_buf = vec![0; 1024];
                        loop {
                            let now = Instant::now();
                            let (recv_from_ss_size, udp_ss_addr) =
                                cloned_new_udp.recv_from(&mut recv_buf).await?;
                            let duration = now.elapsed();
                            trace!(duration = ?duration, size = recv_from_ss_size, src_addr = %udp_ss_addr, local_udp_socket = ?bind_addr, "recv from ss server");
                            let now = Instant::now();
                            let send_local_size = cloned_socket
                                .send_to(&recv_buf[..recv_from_ss_size], &local_src)
                                .await?;
                            let duration = now.elapsed();
                            trace!(duration = ?duration, size = send_local_size, dst_addr = %local_src, local_udp_socket = ?bind_addr, "send to tun socket");
                        }
                        Ok(())
                    }.instrument(trace_span!("ss server to tun socket", socket = %bind_addr)));
                    new_udp
                }
            };
            let bind_addr = udp_socket.local_addr()?;
            trace!(duration = ?duration, size = recv_from_local_size, src_addr = %local_src, local_udp_socket = ?bind_addr, "recv from tun socket");
            let now = Instant::now();
            let send_ss_size = udp_socket
                .send_to(&buf[..recv_from_local_size], sock_addr)
                .await?;
            let duration = now.elapsed();
            trace!(duration = ?duration, size = send_ss_size, dst_addr = %sock_addr, local_udp_socket = ?bind_addr, "send to ss server");
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct RuledClient {
    rule: ProxyRules,
    ssclient: SSClient,
    direct_client: Arc<DirectClient>,
    proxy_uid: Option<u32>,
}

impl RuledClient {
    pub async fn new(conf: Config, proxy_uid: Option<u32>, to_terminal: Arc<AtomicBool>) -> Self {
        let dns = conf.dns_server;
        let dns_server_addr = (dns.ip().to_string(), dns.port());

        let ssclient = SSClient::new(
            conf.server_config.clone(),
            dns_server_addr.clone(),
            to_terminal,
        )
        .await;
        let direct_client = DirectClient::new(dns_server_addr).await;
        RuledClient {
            rule: conf.rules.clone(),
            ssclient,
            direct_client: Arc::new(direct_client),
            proxy_uid,
        }
    }
}

#[async_trait::async_trait]
impl Client for RuledClient {
    async fn handle_tcp(&self, socket: TunTcpSocket, addr: Address) -> Result<()> {
        let domain = match &addr {
            Address::SocketAddress(a) => a.to_string(),
            Address::DomainNameAddress(domain, _port) => domain.to_string(),
        };
        let mut pass_proxy = false;
        if let Some(uid) = self.proxy_uid {
            if !socket_addr_belong_to_user(socket.remote_addr(), uid)? {
                pass_proxy = true;
            }
        }
        let action = if pass_proxy {
            Action::Direct
        } else {
            self.rule
                .action_for_domain(&domain)
                .unwrap_or_else(|| self.rule.default_action())
        };
        trace!(addr = %addr, action = ?action, "RuledClient:handle_tcp");

        match action {
            Action::Reject => Ok(()),
            Action::Direct => {
                self.direct_client
                    .handle_tcp(socket, addr.clone())
                    .instrument(trace_span!("DirectClient.handle_tcp", addr = %addr))
                    .await
            }
            Action::Proxy => {
                self.ssclient
                    .handle_tcp(socket, addr.clone())
                    .instrument(trace_span!("SSClient.handle_tcp", addr = %addr))
                    .await
            }
        }
    }

    async fn handle_udp(&self, socket: TunUdpSocket, addr: Address) -> Result<()> {
        let domain = match &addr {
            Address::SocketAddress(a) => a.to_string(),
            Address::DomainNameAddress(domain, _port) => domain.to_string(),
        };
        let mut pass_proxy = false;
        if let Some(uid) = self.proxy_uid {
            if !socket_addr_belong_to_user(socket.local_addr(), uid)? {
                pass_proxy = true;
            }
        }
        let action = if pass_proxy {
            Action::Direct
        } else {
            self.rule
                .action_for_domain(&domain)
                .unwrap_or_else(|| self.rule.default_action())
        };
        match action {
            Action::Reject => Ok(()),
            Action::Direct => self.direct_client.handle_udp(socket, addr).await,
            Action::Proxy => self.ssclient.handle_udp(socket, addr).await,
        }
    }
}

fn socket_addr_belong_to_user(addr: SocketAddr, uid: u32) -> Result<bool> {
    let user_socks: HashMap<i32, Vec<SocketInfo>> = list_user_proc_socks(uid)?;
    Ok(user_socks
        .values()
        .any(|sockets| sockets.iter().any(|s| s.local == addr)))
}
