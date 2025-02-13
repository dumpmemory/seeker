use async_std_resolver::AsyncStdResolver;
use async_trait::async_trait;
use config::rule::{Action, ProxyRules};
use hermesdns::{DnsPacket, DnsRecord, DnsResolver, Hosts, QueryType, TransientTtl};
use std::any::Any;
use std::io;
use std::io::Result;
use std::sync::Arc;
use store::Store;
use tracing::{debug, error};
use trust_dns_proto::rr::{RData, RecordType};

/// A Forwarding DNS Resolver
///
/// This resolver uses an external DNS server to service a query
#[derive(Clone)]
pub struct RuleBasedDnsResolver {
    inner: Arc<Inner>,
}

struct Inner {
    hosts: Hosts,
    rules: ProxyRules,
    bypass_direct: bool,
    resolver: AsyncStdResolver,
}

impl RuleBasedDnsResolver {
    pub async fn new(bypass_direct: bool, rules: ProxyRules, resolver: AsyncStdResolver) -> Self {
        RuleBasedDnsResolver {
            inner: Arc::new(Inner {
                hosts: Hosts::load().expect("load /etc/hosts"),
                rules,
                bypass_direct,
                resolver,
            }),
        }
    }

    pub fn lookup_host(&self, addr: &str) -> Option<String> {
        let host = Store::global()
            .get_host_by_ipv4(addr.parse().expect("invalid addr"))
            .expect("get host");
        debug!("lookup host: {:?}, addr: {:?}", host, addr);
        host
    }

    async fn resolve_real(&self, domain: &str, qtype: QueryType) -> Result<DnsPacket> {
        let mut packet = DnsPacket::new();
        let lookup = self
            .inner
            .resolver
            .lookup(domain, RecordType::from(qtype.to_num()))
            .await
            .map_err(|e| {
                let msg = e.to_string();
                error!("directly lookup host error: {}", &msg);
                io::Error::new(io::ErrorKind::Other, msg)
            })?;
        for record in lookup.record_iter() {
            let rdata = match record.data() {
                None => {
                    continue;
                }
                Some(RData::A(ip)) => DnsRecord::A {
                    domain: domain.to_string(),
                    addr: *ip,
                    ttl: TransientTtl(record.ttl()),
                },
                Some(RData::AAAA(ip)) => DnsRecord::AAAA {
                    domain: domain.to_string(),
                    addr: *ip,
                    ttl: TransientTtl(record.ttl()),
                },
                Some(RData::CNAME(cname)) => DnsRecord::CNAME {
                    domain: domain.to_string(),
                    host: cname.to_string(),
                    ttl: TransientTtl(record.ttl()),
                },
                Some(RData::MX(mx)) => DnsRecord::MX {
                    domain: domain.to_string(),
                    host: mx.exchange().to_string(),
                    priority: mx.preference(),
                    ttl: TransientTtl(record.ttl()),
                },
                Some(RData::NS(ns)) => DnsRecord::NS {
                    domain: domain.to_string(),
                    host: ns.to_string(),
                    ttl: TransientTtl(record.ttl()),
                },
                Some(RData::SOA(soa)) => DnsRecord::SOA {
                    domain: domain.to_string(),
                    m_name: soa.mname().to_string(),
                    r_name: soa.rname().to_string(),
                    serial: soa.serial(),
                    refresh: soa.refresh() as u32,
                    retry: soa.retry() as u32,
                    expire: soa.expire() as u32,
                    minimum: soa.minimum(),
                    ttl: TransientTtl(record.ttl()),
                },
                Some(RData::TXT(txt)) => DnsRecord::TXT {
                    domain: domain.to_string(),
                    data: txt.to_string(),
                    ttl: TransientTtl(record.ttl()),
                },
                Some(RData::SRV(srv)) => DnsRecord::SRV {
                    domain: domain.to_string(),
                    priority: srv.priority(),
                    weight: srv.weight(),
                    port: srv.port(),
                    host: srv.target().to_string(),
                    ttl: TransientTtl(record.ttl()),
                },
                other => {
                    tracing::error!("unsupported record type: {:?}", other);
                    continue;
                }
            };
            packet.answers.push(rdata)
        }

        Ok(packet)
    }

    async fn resolve(&self, domain: &str, qtype: QueryType) -> Result<DnsPacket> {
        // We only support A record for now, for other records, we just forward them to upstream.
        if !matches!(qtype, QueryType::A | QueryType::AAAA) {
            return self.resolve_real(domain, qtype).await;
        }

        let mut packet = DnsPacket::new();

        // lookup /etc/hosts
        if let Some(ip) = self.inner.hosts.get(domain) {
            packet.answers.push(DnsRecord::A {
                domain: domain.to_string(),
                addr: ip,
                ttl: TransientTtl(3),
            });
            debug!(
                "lookup host for /etc/hosts domain: {}, ip: {:?}",
                domain, ip
            );
            return Ok(packet);
        }

        // direct traffic bypass tun.
        let bypass_direct = self.inner.bypass_direct;
        match self.inner.rules.action_for_domain(Some(domain), None) {
            // Return real ip when `bypass_direct` is true.
            Some(Action::Direct) if bypass_direct => {
                return self.resolve_real(domain, qtype).await;
            }
            // Do not return dns records when action is reject.
            Some(Action::Reject) => return Ok(packet),
            _ => {}
        };

        let ip = Store::global()
            .get_ipv4_by_host(domain)
            .expect("get domain");
        packet.answers.push(DnsRecord::A {
            domain: domain.to_string(),
            addr: ip,
            ttl: TransientTtl(3),
        });
        Ok(packet)
    }
}

#[async_trait]
impl DnsResolver for RuleBasedDnsResolver {
    async fn resolve(&self, domain: &str, qtype: QueryType, _recursive: bool) -> Result<DnsPacket> {
        self.resolve(domain, qtype).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::new_resolver;
    use async_std::task;

    #[test]
    fn test_inner_resolve_ip_and_lookup_host() {
        store::Store::setup_global_for_test();
        let dns = std::env::var("DNS").unwrap_or_else(|_| "223.5.5.5".to_string());
        task::block_on(async {
            let resolver = RuleBasedDnsResolver::new(
                true,
                ProxyRules::new(vec![]),
                new_resolver(dns, 53).await,
            )
            .await;
            let baidu_ip = resolver
                .resolve("baidu.com", QueryType::A)
                .await
                .unwrap()
                .get_random_a();
            assert!(baidu_ip.is_some());
            assert_eq!(
                resolver.lookup_host(&baidu_ip.unwrap()),
                Some("baidu.com".to_string())
            );
            assert!(resolver
                .resolve("mycookbook.allsunday.io", QueryType::TXT)
                .await
                .unwrap()
                .get_txt()
                .is_some());
            assert_eq!(resolver.lookup_host("10.1.0.1"), None);
        });
    }
}
