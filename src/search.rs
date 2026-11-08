//! Bounded literal screen documents, Unicode matching and original-byte highlights.

use crate::model::clean_text;
use std::collections::HashSet;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub const MAX_CONTENT_BYTES: usize = 65_536;
pub const MAX_CONTENT_LINES: usize = 300;
pub const MAX_SNIPPET_CHARS: usize = 240;

pub fn normalize(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    text.nfkc().case_fold().collect()
}

pub fn query_terms(query: &str) -> Vec<String> {
    let normalized = normalize(&clean_text(query, false));
    let mut seen = HashSet::new();
    normalized
        .split_whitespace()
        .filter(|term| seen.insert((*term).to_owned()))
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MappingRun {
    normalized_start: usize,
    normalized_end: usize,
    original_start: usize,
    original_end: usize,
    // Linear runs preserve byte offsets; other runs expand hits to the full original segment.
    linear: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PreparedText {
    normalized: String,
    runs: Vec<MappingRun>,
}

impl PreparedText {
    fn new(text: &str) -> Self {
        if text.is_ascii() {
            return Self {
                normalized: text.to_ascii_lowercase(),
                runs: if text.is_empty() {
                    Vec::new()
                } else {
                    vec![MappingRun {
                        normalized_start: 0,
                        normalized_end: text.len(),
                        original_start: 0,
                        original_end: text.len(),
                        linear: true,
                    }]
                },
            };
        }
        let normalized = normalize(text);
        // Reuse byte offsets only when normalization preserves every character boundary.
        if text.len() == normalized.len()
            && text
                .char_indices()
                .map(|(offset, _)| offset)
                .eq(normalized.char_indices().map(|(offset, _)| offset))
            && text
                .graphemes(true)
                .all(|grapheme| grapheme.chars().count() == 1)
        {
            return Self {
                normalized,
                runs: vec![MappingRun {
                    normalized_start: 0,
                    normalized_end: text.len(),
                    original_start: 0,
                    original_end: text.len(),
                    linear: true,
                }],
            };
        }
        let mut result = Self::default();
        let mut pending_start = 0;
        let mut pending_end = 0;
        let mut pending_normalized = String::new();
        for (start, grapheme) in text.grapheme_indices(true) {
            let end = start + grapheme.len();
            let normalized = normalize(grapheme);
            if pending_end > pending_start {
                let combined = normalize(&text[pending_start..end]);
                if combined != pending_normalized.clone() + &normalized {
                    // NFKC composition can join adjacent compatibility graphemes.
                    pending_end = end;
                    pending_normalized = combined;
                    continue;
                }
                result.push(
                    &text[pending_start..pending_end],
                    pending_start,
                    &pending_normalized,
                );
            }
            pending_start = start;
            pending_end = end;
            pending_normalized = normalized;
        }
        if pending_end > pending_start {
            result.push(
                &text[pending_start..pending_end],
                pending_start,
                &pending_normalized,
            );
        }
        result
    }

    fn push(&mut self, original: &str, original_start: usize, folded: &str) {
        let normalized_start = self.normalized.len();
        self.normalized.push_str(folded);
        if folded.is_empty() {
            return;
        }
        let linear = original.len() == folded.len()
            && original.chars().count() == 1
            && folded.chars().count() == 1;
        if linear
            && let Some(previous) = self.runs.last_mut()
            && previous.linear
            && previous.normalized_end == normalized_start
            && previous.original_end == original_start
        {
            previous.normalized_end += folded.len();
            previous.original_end += original.len();
            return;
        }
        self.runs.push(MappingRun {
            normalized_start,
            normalized_end: self.normalized.len(),
            original_start,
            original_end: original_start + original.len(),
            linear,
        });
    }

    fn original_range(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        if start >= end || end > self.normalized.len() {
            return None;
        }
        let first = self
            .runs
            .get(self.runs.partition_point(|run| run.normalized_end <= start))?;
        let last = self
            .runs
            .get(self.runs.partition_point(|run| run.normalized_end < end))?;
        let original_start = if first.linear {
            first.original_start + start - first.normalized_start
        } else {
            first.original_start
        };
        let original_end = if last.linear {
            last.original_start + end - last.normalized_start
        } else {
            last.original_end
        };
        Some((original_start, original_end))
    }

    fn highlights(&self, terms: &[String]) -> Vec<(usize, usize)> {
        let mut hits = Vec::new();
        for term in terms {
            if term.is_empty() {
                continue;
            }
            for (start, _) in self.normalized.match_indices(term.as_str()) {
                if let Some(range) = self.original_range(start, start + term.len()) {
                    hits.push(range);
                }
            }
        }
        hits.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in hits {
            if let Some(previous) = merged.last_mut()
                && start <= previous.1
            {
                previous.1 = previous.1.max(end);
                continue;
            }
            merged.push((start, end));
        }
        merged
    }
}

pub fn highlight_spans(text: &str, query: &str) -> Vec<(usize, usize)> {
    highlight_spans_terms(text, &query_terms(query))
}

pub fn highlight_spans_terms(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    if terms.is_empty() {
        return Vec::new();
    }
    PreparedText::new(text).highlights(terms)
}

fn bounded_suffix(text: &str, limit: usize) -> &str {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

pub fn clean_body(text: &str) -> String {
    let cleaned = clean_text(bounded_suffix(text, MAX_CONTENT_BYTES), true);
    let lines: Vec<&str> = cleaned.split('\n').collect();
    let bounded = lines[lines.len().saturating_sub(MAX_CONTENT_LINES)..].join("\n");
    bounded_suffix(&bounded, MAX_CONTENT_BYTES).to_owned()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchMatch {
    pub excerpt: Vec<String>,
    pub highlights: Vec<Vec<(usize, usize)>>,
    pub body_hit: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContentDocument {
    pub text: String,
    pub normalized: String,
    pub lines: Vec<String>,
    prepared: PreparedText,
    line_starts: Vec<usize>,
}

impl ContentDocument {
    pub fn new(text: impl AsRef<str>) -> Self {
        let text = clean_body(text.as_ref());
        let prepared = PreparedText::new(&text);
        let lines = text.split('\n').map(str::to_owned).collect();
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        Self {
            text,
            normalized: prepared.normalized.clone(),
            lines,
            prepared,
            line_starts,
        }
    }

    pub fn match_query(&self, query: &str, metadata: &str) -> Option<SearchMatch> {
        find_matches(query, &Self::new(metadata), self)
    }

    pub fn match_document(&self, query: &str, metadata: &ContentDocument) -> Option<SearchMatch> {
        find_matches(query, metadata, self)
    }
}

fn excerpt_line(line: &str, terms: &[String]) -> String {
    let boundaries: Vec<usize> = line
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(line.len()))
        .collect();
    let count = boundaries.len() - 1;
    if count <= MAX_SNIPPET_CHARS {
        return line.to_owned();
    }
    let hits = highlight_spans_terms(line, terms);
    let anchor = hits.last().map_or(0, |hit| {
        boundaries
            .partition_point(|index| *index <= hit.0)
            .saturating_sub(1)
    });
    let start = anchor.saturating_sub(60).min(count - MAX_SNIPPET_CHARS);
    let end = start + MAX_SNIPPET_CHARS;
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        &line[boundaries[start]..boundaries[end]],
        if end < count { "…" } else { "" }
    )
}

pub fn find_matches(
    query: &str,
    metadata: &ContentDocument,
    body: &ContentDocument,
) -> Option<SearchMatch> {
    find_matches_terms(&query_terms(query), metadata, body)
}

pub fn find_matches_terms(
    terms: &[String],
    metadata: &ContentDocument,
    body: &ContentDocument,
) -> Option<SearchMatch> {
    if terms.iter().any(|term| {
        !metadata.normalized.contains(term.as_str()) && !body.normalized.contains(term.as_str())
    }) {
        return None;
    }
    // Anchor the excerpt on the latest matching screen text, rather than the first hit.
    let latest = terms
        .iter()
        .filter_map(|term| {
            body.normalized
                .rfind(term.as_str())
                .map(|position| (position, term.len()))
        })
        .max_by_key(|(position, _)| *position);
    let Some((position, length)) = latest else {
        return Some(SearchMatch::default());
    };
    let (original, _) = body.prepared.original_range(position, position + length)?;
    let line = body
        .line_starts
        .partition_point(|offset| *offset <= original)
        .saturating_sub(1);
    let start = line.saturating_sub(1);
    let end = (start + 2).min(body.lines.len());
    let excerpt: Vec<String> = body.lines[start..end]
        .iter()
        .map(|line| excerpt_line(line, terms))
        .collect();
    let highlights = excerpt
        .iter()
        .map(|line| highlight_spans_terms(line, terms))
        .collect();
    Some(SearchMatch {
        excerpt,
        highlights,
        body_hit: true,
    })
}
