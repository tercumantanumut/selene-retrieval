//! The AI provider list shown in the wizard (page 20) and Providers & Keys (page 21).
//!
//! Single source: the generated provider list `crates/providers/data/selene-providers.json`
//! (M3-P's `xtask sync-providers`, synced from Selene's registry; m3 design-spec intake).
//! `build.rs` copies it into the binary, so the app never hard-codes a provider or model name. A
//! missing or unreadable file gives an empty list (the provider step then has nothing to offer,
//! and search keeps working).
//!
//! Fields used (schema 1, `{ "providers": [...] }`; a bare array is accepted too): `id`,
//! `display_name`, `order`, `enabled` (false, e.g. `disabled_reason: "oauth_pending_design"` →
//! shown, not selectable), `auth_kind` (`api_key` | `oauth` | `none_local`), `default_model`
//! (only an "e.g." hint) and `base_url` (pre-filled address of a `none_local` provider). Unknown
//! fields are ignored, so the manifest can grow without an app change.
//!
//! M4 (page 21, Amendment 8.3): `key_url` (aliases `api_key_url`, `keys_url`) is the provider's
//! own page for getting a key (or, for a local provider, its download page). It is shown as
//! "Where do I get a key?" and opened only through `opener::open_link`, which accepts exactly the
//! `https://` addresses listed here. Requested from M4-E / xtask (`m4-a-progress.md`); until the
//! manifest carries it the link is simply not shown.

use std::sync::OnceLock;

use serde_json::Value;

use crate::dto::{AuthKind, ProviderOption};

/// The manifest as embedded at build time (`[]` when the file didn't exist).
const MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/providers.json"));

fn text<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn auth_kind(s: Option<&str>) -> AuthKind {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("none_local" | "local" | "none" | "no_key") => AuthKind::Local,
        Some("oauth" | "subscription" | "account" | "cli" | "device_code") => AuthKind::Oauth,
        // `api_key` and anything unknown: a key field (the safe, validated path).
        _ => AuthKind::ApiKey,
    }
}

fn entry(v: &Value) -> Option<ProviderOption> {
    let id = text(v, &["id"])?.to_owned();
    let name = text(v, &["display_name", "name", "label"])
        .unwrap_or(&id)
        .to_owned();
    let auth = auth_kind(text(v, &["auth_kind", "auth"]));
    let default_address = match auth {
        AuthKind::Local => {
            text(v, &["default_base_url", "base_url", "address", "endpoint"]).map(str::to_owned)
        }
        AuthKind::ApiKey | AuthKind::Oauth => None,
    };
    let enabled = v.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let disabled_reason = text(v, &["disabled_reason"]).map(str::to_owned);
    // Amendments 10.2 / 10.3 (M5-C): "Claude (sign in)" and "Codex (sign in)" reuse the local
    // Claude Code / Codex login and are on in every local and private build. A vendor block in
    // the list ("not_allowed_by_vendor", "pending_vendor_approval") still wins. Other account
    // rows stay shown and disabled.
    let cli = sr_common::api::cli_signin_provider(&id).filter(|_| auth == AuthKind::Oauth);
    let vendor_block = matches!(
        disabled_reason.as_deref(),
        Some("not_allowed_by_vendor" | "pending_vendor_approval")
    );
    let available = if cli.is_some() {
        enabled && !vendor_block
    } else {
        enabled && auth != AuthKind::Oauth
    };
    // Their label is fixed ("Claude (sign in)", never the tool's product name).
    let name = cli.map_or(name, |c| c.label.to_owned());
    let key_url = text(v, &["key_url", "api_key_url", "keys_url"])
        .filter(|u| crate::opener::is_plain_https(u))
        .map(str::to_owned);
    Some(ProviderOption {
        key_url,
        model_hint: text(v, &["default_model"]).map(str::to_owned),
        available,
        disabled_reason: if available { None } else { disabled_reason },
        id,
        name,
        auth,
        default_address,
    })
}

/// Parses a manifest document (see the module docs). Entries are sorted by `order` (stable, so
/// entries without one keep the manifest's order, after numbered ones); duplicate ids keep the
/// first.
pub fn parse(doc: &str) -> Vec<ProviderOption> {
    let Ok(v) = serde_json::from_str::<Value>(doc) else {
        return Vec::new();
    };
    let list = match &v {
        Value::Array(a) => a.as_slice(),
        Value::Object(o) => o
            .get("providers")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice),
        _ => &[],
    };
    let mut sorted: Vec<&Value> = list.iter().collect();
    sorted.sort_by_key(|v| v.get("order").and_then(Value::as_u64).unwrap_or(u64::MAX));
    let mut out: Vec<ProviderOption> = Vec::new();
    for p in sorted.into_iter().filter_map(entry) {
        if !out.iter().any(|o| o.id == p.id) {
            out.push(p);
        }
    }
    out
}

/// The embedded provider list.
pub fn catalog() -> &'static [ProviderOption] {
    static LIST: OnceLock<Vec<ProviderOption>> = OnceLock::new();
    LIST.get_or_init(|| parse(MANIFEST))
}

/// Every key page in the provider list: the only addresses `open_link` may open.
pub fn key_urls() -> Vec<String> {
    catalog().iter().filter_map(|p| p.key_url.clone()).collect()
}

/// Display name for a provider id (falls back to the id itself, never to a made-up name).
pub fn display_name(id: &str) -> String {
    catalog()
        .iter()
        .find(|p| p.id == id)
        .map_or_else(|| id.to_owned(), |p| p.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_shapes_and_auth_kinds() {
        // Test data only: ids and names here are placeholders, not the shipped list.
        let doc = r#"{"providers": [
            {"id": "alpha", "display_name": "Alpha AI", "auth_kind": "api_key", "default_model": "alpha-1"},
            {"id": "beta", "name": "Beta", "auth_kind": "oauth"},
            {"id": "gamma", "label": "Gamma (on this computer)", "auth_kind": "none_local",
             "default_base_url": "http://localhost:1234"},
            {"id": "alpha", "display_name": "Duplicate"},
            {"display_name": "no id"}
        ]}"#;
        let l = parse(doc);
        assert_eq!(
            l.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta", "gamma"]
        );
        assert_eq!(l[0].name, "Alpha AI");
        assert_eq!(l[0].auth, AuthKind::ApiKey);
        assert_eq!(l[0].model_hint.as_deref(), Some("alpha-1"));
        assert!(l[0].available && l[0].default_address.is_none());
        assert_eq!(l[1].auth, AuthKind::Oauth);
        assert!(
            !l[1].available,
            "account sign-in providers are shown disabled"
        );
        assert_eq!(l[2].auth, AuthKind::Local);
        assert_eq!(
            l[2].default_address.as_deref(),
            Some("http://localhost:1234")
        );
        let arr = parse(r#"[{"id": "x"}]"#);
        assert_eq!((arr[0].name.as_str(), arr[0].auth), ("x", AuthKind::ApiKey));
    }

    #[test]
    fn order_and_enabled_flags_from_the_manifest() {
        let doc = r#"{"schema": 1, "providers": [
            {"id": "b", "display_name": "B", "order": 1, "enabled": true, "auth_kind": "api_key"},
            {"id": "a", "display_name": "A", "order": 0, "enabled": false,
             "disabled_reason": "oauth_pending_design", "auth_kind": "oauth"},
            {"id": "c", "display_name": "C", "order": 2, "enabled": false, "auth_kind": "api_key"},
            {"id": "d", "display_name": "D", "order": 3, "enabled": true, "auth_kind": "none_local",
             "base_url": "http://localhost:9/v1/"}
        ]}"#;
        let l = parse(doc);
        assert_eq!(
            l.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
        assert_eq!(
            l.iter().map(|p| p.available).collect::<Vec<_>>(),
            [false, true, false, true]
        );
        assert_eq!(
            l[3].default_address.as_deref(),
            Some("http://localhost:9/v1/")
        );
    }

    #[test]
    fn key_pages_are_https_only() {
        let doc = r#"{"providers": [
            {"id": "a", "key_url": "https://console.example.com/keys"},
            {"id": "b", "api_key_url": "http://insecure.example.com/keys"},
            {"id": "c", "key_url": "javascript:alert(1)"},
            {"id": "d"}
        ]}"#;
        let l = parse(doc);
        assert_eq!(
            l[0].key_url.as_deref(),
            Some("https://console.example.com/keys")
        );
        assert!(l[1..].iter().all(|p| p.key_url.is_none()));
    }

    #[test]
    fn bad_manifest_is_an_empty_list() {
        assert!(parse("").is_empty());
        assert!(parse("{ not json").is_empty());
        assert!(parse(r#"{"other": 1}"#).is_empty());
    }

    #[test]
    fn cli_sign_in_rows_are_on_and_never_branded_otherwise() {
        // Amendments 10.2 / 10.3: both CLI rows are available when the list enables them; their
        // labels are fixed; other account rows stay disabled.
        let doc = r#"{"providers": [
            {"id": "claudecode", "display_name": "Claude Code", "enabled": true, "auth_kind": "oauth",
             "key_url": "https://claude.com/claude-code"},
            {"id": "codex", "display_name": "Codex", "enabled": true, "auth_kind": "oauth"},
            {"id": "other", "display_name": "Other", "enabled": true, "auth_kind": "oauth"},
            {"id": "antigravity", "display_name": "Antigravity", "enabled": true, "auth_kind": "api_key"}
        ]}"#;
        let l = parse(doc);
        assert_eq!(l[0].name, "Claude (sign in)");
        assert!(l[0].available && l[0].disabled_reason.is_none());
        assert_eq!(
            l[0].key_url.as_deref(),
            Some("https://claude.com/claude-code")
        );
        assert_eq!(l[1].name, "Codex (sign in)");
        assert!(l[1].available);
        assert!(!l[2].available, "other account sign-ins stay disabled");
        assert_eq!((l[3].auth, l[3].available), (AuthKind::ApiKey, true));
        // A vendor block in the list still wins.
        let blocked = parse(
            r#"[{"id": "codex", "enabled": true, "disabled_reason": "pending_vendor_approval", "auth_kind": "oauth"}]"#,
        );
        assert!(!blocked[0].available);
        // The shipped list: both on, never shown as "Claude Code", install pages allowed.
        assert!(catalog().iter().all(|p| p.name != "Claude Code"));
        for id in ["claudecode", "codex"] {
            let p = catalog().iter().find(|p| p.id == id);
            assert!(
                p.is_some_and(|p| p.available && p.key_url.is_some()),
                "{id}"
            );
        }
        assert!(
            key_urls()
                .iter()
                .any(|u| u == "https://claude.com/claude-code")
        );
        assert!(
            key_urls()
                .iter()
                .any(|u| u == "https://developers.openai.com/codex/cli")
        );
    }

    #[test]
    fn embedded_manifest_parses() {
        // Whatever the build embedded (the real list, or `[]` before M3-P's sync ran) must parse
        // without panicking, and every entry has an id and a name.
        for p in catalog() {
            assert!(!p.id.is_empty() && !p.name.is_empty());
        }
    }
}
