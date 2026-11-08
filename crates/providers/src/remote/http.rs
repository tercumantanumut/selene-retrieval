//! The HTTP client every remote provider call uses (genai is handed this client).
//!
//! * TLS: rustls with the **ring** provider only (Amendment 3.13; aws-lc is banned) and the
//!   Windows trust store (reqwest's platform verifier), so corporate TLS inspection with a
//!   company root works like in the browser.
//! * Proxy: the per-host [`ProxyPlan`] (settings override → Windows / PAC → WinHTTP → direct);
//!   Basic proxy credentials from Credential Manager, **only** to a proxy the user or an
//!   administrator configured explicitly (S-07): never to one a PAC script / WPAD returned.
//! * Redirects (S-06): at most 5, and only to the **same origin** (scheme, host, port) over
//!   `https://` (or loopback `http://`). reqwest strips only `Authorization` on a cross-host
//!   redirect; our keys also travel in `x-api-key` / `x-goog-api-key`, and a 307/308 would replay
//!   the prompt body, so a cross-origin redirect is never followed (the call fails instead).
//! * Timeouts: connect 15 s; per-read idle timeout = the attempt timeout (a stalled stream fails
//!   instead of hanging). The whole-attempt timeout is enforced by `Egress`.
//! * User-Agent `SeleneRetrieval/<version>`; no user identifiers are ever added.

use std::time::Duration;

use sr_common::Result;
use sr_common::error::{Error, ErrorCode};

use super::proxy::{ProxyChoice, ProxyPlan, ProxySource, is_loopback};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// `SeleneRetrieval/<crate version>`.
pub fn user_agent() -> String {
    format!("SeleneRetrieval/{}", env!("CARGO_PKG_VERSION"))
}

/// Install the ring rustls provider for this process (idempotent; same as the downloader's).
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn url_allowed(u: &reqwest::Url) -> bool {
    match u.scheme() {
        "https" => true,
        "http" => u.host_str().is_some_and(is_loopback),
        _ => false,
    }
}

/// S-06: a redirect is followed only within the origin of the request that started the chain
/// (same scheme, host and port), so no key header or prompt body reaches another host.
fn redirect_allowed(next: &reqwest::Url, first: &reqwest::Url) -> bool {
    url_allowed(next)
        && next.scheme() == first.scheme()
        && next
            .host_str()
            .zip(first.host_str())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        && next.port_or_known_default() == first.port_or_known_default()
}

/// S-07: the proxy URL for one request. Basic credentials go into it only when the proxy was
/// configured explicitly (Advanced override, the user's manual Windows proxy, the machine
/// WinHTTP proxy); a proxy a PAC script or WPAD answered gets none.
fn proxy_url(
    p: &str,
    source: ProxySource,
    creds: Option<&(String, String)>,
) -> Option<reqwest::Url> {
    let mut u = reqwest::Url::parse(p).ok()?;
    let explicit = matches!(
        source,
        ProxySource::Override | ProxySource::UserSettings | ProxySource::MachineWinHttp
    );
    // Never forward userinfo a PAC script put into its answer either.
    let _ = u.set_username("");
    let _ = u.set_password(None);
    if let (true, Some((user, pass))) = (explicit, creds) {
        u.set_username(user).ok()?;
        u.set_password(Some(pass)).ok()?;
    }
    Some(u)
}

pub fn build_client(plan: &ProxyPlan, read_timeout: Duration) -> Result<reqwest::Client> {
    ensure_crypto_provider();
    let policy = reqwest::redirect::Policy::custom(|a| {
        let first = a.previous().first().cloned();
        if a.previous().len() >= 5 {
            a.error("too many redirects")
        } else if first.is_some_and(|f| redirect_allowed(a.url(), &f)) {
            a.follow()
        } else {
            a.stop()
        }
    });
    let mut b = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout)
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(policy)
        .tls_backend_rustls();
    if !plan.system_fallback {
        // Our plan is authoritative: no second guess from the registry.
        b = b.no_proxy();
        let plan_c = plan.clone();
        let proxy = reqwest::Proxy::custom(move |url| {
            let host = url.host_str()?;
            match plan_c.choice_for(host) {
                ProxyChoice::Direct => None,
                ProxyChoice::Via(p) => {
                    proxy_url(&p, plan_c.source_for(host), plan_c.credentials.as_ref())
                }
            }
        });
        b = b.proxy(proxy);
    }
    b.build()
        .map_err(|e| Error::new(ErrorCode::Internal).with_detail(format!("http client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_targets() {
        let ok = |s: &str| {
            reqwest::Url::parse(s)
                .map(|u| url_allowed(&u))
                .unwrap_or(false)
        };
        assert!(ok("https://openrouter.ai/x"));
        assert!(ok("http://127.0.0.1:11434/v1/"));
        assert!(!ok("http://evil.example/"));
        assert!(!ok("file:///c:/x"));
    }

    #[test]
    fn redirects_stay_on_the_same_origin() {
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        let first = u("https://api.anthropic.com/v1/messages");
        assert!(redirect_allowed(
            &u("https://api.anthropic.com/v1/x"),
            &first
        ));
        assert!(redirect_allowed(
            &u("https://API.anthropic.com:443/y"),
            &first
        ));
        // S-06: another host, a downgrade or another port never sees the key or the body.
        assert!(!redirect_allowed(
            &u("https://evil.example/v1/messages"),
            &first
        ));
        assert!(!redirect_allowed(
            &u("https://anthropic.com/v1/messages"),
            &first
        ));
        assert!(!redirect_allowed(
            &u("http://api.anthropic.com/v1/x"),
            &first
        ));
        assert!(!redirect_allowed(
            &u("https://api.anthropic.com:8443/v1/x"),
            &first
        ));
        let local = u("http://127.0.0.1:11434/v1/chat");
        assert!(redirect_allowed(
            &u("http://127.0.0.1:11434/v1/other"),
            &local
        ));
        assert!(!redirect_allowed(
            &u("http://127.0.0.1:9999/v1/other"),
            &local
        ));
    }

    #[test]
    fn proxy_credentials_only_for_explicit_proxies() {
        let creds = ("alice".to_string(), "p@ss:w/rd".to_string());
        for s in [
            ProxySource::Override,
            ProxySource::UserSettings,
            ProxySource::MachineWinHttp,
        ] {
            let u = proxy_url("http://proxy.corp:8080", s, Some(&creds)).unwrap();
            assert_eq!(u.username(), "alice", "{s:?}");
            assert!(u.password().is_some(), "{s:?}");
        }
        // S-07: a PAC / WPAD answer gets no credentials, and its own userinfo is dropped.
        let u = proxy_url(
            "http://wpad-evil:3128",
            ProxySource::AutoConfig,
            Some(&creds),
        )
        .unwrap();
        assert_eq!((u.username(), u.password()), ("", None));
        let u = proxy_url("http://x:y@pac-proxy:3128", ProxySource::AutoConfig, None).unwrap();
        assert_eq!((u.username(), u.password()), ("", None));
    }

    #[test]
    fn builds_with_and_without_plan() {
        let plan = ProxyPlan::default();
        assert!(build_client(&plan, Duration::from_secs(5)).is_ok());
        let fallback = ProxyPlan {
            system_fallback: true,
            ..ProxyPlan::default()
        };
        assert!(build_client(&fallback, Duration::from_secs(5)).is_ok());
    }
}
