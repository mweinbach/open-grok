//! Closest-match diagnostics for failed hunk applications.
//!
//! When `seek_sequence` cannot locate a hunk's expected lines, the bare codex
//! error only echoes the model's own (wrong) guess back at it, which in
//! practice produces byte-identical retries. These helpers find the region of
//! the file most similar to the expected lines so the failure message can
//! point at what the file actually contains, with line numbers and a
//! similarity score.
//!
//! The first line of the failure message stays byte-identical to codex
//! (`Failed to find expected lines in <path>:\n<lines>`); everything produced
//! here is appended after it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

/// Cap on chars compared per line pair (keeps Levenshtein bounded).
const MAX_CMP_CHARS: usize = 120;
/// Skip the brute-force fallback scan on very large files.
const MAX_FALLBACK_FILE_LINES: usize = 20_000;
/// Below this similarity the "closest" region is noise, not signal.
const MIN_REPORT_SIMILARITY: f32 = 0.3;
/// Cap on lines echoed back from the matched region.
const MAX_DISPLAY_LINES: usize = 20;
/// Number of vote-anchored candidate windows to refine with full scoring.
const MAX_CANDIDATE_STARTS: usize = 5;

/// The most similar region of the file for a pattern that failed to match.
pub struct ClosestMatch {
    /// 0-based index of the window start in the file.
    pub start: usize,
    /// Window length (pattern length clamped to the file length).
    pub len: usize,
    /// Mean per-line similarity in `0.0..=1.0`.
    pub similarity: f32,
}

/// Diagnostic text appended after a "Failed to find expected lines" (or
/// "Failed to find context") error. `search_start` is the line index the
/// failed `seek_sequence` began searching from, used to detect hunks whose
/// match exists but earlier in the file than the previous hunk's match.
pub fn diagnose_missing_lines(
    lines: &[String],
    pattern: &[String],
    path: &Path,
    search_start: usize,
) -> String {
    let no_match = || {
        format!(
            "No similar region was found in {}. Re-read the file to see its current contents.",
            path.display()
        )
    };
    let Some(m) = closest_match(lines, pattern) else {
        return no_match();
    };
    if m.similarity < MIN_REPORT_SIMILARITY {
        return no_match();
    }

    let mut out = format!(
        "Closest match in {} at lines {}-{} (similarity: {:.1}%):\n",
        path.display(),
        m.start + 1,
        m.start + m.len,
        m.similarity * 100.0
    );
    for (offset, line) in lines[m.start..m.start + m.len]
        .iter()
        .take(MAX_DISPLAY_LINES)
        .enumerate()
    {
        let _ = writeln!(out, "{}\t{}", m.start + 1 + offset, line);
    }
    if m.len > MAX_DISPLAY_LINES {
        let _ = writeln!(out, "\t… ({} more lines)", m.len - MAX_DISPLAY_LINES);
    }
    if m.similarity >= 0.999 && m.start < search_start {
        let _ = write!(
            out,
            "These lines exist but occur BEFORE the position where the previous hunk matched; hunks must be ordered top-to-bottom within the file."
        );
    }
    out.trim_end().to_string()
}

/// Find the window of the file most similar to `pattern`.
///
/// Strategy: index the file's trimmed lines, let every pattern line that
/// exists verbatim vote for the window start that would align it, then refine
/// the top-voted starts with per-line similarity scoring. When no pattern
/// line exists verbatim anywhere, fall back to anchoring on the fuzzy best
/// match for the first non-blank pattern line (bounded by file size).
pub fn closest_match(lines: &[String], pattern: &[String]) -> Option<ClosestMatch> {
    let pattern = trim_trailing_blank(pattern);
    if lines.is_empty() || pattern.is_empty() {
        return None;
    }
    let win = pattern.len().min(lines.len());
    let max_start = lines.len() - win;

    let mut index: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, line) in lines.iter().enumerate() {
        let t = line.trim();
        if !t.is_empty() {
            index.entry(t).or_default().push(j);
        }
    }

    let mut votes: HashMap<usize, usize> = HashMap::new();
    for (i, pat) in pattern.iter().enumerate() {
        let t = pat.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(positions) = index.get(t) {
            for &j in positions {
                let start = j.saturating_sub(i).min(max_start);
                *votes.entry(start).or_default() += 1;
            }
        }
    }

    let candidates: Vec<usize> = if votes.is_empty() {
        if lines.len() > MAX_FALLBACK_FILE_LINES {
            return None;
        }
        let anchor = pattern.iter().find(|l| !l.trim().is_empty())?;
        let (best_j, best_sim) = lines
            .iter()
            .enumerate()
            .map(|(j, line)| (j, line_similarity(line, anchor)))
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        if best_sim <= 0.0 {
            return None;
        }
        vec![best_j.min(max_start)]
    } else {
        let mut starts: Vec<(usize, usize)> = votes.into_iter().collect();
        starts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        starts
            .into_iter()
            .take(MAX_CANDIDATE_STARTS)
            .map(|(s, _)| s)
            .collect()
    };

    let mut best_start = 0usize;
    let mut best_sim = f32::MIN;
    for start in candidates {
        let total: f32 = (0..win)
            .map(|k| line_similarity(&lines[start + k], &pattern[k]))
            .sum();
        // Pattern lines beyond the end of a too-short file score zero.
        let similarity = total / pattern.len() as f32;
        if similarity > best_sim {
            best_sim = similarity;
            best_start = start;
        }
    }
    if best_sim < 0.0 {
        return None;
    }
    Some(ClosestMatch {
        start: best_start,
        len: win,
        similarity: best_sim,
    })
}

/// Drop trailing blank lines (e.g. the end-of-file sentinel a patch may carry).
fn trim_trailing_blank(pattern: &[String]) -> &[String] {
    let mut end = pattern.len();
    while end > 0 && pattern[end - 1].trim().is_empty() {
        end -= 1;
    }
    &pattern[..end]
}

/// Similarity of two lines in `0.0..=1.0`: trimmed equality short-circuits to
/// 1.0, otherwise normalized Levenshtein over length-capped chars.
fn line_similarity(a: &str, b: &str) -> f32 {
    let (ta, tb) = (a.trim(), b.trim());
    if ta == tb {
        return 1.0;
    }
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let ca: Vec<char> = ta.chars().take(MAX_CMP_CHARS).collect();
    let cb: Vec<char> = tb.chars().take(MAX_CMP_CHARS).collect();
    let max_len = ca.len().max(cb.len());
    1.0 - levenshtein(&ca, &cb) as f32 / max_len as f32
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn finds_best_window_when_pattern_lines_are_not_adjacent() {
        // Mirrors the real-world failure: the pattern guesses that three
        // existing lines are adjacent when the file has another line between.
        let lines = to_vec(&[
            "var errorMessage: String?",
            "var showingRestPicker = false",
            "var currentRestDuration: TimeInterval = 0",
            "var showingExerciseEditor = false",
            "var isSavingHistory = false",
        ]);
        let pattern = to_vec(&[
            "var showingRestPicker = false",
            "var showingExerciseEditor = false",
            "var isSavingHistory = false",
        ]);
        let m = closest_match(&lines, &pattern).unwrap();
        // Best alignment anchors the last two pattern lines exactly (start 2).
        assert_eq!(m.start, 2);
        assert_eq!(m.len, 3);
        assert!(m.similarity > 0.5, "similarity was {}", m.similarity);
        assert!(m.similarity < 1.0);
    }

    #[test]
    fn fallback_matches_near_miss_line_without_verbatim_anchor() {
        let lines = to_vec(&["fn hello_world() {", "    body();", "}"]);
        let pattern = to_vec(&["fn helo_world() {"]);
        let m = closest_match(&lines, &pattern).unwrap();
        assert_eq!(m.start, 0);
        assert!(m.similarity > 0.8);
    }

    #[test]
    fn empty_file_or_pattern_returns_none() {
        assert!(closest_match(&[], &to_vec(&["x"])).is_none());
        assert!(closest_match(&to_vec(&["x"]), &[]).is_none());
        assert!(closest_match(&to_vec(&["x"]), &to_vec(&["", "  "])).is_none());
    }

    #[test]
    fn pattern_longer_than_file_clamps_window() {
        let lines = to_vec(&["a", "b"]);
        let pattern = to_vec(&["a", "b", "c", "d"]);
        let m = closest_match(&lines, &pattern).unwrap();
        assert_eq!(m.start, 0);
        assert_eq!(m.len, 2);
        // Two exact lines out of four pattern lines.
        assert!((m.similarity - 0.5).abs() < 0.01);
    }

    #[test]
    fn diagnose_reports_line_numbers_and_similarity() {
        let lines = to_vec(&["alpha", "beta", "gamma", "delta"]);
        let pattern = to_vec(&["beta", "gamma", "epsilon"]);
        let msg = diagnose_missing_lines(&lines, &pattern, &PathBuf::from("f.txt"), 0);
        assert!(msg.contains("Closest match in f.txt at lines 2-4"), "{msg}");
        assert!(msg.contains("(similarity:"), "{msg}");
        assert!(msg.contains("2\tbeta"), "{msg}");
    }

    #[test]
    fn diagnose_notes_out_of_order_hunks() {
        let lines = to_vec(&["a", "b", "c", "d"]);
        let pattern = to_vec(&["a"]);
        // Exact match exists at index 0 but the search started at 2.
        let msg = diagnose_missing_lines(&lines, &pattern, &PathBuf::from("f.txt"), 2);
        assert!(msg.contains("BEFORE"), "{msg}");
        assert!(msg.contains("ordered top-to-bottom"), "{msg}");
    }

    #[test]
    fn diagnose_reports_no_similar_region_for_unrelated_pattern() {
        let lines = to_vec(&["alpha", "beta"]);
        let pattern = to_vec(&["zzzzzzzzzzzzzzzzzzzz"]);
        let msg = diagnose_missing_lines(&lines, &pattern, &PathBuf::from("f.txt"), 0);
        assert!(
            msg.contains("No similar region was found in f.txt"),
            "{msg}"
        );
    }
}
