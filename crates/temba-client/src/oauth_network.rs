//! OAuth destination approval is independent of HTTP transport opt-in.
use anyhow::{Result, ensure};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{net::IpAddr, sync::Arc, time::Duration};
use temba_core::endpoint;
use url::{Host, Url};

pub struct Policy {
    origins: Vec<String>,
    resource: Option<Url>,
    allow_http: bool,
}

impl Policy {
    pub fn restricted(origins: &[String], allow_http: bool) -> Self {
        Self {
            origins: origins.to_vec(),
            resource: None,
            allow_http,
        }
    }

    pub fn automatic(resource: &str, allow_http: bool) -> Result<Self> {
        Ok(Self {
            origins: vec![],
            resource: Some(endpoint(resource, allow_http)?),
            allow_http,
        })
    }

    pub fn url(&self, value: &str) -> Result<Url> {
        let u = endpoint(value, self.allow_http)?;
        if let Some(resource) = &self.resource {
            if u.origin() == resource.origin() {
                return Ok(u);
            }
            // The DNS exception for the configured MCP host applies only to its exact origin.
            ensure!(
                u.host() != resource.host(),
                "OAuth on another port or scheme of the MCP host requires explicit allowedOrigins"
            );
            if let Some(ip) = literal_ip(&u) {
                ensure!(
                    public_ip(ip),
                    "Automatically discovered OAuth destination is not a public address"
                );
            }
        } else if !self
            .origins
            .iter()
            .any(|origin| endpoint(origin, self.allow_http).is_ok_and(|o| o.origin() == u.origin()))
        {
            return Err(super::oauth::OriginNotAllowed(u.origin().ascii_serialization()).into());
        }
        Ok(u)
    }

    pub async fn validate(&self, value: &str) -> Result<Url> {
        let u = self.url(value)?;
        if let Some(resource) = &self.resource
            && u.origin() != resource.origin()
        {
            // Also validate browser destinations, which are not requested by reqwest.
            if literal_ip(&u).is_none() {
                public_addresses(u.host_str().unwrap()).await?;
            }
        }
        Ok(u)
    }

    pub fn client(&self, existing: &reqwest::Client) -> Result<reqwest::Client> {
        let Some(resource) = &self.resource else {
            return Ok(existing.clone());
        };
        Ok(reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .dns_resolver(Arc::new(PublicDns {
                resource_host: resource.host_str().unwrap().to_owned(),
            }))
            .build()?)
    }
}

fn literal_ip(u: &Url) -> Option<IpAddr> {
    match u.host()? {
        Host::Ipv4(ip) => Some(ip.into()),
        Host::Ipv6(ip) => Some(ip.into()),
        Host::Domain(_) => None,
    }
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(matches!(a, 0 | 10 | 127 | 224..=255)
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || (b == 0 && matches!(c, 0 | 2))))
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (matches!(b, 18 | 19) || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Global unicast only; exclude protocol assignments, transition and documentation ranges.
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

fn check_addresses(addresses: &[std::net::SocketAddr]) -> Result<()> {
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|a| public_ip(a.ip())),
        "Automatically discovered OAuth hostname must resolve only to public addresses"
    );
    Ok(())
}

async fn public_addresses(host: &str) -> Result<Vec<std::net::SocketAddr>> {
    let addresses =
        tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host((host, 0)))
            .await??
            .collect::<Vec<_>>();
    check_addresses(&addresses)?;
    Ok(addresses)
}

struct PublicDns {
    resource_host: String,
}
impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        let resource = name.as_str() == self.resource_host;
        Box::pin(async move {
            let addresses = if resource {
                tokio::net::lookup_host((name.as_str(), 0))
                    .await?
                    .collect::<Vec<_>>()
            } else {
                public_addresses(name.as_str()).await?
            };
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_discovery_blocks_nonpublic_addresses_and_keeps_resource_exception_exact() {
        let policy = Policy::automatic("http://127.0.0.1:8000/mcp", true).unwrap();
        assert!(policy.url("http://127.0.0.1:8000/.well-known/test").is_ok());
        assert!(policy.url("https://identity.example/token").is_ok());
        for target in [
            "https://127.0.0.1:9000/token",
            "https://10.0.0.1/token",
            "https://[::1]/token",
            "https://169.254.169.254/token",
            "https://[::ffff:127.0.0.1]/token",
        ] {
            assert!(policy.url(target).is_err(), "accepted {target}");
        }
        let policy = Policy::automatic("https://mcp.example/mcp", false).unwrap();
        assert!(policy.url("https://mcp.example:8443/token").is_err());
        for ip in [
            "0.0.0.0",
            "100.64.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2002:7f00:1::1",
            "3fff::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "accepted {ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()));
        }
        assert!(check_addresses(&[]).is_err());
        assert!(
            check_addresses(&[
                "1.1.1.1:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap()
            ])
            .is_err()
        );
    }

    #[test]
    fn transport_opt_in_does_not_disable_oauth_destination_checks() {
        let resource = "https://mcp.example/mcp";
        let strict = Policy::automatic(resource, false).unwrap();
        let insecure = Policy::automatic(resource, true).unwrap();
        assert!(strict.url("http://identity.example/token").is_err());
        assert!(insecure.url("http://identity.example/token").is_ok());
        for url in [
            "http://10.0.0.1/token",
            "https://10.0.0.1/token",
            "http://[::1]/token",
        ] {
            assert!(insecure.url(url).is_err());
        }
        let origins = vec!["http://identity.internal".into()];
        assert!(
            Policy::restricted(&origins, false)
                .url("http://identity.internal/token")
                .is_err()
        );
        assert!(
            Policy::restricted(&origins, true)
                .url("http://identity.internal/token")
                .is_ok()
        );
        assert!(
            Policy::restricted(&origins, true)
                .url("http://other.internal/token")
                .is_err()
        );
    }

    #[tokio::test]
    async fn dns_resolver_rejects_local_names_for_discovered_destinations() {
        let policy = Policy::automatic("https://mcp.example/mcp", false).unwrap();
        assert!(
            policy
                .validate("https://localhost/authorize")
                .await
                .is_err()
        );
        let dns = PublicDns {
            resource_host: "mcp.example".into(),
        };
        assert!(dns.resolve("localhost".parse().unwrap()).await.is_err());
    }
}
