//! Golden-set loading (`eval/golden/*.jsonl`) and engine-agnostic hit types.
//!
//! A golden line is `{"qid","query","kind","relevant":[{"path","grade","spans":[[a,b],..]}]}`.
//! `grade` is 3 (answers the query), 2 (strongly related) or 1 (marginal). An empty `spans` list
//! means "the whole file/document". Line numbers are 1-based and inclusive.

use std::path::Path;

use serde::Deserialize;

/// Grade at and above which a unit counts as *relevant* for recall and MRR. Grade-1 units only
/// contribute partial gain to nDCG.
pub const RELEVANT_GRADE: u8 = 2;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Unit {
    pub path: String,
    pub grade: u8,
    #[serde(default)]
    pub spans: Vec<[u32; 2]>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GoldenQuery {
    pub qid: String,
    pub query: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    pub relevant: Vec<Unit>,
}

fn default_kind() -> String {
    "nl".into()
}

/// One ranked result. Field aliases accept sr-core's `SearchHit` (`line_start`/`line_end`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Hit {
    pub path: String,
    #[serde(default, alias = "line_start")]
    pub start_line: Option<u32>,
    #[serde(default, alias = "line_end")]
    pub end_line: Option<u32>,
    #[serde(default)]
    pub score: Option<f64>,
}

impl Hit {
    #[cfg(test)]
    pub fn new(path: &str, start: u32, end: u32) -> Self {
        Self {
            path: path.into(),
            start_line: Some(start),
            end_line: Some(end),
            score: None,
        }
    }
}

/// Normalises a result path for comparison with golden paths: forward slashes, no leading
/// `./`, and the first matching `strip_prefixes` entry removed.
pub fn normalize_path(p: &str, strip_prefixes: &[String]) -> String {
    let mut s = p.replace('\\', "/");
    for pre in strip_prefixes {
        let pre = pre.replace('\\', "/");
        let pre = pre.trim_end_matches('/');
        if !pre.is_empty()
            && s.len() > pre.len()
            && s.starts_with(pre)
            && s[pre.len()..].starts_with('/')
        {
            s = s[pre.len() + 1..].to_string();
            break;
        }
    }
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    }
    s
}

pub fn load(path: &Path) -> Result<Vec<GoldenQuery>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let q: GoldenQuery =
            serde_json::from_str(line).map_err(|e| format!("{}:{}: {e}", path.display(), i + 1))?;
        if !seen.insert(q.qid.clone()) {
            return Err(format!(
                "{}:{}: duplicate qid {}",
                path.display(),
                i + 1,
                q.qid
            ));
        }
        if !q.relevant.iter().any(|u| u.grade >= RELEVANT_GRADE) {
            return Err(format!(
                "{}:{}: query {} has no unit with grade >= {RELEVANT_GRADE}",
                path.display(),
                i + 1,
                q.qid
            ));
        }
        out.push(q);
    }
    if out.is_empty() {
        return Err(format!("{}: no queries", path.display()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_separators_prefixes_and_dot_slash() {
        let strip = vec![r"C:\repo\".to_string()];
        assert_eq!(normalize_path(r"C:\repo\lib\a.ts", &strip), "lib/a.ts");
        assert_eq!(normalize_path("./lib/a.ts", &[]), "lib/a.ts");
        assert_eq!(normalize_path("lib/a.ts", &strip), "lib/a.ts");
        // a prefix must end at a path boundary
        assert_eq!(
            normalize_path("C:/repository/x", &["C:/repo".into()]),
            "C:/repository/x"
        );
    }

    #[test]
    fn hit_accepts_sr_core_field_names() {
        let h: Hit =
            serde_json::from_str(r#"{"path":"a.rs","line_start":3,"line_end":9,"scores":{}}"#)
                .unwrap();
        assert_eq!(
            h,
            Hit {
                path: "a.rs".into(),
                start_line: Some(3),
                end_line: Some(9),
                score: None
            }
        );
    }

    #[test]
    fn load_rejects_queries_without_relevant_units() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("g.jsonl");
        std::fs::write(
            &p,
            r#"{"qid":"a","query":"x","relevant":[{"path":"f","grade":1}]}"#,
        )
        .unwrap();
        assert!(load(&p).unwrap_err().contains("no unit with grade"));
        std::fs::write(
            &p,
            r#"{"qid":"a","query":"x","relevant":[{"path":"f","grade":3}]}"#,
        )
        .unwrap();
        let g = load(&p).unwrap();
        assert_eq!(g[0].kind, "nl");
        assert!(g[0].relevant[0].spans.is_empty());
    }
}
