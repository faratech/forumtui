//! Shared HTTP client construction. `build()` bakes in the UA and timeouts
//! every caller must use; the pool it hands back is what steady-state traffic
//! rides. In practice the long-lived client is the `WfApiClient`'s — one
//! pooled client keeps us inside Cloudflare's connection expectations rather
//! than opening a fresh TLS session per call — but the process is not limited
//! to exactly one: the login flow and the logout revokes build short-lived
//! clients of their own, deliberately outside the session's pool (#661).

use std::sync::Arc;

use crate::config;
use crate::error::Result;

/// Install *ring* as the process-wide rustls crypto provider, once.
///
/// reqwest 0.13's `rustls-no-provider` feature deliberately ships no provider:
/// `ClientBuilder::build` calls `CryptoProvider::get_default()` and panics with
/// "No provider set" if nothing installed one. We take that feature (rather
/// than plain `rustls`) to avoid aws-lc-rs's vendored C library — see the note
/// in Cargo.toml — so installing the provider is our job. `install_default`
/// returns `Err` when a provider is already set, which is exactly the no-op we
/// want on the second and later calls.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The built-in site's client (its UA names windowsforum.com — hard rule 6).
/// The updater and the login/logout flows for that site use it.
pub fn build() -> Result<reqwest::Client> {
    build_with_ua(&config::user_agent())
}

/// A client wearing a site's own UA (`site::SiteConfig::user_agent`): the
/// same distinctive `wftui/<ver>` prefix, never a library default.
pub fn build_with_ua(ua: &str) -> Result<reqwest::Client> {
    install_crypto_provider();
    Ok(reqwest::Client::builder()
        .user_agent(ua)
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(config::REQUEST_TIMEOUT)
        // Be a well-behaved API client: we speak for one user, not a scraper.
        .redirect(redirect_policy())
        .build()?)
}


/// A client for bytes that forum *content* names (`WfApiClient::fetch_bytes`
/// for anything but our own API): an `[IMG]` URL is written by whoever wrote
/// the post, and it is fetched the moment a reader scrolls past it, so it
/// must not be able to aim this machine at its own loopback, its LAN or a
/// cloud metadata endpoint (#130). Three layers, because each one alone has
/// a hole:
///
/// - `ContentPolicy::check_url` refuses the initial URL when its host is a
///   non-public IP literal, `localhost` or the configured proxy, before
///   anything is sent;
/// - the redirect policy runs the same check on every hop;
/// - the DNS resolver drops every non-public address a *name* resolves to,
///   so neither a hostname pointing at 127.0.0.1 nor a rebinding answer can
///   slip past the literal check — the connection only ever sees what the
///   resolver kept.
///
/// There are two such clients, one per `ContentLane`. Only the `OwnMedia`
/// one keeps a non-public answer for the forum's own name, and
/// `fetch_bytes` sends it only the forum's exact origin on a media path
/// (#146). A forum the user reached by IP literal or as `localhost` is
/// exempt on its whole origin.
///
/// Through an HTTP proxy the proxy resolves the target, not this resolver,
/// which then only ever sees the proxy's own name; that name is exempt
/// (#147). The per-address guarantee for a proxied target is then
/// `ContentPolicy::preflight`'s local lookup, which is best effort; the
/// literal and per-hop checks still apply in full.
pub fn build_public_with_ua(ua: &str, policy: Arc<ContentPolicy>, lane: ContentLane) -> Result<reqwest::Client> {
    Ok(content_client_builder(ua, policy, lane).build()?)
}

/// `build_public_with_ua` before `build()`, so a test can add an explicit
/// proxy instead of setting the process-wide proxy variables.
pub(crate) fn content_client_builder(ua: &str, policy: Arc<ContentPolicy>, lane: ContentLane) -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
        .user_agent(ua)
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(config::REQUEST_TIMEOUT)
        .dns_resolver(PublicOnlyResolver { policy: policy.clone(), lane })
        .redirect(hop_policy(Some((policy, lane))))
}

/// Which of the two content clients a fetch uses (#146).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentLane {
    /// Everything else. Every name is judged by what it resolves to,
    /// the forum's own name included.
    Strict,
    /// The forum's exact origin on one of its media paths. This is the one
    /// lane where the forum's own name keeps a non-public answer.
    OwnMedia,
}

/// XenForo's media paths: avatars, attachments, gallery items, style
/// assets and the image proxy. A forum whose own name resolves to a
/// private or loopback address (split-horizon DNS, or a client running on
/// the forum's own server) keeps its images, and nothing else on that
/// address is reachable from content.
const OWN_MEDIA_PREFIXES: [&str; 4] = ["/data/", "/attachments/", "/media/", "/styles/"];
const OWN_MEDIA_EXACT: [&str; 1] = ["/proxy.php"];

/// The proxy variables reqwest reads (through hyper-util's
/// `Matcher::from_env`), in both spellings.
const PROXY_VARS: [&str; 6] = ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];

/// What forum content may make this client contact (#130, #146, #147):
/// the site's own origin, and the proxies the user configured.
#[derive(Debug, Clone)]
pub struct ContentPolicy {
    /// The site origin, parsed: `scheme://host[:port]`.
    own: Option<reqwest::Url>,
    /// The site is an IP literal or `localhost`. Then the user chose a
    /// local forum, and all of that origin is the forum.
    own_is_local: bool,
    /// Every host the proxy variables name, normalised by `normalize_name`.
    proxy_hosts: Vec<String>,
    /// Test-only answers for `lookup`, checked before the system resolver.
    #[cfg(test)]
    fixed_dns: Vec<(String, Vec<std::net::IpAddr>)>,
}

impl ContentPolicy {
    /// The policy for a site origin, with this process's proxy variables.
    pub fn for_site(base: &str) -> Self {
        Self::with_proxy_env(base, |name| std::env::var(name).ok())
    }

    /// The policy for a site origin, with the proxy variables read through
    /// `var`. It reads all six variables, where reqwest takes the first one
    /// set of each pair. A name listed here that reqwest does not use as a
    /// proxy is only refused as a content host, so that costs nothing.
    pub fn with_proxy_env(base: &str, var: impl Fn(&str) -> Option<String>) -> Self {
        let own = reqwest::Url::parse(base).ok().filter(|u| u.host_str().is_some());
        let own_is_local = own.as_ref().and_then(|u| u.host_str()).is_some_and(|h| {
            let bare = unbracket(h);
            bare.parse::<std::net::IpAddr>().is_ok() || is_localhost_name(bare)
        });
        // reqwest ignores every proxy variable in a CGI process (the
        // httpoxy rule): no proxy is used there, so none is exempt.
        let mut proxy_hosts: Vec<String> = Vec::new();
        if var("REQUEST_METHOD").is_none() {
            for name in PROXY_VARS {
                if let Some(host) = var(name).as_deref().and_then(proxy_host_of)
                    && !proxy_hosts.contains(&host)
                {
                    proxy_hosts.push(host);
                }
            }
        }
        ContentPolicy {
            own,
            own_is_local,
            proxy_hosts,
            #[cfg(test)]
            fixed_dns: Vec::new(),
        }
    }

    /// The initial-URL (and per-hop) half of the gate: http or https, a
    /// host, and, unless it is the forum's exact origin (scheme, host and
    /// port), not a non-public IP literal, not `localhost` and not the
    /// configured proxy. Hostnames are judged by what they resolve to, in
    /// `PublicOnlyResolver`. The host is the URL's *parsed* host, which the
    /// URL parser has already normalised (`http://2130706433/` is
    /// `127.0.0.1` by then, and userinfo is not part of it).
    pub fn check_url(&self, url: &reqwest::Url) -> std::result::Result<(), NonPublicDestination> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(NonPublicDestination(format!("refusing a {} URL from forum content", url.scheme())));
        }
        let Some(host) = url.host_str() else {
            return Err(NonPublicDestination("refusing a URL with no host".into()));
        };
        if self.is_own_origin(url) {
            return Ok(());
        }
        let bare = unbracket(host);
        if let Ok(ip) = bare.parse::<std::net::IpAddr>()
            && !is_public_ip(ip)
        {
            return Err(NonPublicDestination(format!("refusing to fetch from non-public address {host}")));
        }
        if is_localhost_name(bare) {
            return Err(NonPublicDestination(format!("refusing to fetch from {host}")));
        }
        // The proxy's name is exempt in the resolver, so content must not
        // be able to name it as a target (#147).
        if self.is_proxy_host(bare) {
            return Err(NonPublicDestination(format!("refusing to fetch from {host}: it is a configured proxy")));
        }
        Ok(())
    }

    /// The lane for a URL `check_url` accepted: `OwnMedia` for the forum's
    /// exact origin, without userinfo, on a media path (any path when the
    /// site is local); `Strict` for everything else.
    pub fn lane(&self, url: &reqwest::Url) -> ContentLane {
        let own_media = self.is_own_origin(url)
            && url.username().is_empty()
            && url.password().is_none()
            && (self.own_is_local || is_media_path(url.path()));
        if own_media { ContentLane::OwnMedia } else { ContentLane::Strict }
    }

    /// #147, best effort: a proxied request is resolved by the proxy, so
    /// this resolver never sees the target's name. When a proxy is
    /// configured, resolve a `Strict` target's name here first and refuse
    /// it if any address is non-public. We cannot choose which address the
    /// proxy uses, so a mixed answer is refused too. A name that does not
    /// resolve here is allowed: on a network where only the proxy resolves
    /// outside names, refusing it would refuse every image.
    pub async fn preflight(&self, url: &reqwest::Url) -> std::result::Result<(), NonPublicDestination> {
        if self.proxy_hosts.is_empty() {
            return Ok(());
        }
        let Some(host) = url.host_str() else { return Ok(()) };
        let bare = unbracket(host);
        if bare.parse::<std::net::IpAddr>().is_ok() {
            return Ok(()); // a literal is `check_url`'s to judge
        }
        let port = url.port_or_known_default().unwrap_or(80);
        let Ok(found) = self.lookup(bare, port).await else { return Ok(()) };
        if let Some(addr) = found.iter().find(|a| !is_public_ip(a.ip())) {
            return Err(NonPublicDestination(format!(
                "refusing to fetch from {host}: it resolves to the non-public address {}",
                addr.ip()
            )));
        }
        Ok(())
    }

    /// The per-hop check for a client in `lane`: `check_url`, and for the
    /// `OwnMedia` client, no hop to the forum's own name unless the hop is
    /// itself own media. That client's resolver keeps the forum's
    /// non-public addresses, so a hop to another port or path on that
    /// name must not use it (#146).
    fn hop_check(&self, url: &reqwest::Url, lane: ContentLane) -> std::result::Result<(), NonPublicDestination> {
        self.check_url(url)?;
        if lane == ContentLane::OwnMedia
            && url.host_str().is_some_and(|h| self.is_own_name(h))
            && self.lane(url) != ContentLane::OwnMedia
        {
            return Err(NonPublicDestination(format!(
                "refusing a redirect to {}{}: only the forum's own media paths may use its own address",
                url.host_str().unwrap_or(""),
                url.path()
            )));
        }
        Ok(())
    }

    /// Whether the resolver for `lane` keeps every address `name` resolves
    /// to. The proxy's name is kept in both lanes; the forum's own name is
    /// kept in the `OwnMedia` lane only.
    fn resolver_exempts(&self, name: &str, lane: ContentLane) -> bool {
        self.is_proxy_host(name) || (lane == ContentLane::OwnMedia && self.is_own_name(name))
    }

    fn is_own_origin(&self, url: &reqwest::Url) -> bool {
        self.own.as_ref().is_some_and(|own| own.origin() == url.origin())
    }

    fn is_own_name(&self, host: &str) -> bool {
        self.own.as_ref().and_then(|u| u.host_str()).is_some_and(|own| normalize_name(own) == normalize_name(host))
    }

    fn is_proxy_host(&self, host: &str) -> bool {
        let host = normalize_name(unbracket(host));
        self.proxy_hosts.contains(&host)
    }

    /// The system resolver (and, in tests, `fixed_dns` first).
    async fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        #[cfg(test)]
        if let Some((_, ips)) = self.fixed_dns.iter().find(|(name, _)| *name == normalize_name(host)) {
            if ips.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{host} did not resolve")));
            }
            return Ok(ips.iter().map(|ip| std::net::SocketAddr::new(*ip, port)).collect());
        }
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }

    /// Test-only: answer `name` with `ips` (none: it does not resolve).
    #[cfg(test)]
    pub(crate) fn with_fixed_dns(mut self, name: &str, ips: &[&str]) -> Self {
        self.fixed_dns.push((normalize_name(name), ips.iter().map(|ip| ip.parse().unwrap()).collect()));
        self
    }
}

fn is_media_path(path: &str) -> bool {
    // Any percent-encoding disqualifies a path, so an encoded `.` or `/`
    // cannot climb out of a media directory whichever way the server
    // decodes it. The URL parser has already removed literal dot segments.
    !path.contains('%') && (OWN_MEDIA_EXACT.contains(&path) || OWN_MEDIA_PREFIXES.iter().any(|p| path.starts_with(p)))
}

/// The host a proxy variable names, as the connector will pass it to the
/// resolver: the authority's host, without userinfo or port, normalised.
/// A value with no scheme means http, as it does for reqwest.
fn proxy_host_of(value: &str) -> Option<String> {
    let value = value.trim();
    let rest = value.split_once("://").map_or(value, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host_port.split(':').next().unwrap_or(""),
    };
    let host = normalize_name(host);
    (!host.is_empty()).then_some(host)
}

/// A name the way two spellings of it compare: lowercase, no trailing dot.
fn normalize_name(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn unbracket(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

fn is_localhost_name(host: &str) -> bool {
    let lower = normalize_name(host);
    lower == "localhost" || lower.ends_with(".localhost")
}

/// Why a content-named fetch was refused before it reached the network.
/// A distinct type so `fetch_bytes` can find it in reqwest's error chain
/// (a resolver or redirect refusal arrives wrapped) and report it as
/// `Error::FetchRejected` rather than as a transport failure.
#[derive(Debug)]
pub struct NonPublicDestination(pub String);

impl std::fmt::Display for NonPublicDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NonPublicDestination {}

/// The refusal inside a reqwest error's source chain, if that is what it is.
pub fn non_public_refusal<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a NonPublicDestination> {
    let mut cur: Option<&'a (dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(hit) = e.downcast_ref::<NonPublicDestination>() {
            return Some(hit);
        }
        cur = e.source();
    }
    None
}

/// The most of a non-2xx body any caller reads. 64 KiB is far more than an
/// XF error envelope, a Cloudflare page or a GitHub error needs to say
/// what went wrong.
pub(crate) const MAX_ERROR_BODY: usize = 64 * 1024;

/// Read an error body under `MAX_ERROR_BODY` and stop there. A size cap on
/// the success branch alone is not enough: an origin that wants this
/// process to buffer gigabytes only has to answer 403 or 500 instead of
/// 200. `fetch_bytes` reads whatever `[IMG]` names (#46), and the updater
/// asks an off-origin mirror in the background (#145); both read their
/// error bodies through this. Whatever is past the cap is never read; the
/// connection is dropped with the response.
pub(crate) async fn read_error_body(mut resp: reqwest::Response) -> Vec<u8> {
    let mut bytes = Vec::new();
    while bytes.len() < MAX_ERROR_BODY
        && let Ok(Some(chunk)) = resp.chunk().await
    {
        let room = MAX_ERROR_BODY - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    bytes
}

/// Whether an address is one a forum post may make this client contact:
/// globally routable unicast. Loopback, RFC 1918, CGNAT, link-local (the
/// 169.254.169.254 metadata service), unspecified, multicast, broadcast,
/// reserved and documentation ranges are not, and an IPv6 address that
/// embeds an IPv4 one (mapped, compatible, NAT64, 6to4) is judged by the
/// IPv4 address it reaches. `std`'s `is_global` is still unstable.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| {
                std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
            };
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            // ::/96 (IPv4-compatible, deprecated) — also covers :: and ::1.
            if s[..6].iter().all(|&x| x == 0) {
                return is_public_v4(embedded(s[6], s[7]));
            }
            // 64:ff9b::/96, the well-known NAT64 prefix.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0) {
                return is_public_v4(embedded(s[6], s[7]));
            }
            // 2002::/16, 6to4: the IPv4 address is bits 16..48.
            if s[0] == 0x2002 {
                return is_public_v4(embedded(s[1], s[2]));
            }
            let local_nat64 = s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1; // 64:ff9b:1::/48
            let discard = s[0] == 0x100 && s[1..4].iter().all(|&x| x == 0); // 100::/64
            let documentation = s[0] == 0x2001 && s[1] == 0xdb8; // 2001:db8::/32
            let unique_local = (s[0] & 0xfe00) == 0xfc00; // fc00::/7
            let link_local = (s[0] & 0xffc0) == 0xfe80; // fe80::/10
            let site_local = (s[0] & 0xffc0) == 0xfec0; // fec0::/10
            let multicast = (s[0] & 0xff00) == 0xff00; // ff00::/8
            !(local_nat64 || discard || documentation || unique_local || link_local || site_local || multicast)
        }
    }
}

fn is_public_v4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let this_network = a == 0; // 0.0.0.0/8
    let private = a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168);
    let cgnat = a == 100 && (64..=127).contains(&b); // 100.64.0.0/10
    let loopback = a == 127;
    let link_local = a == 169 && b == 254;
    let ietf = a == 192 && b == 0 && c == 0; // 192.0.0.0/24
    let documentation =
        (a == 192 && b == 0 && c == 2) || (a == 198 && b == 51 && c == 100) || (a == 203 && b == 0 && c == 113);
    let benchmarking = a == 198 && (b == 18 || b == 19); // 198.18.0.0/15
    let multicast_or_reserved = a >= 224; // 224/4, 240/4, 255.255.255.255
    !(this_network
        || private
        || cgnat
        || loopback
        || link_local
        || ietf
        || documentation
        || benchmarking
        || multicast_or_reserved)
}

/// Resolves a name the way the system does, then keeps only the public
/// addresses (all of them for a name `ContentPolicy::resolver_exempts`
/// in this lane: the configured proxy, and in the `OwnMedia` lane the
/// forum's own name). An answer with no public address left is a refusal,
/// not an empty success.
struct PublicOnlyResolver {
    policy: Arc<ContentPolicy>,
    lane: ContentLane,
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let exempt = self.policy.resolver_exempts(&host, self.lane);
        let policy = self.policy.clone();
        Box::pin(async move {
            let found = policy.lookup(&host, 0).await?;
            if found.is_empty() {
                return Err(format!("{host} did not resolve").into());
            }
            let kept: Vec<std::net::SocketAddr> =
                found.into_iter().filter(|a| exempt || is_public_ip(a.ip())).collect();
            if kept.is_empty() {
                let refusal = NonPublicDestination(format!("refusing to fetch from {host}: it resolves only to non-public addresses"));
                return Err(Box::new(refusal) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(kept.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn redirect_policy() -> reqwest::redirect::Policy {
    hop_policy(None)
}

/// Follow up to four redirects, but only through a scheme we would have
/// requested in the first place: https, or http to an exact loopback host
/// (the wiremock tests, a local dev forum). The stock `Policy::limited`
/// follows every hop blindly, so an https URL that 302s to http:// would
/// silently downgrade the channel — for the updater that channel is asset
/// integrity: `fetch_capped` gates the initial URL (#775); the per-hop gate
/// is #818. The host here is the URL's *parsed* host, so userinfo and suffix
/// tricks (`127.0.0.1@evil.com`, `127.0.0.1.evil.com`) cannot pass — the
/// same exact-match rule as update.rs's `is_loopback_host` (#68).
///
/// `public_only` is set for the content clients (`build_public_with_ua`):
/// every hop must also pass `ContentPolicy::hop_check`, so a public image
/// URL cannot 302 the fetch onto a LAN address (#130), and the `OwnMedia`
/// client cannot be redirected to another port or path on the forum's
/// own name (#146).
fn hop_policy(public_only: Option<(Arc<ContentPolicy>, ContentLane)>) -> reqwest::redirect::Policy {
    const MAX_HOPS: usize = 4;
    reqwest::redirect::Policy::custom(move |attempt| {
        if let Some((policy, lane)) = &public_only
            && let Err(refusal) = policy.hop_check(attempt.url(), *lane)
        {
            return attempt.error(refusal);
        }
        // previous()[0] is the INITIAL url, not a redirect (reqwest's own
        // Limit policy excludes it and errors only past max) — `>` keeps the
        // four-hop semantics of the Policy::limited(4) this replaced; `>=`
        // here followed one hop fewer (#854).
        if attempt.previous().len() > MAX_HOPS {
            return attempt.error("too many redirects");
        }
        let url = attempt.url();
        let loopback = url.host_str().is_some_and(|h| {
            matches!(
                h.to_ascii_lowercase().as_str(),
                "127.0.0.1" | "localhost" | "::1"
            )
        });
        if url.scheme() == "https" || (url.scheme() == "http" && loopback) {
            attempt.follow()
        } else {
            attempt.error("refusing to follow a redirect to a non-https, non-loopback URL")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // #818: every redirect hop passes the same scheme gate as the initial
    // fetch — https, or http to an exact loopback host. The stock limited(4)
    // policy this replaced followed an https→http downgrade without a word.
    #[tokio::test]
    async fn redirect_to_plain_http_off_loopback_is_refused() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("Location", "http://example.invalid/downgrade"),
            )
            .mount(&server)
            .await;
        let client = build().unwrap();
        let err = client.get(server.uri()).send().await.unwrap_err();
        // reqwest wraps a policy rejection as "error following redirect for
        // url …"; the gate's own message sits further down the source chain.
        assert!(err.is_redirect(), "expected a redirect error, got: {err}");
        let mut saw_gate = false;
        let mut source = std::error::Error::source(&err);
        while let Some(e) = source {
            saw_gate |= e.to_string().contains("non-https");
            source = e.source();
        }
        assert!(saw_gate, "policy gate not in the error chain of: {err}");
    }

    /// #130: the address classes a forum post must not reach, and a few it
    /// must.
    #[test]
    fn public_ip_classification() {
        for ip in [
            "127.0.0.1", "127.255.0.9", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.0.1",
            "169.254.169.254", "100.64.0.1", "100.127.255.255", "0.0.0.0", "0.1.2.3", "192.0.0.8",
            "192.0.2.1", "198.51.100.7", "203.0.113.9", "198.18.0.1", "224.0.0.1", "240.0.0.1",
            "255.255.255.255", "::", "::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1", "::127.0.0.1",
            "64:ff9b::a9fe:a9fe", "64:ff9b:1::1", "2002:c0a8:0101::1", "fd00::1", "fc00::1",
            "fe80::1", "fec0::1", "ff02::1", "2001:db8::1", "100::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} must not count as public");
        }
        for ip in [
            "1.1.1.1", "8.8.8.8", "104.196.9.116", "172.15.0.1", "172.32.0.1", "100.63.0.1",
            "100.128.0.1", "192.169.0.1", "2606:4700::1111", "::ffff:8.8.8.8", "2002:0808:0808::1",
            "64:ff9b::808:808",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} is public");
        }
    }

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    /// A policy for `base` with no proxy configured.
    fn site(base: &str) -> ContentPolicy {
        ContentPolicy::with_proxy_env(base, |_| None)
    }

    /// A policy for `base` whose HTTPS_PROXY and HTTP_PROXY are `proxy`.
    fn site_behind(base: &str, proxy: &str) -> ContentPolicy {
        let proxy = proxy.to_string();
        ContentPolicy::with_proxy_env(base, move |name| matches!(name, "HTTPS_PROXY" | "HTTP_PROXY").then(|| proxy.clone()))
    }

    async fn resolve(policy: &ContentPolicy, lane: ContentLane, name: &str) -> std::result::Result<Vec<std::net::IpAddr>, String> {
        use reqwest::dns::Resolve;
        let resolver = PublicOnlyResolver { policy: Arc::new(policy.clone()), lane };
        match resolver.resolve(name.parse().unwrap()).await {
            Ok(addrs) => Ok(addrs.map(|a| a.ip()).collect()),
            Err(e) => {
                assert!(non_public_refusal(e.as_ref()).is_some(), "{name}: not a refusal: {e}");
                Err(e.to_string())
            }
        }
    }

    /// #130 and #146: only the forum's exact origin skips the literal
    /// checks. The same host on another port or scheme does not.
    #[test]
    fn check_url_exempts_only_the_forum_origin() {
        let local = site("http://127.0.0.1:8080");
        assert!(local.check_url(&url("http://127.0.0.1:8080/a.png")).is_ok());
        assert!(local.check_url(&url("http://127.0.0.1:9999/a.png")).is_err(), "another port is not the forum");
        assert!(local.check_url(&url("https://127.0.0.1:8080/a.png")).is_err(), "another scheme is not the forum");
        let named = site("https://forum.example");
        assert!(named.check_url(&url("http://127.0.0.1:8080/a.png")).is_err());
        assert!(named.check_url(&url("https://cdn.example/a.png")).is_ok());
        assert!(named.check_url(&url("https://127.0.0.1@cdn.example/a.png")).is_ok(), "userinfo is not the host");
        assert!(named.check_url(&url("https://cdn.example@127.0.0.1/a.png")).is_err());
        assert!(named.check_url(&url("file:///etc/passwd")).is_err());
        let dev = site("http://localhost:3000");
        assert!(dev.check_url(&url("http://localhost:3000/a.png")).is_ok());
        assert!(dev.check_url(&url("http://localhost:3001/a.png")).is_err(), "another port on localhost is refused");
    }

    /// #146: the `OwnMedia` lane, the one that keeps a non-public answer
    /// for the forum's own name, takes only the exact origin, without
    /// userinfo, on a media path. A local site takes its whole origin.
    #[test]
    fn own_media_lane_is_the_exact_origin_on_media_paths() {
        use ContentLane::{OwnMedia, Strict};
        let named = site("https://forum.example");
        for (u, lane) in [
            ("https://forum.example/data/avatars/m/0/1.jpg?123", OwnMedia),
            ("https://FORUM.example:443/attachments/a-png.12/", OwnMedia),
            ("https://forum.example/media/x.3/full", OwnMedia),
            ("https://forum.example/styles/default/xenforo/smilies/a.png", OwnMedia),
            ("https://forum.example/proxy.php?image=https%3A%2F%2Fcdn.example%2Fa.png&hash=1", OwnMedia),
            ("https://forum.example/admin.php", Strict),
            ("https://forum.example/proxy.php.bak", Strict),
            ("https://forum.example/datax/a.png", Strict),
            ("https://forum.example/data/../admin.php", Strict),
            ("https://forum.example/data/%2e%2e/admin.php", Strict),
            ("https://forum.example/data/..%2fadmin.php", Strict),
            ("http://forum.example/data/a.png", Strict),
            ("https://forum.example:8443/data/a.png", Strict),
            ("https://user@forum.example/data/a.png", Strict),
            ("https://forum.example./data/a.png", Strict),
            ("https://cdn.example/data/a.png", Strict),
        ] {
            assert_eq!(named.lane(&url(u)), lane, "{u}");
        }
        assert_eq!(site("http://localhost:3000").lane(&url("http://localhost:3000/anything")), OwnMedia);
        assert_eq!(site("https://192.168.1.10").lane(&url("https://192.168.1.10/index.php")), OwnMedia);
    }

    /// #146, the name half: a forum name that resolves to loopback (the
    /// client running on the forum's own server, or split-horizon DNS)
    /// keeps that answer in the `OwnMedia` lane only. In the `Strict`
    /// lane it is judged like any other name. The old resolver kept it
    /// for every request to that name, on any port or path.
    #[tokio::test]
    async fn the_forum_name_keeps_a_loopback_answer_only_in_the_own_media_lane() {
        let policy = site("https://forum.example")
            .with_fixed_dns("forum.example", &["127.0.0.1"])
            .with_fixed_dns("other.example", &["10.0.0.1"]);
        assert_eq!(resolve(&policy, ContentLane::OwnMedia, "forum.example").await.unwrap(), ["127.0.0.1".parse::<std::net::IpAddr>().unwrap()]);
        assert!(resolve(&policy, ContentLane::Strict, "forum.example").await.is_err());
        assert!(resolve(&policy, ContentLane::Strict, "FORUM.example.").await.is_err());
        assert!(resolve(&policy, ContentLane::OwnMedia, "other.example").await.is_err());
        // The system resolver, for the dev-forum case the wiremock tests use.
        let dev = site("http://localhost:3000");
        let kept = resolve(&dev, ContentLane::OwnMedia, "localhost").await.unwrap();
        assert!(!kept.is_empty() && kept.iter().all(|ip| ip.is_loopback()), "{kept:?}");
        assert!(resolve(&dev, ContentLane::Strict, "localhost").await.is_err());
    }

    /// #146, the redirect half: the `OwnMedia` client may not be sent to
    /// another port or path on the forum's own name, because its resolver
    /// would keep that name's non-public address. Other names are fine:
    /// its resolver filters them like the `Strict` one does.
    #[test]
    fn own_media_redirects_stay_on_own_media() {
        let policy = site("https://forum.example");
        let own = ContentLane::OwnMedia;
        assert!(policy.hop_check(&url("https://forum.example/data/b.png"), own).is_ok());
        assert!(policy.hop_check(&url("https://cdn.example/a.png"), own).is_ok());
        for hop in [
            "https://forum.example/admin.php",
            "https://forum.example:8444/data/a.png",
            "http://forum.example/data/a.png",
            "https://Forum.Example./data/a.png",
        ] {
            let err = policy.hop_check(&url(hop), own).expect_err(hop);
            assert!(err.0.contains("own media"), "{hop}: {}", err.0);
        }
        // The Strict client may follow it: its resolver judges the name.
        assert!(policy.hop_check(&url("https://forum.example/admin.php"), ContentLane::Strict).is_ok());
        assert!(!policy.resolver_exempts("forum.example", ContentLane::Strict));
    }

    /// #147: the proxy names are read from the variables reqwest reads,
    /// as the connector will pass them to the resolver.
    #[test]
    fn proxy_hosts_are_read_from_the_proxy_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
        };
        let policy = ContentPolicy::with_proxy_env(
            "https://forum.example",
            env(&[
                ("HTTPS_PROXY", "http://user:p%40ss@Proxy.Corp.Example.:3128/"),
                ("http_proxy", "localhost:3128"),
                ("ALL_PROXY", "socks5h://[fd00::1]:1080"),
                ("HTTP_PROXY", ""),
            ]),
        );
        // ALL_PROXY, HTTPS_PROXY, then http_proxy: an empty HTTP_PROXY names
        // nothing. reqwest itself would skip http_proxy here (an empty
        // HTTP_PROXY is still "set"); listing it anyway only refuses it as a
        // content target.
        assert_eq!(policy.proxy_hosts, ["fd00::1", "proxy.corp.example", "localhost"]);
        let cgi = ContentPolicy::with_proxy_env(
            "https://forum.example",
            env(&[("HTTPS_PROXY", "http://proxy.corp.example:3128"), ("REQUEST_METHOD", "GET")]),
        );
        assert!(cgi.proxy_hosts.is_empty(), "reqwest uses no proxy in a CGI process");
        assert!(site("https://forum.example").proxy_hosts.is_empty());
    }

    /// #147: a proxy named by a hostname that resolves to a private
    /// address is the user's own infrastructure. The resolver keeps that
    /// answer in both lanes; the old one refused it, which failed every
    /// content image behind a corporate or local proxy. Content still may
    /// not name the proxy as a target.
    #[tokio::test]
    async fn a_named_private_proxy_is_kept_but_never_a_content_target() {
        let policy = site_behind("https://forum.example", "http://proxy.corp.example:3128")
            .with_fixed_dns("proxy.corp.example", &["10.1.2.3"]);
        let ten: std::net::IpAddr = "10.1.2.3".parse().unwrap();
        assert_eq!(resolve(&policy, ContentLane::Strict, "proxy.corp.example").await.unwrap(), [ten]);
        assert_eq!(resolve(&policy, ContentLane::OwnMedia, "proxy.corp.example").await.unwrap(), [ten]);
        for target in ["http://proxy.corp.example:8080/a.png", "https://PROXY.corp.example./a.png"] {
            let err = policy.check_url(&url(target)).expect_err(target);
            assert!(err.0.contains("configured proxy"), "{target}: {}", err.0);
        }
    }

    /// #147: behind a proxy, a target's name is looked up here before the
    /// request, since the proxy, not our resolver, resolves it. Any
    /// non-public address refuses it. A name that does not resolve here is
    /// left to the proxy. Without a proxy nothing is looked up, because the
    /// resolver already judges every name.
    #[tokio::test]
    async fn a_proxied_target_is_looked_up_before_it_is_sent() {
        let policy = site_behind("https://forum.example", "http://proxy.corp.example:3128")
            .with_fixed_dns("intranet.example", &["10.0.0.5"])
            .with_fixed_dns("mixed.example", &["8.8.8.8", "127.0.0.1"])
            .with_fixed_dns("cdn.example", &["8.8.8.8"])
            .with_fixed_dns("proxy-only.example", &[]);
        for target in ["http://intranet.example/a.png", "https://mixed.example/a.png"] {
            let err = policy.preflight(&url(target)).await.expect_err(target);
            assert!(err.0.contains("non-public address"), "{target}: {}", err.0);
        }
        assert!(policy.preflight(&url("https://cdn.example/a.png")).await.is_ok());
        assert!(policy.preflight(&url("https://proxy-only.example/a.png")).await.is_ok());
        assert!(policy.preflight(&url("https://8.8.8.8/a.png")).await.is_ok());
        let direct = site("https://forum.example").with_fixed_dns("intranet.example", &["10.0.0.5"]);
        assert!(direct.preflight(&url("http://intranet.example/a.png")).await.is_ok());
    }

    #[tokio::test]
    async fn redirect_between_loopback_hosts_still_follows() {
        let target = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&target)
            .await;
        let hop = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/final", target.uri())),
            )
            .mount(&hop)
            .await;
        let client = build().unwrap();
        let resp = client.get(hop.uri()).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    // #854 (follow-up): previous()[0] is the initial URL, not a redirect —
    // the boundary is `> MAX_HOPS`, exactly like the Policy::limited(4) this
    // policy replaced. An earlier draft used `>=` and followed one hop fewer
    // than the stock policy did.
    #[tokio::test]
    async fn four_redirect_hops_still_follow() {
        let client = build().unwrap();
        let mut servers = Vec::new();
        for _ in 0..5 {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("end"))
                .mount(&server)
                .await;
            servers.push(server);
        }
        // Chain server[i] -> server[i+1]: 4 redirects, 5 servers.
        for i in 0..4 {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .respond_with(
                    wiremock::ResponseTemplate::new(302)
                        .insert_header("Location", format!("{}/hop", servers[i + 1].uri())),
                )
                .mount(&servers[i])
                .await;
        }
        let resp = client.get(servers[0].uri()).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "end");
    }
}
