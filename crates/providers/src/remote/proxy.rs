//! System proxy for provider calls (U9, R-UX10, M3 exit "works behind an authenticated corporate
//! proxy").
//!
//! Order, per provider host (resolved once when the provider client is built, off the async
//! threads, because PAC/WPAD can take seconds):
//! 1. Loopback hosts (Ollama, vLLM, tests) are always **direct**.
//! 2. The Advanced override `network.proxy` (`http://`, `https://`, `socks5://`), if set.
//! 3. The user's Windows proxy settings (the "Internet Options" / Settings → Proxy values) via
//!    WinHTTP: `WinHttpGetIEProxyConfigForCurrentUser`; with "Automatically detect" or a setup
//!    script, `WinHttpGetProxyForUrl` evaluates WPAD / the PAC file for that URL.
//! 4. The machine-wide WinHTTP proxy (`netsh winhttp set proxy`).
//! 5. Direct.
//!
//! If the WinHTTP calls are unavailable, reqwest's own `system-proxy` registry reading stays on
//! as the fallback ([`ProxyPlan::system_fallback`]).
//!
//! Proxy credentials (Basic) come from Credential Manager (`secrets::PROXY_ACCOUNT`, stored as
//! `user:password`), never from `settings.toml`. NTLM / Kerberos proxy sign-in isn't supported by
//! reqwest: such a proxy answers 407 and the call fails with `NetworkBlocked` (open item).

use std::collections::HashMap;

/// Decision for one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyChoice {
    Direct,
    /// Proxy URL (`http://host:port`, `socks5://…`).
    Via(String),
}

/// Where a decision came from (diagnostics only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxySource {
    Loopback,
    Override,
    UserSettings,
    AutoConfig,
    MachineWinHttp,
    None,
}

/// Per-host proxy decisions for one provider client.
#[derive(Debug, Clone, Default)]
pub struct ProxyPlan {
    pub per_host: HashMap<String, (ProxyChoice, ProxySource)>,
    /// Basic credentials for the proxy (`user`, `password`).
    pub credentials: Option<(String, String)>,
    /// WinHTTP could not be asked: leave reqwest's registry-based system proxy on.
    pub system_fallback: bool,
}

impl ProxyPlan {
    /// Choice for `host` (`Direct` when the host wasn't planned: never guess a proxy).
    pub fn choice_for(&self, host: &str) -> ProxyChoice {
        if is_loopback(host) {
            return ProxyChoice::Direct;
        }
        self.per_host
            .get(&host.to_ascii_lowercase())
            .map_or(ProxyChoice::Direct, |(c, _)| c.clone())
    }
    /// Where the decision for `host` came from (S-07: credentials only for explicit proxies).
    pub fn source_for(&self, host: &str) -> ProxySource {
        if is_loopback(host) {
            return ProxySource::Loopback;
        }
        self.per_host
            .get(&host.to_ascii_lowercase())
            .map_or(ProxySource::None, |(_, s)| *s)
    }
    /// One line per host for "Copy diagnostics" (no credentials).
    pub fn describe(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .per_host
            .iter()
            .map(|(h, (c, s))| match c {
                ProxyChoice::Direct => format!("{h}: direct ({s:?})"),
                ProxyChoice::Via(p) => format!("{h}: via {} ({s:?})", strip_userinfo(p)),
            })
            .collect();
        v.sort();
        v
    }
}

/// `user:password` (as stored in Credential Manager) → pair.
pub fn parse_credentials(s: &str) -> Option<(String, String)> {
    let (u, p) = s.split_once(':')?;
    (!u.is_empty()).then(|| (u.to_string(), p.to_string()))
}

pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h == "::1" || h.starts_with("127.")
}

fn strip_userinfo(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.rsplit_once('@') {
            Some((_, host)) => format!("{scheme}://{host}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// Host part of an absolute URL, lowercased (`None` for anything unparsable).
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// A WinHTTP / PAC proxy list → the proxy for `scheme` (`"https"` / `"http"`).
///
/// Accepts `host:port`, `http=h:p;https=h2:p2`, `PROXY h:p; DIRECT` (PAC results),
/// `SOCKS h:p`, and lists separated by `;` or whitespace. The first usable entry wins; `DIRECT`
/// first means no proxy.
pub fn parse_proxy_list(list: &str, scheme: &str) -> Option<ProxyChoice> {
    let entries: Vec<&str> = list
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    // Per-scheme form: `http=…;https=…;socks=…`.
    if entries.iter().any(|e| e.contains('=')) {
        let pick = |key: &str| {
            entries.iter().find_map(|e| {
                let (k, v) = e.split_once('=')?;
                k.trim()
                    .eq_ignore_ascii_case(key)
                    .then(|| v.trim().to_string())
            })
        };
        if let Some(v) = pick(scheme) {
            return Some(ProxyChoice::Via(with_scheme(&v, "http")));
        }
        if let Some(v) = pick("socks") {
            return Some(ProxyChoice::Via(with_scheme(&v, "socks5")));
        }
        return None;
    }
    for e in entries {
        let mut words = e.split_whitespace();
        let first = words.next()?;
        match first.to_ascii_uppercase().as_str() {
            "DIRECT" => return Some(ProxyChoice::Direct),
            "PROXY" | "HTTP" => {
                if let Some(hp) = words.next() {
                    return Some(ProxyChoice::Via(with_scheme(hp, "http")));
                }
            }
            "HTTPS" => {
                if let Some(hp) = words.next() {
                    return Some(ProxyChoice::Via(with_scheme(hp, "https")));
                }
            }
            "SOCKS" | "SOCKS5" => {
                if let Some(hp) = words.next() {
                    return Some(ProxyChoice::Via(with_scheme(hp, "socks5")));
                }
            }
            _ => return Some(ProxyChoice::Via(with_scheme(first, "http"))),
        }
    }
    None
}

fn with_scheme(hostport: &str, scheme: &str) -> String {
    if hostport.contains("://") {
        hostport.to_string()
    } else {
        format!("{scheme}://{hostport}")
    }
}

/// Windows bypass list (`<local>;*.corp.example;10.*`): whether `host` goes direct.
pub fn bypassed(host: &str, bypass: &str) -> bool {
    let host = host.to_ascii_lowercase();
    bypass
        .split([';', ' ', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|pat| {
            let pat = pat.to_ascii_lowercase();
            if pat == "<local>" {
                return !host.contains('.');
            }
            // Strip a scheme / port from the pattern (rare, but allowed by Windows).
            let pat = pat.split("://").last().unwrap_or(&pat);
            let pat = pat.split(':').next().unwrap_or(pat);
            wildcard_match(pat, &host)
        })
}

fn wildcard_match(pat: &str, s: &str) -> bool {
    // `*` matches any run; everything else literal. Small inputs: simple DP.
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
        for j in 1..=t.len() {
            dp[i][j] = if p[i - 1] == '*' {
                dp[i - 1][j] || dp[i][j - 1]
            } else {
                dp[i - 1][j - 1] && p[i - 1] == t[j - 1]
            };
        }
    }
    dp[p.len()][t.len()]
}

/// What Windows reports (one snapshot), as plain data so the decision logic is testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsProxyConfig {
    pub auto_detect: bool,
    pub auto_config_url: Option<String>,
    pub proxy: Option<String>,
    pub bypass: Option<String>,
    /// `netsh winhttp` machine proxy (`proxy`, `bypass`).
    pub machine_proxy: Option<String>,
    pub machine_bypass: Option<String>,
}

/// Pure decision for `url` given the Windows config and (when auto-config applies) the result of
/// evaluating WPAD/PAC (`auto_result`: proxy list, bypass).
pub fn decide(
    url: &str,
    cfg: &WindowsProxyConfig,
    auto_result: Option<(Option<String>, Option<String>)>,
) -> (ProxyChoice, ProxySource) {
    let Some(host) = host_of(url) else {
        return (ProxyChoice::Direct, ProxySource::None);
    };
    let scheme = if url.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    if is_loopback(&host) {
        return (ProxyChoice::Direct, ProxySource::Loopback);
    }
    if let Some((list, bypass)) = auto_result {
        if bypass.as_deref().is_some_and(|b| bypassed(&host, b)) {
            return (ProxyChoice::Direct, ProxySource::AutoConfig);
        }
        if let Some(c) = list.as_deref().and_then(|l| parse_proxy_list(l, scheme)) {
            return (c, ProxySource::AutoConfig);
        }
        return (ProxyChoice::Direct, ProxySource::AutoConfig);
    }
    if let Some(p) = cfg.proxy.as_deref() {
        if cfg.bypass.as_deref().is_some_and(|b| bypassed(&host, b)) {
            return (ProxyChoice::Direct, ProxySource::UserSettings);
        }
        if let Some(c) = parse_proxy_list(p, scheme) {
            return (c, ProxySource::UserSettings);
        }
    }
    if let Some(p) = cfg.machine_proxy.as_deref() {
        if cfg
            .machine_bypass
            .as_deref()
            .is_some_and(|b| bypassed(&host, b))
        {
            return (ProxyChoice::Direct, ProxySource::MachineWinHttp);
        }
        if let Some(c) = parse_proxy_list(p, scheme) {
            return (c, ProxySource::MachineWinHttp);
        }
    }
    (ProxyChoice::Direct, ProxySource::None)
}

/// Plans the proxy for every URL a provider client will call. Blocking (WinHTTP / WPAD): call it
/// through `spawn_blocking` ([`plan_async`]).
pub fn plan_blocking(
    urls: &[String],
    override_url: Option<&str>,
    credentials: Option<(String, String)>,
) -> ProxyPlan {
    let mut plan = ProxyPlan {
        credentials,
        ..ProxyPlan::default()
    };
    let all_local = urls
        .iter()
        .all(|u| host_of(u).is_none_or(|h| is_loopback(&h)));
    let snapshot = if override_url.is_some() || all_local {
        None
    } else {
        win::snapshot()
    };
    if override_url.is_none() && !all_local && snapshot.is_none() {
        plan.system_fallback = cfg!(windows);
    }
    for url in urls {
        let Some(host) = host_of(url) else { continue };
        let decision = if is_loopback(&host) {
            (ProxyChoice::Direct, ProxySource::Loopback)
        } else if let Some(o) = override_url {
            (ProxyChoice::Via(o.to_string()), ProxySource::Override)
        } else if let Some(cfg) = &snapshot {
            let auto = (cfg.auto_detect || cfg.auto_config_url.is_some())
                .then(|| win::proxy_for_url(url, cfg.auto_detect, cfg.auto_config_url.as_deref()))
                .flatten();
            decide(url, cfg, auto)
        } else {
            (ProxyChoice::Direct, ProxySource::None)
        };
        plan.per_host.insert(host, decision);
    }
    plan
}

/// [`plan_blocking`] on the blocking pool.
pub async fn plan_async(
    urls: Vec<String>,
    override_url: Option<String>,
    credentials: Option<(String, String)>,
) -> ProxyPlan {
    let fallback = ProxyPlan {
        system_fallback: true,
        credentials: credentials.clone(),
        ..ProxyPlan::default()
    };
    tokio::task::spawn_blocking(move || plan_blocking(&urls, override_url.as_deref(), credentials))
        .await
        .unwrap_or(fallback)
}

#[cfg(windows)]
mod win;

#[cfg(not(windows))]
mod win {
    pub(super) fn snapshot() -> Option<super::WindowsProxyConfig> {
        None
    }
    pub(super) fn proxy_for_url(
        _url: &str,
        _auto_detect: bool,
        _pac: Option<&str>,
    ) -> Option<(Option<String>, Option<String>)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_list_forms() {
        let via = |s: &str| Some(ProxyChoice::Via(s.into()));
        assert_eq!(
            parse_proxy_list("proxy.corp:8080", "https"),
            via("http://proxy.corp:8080")
        );
        assert_eq!(
            parse_proxy_list("http=a:1;https=b:2", "https"),
            via("http://b:2")
        );
        assert_eq!(parse_proxy_list("http=a:1", "https"), None);
        assert_eq!(
            parse_proxy_list("socks=s:1080", "https"),
            via("socks5://s:1080")
        );
        assert_eq!(
            parse_proxy_list("PROXY p1:3128; DIRECT", "https"),
            via("http://p1:3128")
        );
        assert_eq!(
            parse_proxy_list("DIRECT", "https"),
            Some(ProxyChoice::Direct)
        );
        assert_eq!(parse_proxy_list("", "https"), None);
    }

    #[test]
    fn bypass_rules() {
        assert!(bypassed("intranet", "<local>"));
        assert!(!bypassed("api.openai.com", "<local>"));
        assert!(bypassed("svc.corp.example", "*.corp.example;10.*"));
        assert!(bypassed("10.1.2.3", "*.corp.example;10.*"));
        assert!(!bypassed("openrouter.ai", "*.corp.example;10.*"));
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("https://OpenRouter.ai/api/v1/").as_deref(),
            Some("openrouter.ai")
        );
        assert_eq!(
            host_of("http://u:p@[::1]:11434/v1/").as_deref(),
            Some("::1")
        );
        assert!(is_loopback("127.0.0.1") && is_loopback("localhost") && is_loopback("::1"));
        assert_eq!(strip_userinfo("http://u:p@proxy:8080"), "http://proxy:8080");
    }

    #[test]
    fn decision_order() {
        let cfg = WindowsProxyConfig {
            proxy: Some("user.proxy:8080".into()),
            bypass: Some("*.corp".into()),
            machine_proxy: Some("machine.proxy:3128".into()),
            ..Default::default()
        };
        let url = "https://api.deepseek.com/models";
        assert_eq!(
            decide(url, &cfg, None),
            (
                ProxyChoice::Via("http://user.proxy:8080".into()),
                ProxySource::UserSettings
            )
        );
        // PAC result wins over the static settings.
        let auto = Some((Some("PROXY pac.proxy:80".into()), None));
        assert_eq!(
            decide(url, &cfg, auto).0,
            ProxyChoice::Via("http://pac.proxy:80".into())
        );
        // Bypassed by the user list.
        assert_eq!(
            decide("https://x.corp/v1", &cfg, None).0,
            ProxyChoice::Direct
        );
        // Only the machine proxy.
        let m = WindowsProxyConfig {
            machine_proxy: Some("machine.proxy:3128".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(url, &m, None),
            (
                ProxyChoice::Via("http://machine.proxy:3128".into()),
                ProxySource::MachineWinHttp
            )
        );
        // Loopback is always direct.
        assert_eq!(
            decide("http://localhost:11434/v1/", &cfg, None).0,
            ProxyChoice::Direct
        );
    }

    #[test]
    fn override_and_loopback_plan() {
        let plan = plan_blocking(
            &[
                "https://openrouter.ai/api/v1/".into(),
                "http://127.0.0.1:9/".into(),
            ],
            Some("http://proxy.test:8080"),
            parse_credentials("alice:s3cret"),
        );
        assert_eq!(
            plan.choice_for("openrouter.ai"),
            ProxyChoice::Via("http://proxy.test:8080".into())
        );
        assert_eq!(plan.choice_for("127.0.0.1"), ProxyChoice::Direct);
        assert!(!plan.describe().join(" ").contains("s3cret"));
        assert_eq!(plan.credentials.map(|c| c.0).as_deref(), Some("alice"));
    }
}
