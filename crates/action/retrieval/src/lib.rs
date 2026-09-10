/*!
 * @file TermRetriever
 * @description Term-overlap retrieval over chunked documents.
 *
 * Responsibilities:
 * - Split documents into overlapping character windows.
 * - Score chunks by query term overlap.
 * - Return the top matches in ranked order.
 *
 * This module must not depend on: any other workspace crate. Ranking is
 * deliberately lexical: semantic embeddings arrive behind the same
 * function shape later.
 */

//! Retrieval as pure functions over caller-owned documents.

/// One retrievable document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// Stable document identifier.
    pub id: String,
    /// Full document text.
    pub text: String,
}

/// One scored chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredChunk {
    /// Source document id.
    pub doc_id: String,
    /// Chunk text.
    pub chunk: String,
    /// Number of distinct query terms present.
    pub score: usize,
}

/// Split text into `window`-sized character windows with `overlap`.
///
/// Short texts yield a single window; empty texts yield none.
pub fn chunk_text(text: &str, window: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || window == 0 {
        return Vec::new();
    }
    let step = window.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + window).min(chars.len());
        out.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start += step;
    }
    out
}

/// Tokenize into lowercase alphanumeric terms.
fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Count distinct query terms present in the chunk.
fn score_chunk(query_terms: &[String], chunk: &str) -> usize {
    if query_terms.is_empty() {
        return 0;
    }
    let lower = chunk.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut score = 0;
    for term in query_terms {
        if seen.insert(term) && lower.contains(term.as_str()) {
            score += 1;
        }
    }
    score
}

/// Retrieve the top `top_k` chunks across documents by term overlap.
///
/// Empty queries match nothing; ties keep document then chunk order.
pub fn retrieve(documents: &[Document], query: &str, top_k: usize) -> Vec<ScoredChunk> {
    let query_terms = terms(query);
    let mut scored = Vec::new();
    for (doc_index, doc) in documents.iter().enumerate() {
        for (chunk_index, chunk) in chunk_text(&doc.text, 400, 50).iter().enumerate() {
            let score = score_chunk(&query_terms, chunk);
            if score > 0 {
                scored.push((
                    doc_index,
                    chunk_index,
                    ScoredChunk {
                        doc_id: doc.id.clone(),
                        chunk: chunk.clone(),
                        score,
                    },
                ));
            }
        }
    }
    scored.sort_by(|a, b| {
        b.2.score
            .cmp(&a.2.score)
            .then(a.0.cmp(&b.0))
            .then(a.1.cmp(&b.1))
    });
    scored
        .into_iter()
        .take(top_k)
        .map(|(_, _, chunk)| chunk)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs() -> Vec<Document> {
        vec![
            Document {
                id: "a".to_string(),
                text: "The approval gate parks requests by call id.".to_string(),
            },
            Document {
                id: "b".to_string(),
                text: "Cron expressions match civil time fields.".to_string(),
            },
        ]
    }

    #[test]
    fn ranking_prefers_term_overlap() {
        let hits = retrieve(&docs(), "approval gate call id", 5);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].doc_id, "a");
        assert!(hits[0].score >= 3);
    }

    #[test]
    fn empty_queries_match_nothing() {
        assert!(retrieve(&docs(), "", 5).is_empty());
        assert!(retrieve(&[], "anything", 5).is_empty());
    }

    #[test]
    fn top_k_caps_results() {
        let hits = retrieve(&docs(), "the", 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn chunking_windows_with_overlap() {
        let chunks = chunk_text("abcdefghij", 4, 2);
        assert_eq!(chunks, vec!["abcd", "cdef", "efgh", "ghij"]);
        assert!(chunk_text("", 4, 2).is_empty());
    }
}
