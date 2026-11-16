//! Anonymous, bounded HTTPS reads with address checks before every connection.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, LOCATION},
};
use url::{Host, Url};

use super::{DiscoveryOptions, MetadataFetcher};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_DOCUMENT_BYTES: usize = 512 * 1024;
const MAX_REDIRECTS: usize = 5;

pub(super) struct HttpFetcher {
    pub options: DiscoveryOptions,
}

#[derive(Debug)]
pub(super) struct Fetched {
    pub requested_url: Url,
    pub url: Url,
    pub redirects: Vec<String>,
    pub status: u16,
    pub body: Vec<u8>,
    pub fetched_at_unix_ms: u64,
}

impl MetadataFetcher for HttpFetcher {
    async fn get(&self, requested_url: Url, follow_redirects: bool) -> Result<Fetched> {
        let mut url = requested_url.clone();
        let mut redirects = Vec::new();
        loop {
            // A fresh client has no connection pool, cookies, credentials, or proxy
            // state from other requests. Its only DNS answer is the checked one.
            let (status, location, body) =
                tokio::time::timeout(REQUEST_TIMEOUT, self.read_once(&url))
                    .await
                    .context("Poppy metadata request timed out")??;
            if status.is_redirection() {
                ensure!(
                    follow_redirects,
                    "redirects are forbidden for metadata at {url}"
                );
                ensure!(
                    redirects.len() < MAX_REDIRECTS,
                    "Poppy discovery exceeded its redirect limit"
                );
                let next = redirect_target(&url, location.as_deref(), &self.options)?;
                ensure!(
                    next != url && !redirects.contains(&next.to_string()),
                    "Poppy discovery redirect loop"
                );
                redirects.push(url.to_string());
                url = next;
                continue;
            }
            ensure!(
                status.is_success() || status == StatusCode::NOT_FOUND,
                "metadata request returned HTTP {} at {url}",
                status.as_u16()
            );
            return Ok(Fetched {
                requested_url,
                url,
                redirects,
                status: status.as_u16(),
                body,
                fetched_at_unix_ms: now_ms()?,
            });
        }
    }
}

impl HttpFetcher {
    async fn read_once(&self, url: &Url) -> Result<(StatusCode, Option<String>, Vec<u8>)> {
        validate_url(url, &self.options)?;
        let host = url.host_str().context("metadata URL has no host")?;
        let port = url
            .port_or_known_default()
            .context("metadata URL has no port")?;
        let addresses: Vec<SocketAddr> = match url.host().context("metadata URL has no host")? {
            Host::Ipv4(ip) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
            Host::Ipv6(ip) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
            Host::Domain(domain) => tokio::net::lookup_host((domain, port))
                .await
                .context("resolving metadata host")?
                .collect(),
        };
        ensure!(
            !addresses.is_empty(),
            "metadata host resolved to no addresses"
        );
        ensure!(
            self.options.allow_private_networks
                || addresses.iter().all(|addr| is_public(addr.ip())),
            "metadata host resolved to a non-public address"
        );
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(REQUEST_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .resolve_to_addrs(host, &addresses)
            .build()
            .context("building isolated metadata HTTP client")?;
        let mut response = client
            .get(url.clone())
            .header(ACCEPT, "application/json")
            .send()
            .await
            .context("fetching public discovery metadata")?;
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .map(|value| value.to_str().map(str::to_owned))
            .transpose()
            .context("invalid redirect Location header")?;
        // Redirect and error bodies are irrelevant and must not consume the body
        // budget reserved for a discovery document.
        if status.is_redirection() || !status.is_success() {
            return Ok((status, location, Vec::new()));
        }
        ensure!(
            response
                .content_length()
                .is_none_or(|len| len <= MAX_DOCUMENT_BYTES as u64),
            "metadata response exceeds the document size limit"
        );
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("reading metadata response")?
        {
            ensure!(
                chunk.len() <= MAX_DOCUMENT_BYTES.saturating_sub(body.len()),
                "metadata response exceeds the document size limit"
            );
            body.extend_from_slice(&chunk);
        }
        Ok((status, location, body))
    }
}

pub(super) fn redirect_target(
    current: &Url,
    location: Option<&str>,
    options: &DiscoveryOptions,
) -> Result<Url> {
    let next = current
        .join(location.context("redirect has no Location header")?)
        .context("invalid redirect target")?;
    validate_url(&next, options)?;
    Ok(next)
}

pub(super) fn validate_url(url: &Url, options: &DiscoveryOptions) -> Result<()> {
    ensure!(
        url.scheme() == "https" && url.has_host(),
        "metadata URL must use HTTPS with a host"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "metadata URL must not contain credentials"
    );
    ensure!(
        url.fragment().is_none(),
        "metadata URL must not contain a fragment"
    );
    ensure!(
        options.allow_non_default_ports || url.port_or_known_default() == Some(443),
        "non-default HTTPS ports require explicit discovery configuration"
    );
    if !options.allow_private_networks {
        match url.host().context("metadata URL has no host")? {
            Host::Ipv4(ip) => ensure!(
                is_public(IpAddr::V4(ip)),
                "metadata URL targets a non-public address"
            ),
            Host::Ipv6(ip) => ensure!(
                is_public(IpAddr::V6(ip)),
                "metadata URL targets a non-public address"
            ),
            Host::Domain(host) => ensure!(
                host != "localhost" && !host.ends_with(".localhost"),
                "metadata URL targets localhost"
            ),
        }
    }
    Ok(())
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || ip.is_unspecified()
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19)))
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return public_v4(mapped);
    }
    let s = ip.segments();
    // Only native global unicast. In particular, exclude transition mechanisms
    // whose embedded IPv4 destination could otherwise bypass the IPv4 checks.
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && s[1] < 0x0200)
        && !(s[0] == 0x2001 && s[1] == 0x0db8)
        && s[0] != 0x2002
        && !(s[0] == 0x3fff && s[1] < 0x1000)
}

pub(super) fn now_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the UNIX epoch")?
        .as_millis()
        .try_into()
        .context("system clock is out of range")
}

pub(super) fn parse_json(response: &Fetched) -> Result<serde_json::Value> {
    if response.status != 200 {
        bail!("metadata document returned HTTP {}", response.status);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&response.body).context("metadata response is not valid JSON")?;
    ensure!(value.is_object(), "metadata document must be a JSON object");
    Ok(value)
}
