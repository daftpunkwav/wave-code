//! Fuzzy subsequence matching for completion ranking.
//!
//! Lower scores rank higher. The scoring mirrors the reference matcher:
//! subsequence filter, then bonus for consecutive runs and word-boundary
//! hits, penalty for position and gaps, and a dominant bonus for exact
//! matches.

/// Score `query` against `candidate` (both matched case-insensitively).
/// Returns `None` when the candidate is not a subsequence match; the
/// returned score otherwise (lower is better).
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    let query = query.to_lowercase();
    let candidate_lower = candidate.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let qchars: Vec<char> = query.chars().collect();
    let cchars: Vec<char> = candidate_lower.chars().collect();
    if qchars.len() > cchars.len() {
        return None;
    }

    let mut total: i64 = 0;
    let mut search_from = 0usize;
    let mut prev_matched: Option<usize> = None;
    for (qi, &qc) in qchars.iter().enumerate() {
        let found = (search_from..cchars.len()).find(|&ci| cchars[ci] == qc)?;
        // Gap penalty: one point per skipped candidate char.
        let gap = found - search_from;
        if qi > 0 {
            total += 2 * gap as i64;
        }
        // Consecutive-match bonus, growing with the run length.
        if prev_matched == Some(found.wrapping_sub(1)) {
            total -= 5;
        }
        // Word-boundary bonus.
        if is_boundary(&cchars, found) {
            total -= 10;
        }
        // Position penalty: earlier matches rank higher.
        total += (found as f64 * 0.1) as i64;
        prev_matched = Some(found);
        search_from = found + 1;
    }
    if candidate_lower == query {
        total -= 100;
    }
    Some(total)
}

/// True when the character at `index` starts a word: string start or
/// after a separator (`/`, space, `-`, `_`, `.`, `@`).
fn is_boundary(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    matches!(
        chars.get(index.wrapping_sub(1)),
        Some('/' | ' ' | '-' | '_' | '.' | '@')
    )
}

/// True when `query` fuzzy-matches `candidate`.
pub fn matches(query: &str, candidate: &str) -> bool {
    score(query, candidate).is_some()
}

/// Sort-and-filter convenience: keep matching candidates, ordered best
/// (lowest score) first.
pub fn filter<'a, I: IntoIterator<Item = &'a str>>(query: &str, candidates: I) -> Vec<&'a str> {
    let mut scored: Vec<(i64, &str)> = candidates
        .into_iter()
        .filter_map(|c| score(query, c).map(|s| (s, c)))
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    scored.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_required() {
        assert!(matches("he", "hello"));
        assert!(matches("hlo", "hello"));
        assert!(!matches("hx", "hello"));
        assert!(matches("CC", "CompactCompleted"));
    }

    #[test]
    fn exact_and_boundaries_rank_first() {
        let ranked = filter("he", ["shell", "help", "hello"]);
        assert_eq!(
            ranked[0], "help",
            "exact prefix at boundary wins: {ranked:?}"
        );
        // All subsequence matches come back, best-first.
        assert_eq!(ranked.len(), 3);
    }

    #[test]
    fn consecutive_beats_scattered() {
        let ranked = filter("ab", ["axbx", "abxx"]);
        assert_eq!(ranked[0], "abxx");
    }

    #[test]
    fn empty_query_matches_everything_equally() {
        let ranked = filter("", ["b", "a"]);
        assert_eq!(ranked.len(), 2);
    }
}
