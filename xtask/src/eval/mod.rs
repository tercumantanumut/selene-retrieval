//! `cargo xtask eval`: golden-set retrieval evaluation (docs/04-roadmap.md M1; Amendment 3.4).
//!
//! ```text
//! xtask eval --golden eval/golden/selene-src.jsonl \
//!     --engine v2=results:eval/baselines/selene-src.selene-v2.large-fp32.zf-none.json \
//!     --engine ours=cmd:target/release/sr-bench --index ...   (or mcp:sr-bridge.exe)
//!     [--engine ...] [--k 50] [--primary file|chunk] [--strip-prefix <dir>]...
//!     [--out report.md] [--json report.json] [--title "..."] [--timeout-secs 60]
//!     [--max-ndcg-loss <points>] [--min-ndcg-delta <points>]
//! ```
//! The first engine is the baseline. Every other engine is compared with it on the primary
//! level (default `file`): mean delta in nDCG@10 points (x100), paired-bootstrap 95% CI and
//! win/tie/loss counts. Gates (exit code 1 on failure):
//! * `--max-ndcg-loss P`: each variant may lose at most P nDCG@10 points against the baseline
//!   (Amendment 3.4: int8 returns only with a loss of 1.0 or less).
//! * `--min-ndcg-delta D`: each variant must beat the baseline by at least D points
//!   (M1 exit "nDCG@10 >= Selene V2" is `--min-ndcg-delta 0`).

pub mod adapters;
pub mod golden;
pub mod metrics;
mod report;

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use golden::GoldenQuery;
use metrics::QueryMetrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    File,
    Chunk,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub golden: PathBuf,
    pub engines: Vec<String>,
    pub k: usize,
    pub primary: Level,
    pub strip_prefixes: Vec<String>,
    pub out: Option<PathBuf>,
    pub json: Option<PathBuf>,
    pub title: Option<String>,
    pub timeout: Duration,
    pub max_ndcg_loss: Option<f64>,
    pub min_ndcg_delta: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerQuery {
    pub qid: String,
    pub kind: String,
    pub file: QueryMetrics,
    pub chunk: QueryMetrics,
    pub latency_ms: Option<f64>,
    pub n_hits: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineRun {
    pub label: String,
    pub engine: String,
    pub variant: String,
    pub queries: Vec<PerQuery>,
}

impl EngineRun {
    pub fn metric(&self, level: Level, f: fn(&QueryMetrics) -> f64) -> Vec<f64> {
        self.queries
            .iter()
            .map(|q| {
                f(match level {
                    Level::File => &q.file,
                    Level::Chunk => &q.chunk,
                })
            })
            .collect()
    }
    pub fn latencies(&self) -> Vec<f64> {
        self.queries.iter().filter_map(|q| q.latency_ms).collect()
    }
    pub fn errors(&self) -> usize {
        self.queries.iter().filter(|q| q.error.is_some()).count()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub label: String,
    pub delta_points: f64,
    pub ci_points: (f64, f64),
    pub wins: usize,
    pub ties: usize,
    pub losses: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateResult {
    pub label: String,
    pub rule: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub title: String,
    pub golden: String,
    pub n_queries: usize,
    pub k: usize,
    pub primary: Level,
    pub runs: Vec<EngineRun>,
    pub comparisons: Vec<Comparison>,
    pub gates: Vec<GateResult>,
}

pub fn evaluate(
    golden: &[GoldenQuery],
    label: String,
    adapter: &mut dyn adapters::SearchAdapter,
    opts: &Options,
) -> EngineRun {
    let mut queries = Vec::with_capacity(golden.len());
    for q in golden {
        let (hits, latency_ms, error) = match adapter.search(q, opts.k) {
            Ok(out) => (out.hits, out.latency_ms, None),
            Err(e) => (Vec::new(), None, Some(e)),
        };
        let file =
            metrics::query_metrics(&metrics::judge_file_level(q, &hits, &opts.strip_prefixes));
        let chunk =
            metrics::query_metrics(&metrics::judge_chunk_level(q, &hits, &opts.strip_prefixes));
        queries.push(PerQuery {
            qid: q.qid.clone(),
            kind: q.kind.clone(),
            file,
            chunk,
            latency_ms,
            n_hits: hits.len(),
            error,
        });
    }
    EngineRun {
        label,
        engine: adapter.engine(),
        variant: adapter.variant(),
        queries,
    }
}

pub fn compare(base: &EngineRun, other: &EngineRun, level: Level) -> Comparison {
    let a = base.metric(level, |m| m.ndcg10);
    let b = other.metric(level, |m| m.ndcg10);
    let (lo, hi) = metrics::paired_bootstrap_ci(&a, &b, 10_000, 0x5e1e_e2e7);
    let (mut wins, mut ties, mut losses) = (0, 0, 0);
    for (x, y) in a.iter().zip(&b) {
        if (y - x).abs() < 1e-9 {
            ties += 1;
        } else if y > x {
            wins += 1;
        } else {
            losses += 1;
        }
    }
    Comparison {
        label: other.label.clone(),
        delta_points: (metrics::mean(b.iter().copied()) - metrics::mean(a.iter().copied())) * 100.0,
        ci_points: (lo * 100.0, hi * 100.0),
        wins,
        ties,
        losses,
    }
}

pub fn gates(comparisons: &[Comparison], opts: &Options) -> Vec<GateResult> {
    let mut out = Vec::new();
    for c in comparisons {
        if let Some(p) = opts.max_ndcg_loss {
            out.push(GateResult {
                label: c.label.clone(),
                rule: format!(
                    "nDCG@10 loss vs baseline <= {p:.1} pts (observed {:+.2})",
                    c.delta_points
                ),
                passed: -c.delta_points <= p + 1e-9,
            });
        }
        if let Some(d) = opts.min_ndcg_delta {
            out.push(GateResult {
                label: c.label.clone(),
                rule: format!(
                    "nDCG@10 delta vs baseline >= {d:+.1} pts (observed {:+.2})",
                    c.delta_points
                ),
                passed: c.delta_points >= d - 1e-9,
            });
        }
    }
    out
}

pub fn run(opts: &Options) -> Result<Report, String> {
    if opts.engines.is_empty() {
        return Err("at least one --engine is required".into());
    }
    let golden = golden::load(&opts.golden)?;
    let mut runs = Vec::new();
    for (i, spec) in opts.engines.iter().enumerate() {
        let (label, mut adapter) = adapters::from_spec(spec, opts.timeout)?;
        let label = label.unwrap_or_else(|| format!("E{i}"));
        if runs.iter().any(|r: &EngineRun| r.label == label) {
            return Err(format!("duplicate engine label {label:?}"));
        }
        let r = evaluate(&golden, label, adapter.as_mut(), opts);
        if r.errors() > 0 {
            eprintln!(
                "xtask eval: {}: {} of {} queries failed (scored as empty)",
                r.label,
                r.errors(),
                golden.len()
            );
        }
        runs.push(r);
    }
    let comparisons: Vec<Comparison> = runs
        .iter()
        .skip(1)
        .map(|r| compare(&runs[0], r, opts.primary))
        .collect();
    let gates = gates(&comparisons, opts);
    Ok(Report {
        title: opts
            .title
            .clone()
            .unwrap_or_else(|| "Retrieval evaluation".into()),
        golden: opts.golden.display().to_string().replace('\\', "/"),
        n_queries: golden.len(),
        k: opts.k,
        primary: opts.primary,
        runs,
        comparisons,
        gates,
    })
}

pub fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        golden: PathBuf::new(),
        engines: Vec::new(),
        k: 50,
        primary: Level::File,
        strip_prefixes: Vec::new(),
        out: None,
        json: None,
        title: None,
        timeout: Duration::from_secs(60),
        max_ndcg_loss: None,
        min_ndcg_delta: None,
    };
    let mut it = args.iter();
    let mut golden = None;
    while let Some(a) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--golden" => golden = Some(PathBuf::from(val(a)?)),
            "--engine" => o.engines.push(val(a)?),
            "--k" => o.k = val(a)?.parse().map_err(|e| format!("--k: {e}"))?,
            "--primary" => {
                o.primary = match val(a)?.as_str() {
                    "file" => Level::File,
                    "chunk" => Level::Chunk,
                    other => return Err(format!("--primary: expected file|chunk, got {other:?}")),
                }
            }
            "--strip-prefix" => o.strip_prefixes.push(val(a)?),
            "--out" => o.out = Some(PathBuf::from(val(a)?)),
            "--json" => o.json = Some(PathBuf::from(val(a)?)),
            "--title" => o.title = Some(val(a)?),
            "--timeout-secs" => {
                o.timeout = Duration::from_secs(
                    val(a)?
                        .parse()
                        .map_err(|e| format!("--timeout-secs: {e}"))?,
                )
            }
            "--max-ndcg-loss" => {
                o.max_ndcg_loss = Some(
                    val(a)?
                        .parse()
                        .map_err(|e| format!("--max-ndcg-loss: {e}"))?,
                )
            }
            "--min-ndcg-delta" => {
                o.min_ndcg_delta = Some(
                    val(a)?
                        .parse()
                        .map_err(|e| format!("--min-ndcg-delta: {e}"))?,
                )
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    o.golden = golden.ok_or_else(|| format!("--golden is required\n{USAGE}"))?;
    if o.k == 0 {
        return Err("--k must be > 0".into());
    }
    Ok(o)
}

pub const USAGE: &str = "usage: xtask eval --golden <golden.jsonl> --engine [label=]results:<file>|cmd:<prog>|mcp:<prog> \
[--engine ...] [--k 50] [--primary file|chunk] [--strip-prefix <dir>]... [--out report.md] [--json report.json] \
[--title <t>] [--timeout-secs 60] [--max-ndcg-loss <pts>] [--min-ndcg-delta <pts>]";

fn write_creating_dirs(p: &std::path::Path, body: &str) -> Result<(), String> {
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(p, body).map_err(|e| format!("{}: {e}", p.display()))
}

/// Entry point used by `main`: runs, writes the outputs, returns Err when a gate fails.
pub fn main(args: &[String]) -> Result<(), String> {
    let opts = parse_args(args)?;
    let rep = run(&opts)?;
    let md = report::markdown(&rep);
    match &opts.out {
        Some(p) => write_creating_dirs(p, &md)?,
        None => print!("{md}"),
    }
    if let Some(p) = &opts.json {
        let body = serde_json::to_string_pretty(&rep).map_err(|e| e.to_string())?;
        write_creating_dirs(p, &(body + "\n"))?;
    }
    if opts.out.is_some() {
        print!("{}", report::summary_table(&rep));
    }
    let failed: Vec<&GateResult> = rep.gates.iter().filter(|g| !g.passed).collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} gate(s) failed: {}",
            failed.len(),
            failed
                .iter()
                .map(|g| format!("{}: {}", g.label, g.rule))
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// End to end: golden + two result files -> metrics, comparison, gates, markdown.
    #[test]
    fn end_to_end_two_engines_with_gates() {
        let dir = tempfile::tempdir().unwrap();
        let golden = write(
            dir.path(),
            "g.jsonl",
            concat!(
                r#"{"qid":"q1","query":"a","kind":"identifier","relevant":[{"path":"a.ts","grade":3,"spans":[[1,10]]}]}"#,
                "\n",
                r#"{"qid":"q2","query":"b","kind":"nl","relevant":[{"path":"b.ts","grade":2,"spans":[]}]}"#,
                "\n"
            ),
        );
        // base: q1 perfect, q2 relevant at rank 2.  better: both perfect.
        let base = write(
            dir.path(),
            "base.json",
            r#"{"engine":"v2","results":[
              {"qid":"q1","latency_ms":10,"hits":[{"path":"a.ts","start_line":1,"end_line":5}]},
              {"qid":"q2","latency_ms":30,"hits":[{"path":"x.ts","start_line":1,"end_line":5},{"path":"b.ts","start_line":1,"end_line":5}]}]}"#,
        );
        let better = write(
            dir.path(),
            "better.json",
            r#"{"engine":"ours","results":[
              {"qid":"q1","hits":[{"path":"a.ts","start_line":3,"end_line":4}]},
              {"qid":"q2","hits":[{"path":"b.ts","start_line":9,"end_line":9}]}]}"#,
        );
        let args: Vec<String> = [
            "--golden",
            golden.to_str().unwrap(),
            "--engine",
            &format!("v2=results:{}", base.display()),
            "--engine",
            &format!("ours=results:{}", better.display()),
            "--min-ndcg-delta",
            "0",
            "--max-ndcg-loss",
            "1.0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let opts = parse_args(&args).unwrap();
        let rep = run(&opts).unwrap();
        let base_ndcg = metrics::mean(rep.runs[0].metric(Level::File, |m| m.ndcg10));
        // q1 = 1; q2 = (2/log2 3)/2 = 0.630929754 -> mean 0.815464877
        assert!((base_ndcg - 0.815_464_876_8).abs() < 1e-9);
        let c = &rep.comparisons[0];
        assert!(
            (c.delta_points - 18.453_512_32).abs() < 1e-6,
            "{}",
            c.delta_points
        );
        assert_eq!((c.wins, c.ties, c.losses), (1, 1, 0));
        assert!(rep.gates.iter().all(|g| g.passed));
        let p50 = metrics::percentile(&rep.runs[0].latencies(), 50.0);
        assert_eq!(p50, Some(10.0));
        let md = report::markdown(&rep);
        assert!(md.contains("| v2 |") && md.contains("+18.45"), "{md}");

        // reversed order: "ours" is the baseline, v2 now fails both gates
        let mut rev = opts.clone();
        rev.engines.reverse();
        let rep = run(&rev).unwrap();
        assert_eq!(rep.gates.iter().filter(|g| !g.passed).count(), 2);
    }

    #[test]
    fn arg_errors_are_reported() {
        assert!(
            parse_args(&[])
                .unwrap_err()
                .contains("--golden is required")
        );
        let a: Vec<String> = ["--golden", "g", "--primary", "doc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_args(&a).unwrap_err().contains("file|chunk"));
        let a: Vec<String> = ["--golden", "g", "--k", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_args(&a).is_err());
    }
}
