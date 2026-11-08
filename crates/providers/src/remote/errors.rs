//! Provider failures → the one error model (§2.5 codes 21–28, P1.5).
//!
//! | What happened | Code |
//! |---|---|
//! | 401, 403, or a 400/404 whose body says the key is invalid | `ProviderAuth` (#21) |
//! | 402, or a 400/403/429 whose body says quota / credit / balance | `ProviderQuota` (#22) |
//! | other 429 | `RateLimited` (#23), `retry_after` from `Retry-After` / `retry-after-ms` |
//! | 5xx, 529 (overloaded), body cut off mid-response | `ProviderUnavailable` (#24) |
//! | other 4xx (unknown model, input too long) | `ProviderRequest` (#25) |
//! | 407, TLS / certificate trouble, failed proxy tunnel, HTML instead of JSON | `NetworkBlocked` (#28) |
//! | DNS failure / no connection to a remote host | `Offline` (#27) |
//! | no connection to a server on this computer (Ollama, vLLM) | `ProviderUnavailable` (#24) |
//! | per-attempt timeout | `Timeout` (#19) |
//!
//! Provider bodies go to Details only, truncated and scrubbed of the key ([`crate::secrets::scrub`]).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::HeaderMap;
use sr_common::error::{Error, ErrorCode};

use crate::secrets::scrub;

/// Where the call went, for the user text (`{provider}`, `{host}`) and redaction.
#[derive(Debug, Clone, Copy)]
pub struct Site<'a> {
    /// Display name ("OpenRouter").
    pub provider: &'a str,
    pub host: &'a str,
    /// The key in use (removed from any provider text kept in Details).
    pub key: Option<&'a str>,
    /// A server on this computer: "no connection" means it isn't running, not "offline".
    pub local: bool,
}

const BODY_DETAIL_MAX: usize = 300;

fn detail(site: &Site<'_>, what: &str, body: &str) -> String {
    let mut b = scrub(body, site.key);
    if b.len() > BODY_DETAIL_MAX {
        let mut cut = BODY_DETAIL_MAX;
        while !b.is_char_boundary(cut) {
            cut -= 1;
        }
        b.truncate(cut);
        b.push('…');
    }
    if b.is_empty() {
        what.to_string()
    } else {
        format!("{what}: {b}")
    }
}

fn base(code: ErrorCode, site: &Site<'_>) -> Error {
    let mut e = Error::new(code).with_provider(site.provider);
    if matches!(code, ErrorCode::Offline | ErrorCode::NetworkBlocked) {
        e.ctx.host = Some(site.host.to_string());
    }
    e
}

fn says_quota(low: &str) -> bool {
    [
        "insufficient_quota",
        "insufficient quota",
        "exceeded your current quota",
        "quota exceeded",
        "out of credit",
        "insufficient credit",
        "credit balance",
        "insufficient balance",
        "insufficient_balance",
        "payment required",
        "billing",
    ]
    .iter()
    .any(|k| low.contains(k))
}

fn says_bad_key(low: &str) -> bool {
    [
        "invalid api key",
        "invalid_api_key",
        "incorrect api key",
        "invalid x-api-key",
        "api key not valid",
        "authentication fail",
        "unauthorized",
        "invalid authentication",
    ]
    .iter()
    .any(|k| low.contains(k))
}

/// `Retry-After` (seconds or HTTP date) or `retry-after-ms`, in whole seconds (≥ 1).
pub fn retry_after_secs(headers: &HeaderMap) -> Option<u32> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
    {
        return Some(((ms / 1000.0).ceil() as u32).max(1));
    }
    let v = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(s) = v.parse::<f64>() {
        return (s.is_finite() && s >= 0.0).then(|| (s.ceil() as u32).max(1));
    }
    let at = parse_http_date(v)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some((at.saturating_sub(now) as u32).max(1))
}

/// IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) → Unix seconds.
fn parse_http_date(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, mon, year, time, "GMT"] = parts.as_slice() else {
        return None;
    };
    let day: u64 = day.parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m.eq_ignore_ascii_case(mon))? as u64
        + 1;
    let year: i64 = year.parse().ok()?;
    let mut t = time.split(':').map(|x| x.parse::<u64>().ok());
    let (h, m, sec) = (t.next()??, t.next()??, t.next()??);
    // Days from civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    u64::try_from(days)
        .ok()
        .map(|d| d * 86_400 + h * 3600 + m * 60 + sec)
}

/// An HTTP error response.
pub fn from_status(site: &Site<'_>, status: u16, headers: &HeaderMap, body: &str) -> Error {
    let low = body.to_ascii_lowercase();
    let what = format!("HTTP {status} from {}", site.host);
    let e = match status {
        401 => base(ErrorCode::ProviderAuth, site),
        402 => base(ErrorCode::ProviderQuota, site),
        403 if says_quota(&low) => base(ErrorCode::ProviderQuota, site),
        403 => base(ErrorCode::ProviderAuth, site),
        407 => {
            return base(ErrorCode::NetworkBlocked, site).with_detail(format!(
                "{what}: proxy sign-in required (only Basic is supported)"
            ));
        }
        408 => base(ErrorCode::Timeout, site),
        429 if says_quota(&low) => base(ErrorCode::ProviderQuota, site),
        429 => {
            let secs = retry_after_secs(headers).unwrap_or(10);
            base(ErrorCode::RateLimited, site).with_retry_after(secs)
        }
        400 | 404 if says_quota(&low) => base(ErrorCode::ProviderQuota, site),
        400 | 404 if says_bad_key(&low) => base(ErrorCode::ProviderAuth, site),
        500..=599 => {
            let mut e = base(ErrorCode::ProviderUnavailable, site);
            if let Some(s) = retry_after_secs(headers) {
                e.retry_after = Some(Duration::from_secs(u64::from(s)));
            }
            e
        }
        _ => base(ErrorCode::ProviderRequest, site),
    };
    e.with_detail(detail(site, &what, body))
}

fn chain_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

/// A transport-level reqwest failure (no HTTP status).
pub fn from_reqwest(site: &Site<'_>, err: &reqwest::Error) -> Error {
    if let Some(status) = err.status() {
        return from_status(site, status.as_u16(), &HeaderMap::new(), "");
    }
    let text = chain_text(err);
    from_transport_text(site, &text, err.is_timeout(), err.is_connect())
}

/// Classifies a transport failure from its error-chain text (also used for genai's boxed
/// stream errors, which only keep the text).
pub fn from_transport_text(site: &Site<'_>, text: &str, timeout: bool, connect: bool) -> Error {
    let low = text.to_ascii_lowercase();
    let detail_text = detail(
        site,
        &format!("network error talking to {}", site.host),
        text,
    );
    let code = if timeout || low.contains("timed out") || low.contains("deadline") {
        ErrorCode::Timeout
    } else if [
        "certificate",
        "unknownissuer",
        "unknown issuer",
        "handshake",
        "tls",
        "invalid peer",
    ]
    .iter()
    .any(|k| low.contains(k))
        || low.contains("407")
        || low.contains("tunnel")
        || low.contains("proxy")
    {
        ErrorCode::NetworkBlocked
    } else if connect
        || low.contains("dns error")
        || low.contains("failed to lookup")
        || low.contains("no such host")
        || low.contains("connection refused")
        || low.contains("os error 10061")
        || low.contains("network is unreachable")
    {
        if site.local {
            ErrorCode::ProviderUnavailable
        } else {
            ErrorCode::Offline
        }
    } else {
        // Connection reset / body cut off mid-response: transient.
        ErrorCode::ProviderUnavailable
    };
    base(code, site).with_detail(detail_text)
}

/// A genai error (wraps reqwest / HTTP status / stream / parse failures).
pub fn from_genai(site: &Site<'_>, err: genai::Error) -> Error {
    use genai::Error as G;
    match err {
        G::HttpError {
            status,
            body,
            headers,
            ..
        } => from_status(site, status.as_u16(), &headers, &body),
        G::WebModelCall { webc_error, .. } | G::WebAdapterCall { webc_error, .. } => {
            from_webc(site, webc_error)
        }
        G::WebStream { error, cause, .. } => from_boxed(site, error, &cause),
        G::ChatResponse { body, .. } => {
            // An error event inside an SSE stream (status already 200).
            let text = body.to_string();
            let low = text.to_ascii_lowercase();
            let code = if says_quota(&low) {
                ErrorCode::ProviderQuota
            } else if low.contains("rate") && low.contains("limit") {
                ErrorCode::RateLimited
            } else if says_bad_key(&low) {
                ErrorCode::ProviderAuth
            } else if low.contains("overloaded") || low.contains("server") {
                ErrorCode::ProviderUnavailable
            } else {
                ErrorCode::ProviderRequest
            };
            let mut e = base(code, site);
            if code == ErrorCode::RateLimited {
                e = e.with_retry_after(10);
            }
            e.with_detail(detail(site, "error event in stream", &text))
        }
        other => {
            let text = other.to_string();
            base(ErrorCode::ProviderRequest, site).with_detail(detail(
                site,
                "unexpected response",
                &text,
            ))
        }
    }
}

fn from_webc(site: &Site<'_>, err: genai::webc::Error) -> Error {
    use genai::webc::Error as W;
    match err {
        W::ResponseFailedStatus {
            status,
            body,
            headers,
        } => from_status(site, status.as_u16(), &headers, &body),
        W::Reqwest(e) => from_reqwest(site, &e),
        W::ResponseFailedNotJson { content_type, body } => {
            // An HTML page with status 200 is a proxy, captive portal or sign-in page.
            let code = if content_type.contains("html") {
                ErrorCode::NetworkBlocked
            } else {
                ErrorCode::ProviderUnavailable
            };
            base(code, site).with_detail(detail(
                site,
                &format!("unexpected {content_type} response from {}", site.host),
                &body,
            ))
        }
        other => base(ErrorCode::ProviderUnavailable, site).with_detail(detail(
            site,
            "unreadable response",
            &other.to_string(),
        )),
    }
}

fn from_boxed(site: &Site<'_>, err: genai::BoxError, cause: &str) -> Error {
    let err = match err.downcast::<genai::Error>() {
        Ok(g) => return from_genai(site, *g),
        Err(e) => e,
    };
    let err = match err.downcast::<genai::webc::Error>() {
        Ok(w) => return from_webc(site, *w),
        Err(e) => e,
    };
    match err.downcast::<reqwest::Error>() {
        Ok(r) => from_reqwest(site, &r),
        Err(e) => from_transport_text(site, &format!("{cause}: {}", chain_text(&*e)), false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    const SITE: Site<'static> = Site {
        provider: "OpenRouter",
        host: "openrouter.ai",
        key: Some("sk-or-v1-secretsecret"),
        local: false,
    };

    fn code(status: u16, body: &str) -> ErrorCode {
        from_status(&SITE, status, &HeaderMap::new(), body).code
    }

    #[test]
    fn status_mapping() {
        assert_eq!(code(401, ""), ErrorCode::ProviderAuth);
        assert_eq!(code(403, "Key disabled"), ErrorCode::ProviderAuth);
        assert_eq!(code(402, ""), ErrorCode::ProviderQuota);
        assert_eq!(
            code(429, r#"{"error":{"code":"insufficient_quota"}}"#),
            ErrorCode::ProviderQuota
        );
        assert_eq!(code(429, "slow down"), ErrorCode::RateLimited);
        assert_eq!(
            code(400, "Your credit balance is too low"),
            ErrorCode::ProviderQuota
        );
        assert_eq!(code(400, "invalid x-api-key"), ErrorCode::ProviderAuth);
        assert_eq!(code(400, "context too long"), ErrorCode::ProviderRequest);
        assert_eq!(code(404, "model not found"), ErrorCode::ProviderRequest);
        assert_eq!(code(503, ""), ErrorCode::ProviderUnavailable);
        assert_eq!(code(529, "overloaded"), ErrorCode::ProviderUnavailable);
        assert_eq!(code(407, ""), ErrorCode::NetworkBlocked);
    }

    #[test]
    fn retry_after_forms() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("7"));
        assert_eq!(retry_after_secs(&h), Some(7));
        let e = from_status(&SITE, 429, &h, "");
        assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
        assert!(e.retryable);
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("1500"));
        assert_eq!(retry_after_secs(&h), Some(2));
        let mut h = HeaderMap::new();
        h.insert(
            "retry-after",
            HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        assert_eq!(retry_after_secs(&h), Some(1)); // in the past → minimum
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
    }

    #[test]
    fn details_never_hold_the_key() {
        let e = from_status(
            &SITE,
            401,
            &HeaderMap::new(),
            "Incorrect API key provided: sk-or-v1-secretsecret",
        );
        let d = e.ctx.detail.clone().unwrap_or_default();
        assert!(!d.contains("secretsecret"), "{d}");
        assert_eq!(e.ctx.provider.as_deref(), Some("OpenRouter"));
    }

    #[test]
    fn transport_mapping() {
        let off = from_transport_text(
            &SITE,
            "error sending request: dns error: no such host",
            false,
            true,
        );
        assert_eq!(off.code, ErrorCode::Offline);
        assert_eq!(off.ctx.host.as_deref(), Some("openrouter.ai"));
        let tls = from_transport_text(
            &SITE,
            "invalid peer certificate: UnknownIssuer",
            false,
            true,
        );
        assert_eq!(tls.code, ErrorCode::NetworkBlocked);
        let local = Site {
            local: true,
            ..SITE
        };
        let down =
            from_transport_text(&local, "tcp connect error: connection refused", false, true);
        assert_eq!(down.code, ErrorCode::ProviderUnavailable);
        let t = from_transport_text(&SITE, "operation timed out", true, false);
        assert_eq!(t.code, ErrorCode::Timeout);
    }
}
