//! Pure ranking functions: reciprocal rank fusion and maximal marginal relevance.

use std::collections::HashMap;

use sr_common::ids::ChunkId;

/// Reciprocal rank fusion of ranked lists: `score(d) = Σ 1 / (k + rank)`, rank from 1.
/// Ties keep first-seen order (stable), so results are deterministic.
pub fn rrf_fuse(lists: &[&[ChunkId]], k: u32) -> Vec<(ChunkId, f32)> {
    let mut order: Vec<ChunkId> = Vec::new();
    let mut score: HashMap<ChunkId, f32> = HashMap::new();
    for list in lists {
        for (i, id) in list.iter().enumerate() {
            let s = 1.0 / (k as f32 + i as f32 + 1.0);
            score.entry(*id).and_modify(|v| *v += s).or_insert_with(|| {
                order.push(*id);
                s
            });
        }
    }
    let mut out: Vec<(ChunkId, f32)> = order
        .into_iter()
        .map(|id| (id, score.get(&id).copied().unwrap_or(0.0)))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// Cosine similarity (0 for zero vectors or mismatched lengths).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Maximal marginal relevance re-selection.
///
/// `candidates` are `(index, relevance)` in rank order; relevance is min-max normalised here.
/// `vectors[index]` is the candidate's embedding (`None` = unknown: treated as dissimilar to all).
/// Returns indices in MMR order, at most `k`. `lambda = 1` keeps the input order.
pub fn mmr(
    candidates: &[(usize, f32)],
    vectors: &[Option<&[f32]>],
    lambda: f32,
    k: usize,
) -> Vec<usize> {
    let lambda = lambda.clamp(0.0, 1.0);
    if candidates.is_empty() || k == 0 {
        return Vec::new();
    }
    let (lo, hi) = candidates.iter().fold((f32::MAX, f32::MIN), |(lo, hi), c| {
        (lo.min(c.1), hi.max(c.1))
    });
    let span = (hi - lo).max(f32::EPSILON);
    let rel: Vec<f32> = candidates.iter().map(|c| (c.1 - lo) / span).collect();
    let mut chosen: Vec<usize> = Vec::with_capacity(k.min(candidates.len()));
    let mut used = vec![false; candidates.len()];
    while chosen.len() < k.min(candidates.len()) {
        let mut best: Option<(usize, f32)> = None;
        for (pos, c) in candidates.iter().enumerate() {
            if used[pos] {
                continue;
            }
            let max_sim = chosen
                .iter()
                .map(|&j| {
                    match (
                        vectors.get(c.0).copied().flatten(),
                        vectors.get(j).copied().flatten(),
                    ) {
                        (Some(a), Some(b)) => cosine(a, b),
                        _ => 0.0,
                    }
                })
                .fold(0.0f32, f32::max);
            let score = lambda * rel[pos] - (1.0 - lambda) * max_sim;
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((pos, score));
            }
        }
        let Some((pos, _)) = best else { break };
        used[pos] = true;
        chosen.push(candidates[pos].0);
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_common::ids::{ContentHash, FileId};

    fn id(n: u32) -> ChunkId {
        let cfg = sr_common::ids::ChunkCfgFp::compute("t", "{}", "t", "1");
        ChunkId::derive(
            FileId::from_bytes([0; 16]),
            &ContentHash::of_bytes(b"x"),
            &cfg,
            n,
        )
    }

    #[test]
    fn rrf_rewards_agreement() {
        let (a, b, c) = (id(1), id(2), id(3));
        let fused = rrf_fuse(&[&[a, b], &[b, c]], 60);
        assert_eq!(fused.first().map(|x| x.0), Some(b));
        assert_eq!(fused.len(), 3);
        assert!((fused[0].1 - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-6);
    }

    #[test]
    fn mmr_prefers_diverse_results() {
        let v0 = [1.0, 0.0];
        let v1 = [0.99, 0.01]; // near-duplicate of v0
        let v2 = [0.0, 1.0];
        let vecs: Vec<Option<&[f32]>> = vec![Some(&v0), Some(&v1), Some(&v2)];
        let cands = [(0, 1.0), (1, 0.95), (2, 0.9)];
        assert_eq!(mmr(&cands, &vecs, 1.0, 3), vec![0, 1, 2]);
        assert_eq!(mmr(&cands, &vecs, 0.5, 2), vec![0, 2]);
    }

    #[test]
    fn cosine_edge_cases() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
        assert!((cosine(&[1.0, 1.0], &[2.0, 2.0]) - 1.0).abs() < 1e-6);
    }
}
