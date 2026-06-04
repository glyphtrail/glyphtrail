//! Atlas embedding layer — scaffolding toward the semantic north star (#338).
//!
//! Atlas is built *toward* a queryable semantic graph: embed repos (and later
//! commits) so it answers "which of my repos are similar?" and "have I built this
//! before?". The real answer is a neural sentence embedding, but atlas is
//! **local-only by foundation** — commit text must never leave the machine — so a
//! cloud embedding API is off the table and a bundled neural model is a heavier,
//! separate decision (#338 open question).
//!
//! This module is the additive, provider-agnostic scaffolding: an [`Embedder`]
//! trait, an [`Embedding`] stored in a side-table keyed by node id (mirroring the
//! `Commit` side-table), and [`cosine`] similarity. The default [`HashingEmbedder`]
//! is a fully-local lexical model (the hashing trick over bag-of-words): real,
//! deterministic, dependency-free. It captures lexical/topic overlap between repos
//! that share vocabulary; it does **not** capture cross-stack "same idea, different
//! words" similarity — that needs a neural embedder dropped in behind this trait,
//! which the side-table + `similar` query are already shaped for.

use crate::NodeId;

/// A stored embedding: the dense vector for one node (a `Repo` today), keyed by
/// the node's id so a future neural vector replaces it in place.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    pub node_id: NodeId,
    pub vector: Vec<f32>,
}

/// Turns text into a dense vector. The default is local + lexical
/// ([`HashingEmbedder`]); a neural provider implements the same trait so the
/// side-table and `atlas similar` are unchanged when one is added (#338).
pub trait Embedder {
    /// Vector length (every vector this embedder produces has this dimension).
    fn dim(&self) -> usize;
    /// A stable provider id recorded with the index, so a later re-embed under a
    /// different model can be detected.
    fn id(&self) -> &str;
    /// Embed `text` into a unit-length vector of length [`Self::dim`].
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Local, deterministic lexical embedder: the *hashing trick*. Each token is
/// hashed into one of `dim` buckets, occurrences accumulate with sublinear
/// weighting, and the vector is L2-normalised. Cosine similarity of two such
/// vectors approximates their shared-vocabulary overlap — enough for repo↔repo and
/// query↔repo lexical/topic similarity, with no model or network.
#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    dim: usize,
}

/// Default vector width — small enough to brute-force, wide enough to keep token
/// collisions rare for commit-scale vocabularies.
pub const DEFAULT_DIM: usize = 256;

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new(DEFAULT_DIM)
    }
}

impl HashingEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        "lexical-hash-v1"
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut counts = vec![0u32; self.dim];
        for token in tokenize(text) {
            let bucket = (fnv1a(token.as_bytes()) % self.dim as u64) as usize;
            counts[bucket] += 1;
        }
        // Sublinear term weighting (`1 + ln(tf)`), then L2-normalise so cosine is a
        // plain dot product and document length doesn't dominate.
        let mut v: Vec<f32> = counts
            .into_iter()
            .map(|c| if c == 0 { 0.0 } else { 1.0 + (c as f32).ln() })
            .collect();
        l2_normalize(&mut v);
        v
    }
}

/// Cosine similarity in `[-1, 1]` (0 for a length mismatch or a zero vector). With
/// unit-length inputs this is just the dot product, but the norms are divided out
/// so it's correct for any vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Lowercase alphanumeric tokens of length ≥ 2, splitting on everything else and
/// dropping purely-numeric runs (commit hashes / issue numbers carry no topical
/// signal). Deliberately light on stopwords — domain words are the signal.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 2 && !t.bytes().all(|b| b.is_ascii_digit()))
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// FNV-1a 64-bit hash — small, stable, no dependency, good enough for bucketing.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// L2-normalise a vector in place (no-op for a zero vector).
fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v {
            *x /= norm;
        }
    }
}

/// A repo's structural profile: histograms over its code graph's node kinds, edge
/// kinds, and languages. The structural analogue of the lexical document — the
/// input a [`GraphEmbedder`] turns into a vector (#338).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphProfile {
    pub node_kinds: Vec<(String, usize)>,
    pub edge_kinds: Vec<(String, usize)>,
    pub languages: Vec<(String, usize)>,
}

/// The model id recorded with a structural graph embedding, so it's distinguishable
/// from a text embedding in the shared side-table.
pub const GRAPH_MODEL_ID: &str = "graph-struct-v1";

/// Turns a repo's [`GraphProfile`] into a vector, so repos with a similar
/// architecture (kind / edge / language mix) read as similar — the structural
/// counterpart to text [`Embedder`]. Local-first and pluggable: a real graph model
/// (a GNN over the actual graph) implements the same trait later.
pub trait GraphEmbedder {
    fn dim(&self) -> usize;
    fn id(&self) -> &str;
    fn embed(&self, profile: &GraphProfile) -> Vec<f32>;
}

/// Local structural graph embedder: hashes each `facet:name` feature (a node kind,
/// edge kind, or language) into a bucket weighted by the square root of its count
/// (so a huge file count doesn't swamp the signal), then L2-normalises. Cosine of
/// two such vectors reflects how alike two repos' structural distributions are.
#[derive(Debug, Clone)]
pub struct StructuralEmbedder {
    dim: usize,
}

impl Default for StructuralEmbedder {
    fn default() -> Self {
        Self::new(DEFAULT_DIM)
    }
}

impl StructuralEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }

    fn accumulate(&self, v: &mut [f32], facet: &str, features: &[(String, usize)]) {
        for (name, count) in features {
            if *count == 0 {
                continue;
            }
            // Normalise the feature name so a source emitting "Function" lands in the
            // same bucket as one emitting "function".
            let feature = format!("{facet}:{}", name.trim().to_ascii_lowercase());
            let bucket = (fnv1a(feature.as_bytes()) % self.dim as u64) as usize;
            v[bucket] += (*count as f32).sqrt();
        }
    }
}

impl GraphEmbedder for StructuralEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        GRAPH_MODEL_ID
    }

    fn embed(&self, profile: &GraphProfile) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        self.accumulate(&mut v, "nk", &profile.node_kinds);
        self.accumulate(&mut v, "ek", &profile.edge_kinds);
        self.accumulate(&mut v, "lang", &profile.languages);
        l2_normalize(&mut v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn embeds_to_unit_length() {
        let e = HashingEmbedder::default();
        let v = e.embed("a wgpu renderer for the game engine");
        check!(v.len() == DEFAULT_DIM);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        check!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn empty_text_is_a_zero_vector() {
        let e = HashingEmbedder::default();
        let v = e.embed("  123  !!  ");
        check!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn shared_vocabulary_scores_higher_than_disjoint() {
        let e = HashingEmbedder::default();
        let renderer = e.embed("wgpu renderer for the game engine, sprite batching");
        let renderer2 = e.embed("game engine renderer using wgpu and sprite atlases");
        let parser = e.embed("sql parser and query planner for postgres");
        check!(cosine(&renderer, &renderer2) > cosine(&renderer, &parser));
    }

    #[test]
    fn identical_text_is_maximally_similar() {
        let e = HashingEmbedder::default();
        let a = e.embed("tree-sitter incremental parsing");
        let b = e.embed("tree-sitter incremental parsing");
        check!((cosine(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_handles_mismatched_lengths() {
        check!(cosine(&[1.0, 0.0], &[1.0]) == 0.0);
    }

    fn profile(nk: &[(&str, usize)], lang: &[(&str, usize)]) -> GraphProfile {
        GraphProfile {
            node_kinds: nk.iter().map(|(k, c)| (k.to_string(), *c)).collect(),
            edge_kinds: vec![("Calls".to_string(), 50)],
            languages: lang.iter().map(|(k, c)| (k.to_string(), *c)).collect(),
        }
    }

    #[test]
    fn structural_embedding_is_unit_length() {
        let e = StructuralEmbedder::default();
        let v = e.embed(&profile(
            &[("Function", 100), ("Struct", 20)],
            &[("rust", 30)],
        ));
        check!(v.len() == DEFAULT_DIM);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        check!((norm - 1.0).abs() < 1e-5);
        check!(e.id() == GRAPH_MODEL_ID);
    }

    #[test]
    fn similar_architectures_score_higher_than_different_ones() {
        let e = StructuralEmbedder::default();
        // Two Rust function-heavy repos vs an HTML/CSS, table-heavy one.
        let code_a = e.embed(&profile(
            &[("Function", 200), ("Struct", 40)],
            &[("rust", 60)],
        ));
        let code_b = e.embed(&profile(
            &[("Function", 150), ("Struct", 30)],
            &[("rust", 50)],
        ));
        let data_repo = e.embed(&profile(&[("Table", 80), ("Column", 400)], &[("sql", 40)]));
        check!(cosine(&code_a, &code_b) > cosine(&code_a, &data_repo));
    }

    #[test]
    fn empty_profile_is_a_zero_vector() {
        let e = StructuralEmbedder::default();
        let v = e.embed(&GraphProfile::default());
        check!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn feature_casing_does_not_change_the_embedding() {
        let e = StructuralEmbedder::default();
        let upper = e.embed(&profile(
            &[("Function", 100), ("Struct", 20)],
            &[("Rust", 30)],
        ));
        let lower = e.embed(&profile(
            &[("function", 100), ("struct", 20)],
            &[("rust", 30)],
        ));
        check!((cosine(&upper, &lower) - 1.0).abs() < 1e-6);
    }
}
