//! Shared HTTP client construction. `build()` bakes in the UA and timeouts
//! every caller must use; the pool it hands back is what steady-state traffic
//! rides. In practice the long-lived client is the `WfApiClient`'s — one
//! pooled client keeps us inside Cloudflare's connection expectations rather
//! than opening a fresh TLS session per call — but the process is not limited
//! to exactly one: the login flow and the logout revokes build short-lived
//! clients of their own, deliberately outside the session's pool (#661).

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


/// The client for bytes that forum *content* names (`WfApiClient::fetch_bytes`
/// for anything but our own API): an `[IMG]` URL is written by whoever wrote
/// the post, and it is fetched the moment a reader scrolls past it, so it
/// must not be able to aim this machine at its own loopback, its LAN or a
/// cloud metadata endpoint (#130). Three layers, because each one alone has
/// a hole:
///
/// - `check_public_url` refuses the initial URL when its host is a
///   non-public IP literal (or `localhost`), before anything is sent;
/// - the redirect policy runs the same check on every hop;
/// - the DNS resolver drops every non-public address a *name* resolves to,
///   so neither a hostname pointing at 127.0.0.1 nor a rebinding answer can
///   slip past the literal check — the connection only ever sees what the
///   resolver kept.
///
/// `own_host` — the forum's own host — is exempt from all three: the user
/// chose that site, and a forum on a LAN address (or the wiremock tests on
/// 127.0.0.1) must still be able to show its own avatars.
pub fn build_public_with_ua(ua: &str, own_host: Option<String>) -> Result<reqwest::Client> {
    install_crypto_provider();
    Ok(reqwest::Client::builder()
        .user_agent(ua)
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(config::REQUEST_TIMEOUT)
        .dns_resolver(PublicOnlyResolver { own_host: own_host.clone() })
        .redirect(hop_policy(Some(own_host)))
        .build()?)
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

/// The initial-URL (and per-hop) half of the #130 gate: http or https, a
/// host, and — unless it is the forum's own host — not a non-public IP
/// literal and not `localhost`. Hostnames are judged by what they resolve
/// to, in `PublicOnlyResolver`. The host is the URL's *parsed* host, which
/// the URL parser has already normalised (`http://2130706433/` is
/// `127.0.0.1` by then, and userinfo is not part of it).
pub fn check_public_url(url: &reqwest::Url, own_host: Option<&str>) -> std::result::Result<(), NonPublicDestination> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(NonPublicDestination(format!("refusing a {} URL from forum content", url.scheme())));
    }
    let Some(host) = url.host_str() else {
        return Err(NonPublicDestination("refusing a URL with no host".into()));
    };
    if own_host.is_some_and(|own| own.eq_ignore_ascii_case(host)) {
        return Ok(());
    }
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = bare.parse::<std::net::IpAddr>()
        && !is_public_ip(ip)
    {
        return Err(NonPublicDestination(format!("refusing to fetch from non-public address {host}")));
    }
    let lower = bare.trim_end_matches('.').to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Err(NonPublicDestination(format!("refusing to fetch from {host}")));
    }
    Ok(())
}

/// Resolves a name the way the system does, then keeps only the public
/// addresses (all of them, for the forum's own host). An answer with no
/// public address left is a refusal, not an empty success.
struct PublicOnlyResolver {
    own_host: Option<String>,
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let exempt = self.own_host.as_deref().is_some_and(|own| own.eq_ignore_ascii_case(&host));
        Box::pin(async move {
            let found: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
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
/// `public_only` is `Some(own_host)` for the content client
/// (`build_public_with_ua`): every hop must also pass `check_public_url`,
/// so a public image URL cannot 302 the fetch onto a LAN address (#130).
fn hop_policy(public_only: Option<Option<String>>) -> reqwest::redirect::Policy {
    const MAX_HOPS: usize = 4;
    reqwest::redirect::Policy::custom(move |attempt| {
        if let Some(own_host) = &public_only
            && let Err(refusal) = check_public_url(attempt.url(), own_host.as_deref())
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

    #[test]
    fn check_public_url_exempts_only_the_forum_host() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(check_public_url(&url("http://127.0.0.1:8080/a.png"), Some("127.0.0.1")).is_ok());
        assert!(check_public_url(&url("http://127.0.0.1:8080/a.png"), Some("forum.example")).is_err());
        assert!(check_public_url(&url("https://cdn.example/a.png"), Some("forum.example")).is_ok());
        assert!(check_public_url(&url("https://127.0.0.1@cdn.example/a.png"), None).is_ok(), "userinfo is not the host");
        assert!(check_public_url(&url("https://cdn.example@127.0.0.1/a.png"), None).is_err());
        assert!(check_public_url(&url("file:///etc/passwd"), None).is_err());
    }

    /// The name half: a hostname is judged by what it resolves to, so a
    /// name pointing at loopback is refused even though no literal check
    /// could see it — unless it is the forum's own host.
    #[tokio::test]
    async fn resolver_drops_non_public_answers_except_for_the_forum_host() {
        use reqwest::dns::Resolve;
        let guarded = PublicOnlyResolver { own_host: Some("forum.example".into()) };
        let err = match guarded.resolve("localhost".parse().unwrap()).await {
            Ok(addrs) => panic!("localhost resolved through the guard to {:?}", addrs.collect::<Vec<_>>()),
            Err(e) => e,
        };
        assert!(non_public_refusal(err.as_ref()).is_some(), "{err}");
        let own = PublicOnlyResolver { own_host: Some("localhost".into()) };
        let addrs: Vec<_> = own.resolve("localhost".parse().unwrap()).await.unwrap().collect();
        assert!(addrs.iter().all(|a| a.ip().is_loopback()) && !addrs.is_empty(), "{addrs:?}");
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
