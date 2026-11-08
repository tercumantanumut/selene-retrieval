//! Ranking metrics. Pure functions; every formula is pinned by hand-computed tests below.
//!
//! Two relevance views of one ranked chunk list:
//! * **file level**: hits are collapsed to unique paths in first-occurrence order. A path's grade
//!   is the max grade of its golden units. Ideal = all labelled paths, grade-descending.
//! * **chunk level**: every hit keeps its rank. A hit earns the grade of the best *not yet
//!   credited* golden unit it overlaps (same path and line ranges intersect; a unit without spans
//!   covers the whole file). Each unit is credited at most once, so near-duplicate overlapping
//!   chunks don't inflate scores. Ideal = all unit grades, descending.
//!
//! Formulas (trec_eval conventions):
//! * nDCG@k = DCG@k / IDCG@k with linear gain and log2(rank+1) discount (rank is 1-based).
//! * recall@k = relevant items (grade >= 2) in the top k / all relevant items.
//! * MRR@10 = 1 / rank of the first relevant item within the top 10, else 0.

use std::collections::{HashMap, HashSet};

use super::golden::{GoldenQuery, Hit, RELEVANT_GRADE, normalize_path};

/// Gains in ranked order plus the ideal gain multiset for one query.
#[derive(Debug, Clone, PartialEq)]
pub struct Judged {
    pub gains: Vec<u8>,
    pub ideal: Vec<u8>,
}

pub fn judge_file_level(q: &GoldenQuery, hits: &[Hit], strip: &[String]) -> Judged {
    let mut grade_by_path: HashMap<&str, u8> = HashMap::new();
    for u in &q.relevant {
        let g = grade_by_path.entry(u.path.as_str()).or_insert(0);
        *g = (*g).max(u.grade);
    }
    let mut seen = HashSet::new();
    let mut gains = Vec::new();
    for h in hits {
        let p = normalize_path(&h.path, strip);
        if seen.insert(p.clone()) {
            gains.push(grade_by_path.get(p.as_str()).copied().unwrap_or(0));
        }
    }
    let mut ideal: Vec<u8> = grade_by_path.values().copied().filter(|g| *g > 0).collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    Judged { gains, ideal }
}

fn overlaps(hit: &Hit, spans: &[[u32; 2]]) -> bool {
    if spans.is_empty() {
        return true;
    }
    let (Some(s), Some(e)) = (hit.start_line, hit.end_line) else {
        return true; // a hit without line info is treated as covering the whole file
    };
    let (s, e) = (s.min(e), s.max(e));
    spans.iter().any(|[a, b]| s <= *b && *a <= e)
}

pub fn judge_chunk_level(q: &GoldenQuery, hits: &[Hit], strip: &[String]) -> Judged {
    let mut credited = vec![false; q.relevant.len()];
    let mut gains = Vec::with_capacity(hits.len());
    for h in hits {
        let p = normalize_path(&h.path, strip);
        let best = q
            .relevant
            .iter()
            .enumerate()
            .filter(|(i, u)| !credited[*i] && u.grade > 0 && u.path == p && overlaps(h, &u.spans))
            .max_by(|(ia, a), (ib, b)| a.grade.cmp(&b.grade).then(ib.cmp(ia)));
        match best {
            Some((i, u)) => {
                credited[i] = true;
                gains.push(u.grade);
            }
            None => gains.push(0),
        }
    }
    let mut ideal: Vec<u8> = q
        .relevant
        .iter()
        .map(|u| u.grade)
        .filter(|g| *g > 0)
        .collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    Judged { gains, ideal }
}

fn dcg(gains: &[u8], k: usize) -> f64 {
    gains
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, g)| f64::from(*g) / ((i + 2) as f64).log2())
        .sum()
}

pub fn ndcg_at(j: &Judged, k: usize) -> f64 {
    let idcg = dcg(&j.ideal, k);
    if idcg == 0.0 {
        0.0
    } else {
        dcg(&j.gains, k) / idcg
    }
}

/// `None` when the query has no relevant (grade >= 2) items.
pub fn recall_at(j: &Judged, k: usize) -> Option<f64> {
    let total = j.ideal.iter().filter(|g| **g >= RELEVANT_GRADE).count();
    if total == 0 {
        return None;
    }
    let found = j
        .gains
        .iter()
        .take(k)
        .filter(|g| **g >= RELEVANT_GRADE)
        .count();
    Some(found as f64 / total as f64)
}

pub fn mrr_at(j: &Judged, k: usize) -> f64 {
    j.gains
        .iter()
        .take(k)
        .position(|g| *g >= RELEVANT_GRADE)
        .map_or(0.0, |i| 1.0 / (i + 1) as f64)
}

/// The per-query metric vector reported by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct QueryMetrics {
    pub ndcg10: f64,
    pub recall5: f64,
    pub recall10: f64,
    pub recall50: f64,
    pub mrr10: f64,
}

pub fn query_metrics(j: &Judged) -> QueryMetrics {
    QueryMetrics {
        ndcg10: ndcg_at(j, 10),
        recall5: recall_at(j, 5).unwrap_or(0.0),
        recall10: recall_at(j, 10).unwrap_or(0.0),
        recall50: recall_at(j, 50).unwrap_or(0.0),
        mrr10: mrr_at(j, 10),
    }
}

pub fn mean(xs: impl IntoIterator<Item = f64>) -> f64 {
    let (s, n) = xs
        .into_iter()
        .fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    if n == 0 { 0.0 } else { s / n as f64 }
}

/// Nearest-rank percentile (p in 0..=100) of unsorted samples; `None` if empty.
pub fn percentile(samples: &[f64], p: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v.get(rank.min(v.len()) - 1).copied()
}

/// Paired bootstrap 95% CI of mean(b - a) over queries. Deterministic (fixed-seed xorshift).
pub fn paired_bootstrap_ci(a: &[f64], b: &[f64], resamples: usize, seed: u64) -> (f64, f64) {
    let d: Vec<f64> = a.iter().zip(b).map(|(x, y)| y - x).collect();
    if d.is_empty() {
        return (0.0, 0.0);
    }
    let mut state = seed.max(1);
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let n = d.len();
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| (0..n).map(|_| d[(next() % n as u64) as usize]).sum::<f64>() / n as f64)
        .collect();
    means.sort_by(f64::total_cmp);
    let lo = means[((resamples as f64) * 0.025) as usize];
    let hi = means[(((resamples as f64) * 0.975) as usize).min(resamples - 1)];
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::golden::Unit;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    fn j(gains: &[u8], ideal: &[u8]) -> Judged {
        Judged {
            gains: gains.to_vec(),
            ideal: ideal.to_vec(),
        }
    }

    fn unit(path: &str, grade: u8, spans: &[[u32; 2]]) -> Unit {
        Unit {
            path: path.into(),
            grade,
            spans: spans.to_vec(),
        }
    }

    fn query(units: Vec<Unit>) -> GoldenQuery {
        GoldenQuery {
            qid: "q".into(),
            query: "x".into(),
            kind: "nl".into(),
            relevant: units,
        }
    }

    #[test]
    fn ndcg_hand_computed() {
        // gains [3,2,3,0,1], ideal [3,3,2,1]
        // DCG  = 3/1 + 2/log2(3) + 3/2 + 0 + 1/log2(6) = 3 + 1.261859507 + 1.5 + 0.386852807
        //      = 6.148712314
        // IDCG = 3 + 3/log2(3) + 2/2 + 1/log2(5) = 3 + 1.892789261 + 1 + 0.430676558 = 6.323465819
        let x = j(&[3, 2, 3, 0, 1], &[3, 3, 2, 1]);
        approx(dcg(&x.gains, 10), 6.148_712_314_6);
        approx(ndcg_at(&x, 10), 6.148_712_314_6 / 6.323_465_819_3);
        // perfect ranking = 1, nothing relevant retrieved = 0
        approx(ndcg_at(&j(&[3, 3, 2, 1], &[3, 3, 2, 1]), 10), 1.0);
        approx(ndcg_at(&j(&[0, 0], &[3]), 10), 0.0);
        // cutoff: relevant item at rank 11 doesn't count at @10
        approx(
            ndcg_at(&j(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3], &[3]), 10),
            0.0,
        );
        // single relevant at rank 2: 1/log2(3) = 0.630929754
        approx(ndcg_at(&j(&[0, 2], &[2]), 10), 0.630_929_753_6);
    }

    #[test]
    fn recall_and_mrr_hand_computed() {
        // relevant (>=2) items: ideal has three of them (3, 2, 2); grade-1 units are ignored
        let x = j(&[1, 0, 2, 3, 0, 0, 2], &[3, 2, 2, 1]);
        approx(recall_at(&x, 1).unwrap(), 0.0); // the grade-1 hit at rank 1 doesn't count
        approx(recall_at(&x, 3).unwrap(), 1.0 / 3.0);
        approx(recall_at(&x, 5).unwrap(), 2.0 / 3.0);
        approx(recall_at(&x, 50).unwrap(), 1.0);
        approx(mrr_at(&x, 10), 1.0 / 3.0);
        approx(mrr_at(&x, 2), 0.0);
        assert_eq!(recall_at(&j(&[1], &[1]), 10), None);
    }

    #[test]
    fn file_level_collapses_duplicates_in_first_occurrence_order() {
        let q = query(vec![
            unit("a.ts", 3, &[[10, 20]]),
            unit("a.ts", 1, &[[90, 95]]),
            unit("b.ts", 2, &[]),
        ]);
        let hits = [
            Hit::new("c.ts", 1, 5),
            Hit::new("a.ts", 200, 210),
            Hit::new("a.ts", 10, 12),
            Hit::new("b.ts", 1, 2),
        ];
        let r = judge_file_level(&q, &hits, &[]);
        // ranked unique files: c (0), a (max grade 3, even though this chunk misses the span), b (2)
        assert_eq!(r, j(&[0, 3, 2], &[3, 2]));
        // DCG = 3/log2(3) + 2/2 = 2.892789261; IDCG = 3 + 2/log2(3) = 4.261859507
        approx(ndcg_at(&r, 10), 2.892_789_260_7 / 4.261_859_507_1);
        approx(mrr_at(&r, 10), 0.5);
    }

    #[test]
    fn chunk_level_credits_each_unit_once_and_prefers_highest_grade() {
        let q = query(vec![
            unit("a.ts", 3, &[[10, 20]]),
            unit("a.ts", 2, &[[15, 40]]),
            unit("b.md", 1, &[]),
        ]);
        let hits = [
            Hit::new("a.ts", 18, 30),   // overlaps both units: takes the grade-3 one
            Hit::new("a.ts", 12, 16),   // grade-3 unit already credited: takes the grade-2 unit
            Hit::new("a.ts", 19, 19),   // both credited: 0
            Hit::new("a.ts", 21, 30),   // only the credited grade-2 unit overlaps: 0
            Hit::new("b.md", 500, 510), // span-less unit covers the whole file
            Hit::new("a.ts", 41, 50),   // outside every span
        ];
        let r = judge_chunk_level(&q, &hits, &[]);
        assert_eq!(r, j(&[3, 2, 0, 0, 1, 0], &[3, 2, 1]));
        approx(recall_at(&r, 2).unwrap(), 1.0);
        // inclusive boundaries: a hit ending exactly on the span start overlaps
        let edge = judge_chunk_level(
            &query(vec![unit("a.ts", 3, &[[10, 20]])]),
            &[Hit::new("a.ts", 1, 10)],
            &[],
        );
        assert_eq!(edge.gains, vec![3]);
        let miss = judge_chunk_level(
            &query(vec![unit("a.ts", 3, &[[10, 20]])]),
            &[Hit::new("a.ts", 21, 30)],
            &[],
        );
        assert_eq!(miss.gains, vec![0]);
    }

    #[test]
    fn hit_without_lines_matches_whole_file_and_paths_are_normalised() {
        let q = query(vec![unit("lib/a.ts", 3, &[[10, 20]])]);
        let hit = Hit {
            path: r"C:\x\lib\a.ts".into(),
            start_line: None,
            end_line: None,
            score: None,
        };
        let r = judge_chunk_level(&q, &[hit], &[r"C:\x".into()]);
        assert_eq!(r.gains, vec![3]);
    }

    #[test]
    fn percentiles_nearest_rank() {
        let s = [5.0, 1.0, 4.0, 2.0, 3.0, 10.0, 9.0, 8.0, 7.0, 6.0];
        assert_eq!(percentile(&s, 50.0), Some(5.0));
        assert_eq!(percentile(&s, 95.0), Some(10.0));
        assert_eq!(percentile(&s, 0.0), Some(1.0));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[7.0], 95.0), Some(7.0));
    }

    #[test]
    fn bootstrap_ci_brackets_a_constant_shift_and_is_deterministic() {
        let a = [0.1, 0.5, 0.9, 0.3];
        let b: Vec<f64> = a.iter().map(|x| x + 0.2).collect();
        let (lo, hi) = paired_bootstrap_ci(&a, &b, 1000, 7);
        approx(lo, 0.2);
        approx(hi, 0.2);
        let c = [0.0, 1.0, 0.0, 1.0];
        assert_eq!(
            paired_bootstrap_ci(&a, &c, 500, 3),
            paired_bootstrap_ci(&a, &c, 500, 3)
        );
        let (lo, hi) = paired_bootstrap_ci(&a, &c, 2000, 3);
        assert!(lo < 0.0 && hi > 0.0 && lo <= hi);
    }

    #[test]
    fn mean_of_empty_is_zero() {
        approx(mean(Vec::<f64>::new()), 0.0);
        approx(mean([1.0, 2.0, 6.0]), 3.0);
    }
}
