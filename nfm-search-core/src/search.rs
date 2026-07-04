use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;

use crate::store::ItemsSource;
use crate::timing;

pub const DISPLAY_LIMIT: usize = 15;
pub const RESULT_LIMIT: usize = 1_000;
const SEARCH_TIMING_SAMPLE_RATE: usize = 256;
const SLAB_CAP: usize = 2_000_000;

const SCORE_MATCH: i32 = 16;
const SCORE_GAP_START: i32 = -3;
const SCORE_GAP_EXTENSION: i32 = -1;
const BOUNDARY_BONUS: i32 = SCORE_MATCH / 2;
const NON_WORD_BONUS: i32 = SCORE_MATCH / 2;
const CAMEL_CASE_BONUS: i32 = BOUNDARY_BONUS + SCORE_GAP_EXTENSION;
const BONUS_CONSECUTIVE: i32 = -(SCORE_GAP_START + SCORE_GAP_EXTENSION);
const BONUS_FIRST_CHAR_MULTIPLIER: i32 = 2;

#[derive(Clone, Debug, Default)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub matched: usize,
    pub total: usize,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SearchResult {
    pub node_index: usize,
    pub score: u32,
    pub path: String,
    pub positions: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Candidate {
    node_index: usize,
    length: usize,
    score: u32,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.length.cmp(&self.length))
            .then_with(|| other.node_index.cmp(&self.node_index))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorstFirst(Candidate);

impl Ord for WorstFirst {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for WorstFirst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub fn search<S, F>(snapshot: Arc<S>, query: &str, is_cancelled: F) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    let end = snapshot.len();
    search_range(snapshot, query, 0..end, is_cancelled)
}

pub fn search_range<S, F>(
    snapshot: Arc<S>,
    query: &str,
    range: Range<usize>,
    is_cancelled: F,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    let total_start = Instant::now();
    let total = snapshot.len();
    let start_index = range.start.min(total);
    let end_index = range.end.min(total);
    let searched = end_index.saturating_sub(start_index);
    if is_cancelled() {
        return None;
    }

    if snapshot.is_empty() || searched == 0 {
        let total_us = timing::elapsed_us(total_start);
        timing::write(format!(
            "search_detail total_us={total_us} parse_us=0 match_us=0 sort_us=0 append_us=0 shown=0 matched=0 total={total}",
        ));
        return Some(SearchOutput {
            results: Vec::new(),
            matched: 0,
            total,
        });
    }

    if query.is_empty() {
        let append_start = Instant::now();
        let output = SearchOutput {
            results: materialize_unfiltered(snapshot, start_index..end_index, RESULT_LIMIT),
            matched: searched,
            total,
        };
        let append_us = timing::elapsed_us(append_start);
        let total_us = timing::elapsed_us(total_start);
        timing::write(format!(
            "search_detail mode=unfiltered total_us={total_us} parse_us=0 match_us=0 sort_us=0 append_us={append_us} shown={} matched={} total={}",
            output.results.len(),
            output.matched,
            output.total
        ));
        return Some(output);
    }

    let parse_start = Instant::now();
    let Some(pattern) = AsciiPattern::parse(query) else {
        let total_us = timing::elapsed_us(total_start);
        timing::write(format!(
            "search_detail mode=fuzzy total_us={total_us} parse_us=0 match_us=0 sort_us=0 append_us=0 utf8_path_estimate_us=0 ascii_score_estimate_us=0 char_fallback_estimate_us=0 heap_estimate_us=0 timing_sample_rate={SEARCH_TIMING_SAMPLE_RATE} timing_samples=0 utf8_count=0 fallback_count={searched} shown=0 matched=0 total={total}",
        ));
        return Some(SearchOutput {
            results: Vec::new(),
            matched: 0,
            total,
        });
    };
    let parse_us = timing::elapsed_us(parse_start);

    let match_start = Instant::now();
    let (
        candidates,
        matched,
        _,
        _,
        _,
        _,
        utf8_count,
        timing_samples,
        path_us,
        score_us,
        heap_us,
        cancelled,
    ) = (start_index..end_index)
        .into_par_iter()
        .fold(
            || {
                (
                    BinaryHeap::new(),
                    0usize,
                    MatchScratch::default(),
                    Vec::with_capacity(512),
                    Box::new([0u8; 4096]),
                    Box::new([0; 256]),
                    0usize,
                    0usize,
                    0u128,
                    0u128,
                    0u128,
                    false,
                )
            },
            |(
                mut heap,
                mut matched,
                mut scratch,
                mut path_buffer,
                mut stack_path_buffer,
                stack_segments,
                mut utf8_count,
                mut timing_samples,
                mut path_us,
                mut score_us,
                mut heap_us,
                mut cancelled,
            ),
             node_index| {
                if cancelled {
                    return (
                        heap,
                        matched,
                        scratch,
                        path_buffer,
                        stack_path_buffer,
                        stack_segments,
                        utf8_count,
                        timing_samples,
                        path_us,
                        score_us,
                        heap_us,
                        cancelled,
                    );
                }

                if (node_index & 0x3ff) == 0 && is_cancelled() {
                    cancelled = true;
                    return (
                        heap,
                        matched,
                        scratch,
                        path_buffer,
                        stack_path_buffer,
                        stack_segments,
                        utf8_count,
                        timing_samples,
                        path_us,
                        score_us,
                        heap_us,
                        cancelled,
                    );
                }

                let time_sample = (node_index & (SEARCH_TIMING_SAMPLE_RATE - 1)) == 0;
                let path_start = time_sample.then(Instant::now);
                let path_bytes =
                    snapshot.get_string(node_index, &mut stack_path_buffer[..], &mut path_buffer);
                if let Some(path_start) = path_start {
                    path_us += timing::elapsed_us(path_start);
                    timing_samples += 1;
                }

                utf8_count += 1;
                let score_start = time_sample.then(Instant::now);
                let score = if path_bytes.is_ascii() {
                    pattern.score(path_bytes, &mut scratch)
                } else {
                    None
                };
                if let Some(score_start) = score_start {
                    score_us += timing::elapsed_us(score_start);
                }

                if let Some(score) = score {
                    matched += 1;
                    let heap_start = time_sample.then(Instant::now);
                    push_bounded(
                        &mut heap,
                        Candidate {
                            node_index,
                            length: path_bytes.len(),
                            score,
                        },
                        RESULT_LIMIT,
                    );
                    if let Some(heap_start) = heap_start {
                        heap_us += timing::elapsed_us(heap_start);
                    }
                }

                (
                    heap,
                    matched,
                    scratch,
                    path_buffer,
                    stack_path_buffer,
                    stack_segments,
                    utf8_count,
                    timing_samples,
                    path_us,
                    score_us,
                    heap_us,
                    cancelled,
                )
            },
        )
        .reduce(
            || {
                (
                    BinaryHeap::new(),
                    0usize,
                    MatchScratch::default(),
                    Vec::new(),
                    Box::new([0u8; 4096]),
                    Box::new([0; 256]),
                    0usize,
                    0usize,
                    0u128,
                    0u128,
                    0u128,
                    false,
                )
            },
            |(
                mut left,
                left_matched,
                left_scratch,
                left_path_buffer,
                left_stack_path_buffer,
                left_stack_segments,
                left_utf8_count,
                left_timing_samples,
                left_path_us,
                left_score_us,
                left_heap_us,
                left_cancelled,
            ),
             (
                right,
                right_matched,
                _,
                _,
                _,
                _,
                right_utf8_count,
                right_timing_samples,
                right_path_us,
                right_score_us,
                right_heap_us,
                right_cancelled,
            )| {
                for candidate in right.into_iter().map(|wrapped| wrapped.0) {
                    push_bounded(&mut left, candidate, RESULT_LIMIT);
                }
                (
                    left,
                    left_matched + right_matched,
                    left_scratch,
                    left_path_buffer,
                    left_stack_path_buffer,
                    left_stack_segments,
                    left_utf8_count + right_utf8_count,
                    left_timing_samples + right_timing_samples,
                    left_path_us + right_path_us,
                    left_score_us + right_score_us,
                    left_heap_us + right_heap_us,
                    left_cancelled || right_cancelled,
                )
            },
        );
    let match_us = timing::elapsed_us(match_start);
    if cancelled || is_cancelled() {
        timing::write(format!(
            "search_cancelled mode=fuzzy match_us={match_us} matched={matched} total={total}"
        ));
        return None;
    }

    let sort_start = Instant::now();
    let mut candidates: Vec<_> = candidates.into_iter().map(|wrapped| wrapped.0).collect();
    candidates.sort_by(|left, right| right.cmp(left));
    candidates.truncate(RESULT_LIMIT);
    let sort_us = timing::elapsed_us(sort_start);

    let append_start = Instant::now();
    let mut path_buffer = Vec::with_capacity(512);
    let mut position_scratch = MatchScratch::default();
    let results: Vec<SearchResult> = candidates
        .into_iter()
        .map(|candidate| {
            let path = snapshot.get_string_lossy(candidate.node_index, &mut path_buffer);
            let positions = pattern.positions(path.as_bytes(), &mut position_scratch);
            SearchResult {
                node_index: candidate.node_index,
                score: candidate.score,
                path,
                positions,
            }
        })
        .collect();
    let append_us = timing::elapsed_us(append_start);
    let total_us = timing::elapsed_us(total_start);

    let sampled_scale = if timing_samples > 0 {
        SEARCH_TIMING_SAMPLE_RATE as u128
    } else {
        0
    };
    let utf8_path_estimate_us = path_us * sampled_scale;
    let ascii_score_estimate_us = score_us * sampled_scale;
    let heap_estimate_us = heap_us * sampled_scale;

    timing::write(format!(
        "search_detail mode=fuzzy total_us={total_us} parse_us={parse_us} match_us={match_us} sort_us={sort_us} append_us={append_us} utf8_path_estimate_us={utf8_path_estimate_us} ascii_score_estimate_us={ascii_score_estimate_us} char_fallback_estimate_us=0 heap_estimate_us={heap_estimate_us} timing_sample_rate={SEARCH_TIMING_SAMPLE_RATE} timing_samples={timing_samples} utf8_count={utf8_count} fallback_count=0 shown={} matched={matched} total={total}",
        results.len()
    ));

    Some(SearchOutput {
        results,
        matched,
        total,
    })
}

fn materialize_unfiltered<S>(
    snapshot: Arc<S>,
    range: Range<usize>,
    limit: usize,
) -> Vec<SearchResult>
where
    S: ItemsSource + Send + Sync,
{
    let mut path_buffer = Vec::with_capacity(512);
    range
        .take(limit)
        .map(|node_index| SearchResult {
            node_index,
            score: 0,
            path: snapshot.get_string_lossy(node_index, &mut path_buffer),
            positions: Vec::new(),
        })
        .collect()
}

fn push_bounded(heap: &mut BinaryHeap<WorstFirst>, candidate: Candidate, limit: usize) {
    if heap.len() < limit {
        heap.push(WorstFirst(candidate));
        return;
    }

    let Some(worst) = heap.peek() else {
        heap.push(WorstFirst(candidate));
        return;
    };

    if candidate > worst.0 {
        heap.pop();
        heap.push(WorstFirst(candidate));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatchKind {
    Fuzzy,
    Exact,
    Prefix,
    Suffix,
}

#[derive(Clone, Debug)]
struct Term {
    kind: MatchKind,
    inv: bool,
    case_sensitive: bool,
    text: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct TermSet {
    terms: Vec<Term>,
}

#[derive(Clone, Debug)]
struct AsciiPattern {
    term_sets: Vec<TermSet>,
    only_inv: bool,
}

impl AsciiPattern {
    fn parse(query: &str) -> Option<Self> {
        if !query.is_ascii() {
            return None;
        }

        let pattern = trim_suffix_spaces(query.trim_start());
        if pattern.is_empty() {
            return Some(Self {
                term_sets: Vec::new(),
                only_inv: false,
            });
        }

        let pattern_copy = pattern.replace("\\ ", "\t");
        let mut term_sets = Vec::new();
        let mut set = TermSet::default();
        let mut switch_set = false;
        let mut after_bar = false;

        for token in pattern_copy.split(' ') {
            let mut text = token.replace('\t', " ");
            if !after_bar && !set.terms.is_empty() && text == "|" {
                switch_set = false;
                after_bar = true;
                continue;
            }
            after_bar = false;

            if text.is_empty() {
                continue;
            }

            let lower_text = text.to_ascii_lowercase();
            let case_sensitive = text != lower_text;
            if !case_sensitive {
                text = lower_text;
            }

            let mut kind = MatchKind::Fuzzy;
            let mut inv = false;

            if let Some(rest) = text.strip_prefix('!') {
                inv = true;
                kind = MatchKind::Exact;
                text = rest.to_string();
            }

            if text.ends_with('$') && text != "$" {
                kind = MatchKind::Suffix;
                text.pop();
            }

            if let Some(rest) = text.strip_prefix('\'') {
                if !inv {
                    kind = MatchKind::Exact;
                } else {
                    kind = MatchKind::Fuzzy;
                }
                text = rest.to_string();
            } else if let Some(rest) = text.strip_prefix('^') {
                kind = MatchKind::Prefix;
                text = rest.to_string();
            }

            if text.is_empty() {
                continue;
            }

            if switch_set {
                term_sets.push(set);
                set = TermSet::default();
            }

            set.terms.push(Term {
                kind,
                inv,
                case_sensitive,
                text: text.into_bytes(),
            });
            switch_set = true;
        }

        if !set.terms.is_empty() {
            term_sets.push(set);
        }

        let only_inv = !term_sets.is_empty()
            && term_sets
                .iter()
                .all(|set| set.terms.len() == 1 && set.terms[0].inv);

        Some(Self {
            term_sets,
            only_inv,
        })
    }

    fn score(&self, text: &[u8], scratch: &mut MatchScratch) -> Option<u32> {
        if self.term_sets.is_empty() {
            return Some(1);
        }

        if self.term_sets.len() == 1 && self.term_sets[0].terms.len() == 1 {
            let term = &self.term_sets[0].terms[0];
            let res = match_ascii(term, text, scratch);
            if term.inv {
                return if res.start >= 0 { None } else { Some(1) };
            }

            return if res.start >= 0 && res.score > 0 {
                Some(res.score as u32)
            } else {
                None
            };
        }

        if self.only_inv {
            let mut final_score = 0;
            for term_set in &self.term_sets {
                let term = &term_set.terms[0];
                let res = match_ascii(term, text, scratch);
                final_score += res.score;
            }
            return if final_score > 0 { None } else { Some(1) };
        }

        let mut total_score = 0;
        for term_set in &self.term_sets {
            let mut current_score = 0;
            let mut matched = false;
            for term in &term_set.terms {
                let res = match_ascii(term, text, scratch);
                if res.start >= 0 {
                    if term.inv {
                        continue;
                    }

                    current_score = res.score;
                    matched = true;
                    break;
                }

                if term.inv {
                    current_score = 0;
                    matched = true;
                }
            }

            if matched {
                total_score += current_score;
            } else {
                return None;
            }
        }

        if total_score > 0 {
            Some(total_score as u32)
        } else {
            None
        }
    }

    fn positions(&self, text: &[u8], scratch: &mut MatchScratch) -> Vec<usize> {
        if !text.is_ascii() {
            return Vec::new();
        }

        let mut positions = Vec::new();
        for term_set in &self.term_sets {
            for term in &term_set.terms {
                if term.inv {
                    continue;
                }

                if append_term_positions(term, text, scratch, &mut positions) {
                    break;
                }
            }
        }

        positions.sort_unstable();
        positions.dedup();
        positions
    }
}

#[derive(Default)]
struct MatchScratch {
    text_copy: Vec<u8>,
    initial_scores: Vec<i32>,
    consecutive_scores: Vec<i32>,
    bonuses: Vec<i32>,
    first_occurrence: Vec<usize>,
    score_matrix: Vec<i32>,
    consecutive_matrix: Vec<i32>,
}

#[derive(Clone, Copy)]
struct FzfResult {
    start: isize,
    end: isize,
    score: i32,
}

fn match_ascii(term: &Term, text: &[u8], scratch: &mut MatchScratch) -> FzfResult {
    match term.kind {
        MatchKind::Fuzzy => {
            fzf_fuzzy_match_v2_ascii(term.case_sensitive, text, &term.text, scratch, None)
        }
        MatchKind::Exact => fzf_exact_match_naive_ascii(term.case_sensitive, text, &term.text),
        MatchKind::Prefix => fzf_prefix_match_ascii(term.case_sensitive, text, &term.text),
        MatchKind::Suffix => fzf_suffix_match_ascii(term.case_sensitive, text, &term.text),
    }
}

fn append_term_positions(
    term: &Term,
    text: &[u8],
    scratch: &mut MatchScratch,
    positions: &mut Vec<usize>,
) -> bool {
    let res = match term.kind {
        MatchKind::Fuzzy => {
            return fzf_fuzzy_match_v2_ascii(
                term.case_sensitive,
                text,
                &term.text,
                scratch,
                Some(positions),
            )
            .start
                >= 0;
        }
        _ => match_ascii(term, text, scratch),
    };

    if res.start < 0 || res.end < res.start {
        return false;
    }

    append_positions_ascii(
        term.case_sensitive,
        text,
        &term.text,
        res.start as usize,
        res.end as usize,
        positions,
    );
    true
}

fn fzf_prefix_match_ascii(case_sensitive: bool, text: &[u8], pattern: &[u8]) -> FzfResult {
    if pattern.is_empty() {
        return no_match();
    }

    let trimmed_len = if !is_ascii_whitespace(pattern[0]) {
        leading_whitespace_ascii(text)
    } else {
        0
    };

    if text.len().saturating_sub(trimmed_len) < pattern.len() {
        return no_match();
    }

    for index in 0..pattern.len() {
        if eq_byte(text[trimmed_len + index], pattern[index], case_sensitive) {
            continue;
        }
        return no_match();
    }

    let start = trimmed_len;
    let end = trimmed_len + pattern.len();
    FzfResult {
        start: start as isize,
        end: end as isize,
        score: calculate_score_ascii(case_sensitive, text, pattern, start, end),
    }
}

fn fzf_suffix_match_ascii(case_sensitive: bool, text: &[u8], pattern: &[u8]) -> FzfResult {
    if pattern.is_empty() {
        return no_match();
    }

    let trimmed_len = if !is_ascii_whitespace(*pattern.last().expect("non-empty")) {
        trim_trailing_spaces_ascii(text)
    } else {
        text.len()
    };

    if trimmed_len < pattern.len() {
        return no_match();
    }

    let start = trimmed_len - pattern.len();
    for index in 0..pattern.len() {
        if eq_byte(text[start + index], pattern[index], case_sensitive) {
            continue;
        }
        return no_match();
    }

    FzfResult {
        start: start as isize,
        end: trimmed_len as isize,
        score: calculate_score_ascii(
            case_sensitive,
            &text[..trimmed_len],
            pattern,
            start,
            trimmed_len,
        ),
    }
}

fn fzf_exact_match_naive_ascii(case_sensitive: bool, text: &[u8], pattern: &[u8]) -> FzfResult {
    if pattern.is_empty() {
        return FzfResult {
            start: 0,
            end: 0,
            score: 0,
        };
    }

    if text.len() < pattern.len() || fuzzy_index_of_ascii(text, pattern, case_sensitive).is_none() {
        return no_match();
    }

    let mut pattern_index = 0usize;
    let mut best_pos: Option<usize> = None;
    let mut bonus = 0;
    let mut best_bonus = -1;
    let mut index = 0usize;
    while index < text.len() {
        if eq_byte(text[index], pattern[pattern_index], case_sensitive) {
            if pattern_index == 0 {
                bonus = bonus_at_ascii(text, index);
            }

            pattern_index += 1;
            if pattern_index == pattern.len() {
                if bonus > best_bonus {
                    best_pos = Some(index);
                    best_bonus = bonus;
                }

                if bonus == BOUNDARY_BONUS {
                    break;
                }

                index = index.saturating_sub(pattern_index - 1);
                pattern_index = 0;
                bonus = 0;
            }
        } else {
            index = index.saturating_sub(pattern_index);
            pattern_index = 0;
            bonus = 0;
        }
        index += 1;
    }

    let Some(best_pos) = best_pos else {
        return no_match();
    };
    let start = best_pos + 1 - pattern.len();
    let end = best_pos + 1;
    FzfResult {
        start: start as isize,
        end: end as isize,
        score: calculate_score_ascii(case_sensitive, text, pattern, start, end),
    }
}

fn fzf_fuzzy_match_v2_ascii(
    case_sensitive: bool,
    text: &[u8],
    pattern: &[u8],
    scratch: &mut MatchScratch,
    mut positions: Option<&mut Vec<usize>>,
) -> FzfResult {
    let pattern_size = pattern.len();
    let text_size = text.len();

    if pattern_size == 0 {
        return FzfResult {
            start: 0,
            end: 0,
            score: 0,
        };
    }

    if pattern_size.saturating_mul(text_size) >= SLAB_CAP {
        let res = fuzzy_match_v1_ascii(case_sensitive, text, pattern);
        if let Some(positions) = positions.as_deref_mut()
            && res.start >= 0
        {
            append_positions_ascii(
                case_sensitive,
                text,
                pattern,
                res.start as usize,
                res.end as usize,
                positions,
            );
        }
        return res;
    }

    let Some(first_index_of) = fuzzy_index_of_ascii(text, pattern, case_sensitive) else {
        return no_match();
    };

    scratch.text_copy.clear();
    scratch.text_copy.extend_from_slice(text);
    scratch.initial_scores.clear();
    scratch.initial_scores.resize(text_size, 0);
    scratch.consecutive_scores.clear();
    scratch.consecutive_scores.resize(text_size, 0);
    scratch.bonuses.clear();
    scratch.bonuses.resize(text_size, 0);
    scratch.first_occurrence.clear();
    scratch.first_occurrence.resize(pattern_size, 0);

    let mut max_score = 0;
    let mut max_score_pos = 0usize;
    let mut pattern_index = 0usize;
    let mut last_index = 0usize;
    let first_pattern_char = normalize_byte(pattern[0], case_sensitive);
    let mut current_pattern_char = first_pattern_char;
    let mut previous_initial_score = 0;
    let mut previous_class = CharClass::NonWord;
    let mut in_gap = false;

    for i in first_index_of..text_size {
        let mut current_char = scratch.text_copy[i];
        let current_class = class_of_ascii(current_char);
        if !case_sensitive && current_class == CharClass::CharUpper {
            current_char = to_lower_ascii(current_char);
        }

        scratch.text_copy[i] = current_char;
        let bonus = calculate_bonus(previous_class, current_class);
        scratch.bonuses[i] = bonus;
        previous_class = current_class;

        if current_char == current_pattern_char {
            if pattern_index < pattern_size {
                scratch.first_occurrence[pattern_index] = i;
                pattern_index += 1;
                current_pattern_char =
                    normalize_byte(pattern[pattern_index.min(pattern_size - 1)], case_sensitive);
            }

            last_index = i;
        }

        if current_char == first_pattern_char {
            let score = SCORE_MATCH + bonus * BONUS_FIRST_CHAR_MULTIPLIER;
            scratch.initial_scores[i] = score;
            scratch.consecutive_scores[i] = 1;
            if pattern_size == 1 && score > max_score {
                max_score = score;
                max_score_pos = i;
                if bonus == BOUNDARY_BONUS {
                    break;
                }
            }

            in_gap = false;
        } else {
            scratch.initial_scores[i] = if in_gap {
                (previous_initial_score + SCORE_GAP_EXTENSION).max(0)
            } else {
                (previous_initial_score + SCORE_GAP_START).max(0)
            };
            scratch.consecutive_scores[i] = 0;
            in_gap = true;
        }

        previous_initial_score = scratch.initial_scores[i];
    }

    if pattern_index != pattern_size {
        return no_match();
    }

    if pattern_size == 1 {
        if let Some(positions) = positions.as_deref_mut() {
            positions.push(max_score_pos);
        }
        return FzfResult {
            start: max_score_pos as isize,
            end: max_score_pos as isize + 1,
            score: max_score,
        };
    }

    let first_occurrence_of_first_char = scratch.first_occurrence[0];
    let width = last_index - first_occurrence_of_first_char + 1;
    scratch.score_matrix.clear();
    scratch.score_matrix.resize(width * pattern_size, 0);
    scratch.consecutive_matrix.clear();
    scratch.consecutive_matrix.resize(width * pattern_size, 0);

    for index in 0..width {
        let source = first_occurrence_of_first_char + index;
        scratch.score_matrix[index] = scratch.initial_scores[source];
        scratch.consecutive_matrix[index] = scratch.consecutive_scores[source];
    }

    for off in 0..(pattern_size - 1) {
        let pattern_char_offset = scratch.first_occurrence[off + 1];
        let current_pattern_char = normalize_byte(pattern[off + 1], case_sensitive);
        let pattern_index = off + 1;
        let row = pattern_index * width;
        let mut in_gap = false;

        for column in pattern_char_offset..=last_index {
            let local = column - first_occurrence_of_first_char;
            let matrix_index = row + local;
            let left_score = if local == 0 {
                0
            } else {
                scratch.score_matrix[matrix_index - 1]
            };
            let score = if in_gap {
                left_score + SCORE_GAP_EXTENSION
            } else {
                left_score + SCORE_GAP_START
            };

            let mut diagonal_score = 0;
            let mut consecutive = 0;
            if scratch.text_copy[column] == current_pattern_char && local > 0 {
                let diagonal_index = matrix_index - 1 - width;
                diagonal_score = scratch.score_matrix[diagonal_index] + SCORE_MATCH;
                let mut bonus = scratch.bonuses[column];
                consecutive = scratch.consecutive_matrix[diagonal_index] + 1;
                if bonus == BOUNDARY_BONUS {
                    consecutive = 1;
                } else if consecutive > 1 {
                    let consecutive_start = column - consecutive as usize + 1;
                    bonus = bonus
                        .max(BONUS_CONSECUTIVE)
                        .max(scratch.bonuses[consecutive_start]);
                }

                if diagonal_score + bonus < score {
                    diagonal_score += scratch.bonuses[column];
                    consecutive = 0;
                } else {
                    diagonal_score += bonus;
                }
            }

            scratch.consecutive_matrix[matrix_index] = consecutive;
            in_gap = diagonal_score < score;
            let score2 = 0.max(diagonal_score.max(score));
            if pattern_index == pattern_size - 1 && score2 > max_score {
                max_score = score2;
                max_score_pos = column;
            }

            scratch.score_matrix[matrix_index] = score2;
        }
    }

    let mut start = max_score_pos;
    if let Some(positions) = positions.as_deref_mut() {
        let mut pattern_index = pattern_size - 1;
        let mut prefer_match = true;

        loop {
            let row = pattern_index * width;
            let column = start - first_occurrence_of_first_char;
            let current_score = scratch.score_matrix[row + column];

            let mut diagonal_score = 0;
            if pattern_index > 0 && start >= scratch.first_occurrence[pattern_index] {
                diagonal_score = scratch.score_matrix[row - width + column - 1];
            }

            let mut left_score = 0;
            if start > scratch.first_occurrence[pattern_index] {
                left_score = scratch.score_matrix[row + column - 1];
            }

            if current_score > diagonal_score
                && (current_score > left_score || (current_score == left_score && prefer_match))
            {
                positions.push(start);
                if pattern_index == 0 {
                    break;
                }

                pattern_index -= 1;
            }

            if start == first_occurrence_of_first_char {
                break;
            }

            prefer_match = scratch.consecutive_matrix[row + column] > 1
                || (row + width + column + 1 < scratch.consecutive_matrix.len()
                    && scratch.consecutive_matrix[row + width + column + 1] > 0);
            start -= 1;
        }
    }

    FzfResult {
        start: start as isize,
        end: max_score_pos as isize + 1,
        score: max_score,
    }
}

fn fuzzy_match_v1_ascii(case_sensitive: bool, text: &[u8], pattern: &[u8]) -> FzfResult {
    if pattern.is_empty() {
        return FzfResult {
            start: 0,
            end: 0,
            score: 0,
        };
    }

    if fuzzy_index_of_ascii(text, pattern, case_sensitive).is_none() {
        return no_match();
    }

    let mut pattern_index = 0usize;
    let mut start_index: Option<usize> = None;
    let mut end_index: Option<usize> = None;

    for (index, &current_char) in text.iter().enumerate() {
        if eq_byte(current_char, pattern[pattern_index], case_sensitive) {
            if start_index.is_none() {
                start_index = Some(index);
            }

            pattern_index += 1;
            if pattern_index == pattern.len() {
                end_index = Some(index + 1);
                break;
            }
        }
    }

    let (Some(mut start), Some(end)) = (start_index, end_index) else {
        return no_match();
    };

    pattern_index -= 1;
    for index in (start..end).rev() {
        if eq_byte(text[index], pattern[pattern_index], case_sensitive) {
            if pattern_index == 0 {
                start = index;
                break;
            }
            pattern_index -= 1;
        }
    }

    FzfResult {
        start: start as isize,
        end: end as isize,
        score: calculate_score_ascii(case_sensitive, text, pattern, start, end),
    }
}

fn append_positions_ascii(
    case_sensitive: bool,
    text: &[u8],
    pattern: &[u8],
    start_index: usize,
    end_index: usize,
    positions: &mut Vec<usize>,
) {
    let mut pattern_index = 0usize;
    for index in start_index..end_index {
        if pattern_index >= pattern.len() {
            break;
        }

        if eq_byte(text[index], pattern[pattern_index], case_sensitive) {
            positions.push(index);
            pattern_index += 1;
        }
    }
}

fn calculate_score_ascii(
    case_sensitive: bool,
    text: &[u8],
    pattern: &[u8],
    start_index: usize,
    end_index: usize,
) -> i32 {
    let mut pattern_index = 0usize;
    let mut score = 0;
    let mut consecutive = 0;
    let mut in_gap = false;
    let mut first_bonus = 0;
    let mut prev_class = if start_index > 0 {
        class_of_ascii(text[start_index - 1])
    } else {
        CharClass::NonWord
    };

    for &raw_char in &text[start_index..end_index] {
        let current_class = class_of_ascii(raw_char);
        let current_char = normalize_byte(raw_char, case_sensitive);
        let pattern_char = normalize_byte(pattern[pattern_index], case_sensitive);

        if current_char == pattern_char {
            score += SCORE_MATCH;
            let mut bonus = calculate_bonus(prev_class, current_class);
            if consecutive == 0 {
                first_bonus = bonus;
            } else {
                if bonus == BOUNDARY_BONUS {
                    first_bonus = bonus;
                }
                bonus = bonus.max(first_bonus).max(BONUS_CONSECUTIVE);
            }

            if pattern_index == 0 {
                score += bonus * BONUS_FIRST_CHAR_MULTIPLIER;
            } else {
                score += bonus;
            }

            in_gap = false;
            consecutive += 1;
            pattern_index += 1;
        } else {
            score += if in_gap {
                SCORE_GAP_EXTENSION
            } else {
                SCORE_GAP_START
            };
            in_gap = true;
            consecutive = 0;
            first_bonus = 0;
        }

        prev_class = current_class;
    }

    score
}

fn fuzzy_index_of_ascii(input: &[u8], pattern: &[u8], case_sensitive: bool) -> Option<usize> {
    let mut index = 0usize;
    let mut first_index = 0usize;

    for (pattern_index, &byte) in pattern.iter().enumerate() {
        let found = index_of_byte_ascii(input, byte, index, case_sensitive)?;
        if pattern_index == 0 && found > 0 {
            first_index = found - 1;
        }
        index = found + 1;
    }

    Some(first_index)
}

fn index_of_byte_ascii(
    input: &[u8],
    value: u8,
    start_index: usize,
    case_sensitive: bool,
) -> Option<usize> {
    let value = normalize_byte(value, case_sensitive);
    for (index, &current) in input.iter().enumerate().skip(start_index) {
        if normalize_byte(current, case_sensitive) == value {
            return Some(index);
        }
    }
    None
}

fn bonus_at_ascii(text: &[u8], index: usize) -> i32 {
    let current = class_of_ascii(text[index]);
    let previous = if index > 0 {
        class_of_ascii(text[index - 1])
    } else {
        CharClass::NonWord
    };
    calculate_bonus(previous, current)
}

fn calculate_bonus(previous: CharClass, current: CharClass) -> i32 {
    if previous == CharClass::NonWord && current != CharClass::NonWord {
        return BOUNDARY_BONUS;
    }

    if (previous == CharClass::CharLower && current == CharClass::CharUpper)
        || (previous != CharClass::Digit && current == CharClass::Digit)
    {
        return CAMEL_CASE_BONUS;
    }

    if current == CharClass::NonWord {
        return NON_WORD_BONUS;
    }

    0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CharClass {
    NonWord,
    CharLower,
    CharUpper,
    Digit,
}

fn class_of_ascii(byte: u8) -> CharClass {
    match byte {
        b'a'..=b'z' => CharClass::CharLower,
        b'A'..=b'Z' => CharClass::CharUpper,
        b'0'..=b'9' => CharClass::Digit,
        _ => CharClass::NonWord,
    }
}

fn normalize_byte(byte: u8, case_sensitive: bool) -> u8 {
    if case_sensitive {
        byte
    } else {
        to_lower_ascii(byte)
    }
}

fn eq_byte(left: u8, right: u8, case_sensitive: bool) -> bool {
    normalize_byte(left, case_sensitive) == normalize_byte(right, case_sensitive)
}

fn to_lower_ascii(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte + 32
    } else {
        byte
    }
}

fn is_ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn leading_whitespace_ascii(text: &[u8]) -> usize {
    text.iter()
        .position(|byte| !is_ascii_whitespace(*byte))
        .unwrap_or(text.len())
}

fn trim_trailing_spaces_ascii(text: &[u8]) -> usize {
    text.iter()
        .rposition(|byte| !is_ascii_whitespace(*byte))
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn trim_suffix_spaces(value: &str) -> &str {
    value.trim_end_matches(' ')
}

fn no_match() -> FzfResult {
    FzfResult {
        start: -1,
        end: -1,
        score: 0,
    }
}
