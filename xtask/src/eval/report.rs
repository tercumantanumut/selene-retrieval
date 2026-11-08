//! Markdown rendering of an eval [`Report`].

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::metrics::{QueryMetrics, mean, percentile};
use super::{EngineRun, Level, Report};

fn level_name(l: Level) -> &'static str {
    match l {
        Level::File => "file",
        Level::Chunk => "chunk",
    }
}

fn ms(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".into(), |x| format!("{x:.1}"))
}

fn row(run: &EngineRun, level: Level) -> [f64; 5] {
    let m = |f: fn(&QueryMetrics) -> f64| mean(run.metric(level, f));
    [
        m(|q| q.ndcg10),
        m(|q| q.recall5),
        m(|q| q.recall10),
        m(|q| q.recall50),
        m(|q| q.mrr10),
    ]
}

/// The compact primary-level table (also printed to stdout when `--out` is used).
pub fn summary_table(rep: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "| engine | variant | nDCG@10 | R@5 | R@10 | R@50 | MRR@10 | p50 ms | p95 ms | Δ nDCG@10 pts (95% CI) | W/T/L |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|");
    for (i, run) in rep.runs.iter().enumerate() {
        let r = row(run, rep.primary);
        let lat = run.latencies();
        let cmp = if i == 0 {
            ("baseline".to_string(), "".to_string())
        } else {
            rep.comparisons
                .iter()
                .find(|c| c.label == run.label)
                .map(|c| {
                    (
                        format!(
                            "{:+.2} ({:+.2} … {:+.2})",
                            c.delta_points, c.ci_points.0, c.ci_points.1
                        ),
                        format!("{}/{}/{}", c.wins, c.ties, c.losses),
                    )
                })
                .unwrap_or_default()
        };
        let _ = writeln!(
            s,
            "| {} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {} | {} | {} | {} |",
            run.label,
            run.variant.replace('|', "/"),
            r[0],
            r[1],
            r[2],
            r[3],
            r[4],
            ms(percentile(&lat, 50.0)),
            ms(percentile(&lat, 95.0)),
            cmp.0,
            cmp.1
        );
    }
    s
}

pub fn markdown(rep: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# {}\n", rep.title);
    let _ = writeln!(
        s,
        "Golden set `{}`: {} queries, k = {}. Primary level: **{}**. The first engine is the baseline. \
         Δ is the mean nDCG@10 difference in points (x100) against the baseline, with a paired-bootstrap \
         95% CI (10k resamples). W/T/L counts per-query nDCG@10 wins, ties and losses.\n",
        rep.golden,
        rep.n_queries,
        rep.k,
        level_name(rep.primary)
    );
    let _ = writeln!(s, "## Summary ({} level)\n", level_name(rep.primary));
    s.push_str(&summary_table(rep));

    for level in [Level::File, Level::Chunk] {
        let _ = writeln!(
            s,
            "\n## {} level\n",
            if level == Level::File {
                "File"
            } else {
                "Chunk"
            }
        );
        let _ = writeln!(
            s,
            "| engine | nDCG@10 | R@5 | R@10 | R@50 | MRR@10 |\n|---|---|---|---|---|---|"
        );
        for run in &rep.runs {
            let r = row(run, level);
            let _ = writeln!(
                s,
                "| {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
                run.label, r[0], r[1], r[2], r[3], r[4]
            );
        }
    }

    // nDCG@10 by query kind (primary level)
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for q in &rep.runs[0].queries {
        *kinds.entry(q.kind.as_str()).or_default() += 1;
    }
    if kinds.len() > 1 {
        let _ = writeln!(
            s,
            "\n## nDCG@10 by query kind ({} level)\n",
            level_name(rep.primary)
        );
        let _ = write!(s, "| engine |");
        for (k, n) in &kinds {
            let _ = write!(s, " {k} (n={n}) |");
        }
        let _ = writeln!(s, "\n|---|{}", "---|".repeat(kinds.len()));
        for run in &rep.runs {
            let _ = write!(s, "| {} |", run.label);
            for k in kinds.keys() {
                let v = mean(run.queries.iter().filter(|q| q.kind == *k).map(
                    |q| match rep.primary {
                        Level::File => q.file.ndcg10,
                        Level::Chunk => q.chunk.ndcg10,
                    },
                ));
                let _ = write!(s, " {v:.3} |");
            }
            s.push('\n');
        }
    }

    if !rep.gates.is_empty() {
        let _ = writeln!(s, "\n## Gates\n");
        for g in &rep.gates {
            let _ = writeln!(
                s,
                "- **{}** {}: {}",
                if g.passed { "PASS" } else { "FAIL" },
                g.label,
                g.rule
            );
        }
    }

    // largest per-query regressions against the baseline (primary level)
    if rep.runs.len() > 1 {
        let _ = writeln!(
            s,
            "\n## Largest nDCG@10 regressions vs baseline ({} level, top 10 per engine)\n",
            level_name(rep.primary)
        );
        let pick = |q: &super::PerQuery| match rep.primary {
            Level::File => q.file.ndcg10,
            Level::Chunk => q.chunk.ndcg10,
        };
        for run in rep.runs.iter().skip(1) {
            let mut d: Vec<(f64, &str)> = run
                .queries
                .iter()
                .zip(&rep.runs[0].queries)
                .map(|(o, b)| (pick(o) - pick(b), o.qid.as_str()))
                .filter(|(x, _)| *x < -1e-9)
                .collect();
            d.sort_by(|a, b| a.0.total_cmp(&b.0));
            let list: Vec<String> = d
                .iter()
                .take(10)
                .map(|(x, q)| format!("{q} ({:+.2})", x))
                .collect();
            let _ = writeln!(
                s,
                "- {}: {}",
                run.label,
                if list.is_empty() {
                    "none".into()
                } else {
                    list.join(", ")
                }
            );
        }
    }

    let _ = writeln!(s, "\n## Engines\n");
    for run in &rep.runs {
        let errs = run.errors();
        let _ = writeln!(
            s,
            "- `{}`: engine `{}`, variant `{}`; {} latency samples{}",
            run.label,
            run.engine,
            run.variant,
            run.latencies().len(),
            if errs > 0 {
                format!("; **{errs} failed queries scored as empty**")
            } else {
                String::new()
            }
        );
    }
    s
}
