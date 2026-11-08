//! `SecretFilter`: files that usually hold passwords or keys are never read, indexed, grepped or
//! sent anywhere (docs/02-architecture.md §2.6 "Secret leakage"). Always on; there is no switch
//! to turn the default list off (P1.7: security is never weakened for simplicity).
//!
//! Matching is case-insensitive and applied to **every** component of a root-relative path, after
//! the sandbox has expanded 8.3 short names, so `CREDEN~1.JSO` or `.SSH\config` cannot slip past.

/// How one denylist entry matches a path component.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// Component equals this name (used for directories such as `.ssh`).
    Exact(&'static str),
    /// Component starts with this prefix (`.env*`, `id_rsa*`).
    Prefix(&'static str),
    /// Component ends with this suffix (`*.pem`).
    Suffix(&'static str),
    /// Component starts with the prefix and ends with the suffix (`credentials*.json`).
    PrefixSuffix(&'static str, &'static str),
}

impl Rule {
    fn matches(&self, lower: &str) -> bool {
        match *self {
            Rule::Exact(n) => lower == n,
            Rule::Prefix(p) => lower.starts_with(p),
            Rule::Suffix(s) => lower.ends_with(s),
            Rule::PrefixSuffix(p, s) => {
                lower.len() >= p.len() + s.len() && lower.starts_with(p) && lower.ends_with(s)
            }
        }
    }
}

/// The default denylist (docs/02-architecture.md §2.6).
const DEFAULT_RULES: &[Rule] = &[
    Rule::Prefix(".env"),
    Rule::Suffix(".pem"),
    Rule::Suffix(".key"),
    Rule::Suffix(".pfx"),
    Rule::Suffix(".p12"),
    Rule::Prefix("id_rsa"),
    Rule::Prefix("id_ed25519"),
    Rule::Exact(".ssh"),
    Rule::Exact(".aws"),
    Rule::Exact(".gnupg"),
    Rule::Exact(".npmrc"),
    Rule::Exact(".pypirc"),
    Rule::PrefixSuffix("credentials", ".json"),
    Rule::Suffix(".kdbx"),
    // M5-E S-10: common credential stores the M1 list missed.
    Rule::Exact(".git-credentials"),
    Rule::Exact(".netrc"),
    Rule::Exact("_netrc"),
    Rule::Exact(".kube"),
    Rule::Exact(".docker"),
    Rule::Exact(".azure"),
    Rule::Suffix(".ppk"),
    Rule::Prefix("id_ecdsa"),
    Rule::Prefix("id_dsa"),
    Rule::Exact("application_default_credentials.json"),
    Rule::Suffix(".jks"),
    Rule::Suffix(".keystore"),
    Rule::Exact(".htpasswd"),
    Rule::Suffix(".tfstate"),
    Rule::Suffix(".tfstate.backup"),
    Rule::Exact("login data"),
];

/// Secret denylist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFilter {
    rules: &'static [Rule],
}

impl Default for SecretFilter {
    fn default() -> Self {
        Self {
            rules: DEFAULT_RULES,
        }
    }
}

impl SecretFilter {
    /// Whether one file or directory name is denied.
    pub fn is_denied_name(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.rules.iter().any(|r| r.matches(&lower))
    }

    /// Whether any component of a root-relative path (`/` or `\` separated) is denied. A denied
    /// directory (`.ssh/`) denies everything below it.
    pub fn is_denied_rel(&self, rel: &str) -> bool {
        rel.split(['/', '\\'])
            .filter(|c| !c.is_empty())
            .any(|c| self.is_denied_name(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_denylist() {
        let f = SecretFilter::default();
        for p in [
            ".env",
            ".env.local",
            "app/.ENV.production",
            "certs/server.pem",
            "a/b/tls.KEY",
            "me.pfx",
            "me.p12",
            "id_rsa",
            "id_rsa.pub",
            "id_ed25519",
            ".ssh/config",
            "home/.aws/credentials",
            ".gnupg/pubring.kbx",
            ".npmrc",
            ".pypirc",
            "credentials.json",
            "gcp/credentials-prod.json",
            "vault.kdbx",
            // S-10
            ".git-credentials",
            "home/_netrc",
            ".netrc",
            ".kube/config",
            ".docker/config.json",
            ".azure/msal_token_cache.json",
            "putty/server.PPK",
            "id_ecdsa",
            "id_ecdsa.pub",
            "id_dsa",
            "gcloud/application_default_credentials.json",
            "android/release.jks",
            "app.keystore",
            "web/.htpasswd",
            "infra/terraform.tfstate",
            "infra/terraform.tfstate.backup",
            "Chrome/Default/Login Data",
        ] {
            assert!(f.is_denied_rel(p), "{p} should be denied");
        }
        for p in [
            "src/main.rs",
            "README.md",
            "docs/keyboard.md",
            "environment.md",
            "my.env.example.txt.bak.md",
            "credentials.md",
            "notes/ssh.txt",
            "keys/readme.txt",
            "monkey.rs",
            "docs/netrc.md",
            "kube.md",
            "docker-compose.yml",
            "ppk-notes.md",
            "login data.txt",
        ] {
            assert!(!f.is_denied_rel(p), "{p} should be allowed");
        }
    }
}
