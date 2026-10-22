//! Decides what a `!play` argument is before anything is fetched.

use std::net::IpAddr;

use anyhow::{Context as _, Result, bail};
use url::{Host, Url};

#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// A link to any site yt-dlp handles; yt-dlp decides if it is one track
    /// or a playlist.
    Url(String),
    Search(String),
}

pub fn classify(input: &str) -> Source {
    let input = input.trim().trim_start_matches('<').trim_end_matches('>');
    if (input.starts_with("http://") || input.starts_with("https://"))
        && let Ok(url) = Url::parse(input)
    {
        return Source::Url(url.to_string());
    }
    Source::Search(input.to_string())
}

/// Refuses links to this machine or the local network. yt-dlp's generic
/// extractor fetches any URL, and anyone in the server can post one.
pub async fn ensure_public(link: &str) -> Result<()> {
    let url = Url::parse(link).context("not a valid link")?;
    let addrs: Vec<IpAddr> = match url.host().context("link has no host")? {
        Host::Ipv4(ip) => vec![ip.into()],
        Host::Ipv6(ip) => vec![ip.into()],
        Host::Domain(domain) => {
            let port = url.port_or_known_default().unwrap_or(443);
            tokio::net::lookup_host((domain, port))
                .await
                .with_context(|| format!("can't resolve {domain}"))?
                .map(|a| a.ip())
                .collect()
        }
    };
    if addrs.is_empty() || addrs.iter().any(|ip| !is_public(*ip)) {
        bail!("links to local or private addresses aren't allowed");
    }
    Ok(())
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || a == 0
                // 100.64.0.0/10, carrier-grade NAT and Tailscale.
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public(v4.into()),
            None => {
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
                    || v6.is_multicast())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_search() {
        for link in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://soundcloud.com/forss/flickermood",
            "<https://youtu.be/dQw4w9WgXcQ>",
            "http://example.com/a.mp3",
        ] {
            assert!(matches!(classify(link), Source::Url(_)), "{link}");
        }
        assert_eq!(classify("daft punk one more time"), Source::Search("daft punk one more time".into()));
    }

    #[test]
    fn private_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.5", "169.254.1.1", "100.100.1.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:192.168.1.1"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "142.250.1.1", "2a00:1450::1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn refuses_local_links() {
        assert!(ensure_public("http://192.168.1.1/").await.is_err());
        assert!(ensure_public("http://[::1]:8080/x").await.is_err());
        assert!(ensure_public("http://localhost/").await.is_err());
        assert!(ensure_public("https://1.1.1.1/").await.is_ok());
    }
}
