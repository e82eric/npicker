use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;

use crate::store::{ItemsSource, SearchPlan};
use crate::timing;

pub const DISPLAY_LIMIT: usize = 15;
pub const RESULT_LIMIT: usize = 1_000;
const SEARCH_TIMING_SAMPLE_RATE: usize = 256;
const SLAB_CAP: usize = 2_000_000;
const SCORE_MATCH: i32 = 8;
const SCORE_GAP_START: i32 = -2;
const SCORE_GAP_EXTENSION: i32 = -1;
const BOUNDARY_BONUS: i32 = 4;
const NON_WORD_BONUS: i32 = 4;
const CAMEL_CASE_BONUS: i32 = 3;
const BONUS_CONSECUTIVE: i32 = 2;
const BONUS_FIRST_CHAR_MULTIPLIER: i32 = 2;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchSortMode {
    #[default]
    Score,
    SourceOrder,
}

impl SearchSortMode {
    fn timing_label(self) -> &'static str {
        match self {
            Self::Score => "fuzzy",
            Self::SourceOrder => "fuzzy_source_order",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub matched: usize,
    pub total: usize,
    pub(crate) match_bitmap: Option<MatchBitmap>,
}

/// Compact set of matching item indexes, stored as one bit per item.
/// `words` is a contiguous array of 64-bit integers: item `i` uses bit
/// `i % 64` in `words[i / 64]`. A set bit means the item matched; a clear
/// bit means it did not. For example, indexes 0..64 occupy the first word,
/// and index 64 uses the lowest bit of the second word.
///
/// `covered_len` is the number of items represented, not the match count.
/// The final word may contain unused padding bits. Items at or beyond
/// `covered_len` have not been checked by this bitmap, so iteration and
/// counting include them as candidates rather than treating them as nonmatches.
#[derive(Clone, Debug, Default)]
pub(crate) struct MatchBitmap {
    words: Vec<u64>,
    covered_len: usize,
}

impl MatchBitmap {
    fn from_indexes(covered_len: usize, indexes: &[u32]) -> Self {
        let mut words = vec![0; covered_len.div_ceil(64)];
        for &index in indexes {
            let index = index as usize;
            words[index / 64] |= 1u64 << (index % 64);
        }
        Self { words, covered_len }
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        if other.covered_len > self.covered_len {
            self.words.resize(other.words.len(), 0);
            self.covered_len = other.covered_len;
        }
        for (target, source) in self.words.iter_mut().zip(&other.words) {
            *target |= source;
        }
    }

    fn matching_indexes(&self, end: usize) -> impl ParallelIterator<Item = usize> + '_ {
        self.words
            .par_iter()
            .enumerate()
            .flat_map_iter(move |(word_index, &remaining)| SetBitIndexes {
                base: word_index * 64,
                remaining,
                end,
            })
            .chain(self.covered_len.min(end)..end)
    }

    fn count_matches(&self, end: usize) -> usize {
        let covered = end.min(self.covered_len);
        let full_words = covered / 64;
        let mut count: usize = self.words[..full_words]
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();
        let trailing = covered % 64;
        if trailing != 0 {
            count += (self.words[full_words] & ((1u64 << trailing) - 1)).count_ones() as usize;
        }
        count + end.saturating_sub(self.covered_len)
    }
}

struct SetBitIndexes {
    base: usize,
    remaining: u64,
    end: usize,
}

impl Iterator for SetBitIndexes {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining != 0 {
            let bit = self.remaining.trailing_zeros() as usize;
            self.remaining &= self.remaining - 1;
            let index = self.base + bit;
            if index < self.end {
                return Some(index);
            }
        }
        None
    }
}

#[cfg(test)]
mod match_bitmap_tests {
    use super::*;

    #[test]
    fn iterates_only_matches_and_the_uncovered_tail() {
        let bitmap = MatchBitmap::from_indexes(70, &[0, 2, 63, 64, 69]);
        let mut indexes: Vec<_> = bitmap.matching_indexes(73).collect();
        indexes.sort_unstable();
        assert_eq!(indexes, vec![0, 2, 63, 64, 69, 70, 71, 72]);
        assert_eq!(bitmap.count_matches(73), indexes.len());
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SearchResult {
    pub node_index: usize,
    pub score: u32,
    pub path: String,
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

enum CandidateCollector {
    Score(BinaryHeap<WorstFirst>),
    SourceOrder(Vec<Candidate>),
}

impl CandidateCollector {
    fn new(sort_mode: SearchSortMode) -> Self {
        match sort_mode {
            SearchSortMode::Score => Self::Score(BinaryHeap::new()),
            SearchSortMode::SourceOrder => Self::SourceOrder(Vec::new()),
        }
    }

    fn push(&mut self, candidate: Candidate) {
        match self {
            Self::Score(heap) => push_bounded(heap, candidate, RESULT_LIMIT),
            Self::SourceOrder(candidates) => {
                if candidates.len() < RESULT_LIMIT {
                    candidates.push(candidate);
                }
            }
        }
    }

    fn merge(&mut self, other: Self) {
        match (self, other) {
            (Self::Score(left), Self::Score(right)) => {
                for candidate in right.into_iter().map(|wrapped| wrapped.0) {
                    push_bounded(left, candidate, RESULT_LIMIT);
                }
            }
            (Self::SourceOrder(left), Self::SourceOrder(mut right)) => {
                left.append(&mut right);
                left.sort_unstable_by_key(|candidate| candidate.node_index);
                left.truncate(RESULT_LIMIT);
            }
            _ => unreachable!("candidate collectors must have the same sort mode"),
        }
    }

    fn into_sorted_candidates(self) -> Vec<Candidate> {
        match self {
            Self::Score(heap) => {
                let mut candidates: Vec<_> = heap.into_iter().map(|wrapped| wrapped.0).collect();
                candidates.sort_unstable_by(|left, right| right.cmp(left));
                candidates
            }
            Self::SourceOrder(mut candidates) => {
                candidates.sort_unstable_by_key(|candidate| candidate.node_index);
                candidates
            }
        }
    }
}

struct SearchAccumulator {
    candidates: CandidateCollector,
    matched: usize,
    path_buffer: Vec<u8>,
    stack_path_buffer: Box<[u8; 4096]>,
    utf8_count: usize,
    timing_samples: usize,
    path_us: u128,
    score_us: u128,
    retention_us: u128,
    matched_indexes: Option<Vec<u32>>,
    processed_since_fold_start: usize,
    cancelled: bool,
}

impl SearchAccumulator {
    fn new(sort_mode: SearchSortMode, collect_matches: bool) -> Self {
        Self {
            candidates: CandidateCollector::new(sort_mode),
            matched: 0,
            path_buffer: Vec::with_capacity(512),
            stack_path_buffer: Box::new([0u8; 4096]),
            utf8_count: 0,
            timing_samples: 0,
            path_us: 0,
            score_us: 0,
            retention_us: 0,
            matched_indexes: collect_matches.then(Vec::new),
            processed_since_fold_start: 0,
            cancelled: false,
        }
    }

    fn merge(&mut self, other: Self) {
        self.candidates.merge(other.candidates);
        self.matched += other.matched;
        self.utf8_count += other.utf8_count;
        self.timing_samples += other.timing_samples;
        self.path_us += other.path_us;
        self.score_us += other.score_us;
        self.retention_us += other.retention_us;
        if let (Some(left), Some(mut right)) = (&mut self.matched_indexes, other.matched_indexes) {
            left.append(&mut right);
        }
        self.cancelled |= other.cancelled;
    }
}

fn accumulate_fzf<S, F, I>(
    snapshot: Arc<S>,
    indexes: I,
    pattern: &fzf::SearchPattern,
    sort_mode: SearchSortMode,
    is_cancelled: &F,
    collect_matches: bool,
    plan: &(dyn SearchPlan + '_),
    filters_items: bool,
    timing_enabled: bool,
) -> SearchAccumulator
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
    I: ParallelIterator<Item = usize>,
{
    indexes
        .fold(
            || {
                (
                    SearchAccumulator::new(sort_mode, collect_matches),
                    fzf::MatchScratch::default(),
                )
            },
            |(mut state, mut scratch), node_index| {
                if state.cancelled {
                    return (state, scratch);
                }

                if (state.processed_since_fold_start & 0x3ff) == 0 && is_cancelled() {
                    state.cancelled = true;
                    return (state, scratch);
                }
                state.processed_since_fold_start += 1;

                if filters_items && !plan.includes(node_index) {
                    return (state, scratch);
                }

                let time_sample =
                    timing_enabled && (node_index & (SEARCH_TIMING_SAMPLE_RATE - 1)) == 0;
                let path_start = time_sample.then(Instant::now);
                let path_bytes = snapshot.get_string(
                    node_index,
                    &mut state.stack_path_buffer[..],
                    &mut state.path_buffer,
                );
                if let Some(path_start) = path_start {
                    state.path_us += timing::elapsed_us(path_start);
                    state.timing_samples += 1;
                }

                let is_ascii = path_bytes.is_ascii();
                if timing_enabled {
                    state.utf8_count += usize::from(!is_ascii);
                }
                let score_start = time_sample.then(Instant::now);
                let score = pattern.score(path_bytes, is_ascii, &mut scratch);
                if let Some(score_start) = score_start {
                    state.score_us += timing::elapsed_us(score_start);
                }

                if let Some(score) = score {
                    state.matched += 1;
                    if let Some(indexes) = &mut state.matched_indexes {
                        indexes.push(node_index as u32);
                    }
                    let retention_start = time_sample.then(Instant::now);
                    state.candidates.push(Candidate {
                        node_index,
                        length: path_bytes.len(),
                        score,
                    });
                    if let Some(retention_start) = retention_start {
                        state.retention_us += timing::elapsed_us(retention_start);
                    }
                }

                (state, scratch)
            },
        )
        .map(|(state, _)| state)
        .reduce(
            || SearchAccumulator::new(sort_mode, collect_matches),
            |mut left, right| {
                left.merge(right);
                left
            },
        )
}

pub fn search<S, F>(snapshot: Arc<S>, query: &str, is_cancelled: F) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    search_with_sort(snapshot, query, SearchSortMode::Score, is_cancelled)
}

pub fn search_with_sort<S, F>(
    snapshot: Arc<S>,
    query: &str,
    sort_mode: SearchSortMode,
    is_cancelled: F,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    let end = snapshot.len();
    search_range_with_options(
        snapshot,
        query,
        0..end,
        sort_mode,
        is_cancelled,
        None,
        false,
    )
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
    search_range_with_options(
        snapshot,
        query,
        range,
        SearchSortMode::Score,
        is_cancelled,
        None,
        false,
    )
}

pub fn search_range_with_sort<S, F>(
    snapshot: Arc<S>,
    query: &str,
    range: Range<usize>,
    sort_mode: SearchSortMode,
    is_cancelled: F,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    search_range_with_options(snapshot, query, range, sort_mode, is_cancelled, None, false)
}

pub(crate) fn search_range_with_match_collection<S, F>(
    snapshot: Arc<S>,
    query: &str,
    range: Range<usize>,
    sort_mode: SearchSortMode,
    is_cancelled: F,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    search_range_with_options(snapshot, query, range, sort_mode, is_cancelled, None, true)
}

pub(crate) fn search_with_match_cache<S, F>(
    snapshot: Arc<S>,
    query: &str,
    sort_mode: SearchSortMode,
    is_cancelled: F,
    filter: Option<&MatchBitmap>,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    let end = snapshot.len();
    search_range_with_options(
        snapshot,
        query,
        0..end,
        sort_mode,
        is_cancelled,
        filter,
        true,
    )
}

fn search_range_with_options<S, F>(
    snapshot: Arc<S>,
    query: &str,
    range: Range<usize>,
    sort_mode: SearchSortMode,
    is_cancelled: F,
    filter: Option<&MatchBitmap>,
    collect_matches: bool,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    let timing_enabled = timing::is_enabled();
    let total_start = timing_enabled.then(Instant::now);
    let plan = snapshot.create_search_plan(query);
    let fuzzy_query = plan.fuzzy_query();
    let filters_items = plan.filters_items();
    let total = snapshot.len();
    let start_index = range.start.min(total);
    let end_index = range.end.min(total);
    let searched = end_index.saturating_sub(start_index);
    if is_cancelled() {
        return None;
    }

    if snapshot.is_empty() || searched == 0 {
        let total_us = total_start.map_or(0, timing::elapsed_us);
        timing::write_lazy(|| {
            format!(
            "search_detail total_us={total_us} parse_us=0 match_us=0 sort_us=0 append_us=0 shown=0 matched=0 total={total}",
        )
        });
        return Some(SearchOutput {
            results: Vec::new(),
            matched: 0,
            total,
            match_bitmap: None,
        });
    }

    if fuzzy_query.is_empty() {
        let append_start = timing_enabled.then(Instant::now);
        let mut included = Vec::with_capacity(RESULT_LIMIT.min(searched));
        let mut matched = 0;
        for index in start_index..end_index {
            if !filters_items || plan.includes(index) {
                matched += 1;
                if included.len() < RESULT_LIMIT {
                    included.push(index);
                }
            }
        }
        let mut output = SearchOutput {
            results: materialize_indexes(
                Arc::clone(&snapshot),
                included.into_iter(),
                RESULT_LIMIT,
                plan.as_ref(),
            ),
            matched,
            total,
            match_bitmap: None,
        };
        apply_custom_sort(&mut output.results, plan.as_ref());
        let append_us = append_start.map_or(0, timing::elapsed_us);
        let total_us = total_start.map_or(0, timing::elapsed_us);
        timing::write_lazy(|| {
            format!(
            "search_detail mode=unfiltered total_us={total_us} parse_us=0 match_us=0 sort_us=0 append_us={append_us} shown={} matched={} total={}",
            output.results.len(),
            output.matched,
            output.total
        )
        });
        return Some(output);
    }

    let parse_start = timing_enabled.then(Instant::now);
    let pattern = fzf::SearchPattern::parse(fuzzy_query);
    let parse_us = parse_start.map_or(0, timing::elapsed_us);

    let match_start = timing_enabled.then(Instant::now);
    debug_assert!(filter.is_none() || start_index == 0);
    let accumulator = if let Some(filter) = filter {
        accumulate_fzf(
            Arc::clone(&snapshot),
            filter.matching_indexes(end_index),
            &pattern,
            sort_mode,
            &is_cancelled,
            collect_matches,
            plan.as_ref(),
            filters_items,
            timing_enabled,
        )
    } else {
        accumulate_fzf(
            Arc::clone(&snapshot),
            (start_index..end_index).into_par_iter(),
            &pattern,
            sort_mode,
            &is_cancelled,
            collect_matches,
            plan.as_ref(),
            filters_items,
            timing_enabled,
        )
    };
    let match_us = match_start.map_or(0, timing::elapsed_us);
    let SearchAccumulator {
        candidates,
        matched,
        utf8_count,
        timing_samples,
        path_us,
        score_us,
        retention_us,
        cancelled,
        matched_indexes,
        ..
    } = accumulator;
    if cancelled || is_cancelled() {
        timing::write_lazy(|| {
            format!(
                "search_cancelled mode={} match_us={match_us} matched={matched} total={total}",
                sort_mode.timing_label()
            )
        });
        return None;
    }

    let sort_start = timing_enabled.then(Instant::now);
    let candidates = candidates.into_sorted_candidates();
    let sort_us = sort_start.map_or(0, timing::elapsed_us);

    let append_start = timing_enabled.then(Instant::now);
    let mut path_buffer = Vec::with_capacity(512);
    let mut results: Vec<SearchResult> = candidates
        .into_iter()
        .map(|candidate| {
            let path = plan.display_text(candidate.node_index).unwrap_or_else(|| {
                snapshot.get_string_lossy(candidate.node_index, &mut path_buffer)
            });
            SearchResult {
                node_index: candidate.node_index,
                score: candidate.score,
                path,
            }
        })
        .collect();
    apply_custom_sort(&mut results, plan.as_ref());
    let append_us = append_start.map_or(0, timing::elapsed_us);
    let total_us = total_start.map_or(0, timing::elapsed_us);

    let match_bitmap = matched_indexes
        .as_deref()
        .map(|indexes| MatchBitmap::from_indexes(end_index, indexes));

    timing::write_lazy(|| {
        let sampled_scale = if timing_samples > 0 {
            SEARCH_TIMING_SAMPLE_RATE as u128
        } else {
            0
        };
        let utf8_path_estimate_us = path_us * sampled_scale;
        let ascii_score_estimate_us = score_us * sampled_scale;
        let retention_estimate_us = retention_us * sampled_scale;
        let searched = filter.map_or(searched, |filter| filter.count_matches(end_index));
        format!(
        "search_detail mode={} total_us={total_us} parse_us={parse_us} match_us={match_us} sort_us={sort_us} append_us={append_us} utf8_path_estimate_us={utf8_path_estimate_us} ascii_score_estimate_us={ascii_score_estimate_us} char_fallback_estimate_us=0 retention_estimate_us={retention_estimate_us} timing_sample_rate={SEARCH_TIMING_SAMPLE_RATE} timing_samples={timing_samples} utf8_count={utf8_count} fallback_count=0 shown={} matched={matched} total={total} searched={searched}",
        sort_mode.timing_label(),
        results.len()
    )
    });

    Some(SearchOutput {
        results,
        matched,
        total,
        match_bitmap,
    })
}

/// Resolves the byte offsets used to highlight one already-ranked result.
///
/// Search results contain ranking data only, so callers can pay the
/// backtracking cost only for results they are about to display.
pub fn resolve_match_positions(query: &str, text: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let pattern = fzf::SearchPattern::parse_for_positions(query);
    pattern.positions(text, &mut fzf::MatchScratch::default())
}

fn materialize_indexes<S, I>(
    snapshot: Arc<S>,
    indexes: I,
    limit: usize,
    plan: &(dyn SearchPlan + '_),
) -> Vec<SearchResult>
where
    S: ItemsSource + Send + Sync,
    I: Iterator<Item = usize>,
{
    let mut path_buffer = Vec::with_capacity(512);
    indexes
        .take(limit)
        .map(|node_index| SearchResult {
            node_index,
            score: 0,
            path: plan
                .display_text(node_index)
                .unwrap_or_else(|| snapshot.get_string_lossy(node_index, &mut path_buffer)),
        })
        .collect()
}

fn apply_custom_sort(results: &mut [SearchResult], plan: &(dyn SearchPlan + '_)) {
    if plan.has_custom_sort() {
        results.sort_by(|left, right| {
            plan.compare(left.node_index, right.node_index)
                .then_with(|| right.score.cmp(&left.score))
                .then_with(|| left.node_index.cmp(&right.node_index))
        });
    }
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

#[rustfmt::skip]
#[allow(dead_code)]
mod fzf {
use super::*;
use memchr::{memrchr, memrchr2};
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

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
    #[cfg(target_arch = "x86_64")]
    v4: Option<V4CompiledQuery>,
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
        Self::parse_with_v4(query, true)
    }

    fn parse_with_v4(query: &str, compile_v4: bool) -> Option<Self> {
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

            let text = text.into_bytes();
            #[cfg(target_arch = "x86_64")]
            let v4 = if compile_v4
                && kind == MatchKind::Fuzzy
                && !inv
                && is_x86_feature_detected!("avx2")
                && v4_score_fits_u8(text.len())
            {
                // SAFETY: AVX2 was checked above, and the conservative score
                // bound guarantees that every V4 score fits in a u8 lane.
                Some(unsafe { V4CompiledQuery::new(&text, case_sensitive) })
            } else {
                None
            };

            set.terms.push(Term {
                kind,
                inv,
                case_sensitive,
                text,
                #[cfg(target_arch = "x86_64")]
                v4,
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

pub(super) struct SearchPattern {
    ascii: Option<AsciiPattern>,
    unicode: UnicodePattern,
}

impl SearchPattern {
    pub(super) fn parse(query: &str) -> Self {
        Self {
            ascii: AsciiPattern::parse(query),
            unicode: UnicodePattern::parse(query),
        }
    }

    pub(super) fn parse_for_positions(query: &str) -> Self {
        Self {
            ascii: AsciiPattern::parse_with_v4(query, false),
            unicode: UnicodePattern::parse(query),
        }
    }

    pub(super) fn score(
        &self,
        text: &[u8],
        is_ascii: bool,
        scratch: &mut MatchScratch,
    ) -> Option<u32> {
        // Smart Unicode mode: an ASCII query can use the byte scorer for any
        // UTF-8 haystack. Non-ASCII bytes cannot match an ASCII term, although
        // they intentionally count as individual scoring columns.
        if let Some(pattern) = &self.ascii {
            return pattern.score(text, scratch);
        }

        if is_ascii {
            if !self.unicode.ascii_can_match {
                return None;
            }
        }

        let text = std::str::from_utf8(text).ok()?;
        self.unicode.score(text, scratch)
    }

    pub(super) fn positions(&self, text: &str, scratch: &mut MatchScratch) -> Vec<usize> {
        if text.is_ascii() {
            if let Some(pattern) = &self.ascii {
                return pattern.positions(text.as_bytes(), scratch);
            }
        }
        self.unicode.positions(text, scratch)
    }
}

#[derive(Clone, Debug)]
struct UnicodeTerm {
    kind: MatchKind,
    inv: bool,
    case_sensitive: bool,
    text: Vec<char>,
}

#[derive(Clone, Debug, Default)]
struct UnicodeTermSet {
    terms: Vec<UnicodeTerm>,
}

#[derive(Clone, Debug)]
struct UnicodePattern {
    term_sets: Vec<UnicodeTermSet>,
    only_inv: bool,
    ascii_can_match: bool,
}

impl UnicodePattern {
    fn parse(query: &str) -> Self {
        let pattern = trim_suffix_spaces(query.trim_start());
        let pattern_copy = pattern.replace("\\ ", "\t");
        let mut term_sets = Vec::new();
        let mut set = UnicodeTermSet::default();
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

            let case_sensitive = text.chars().any(char::is_uppercase);
            let mut kind = MatchKind::Fuzzy;
            let mut inv = false;
            if let Some(rest) = text.strip_prefix('!') {
                inv = true;
                kind = MatchKind::Exact;
                text = rest.to_owned();
            }
            if text.ends_with('$') && text != "$" {
                kind = MatchKind::Suffix;
                text.pop();
            }
            if let Some(rest) = text.strip_prefix('\'') {
                kind = if inv {
                    MatchKind::Fuzzy
                } else {
                    MatchKind::Exact
                };
                text = rest.to_owned();
            } else if let Some(rest) = text.strip_prefix('^') {
                kind = MatchKind::Prefix;
                text = rest.to_owned();
            }
            if text.is_empty() {
                continue;
            }
            if switch_set {
                term_sets.push(set);
                set = UnicodeTermSet::default();
            }
            set.terms.push(UnicodeTerm {
                kind,
                inv,
                case_sensitive,
                text: text.chars().map(simple_fold_if(!case_sensitive)).collect(),
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
        let ascii_can_match = term_sets.iter().all(|set| {
            set.terms
                .iter()
                .any(|term| term.inv || term.text.iter().all(|character| character.is_ascii()))
        });
        Self {
            term_sets,
            only_inv,
            ascii_can_match,
        }
    }

    fn score(&self, text: &str, scratch: &mut MatchScratch) -> Option<u32> {
        prepare_unicode(text, scratch);
        let characters = std::mem::take(&mut scratch.unicode_chars);
        let score = self.score_chars(&characters, scratch);
        scratch.unicode_chars = characters;
        score
    }

    fn score_chars(&self, text: &[char], scratch: &mut MatchScratch) -> Option<u32> {
        if self.term_sets.is_empty() {
            return Some(1);
        }
        if self.only_inv {
            return self
                .term_sets
                .iter()
                .all(|set| match_unicode(&set.terms[0], text, scratch).start < 0)
                .then_some(1);
        }
        let mut total = 0i32;
        for set in &self.term_sets {
            let mut set_score = None;
            for term in &set.terms {
                let result = match_unicode(term, text, scratch);
                if term.inv {
                    if result.start < 0 {
                        set_score = Some(0);
                        break;
                    }
                } else if result.start >= 0 {
                    set_score = Some(result.score);
                    break;
                }
            }
            total += set_score?;
        }
        (total > 0).then_some(total as u32)
    }

    fn positions(&self, text: &str, scratch: &mut MatchScratch) -> Vec<usize> {
        prepare_unicode(text, scratch);
        let characters = std::mem::take(&mut scratch.unicode_chars);
        let mut char_positions = Vec::new();
        for set in &self.term_sets {
            for term in &set.terms {
                if term.inv {
                    continue;
                }
                let mut positions = Vec::new();
                let result =
                    match_unicode_with_positions(term, &characters, scratch, Some(&mut positions));
                if result.start >= 0 {
                    char_positions.extend(positions);
                    break;
                }
            }
        }
        char_positions.sort_unstable();
        char_positions.dedup();
        let result = char_positions
            .into_iter()
            .filter_map(|index| scratch.unicode_byte_offsets.get(index).copied())
            .collect::<Vec<_>>();
        scratch.unicode_chars = characters;
        result
    }
}

fn simple_fold_if(fold: bool) -> impl Fn(char) -> char {
    move |character| {
        if fold {
            simple_fold(character)
        } else {
            character
        }
    }
}

fn simple_fold(character: char) -> char {
    character.to_lowercase().next().unwrap_or(character)
}

fn prepare_unicode(text: &str, scratch: &mut MatchScratch) {
    scratch.unicode_chars.clear();
    scratch.unicode_byte_offsets.clear();
    for (offset, character) in text.char_indices() {
        scratch.unicode_chars.push(character);
        scratch.unicode_byte_offsets.push(offset);
    }
}

fn unicode_eq(left: char, right: char, case_sensitive: bool) -> bool {
    left == right || (!case_sensitive && simple_fold(left) == right)
}

fn match_unicode(term: &UnicodeTerm, text: &[char], scratch: &mut MatchScratch) -> FzfResult {
    match_unicode_with_positions(term, text, scratch, None)
}

fn match_unicode_with_positions(
    term: &UnicodeTerm,
    text: &[char],
    scratch: &mut MatchScratch,
    mut positions: Option<&mut Vec<usize>>,
) -> FzfResult {
    let pattern = &term.text;
    if pattern.is_empty() {
        return no_match();
    }
    match term.kind {
        MatchKind::Prefix => {
            let start = text
                .iter()
                .position(|character| !character.is_whitespace())
                .unwrap_or(text.len());
            contiguous_unicode_match(term, text, start, positions)
        }
        MatchKind::Suffix => {
            let end = text
                .iter()
                .rposition(|character| !character.is_whitespace())
                .map_or(0, |index| index + 1);
            if end < pattern.len() {
                no_match()
            } else {
                contiguous_unicode_match(term, text, end - pattern.len(), positions)
            }
        }
        MatchKind::Exact => {
            if text.len() < pattern.len() {
                return no_match();
            }
            for start in 0..=text.len() - pattern.len() {
                let result = contiguous_unicode_match(term, text, start, None);
                if result.start >= 0 {
                    if let Some(positions) = positions.as_deref_mut() {
                        positions.extend(start..start + pattern.len());
                    }
                    return result;
                }
            }
            no_match()
        }
        MatchKind::Fuzzy => {
            fzf_fuzzy_match_v2_unicode(term.case_sensitive, text, pattern, scratch, positions)
        }
    }
}

fn contiguous_unicode_match(
    term: &UnicodeTerm,
    text: &[char],
    start: usize,
    positions: Option<&mut Vec<usize>>,
) -> FzfResult {
    if start + term.text.len() > text.len()
        || !term.text.iter().enumerate().all(|(offset, &pattern)| {
            unicode_eq(text[start + offset], pattern, term.case_sensitive)
        })
    {
        return no_match();
    }
    if let Some(positions) = positions {
        positions.extend(start..start + term.text.len());
    }
    let matched: Vec<_> = (start..start + term.text.len()).collect();
    FzfResult {
        start: start as isize,
        end: (start + term.text.len()) as isize,
        score: score_unicode_positions(text, &matched),
    }
}

fn score_unicode_positions(text: &[char], positions: &[usize]) -> i32 {
    let mut score = 0;
    let mut previous = None;
    for &position in positions {
        score += SCORE_MATCH + unicode_bonus_at(text, position);
        if let Some(previous) = previous {
            let gap = position - previous - 1;
            if gap == 0 {
                score += BONUS_CONSECUTIVE;
            } else {
                score += SCORE_GAP_START + SCORE_GAP_EXTENSION * gap as i32;
            }
        }
        previous = Some(position);
    }
    score.max(1)
}

fn unicode_bonus_at(text: &[char], index: usize) -> i32 {
    if index == 0 {
        return BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER;
    }
    let previous = text[index - 1];
    let current = text[index];
    if previous.is_whitespace() || !previous.is_alphanumeric() {
        BOUNDARY_BONUS
    } else if previous.is_lowercase() && current.is_uppercase() {
        CAMEL_CASE_BONUS
    } else {
        0
    }
}

fn class_of_unicode(character: char) -> CharClass {
    if character.is_lowercase() {
        CharClass::CharLower
    } else if character.is_uppercase() {
        CharClass::CharUpper
    } else if character.is_numeric() {
        CharClass::Digit
    } else if character.is_alphabetic() {
        // ICU classifies modifier and other letters as CharLower for fzf's
        // scoring purposes. This includes uncased scripts such as CJK.
        CharClass::CharLower
    } else {
        CharClass::NonWord
    }
}

fn normalize_unicode(character: char, case_sensitive: bool) -> char {
    if case_sensitive {
        character
    } else {
        simple_fold(character)
    }
}

fn fuzzy_index_of_unicode(text: &[char], pattern: &[char], case_sensitive: bool) -> Option<usize> {
    let mut pattern_index = 0;
    let mut first = None;
    for (index, &character) in text.iter().enumerate() {
        if unicode_eq(character, pattern[pattern_index], case_sensitive) {
            // Retain the character immediately before the first match so the
            // scorer can classify the actual boundary. Starting directly at
            // the match would make every first match look like a word boundary.
            first.get_or_insert(index.saturating_sub(1));
            pattern_index += 1;
            if pattern_index == pattern.len() {
                return first;
            }
        }
    }
    None
}

fn fuzzy_match_v1_unicode(
    case_sensitive: bool,
    text: &[char],
    pattern: &[char],
    positions: Option<&mut Vec<usize>>,
) -> FzfResult {
    let mut found = Vec::with_capacity(pattern.len());
    let mut pattern_index = 0;
    for (index, &character) in text.iter().enumerate() {
        if unicode_eq(character, pattern[pattern_index], case_sensitive) {
            found.push(index);
            pattern_index += 1;
            if pattern_index == pattern.len() {
                break;
            }
        }
    }
    if pattern_index != pattern.len() {
        return no_match();
    }
    if let Some(positions) = positions {
        positions.extend_from_slice(&found);
    }
    FzfResult {
        start: found[0] as isize,
        end: (found[found.len() - 1] + 1) as isize,
        score: score_unicode_positions(text, &found),
    }
}

fn fzf_fuzzy_match_v2_unicode(
    case_sensitive: bool,
    text: &[char],
    pattern: &[char],
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
        let result = fuzzy_match_v1_unicode(case_sensitive, text, pattern, positions);
        if result.start < 0 {
        } else {
        }
        return result;
    }
    let Some(first_index_of) = fuzzy_index_of_unicode(text, pattern, case_sensitive) else {
        return no_match();
    };

    scratch.unicode_work.clear();
    scratch.unicode_work.extend_from_slice(text);
    scratch.initial_scores.clear();
    scratch.initial_scores.resize(text_size, 0);
    scratch.consecutive_scores.clear();
    scratch.consecutive_scores.resize(text_size, 0);
    scratch.bonuses.clear();
    scratch.bonuses.resize(text_size, 0);
    scratch.first_occurrence.clear();
    scratch.first_occurrence.resize(pattern_size, 0);

    let mut max_score = 0;
    let mut max_score_pos = 0;
    let mut pattern_index = 0;
    let mut last_index = 0;
    let first_pattern_char = normalize_unicode(pattern[0], case_sensitive);
    let mut current_pattern_char = first_pattern_char;
    let mut previous_initial_score = 0;
    let mut previous_class = CharClass::NonWord;
    let mut in_gap = false;

    for index in first_index_of..text_size {
        let original = scratch.unicode_work[index];
        let current_class = class_of_unicode(original);
        let current = normalize_unicode(original, case_sensitive);
        scratch.unicode_work[index] = current;
        let bonus = calculate_bonus(previous_class, current_class);
        scratch.bonuses[index] = bonus;
        previous_class = current_class;

        if current == current_pattern_char {
            if pattern_index < pattern_size {
                scratch.first_occurrence[pattern_index] = index;
                pattern_index += 1;
                current_pattern_char =
                    normalize_unicode(pattern[pattern_index.min(pattern_size - 1)], case_sensitive);
            }
            last_index = index;
        }

        if current == first_pattern_char {
            let score = SCORE_MATCH + bonus * BONUS_FIRST_CHAR_MULTIPLIER;
            scratch.initial_scores[index] = score;
            scratch.consecutive_scores[index] = 1;
            if pattern_size == 1 && score > max_score {
                max_score = score;
                max_score_pos = index;
            }
            in_gap = false;
        } else {
            scratch.initial_scores[index] = if in_gap {
                (previous_initial_score + SCORE_GAP_EXTENSION).max(0)
            } else {
                (previous_initial_score + SCORE_GAP_START).max(0)
            };
            scratch.consecutive_scores[index] = 0;
            in_gap = true;
        }
        previous_initial_score = scratch.initial_scores[index];
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

    let first_occurrence = scratch.first_occurrence[0];
    let width = last_index - first_occurrence + 1;
    scratch.score_matrix.clear();
    scratch.score_matrix.resize(width * pattern_size, 0);
    scratch.consecutive_matrix.clear();
    scratch.consecutive_matrix.resize(width * pattern_size, 0);
    for index in 0..width {
        let source = first_occurrence + index;
        scratch.score_matrix[index] = scratch.initial_scores[source];
        scratch.consecutive_matrix[index] = scratch.consecutive_scores[source];
    }

    for offset in 0..pattern_size - 1 {
        let pattern_char_offset = scratch.first_occurrence[offset + 1];
        let pattern_char = normalize_unicode(pattern[offset + 1], case_sensitive);
        let pattern_index = offset + 1;
        let row = pattern_index * width;
        let mut in_gap = false;
        for column in pattern_char_offset..=last_index {
            let local = column - first_occurrence;
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
            if scratch.unicode_work[column] == pattern_char && local > 0 {
                let diagonal_index = matrix_index - 1 - width;
                diagonal_score = scratch.score_matrix[diagonal_index] + SCORE_MATCH;
                let mut bonus = scratch.bonuses[column];
                consecutive = scratch.consecutive_matrix[diagonal_index] + 1;
                if bonus == BOUNDARY_BONUS {
                    consecutive = 1;
                } else if consecutive > 1 {
                    let start = column + 1 - consecutive as usize;
                    bonus = bonus.max(BONUS_CONSECUTIVE).max(scratch.bonuses[start]);
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
            let resulting_score = 0.max(diagonal_score.max(score));
            if pattern_index == pattern_size - 1 && resulting_score > max_score {
                max_score = resulting_score;
                max_score_pos = column;
            }
            scratch.score_matrix[matrix_index] = resulting_score;
        }
    }

    let mut start = max_score_pos;
    let mut pattern_index = pattern_size - 1;
    let mut prefer_match = true;
    loop {
        let row = pattern_index * width;
        let column = start - first_occurrence;
        let current_score = scratch.score_matrix[row + column];
        let diagonal_score =
            if pattern_index > 0 && start >= scratch.first_occurrence[pattern_index] {
                scratch.score_matrix[row - width + column - 1]
            } else {
                0
            };
        let left_score = if start > scratch.first_occurrence[pattern_index] {
            scratch.score_matrix[row + column - 1]
        } else {
            0
        };
        if current_score > diagonal_score
            && (current_score > left_score || (current_score == left_score && prefer_match))
        {
            if let Some(positions) = positions.as_deref_mut() {
                positions.push(start);
            }
            if pattern_index == 0 {
                break;
            }
            pattern_index -= 1;
        }
        if start == first_occurrence {
            break;
        }
        prefer_match = scratch.consecutive_matrix[row + column] > 1
            || (row + width + column + 1 < scratch.consecutive_matrix.len()
                && scratch.consecutive_matrix[row + width + column + 1] > 0);
        start -= 1;
    }
    FzfResult {
        start: start as isize,
        end: max_score_pos as isize + 1,
        score: max_score,
    }
}


#[derive(Clone, Debug)]
#[cfg(target_arch = "x86_64")]
struct V4CompiledQuery {
    pattern: Vec<u8>,
    case_sensitive: bool,
    match_vectors: Vec<(__m256i, __m256i)>,
}

#[cfg(target_arch = "x86_64")]
impl V4CompiledQuery {
    #[target_feature(enable = "avx2")]
    unsafe fn new(pattern: &[u8], case_sensitive: bool) -> Self {
        let mut match_vectors = Vec::with_capacity(pattern.len());
        for &byte in pattern {
            let flipped = if case_sensitive {
                byte
            } else if byte.is_ascii_lowercase() {
                byte.to_ascii_uppercase()
            } else {
                byte.to_ascii_lowercase()
            };
            match_vectors.push((
                _mm256_set1_epi8(byte as i8),
                _mm256_set1_epi8(flipped as i8),
            ));
        }
        Self {
            pattern: pattern.to_vec(),
            case_sensitive,
            match_vectors,
        }
    }
}

#[inline(always)]
fn v4_score_fits_u8(pattern_size: usize) -> bool {
    // The maximum completed score is 12 points per needle byte plus four
    // one-time first-character boundary points. SIMD adds the three-point
    // mismatch value before subtracting it, so retain that transient headroom.
    pattern_size
        .checked_mul(12)
        .and_then(|score| score.checked_add(7))
        .is_some_and(|score| score <= u8::MAX as usize)
}

/// Dense u8 implementation of the V3 scoring semantics. Two rolling
/// vector banks carry scores between 32-byte text chunks, while five prefix
/// steps propagate horizontal gaps across each AVX2 vector.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn fzf_fuzzy_match_v4_ascii_avx2_u8(
    text: &[u8],
    query: &V4CompiledQuery,
    scratch: &mut MatchScratch,
) -> FzfResult {
    let pattern = query.pattern.as_slice();
    debug_assert!(pattern.is_ascii());
    debug_assert!(v4_score_fits_u8(pattern.len()));

    if pattern.is_empty() {
        return FzfResult {
            start: 0,
            end: 0,
            score: 0,
        };
    }

    let Some((window_start, window_end)) =
        (unsafe { fuzzy_window_v4_ascii_avx2(text, query) })
    else {
        return no_match();
    };
    let width = window_end - window_start;
    let padded_width = width.next_multiple_of(32);

    let text_chunks = padded_width / 32;
    let bank_stride = pattern.len() + 1;
    let matrix_len = bank_stride * 2;
    let zero = _mm256_setzero_si256();
    scratch.v4_score_vectors.resize(matrix_len, zero);
    scratch.v4_packed_match_vectors.resize(matrix_len, zero);
    // The first text chunk uses explicit zero vectors instead of reading stale
    // bank contents. Every bank row is overwritten before a later chunk reads it.

    let gap_extend = _mm256_set1_epi8(1);
    let gap_open_after_extend = _mm256_set1_epi8(1);
    let match_plus_mismatch = _mm256_set1_epi8(11);
    let mismatch = _mm256_set1_epi8(3);
    let consecutive_bonus = _mm256_set1_epi8(BONUS_CONSECUTIVE as i8);
    let first_lane = _mm256_setr_epi8(
        -1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    );
    let mut final_max = _mm256_setzero_si256();
    let all_lanes = _mm256_set1_epi8(-1);
    let capitalization_bonus = _mm256_set1_epi8(CAMEL_CASE_BONUS as i8);
    let delimiter_bonus = _mm256_set1_epi8(BOUNDARY_BONUS as i8);
    let mut previous_lower_mask = _mm256_setzero_si256();
    let mut previous_digit_mask = _mm256_setzero_si256();
    let mut previous_delimiter_mask = _mm256_setzero_si256();

    for chunk_index in 0..text_chunks {
        let chunk_start = chunk_index * 32;
        let previous_bank = (chunk_index & 1) * bank_stride;
        let current_bank = ((chunk_index + 1) & 1) * bank_stride;
        let has_previous_chunk = chunk_index != 0;
        let text_bytes =
            unsafe { v4_load_partial_32(text.as_ptr().add(window_start), chunk_start, width) };
        let is_upper = _mm256_and_si256(
            _mm256_cmpgt_epi8(text_bytes, _mm256_set1_epi8((b'A' - 1) as i8)),
            _mm256_cmpgt_epi8(_mm256_set1_epi8((b'Z' + 1) as i8), text_bytes),
        );
        let is_lower = _mm256_and_si256(
            _mm256_cmpgt_epi8(text_bytes, _mm256_set1_epi8((b'a' - 1) as i8)),
            _mm256_cmpgt_epi8(_mm256_set1_epi8((b'z' + 1) as i8), text_bytes),
        );
        let is_digit = _mm256_and_si256(
            _mm256_cmpgt_epi8(text_bytes, _mm256_set1_epi8((b'0' - 1) as i8)),
            _mm256_cmpgt_epi8(_mm256_set1_epi8((b'9' + 1) as i8), text_bytes),
        );
        let is_alphanumeric = _mm256_or_si256(_mm256_or_si256(is_upper, is_lower), is_digit);
        let is_delimiter = _mm256_andnot_si256(
            is_alphanumeric,
            all_lanes,
        );
        let previous_is_lower = unsafe { v4_shift_right::<1>(is_lower, previous_lower_mask) };
        let previous_is_digit = unsafe { v4_shift_right::<1>(is_digit, previous_digit_mask) };
        let previous_is_delimiter =
            unsafe { v4_shift_right::<1>(is_delimiter, previous_delimiter_mask) };
        let capitalization_mask = _mm256_and_si256(is_upper, previous_is_lower);
        let digit_boundary_mask = _mm256_andnot_si256(previous_is_digit, is_digit);
        let camel_or_digit_boundary_mask =
            _mm256_or_si256(capitalization_mask, digit_boundary_mask);
        let mut delimiter_boundary_mask =
            _mm256_andnot_si256(is_delimiter, previous_is_delimiter);
        if window_start == 0 && chunk_index == 0 {
            delimiter_boundary_mask = _mm256_or_si256(delimiter_boundary_mask, first_lane);
        }
        let non_word_or_boundary_mask =
            _mm256_or_si256(delimiter_boundary_mask, is_delimiter);
        let boundary_bonus = _mm256_max_epu8(
            _mm256_and_si256(camel_or_digit_boundary_mask, capitalization_bonus),
            _mm256_and_si256(non_word_or_boundary_mask, delimiter_bonus),
        );
        previous_lower_mask = is_lower;
        previous_digit_mask = is_digit;
        previous_delimiter_mask = is_delimiter;
        let mut previous_row_scores = _mm256_setzero_si256();
        let mut previous_row_match_mask = _mm256_setzero_si256();
        let mut previous_row_run_bonus = _mm256_setzero_si256();
        let mut adjacent_previous_scores = _mm256_setzero_si256();
        let mut adjacent_previous_match_mask = _mm256_setzero_si256();
        let mut adjacent_previous_run_bonus = _mm256_setzero_si256();

        for (row_index, &(pattern_vector, flipped_pattern_vector)) in
            query.match_vectors.iter().enumerate()
        {
            let row = row_index + 1;
            let match_mask = _mm256_or_si256(
                _mm256_cmpeq_epi8(text_bytes, pattern_vector),
                _mm256_cmpeq_epi8(text_bytes, flipped_pattern_vector),
            );
            let current_adjacent = previous_bank + row;
            let current_cell = current_bank + row;
            let adjacent_row = if has_previous_chunk {
                unsafe { *scratch.v4_score_vectors.get_unchecked(current_adjacent) }
            } else {
                zero
            };
            let adjacent_match_state = if has_previous_chunk {
                unsafe {
                    *scratch
                        .v4_packed_match_vectors
                        .get_unchecked(current_adjacent)
                }
            } else {
                zero
            };
            let adjacent_match_mask =
                _mm256_cmpgt_epi8(adjacent_match_state, _mm256_setzero_si256());
            let adjacent_run_bonus = _mm256_subs_epu8(adjacent_match_state, gap_extend);

            let diagonal_scores =
                unsafe { v4_shift_right::<1>(previous_row_scores, adjacent_previous_scores) };
            let diagonal_match_mask = unsafe {
                v4_shift_right::<1>(previous_row_match_mask, adjacent_previous_match_mask)
            };
            let diagonal_run_bonus =
                unsafe { v4_shift_right::<1>(previous_row_run_bonus, adjacent_previous_run_bonus) };
            let can_continue = _mm256_andnot_si256(
                non_word_or_boundary_mask,
                _mm256_and_si256(match_mask, diagonal_match_mask),
            );
            let continued_bonus = _mm256_and_si256(
                can_continue,
                _mm256_max_epu8(diagonal_run_bonus, consecutive_bonus),
            );
            let effective_bonus = _mm256_max_epu8(boundary_bonus, continued_bonus);

            let mut diagonal = _mm256_add_epi8(
                diagonal_scores,
                _mm256_and_si256(match_mask, match_plus_mismatch),
            );
            diagonal = _mm256_add_epi8(diagonal, _mm256_and_si256(match_mask, effective_bonus));
            if row_index == 0 {
                diagonal = _mm256_add_epi8(diagonal, _mm256_and_si256(match_mask, boundary_bonus));
            }
            diagonal = _mm256_subs_epu8(diagonal, mismatch);

            let up = _mm256_subs_epu8(previous_row_scores, gap_extend);
            let up = _mm256_subs_epu8(
                up,
                _mm256_and_si256(previous_row_match_mask, gap_open_after_extend),
            );
            let row_scores = unsafe {
                v4_propagate_v3_gaps(
                    _mm256_max_epu8(diagonal, up),
                    adjacent_row,
                    match_mask,
                    adjacent_match_mask,
                    gap_open_after_extend,
                    gap_extend,
                )
            };
            let run_bonus = _mm256_and_si256(match_mask, effective_bonus);
            let packed_match_state = _mm256_add_epi8(
                run_bonus,
                _mm256_and_si256(match_mask, gap_extend),
            );
            unsafe {
                *scratch.v4_score_vectors.get_unchecked_mut(current_cell) = row_scores;
                *scratch
                    .v4_packed_match_vectors
                    .get_unchecked_mut(current_cell) = packed_match_state;
            }
            previous_row_scores = row_scores;
            previous_row_match_mask = match_mask;
            previous_row_run_bonus = run_bonus;
            adjacent_previous_scores = adjacent_row;
            adjacent_previous_match_mask = adjacent_match_mask;
            adjacent_previous_run_bonus = adjacent_run_bonus;
        }
        final_max = _mm256_max_epu8(final_max, previous_row_scores);
    }

    FzfResult {
        start: window_start as isize,
        end: window_end as isize,
        score: unsafe { v4_horizontal_max_u8(final_max) as i32 },
    }
}

/// Finds the same no-typo ASCII window as `fuzzy_window_ascii`, but keeps each
/// 32-byte haystack chunk loaded while advancing through every query byte that
/// occurs in that chunk. The reverse endpoint remains delegated to `memrchr`
/// or `memrchr2`, which are already vectorized by the `memchr` crate.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn fuzzy_window_v4_ascii_avx2(
    input: &[u8],
    query: &V4CompiledQuery,
) -> Option<(usize, usize)> {
    debug_assert!(!query.pattern.is_empty());

    let mut pattern_index = 0usize;
    let mut first_index = 0usize;
    let mut chunk_start = 0usize;

    while chunk_start < input.len() {
        let remaining = input.len() - chunk_start;
        let valid_lanes = remaining.min(32);
        let mut available = if valid_lanes == 32 {
            u32::MAX
        } else {
            (1u32 << valid_lanes) - 1
        };
        let chunk = unsafe { v4_load_partial_32(input.as_ptr(), chunk_start, input.len()) };

        loop {
            // SAFETY: pattern_index starts at zero, advances only after a match,
            // and the function returns as soon as it reaches pattern.len().
            let &(pattern_vector, flipped_pattern_vector) =
                unsafe { query.match_vectors.get_unchecked(pattern_index) };
            let matches = _mm256_or_si256(
                _mm256_cmpeq_epi8(chunk, pattern_vector),
                _mm256_cmpeq_epi8(chunk, flipped_pattern_vector),
            );
            let mask = (_mm256_movemask_epi8(matches) as u32) & available;
            if mask == 0 {
                break;
            }

            let lane = mask.trailing_zeros() as usize;
            if pattern_index == 0 {
                first_index = chunk_start + lane;
            }
            pattern_index += 1;

            if pattern_index == query.pattern.len() {
                let last = *query.pattern.last().unwrap();
                let last_index = find_byte_ascii_reverse(input, last, query.case_sensitive)?;
                return Some((first_index.saturating_sub(1), last_index + 1));
            }

            available = if lane == 31 {
                0
            } else {
                available & (u32::MAX << (lane + 1))
            };
        }

        chunk_start += 32;
    }

    None
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn v4_load_partial_32(input: *const u8, offset: usize, len: usize) -> __m256i {
    if offset + 32 <= len {
        return unsafe { _mm256_loadu_si256(input.add(offset).cast()) };
    }

    let mut tail = [0u8; 32];
    let remaining = len - offset;
    unsafe { std::ptr::copy_nonoverlapping(input.add(offset), tail.as_mut_ptr(), remaining) };
    unsafe { _mm256_loadu_si256(tail.as_ptr().cast()) }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn v4_shift_right<const LANES: i32>(value: __m256i, previous: __m256i) -> __m256i {
    let between_halves = _mm256_permute2x128_si256::<0x21>(previous, value);
    match LANES {
        1 => _mm256_alignr_epi8::<15>(value, between_halves),
        2 => _mm256_alignr_epi8::<14>(value, between_halves),
        4 => _mm256_alignr_epi8::<12>(value, between_halves),
        8 => _mm256_alignr_epi8::<8>(value, between_halves),
        16 => between_halves,
        _ => unreachable!(),
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn v4_propagate_v3_gaps(
    mut row: __m256i,
    adjacent_row: __m256i,
    match_mask: __m256i,
    adjacent_match_mask: __m256i,
    gap_open_penalty: __m256i,
    mut gap_extend_penalty: __m256i,
) -> __m256i {
    macro_rules! propagate {
        ($shift:literal) => {{
            let shifted_row = unsafe { v4_shift_right::<$shift>(row, adjacent_row) };
            let shifted_match =
                unsafe { v4_shift_right::<$shift>(match_mask, adjacent_match_mask) };
            let penalty = _mm256_add_epi8(
                gap_extend_penalty,
                _mm256_and_si256(gap_open_penalty, shifted_match),
            );
            row = _mm256_max_epu8(row, _mm256_subs_epu8(shifted_row, penalty));
            gap_extend_penalty = _mm256_add_epi8(gap_extend_penalty, gap_extend_penalty);
        }};
    }

    propagate!(1);
    propagate!(2);
    propagate!(4);
    propagate!(8);
    propagate!(16);
    let _ = gap_extend_penalty;
    row
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn v4_horizontal_max_u8(value: __m256i) -> u8 {
    let low = _mm256_castsi256_si128(value);
    let high = _mm256_extracti128_si256::<1>(value);
    let max = _mm_max_epu8(low, high);
    let max = _mm_max_epu8(max, _mm_srli_si128::<8>(max));
    let max = _mm_max_epu8(max, _mm_srli_si128::<4>(max));
    let max = _mm_max_epu8(max, _mm_srli_si128::<2>(max));
    let max = _mm_max_epu8(max, _mm_srli_si128::<1>(max));
    _mm_extract_epi8::<0>(max) as u8
}

fn find_byte_ascii_reverse(input: &[u8], value: u8, case_sensitive: bool) -> Option<usize> {
    let value = normalize_byte(value, case_sensitive);
    if case_sensitive || !value.is_ascii_lowercase() {
        memrchr(value, input)
    } else {
        memrchr2(value, value.to_ascii_uppercase(), input)
    }
}


#[derive(Default)]
pub(super) struct MatchScratch {
    text_copy: Vec<u8>,
    initial_scores: Vec<i32>,
    consecutive_scores: Vec<i32>,
    bonuses: Vec<i32>,
    first_occurrence: Vec<usize>,
    score_matrix: Vec<i32>,
    consecutive_matrix: Vec<i32>,
    unicode_chars: Vec<char>,
    unicode_work: Vec<char>,
    unicode_byte_offsets: Vec<usize>,
    #[cfg(target_arch = "x86_64")]
    v4_score_vectors: Vec<__m256i>,
    #[cfg(target_arch = "x86_64")]
    v4_packed_match_vectors: Vec<__m256i>,
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
            #[cfg(target_arch = "x86_64")]
            if let Some(query) = &term.v4 {
                // SAFETY: queries are compiled only after AVX2 and the u8
                // score bound have been checked; this path receives ASCII.
                return unsafe { fzf_fuzzy_match_v4_ascii_avx2_u8(text, query, scratch) };
            }
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
        if res.start >= 0 {
            if let Some(positions) = positions.as_deref_mut() {
                append_positions_ascii(
                    case_sensitive,
                    text,
                    pattern,
                    res.start as usize,
                    res.end as usize,
                    positions,
                );
            }
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
                    let consecutive_start = column + 1 - consecutive as usize;
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
            if let Some(positions) = positions.as_deref_mut() {
                positions.push(start);
            }

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

#[cfg(test)]
mod search_sort_tests {
    use super::*;
    use crate::store::FlatSnapshot;

    #[test]
    fn source_order_returns_first_matches_up_to_result_limit() {
        let item_count = RESULT_LIMIT + 25;
        let snapshot = Arc::new(FlatSnapshot::from_items(
            (0..item_count).map(|index| (format!("match-{index:04}"), ())),
        ));

        let output = search_with_sort(snapshot, "match", SearchSortMode::SourceOrder, || false)
            .expect("search should complete");

        assert_eq!(output.matched, item_count);
        assert_eq!(output.results.len(), RESULT_LIMIT);
        assert_eq!(
            output
                .results
                .iter()
                .map(|result| result.node_index)
                .collect::<Vec<_>>(),
            (0..RESULT_LIMIT).collect::<Vec<_>>()
        );
    }

    #[test]
    fn sort_modes_report_the_same_match_count() {
        let snapshot = Arc::new(FlatSnapshot::from_items([
            ("prefix needle", ()),
            ("not a result", ()),
            ("needle", ()),
            ("another needle result", ()),
        ]));

        let score = search_with_sort(
            Arc::clone(&snapshot),
            "needle",
            SearchSortMode::Score,
            || false,
        )
        .expect("score search should complete");
        let source_order =
            search_with_sort(snapshot, "needle", SearchSortMode::SourceOrder, || false)
                .expect("source-order search should complete");

        assert_eq!(source_order.matched, score.matched);
        assert_eq!(
            source_order
                .results
                .iter()
                .map(|result| result.node_index)
                .collect::<Vec<_>>(),
            vec![0, 2, 3]
        );
    }

    #[test]
    fn search_defers_positions_until_the_result_is_materialized() {
        let snapshot = Arc::new(FlatSnapshot::from_items([("alpha-beta", ())]));
        let output = search(snapshot, "ab", || false).expect("search should complete");

        assert_eq!(output.results[0].path, "alpha-beta");
        assert_eq!(resolve_match_positions("ab", "alpha-beta"), vec![0, 6]);
    }

    #[test]
    fn unicode_queries_and_paths_use_v2_and_return_byte_positions() {
        let snapshot = Arc::new(FlatSnapshot::from_items([
            ("plain ascii", ()),
            ("café", ()),
        ]));
        let output = search(snapshot, "fé", || false).expect("search should complete");

        assert_eq!(output.matched, 1);
        assert_eq!(output.results[0].path, "café");
        assert_eq!(resolve_match_positions("fé", "café"), vec![2, 3]);
    }

    #[test]
    fn ascii_queries_use_byte_scoring_for_unicode_paths_but_unicode_positions() {
        let text = "aéb";
        let pattern = SearchPattern::parse("ab");

        let smart_score = pattern
            .score(text.as_bytes(), false, &mut MatchScratch::default())
            .expect("ASCII query should match across the Unicode character");
        let byte_score = pattern
            .ascii
            .as_ref()
            .expect("ASCII query")
            .score(text.as_bytes(), &mut MatchScratch::default())
            .expect("byte scorer should match");
        let unicode_score = pattern
            .unicode
            .score(text, &mut MatchScratch::default())
            .expect("Unicode scorer should match");

        assert_eq!(smart_score, byte_score);
        assert_ne!(smart_score, unicode_score);
        assert_eq!(resolve_match_positions("ab", text), vec![0, 3]);
    }

    #[test]
    fn v4_is_selected_only_when_avx2_and_the_u8_bound_allow_it() {
        let short = AsciiPattern::parse("simm").expect("ASCII query");
        let short_term = &short.term_sets[0].terms[0];
        #[cfg(target_arch = "x86_64")]
        assert_eq!(short_term.v4.is_some(), is_x86_feature_detected!("avx2"));

        let positions = SearchPattern::parse_for_positions("simm");
        #[cfg(target_arch = "x86_64")]
        assert!(positions.ascii.unwrap().term_sets[0].terms[0].v4.is_none());

        let longest_v4_query = "a".repeat(20);
        let longest_v4 = AsciiPattern::parse(&longest_v4_query).expect("ASCII query");
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            longest_v4.term_sets[0].terms[0].v4.is_some(),
            is_x86_feature_detected!("avx2")
        );

        let long_query = "a".repeat(21);
        let long = AsciiPattern::parse(&long_query).expect("ASCII query");
        #[cfg(target_arch = "x86_64")]
        assert!(long.term_sets[0].terms[0].v4.is_none());
    }

}

#[test]
fn fuzzy_match_consecutive_at_start_does_not_underflow() {
    let mut scratch = MatchScratch::default();

    let result = fzf_fuzzy_match_v2_ascii(false, b"rights", b"ri", &mut scratch, None);

    assert!(result.score > 0);
    assert_eq!(result.start, 0);
    assert_eq!(result.end, 2);
}

#[cfg(test)]
mod tests_from_fzf {
    use super::*;

    #[derive(Clone, Copy)]
    enum Algorithm {
        FuzzyV1,
        FuzzyV2,
        Exact,
        Prefix,
        Suffix,
    }

    fn run_match(
        algorithm: Algorithm,
        case_sensitive: bool,
        text: &[u8],
        pattern: &[u8],
    ) -> FzfResult {
        let mut scratch = MatchScratch::default();
        match algorithm {
            Algorithm::FuzzyV1 => fuzzy_match_v1_ascii(case_sensitive, text, pattern),
            Algorithm::FuzzyV2 => {
                fzf_fuzzy_match_v2_ascii(case_sensitive, text, pattern, &mut scratch, None)
            }
            Algorithm::Exact => fzf_exact_match_naive_ascii(case_sensitive, text, pattern),
            Algorithm::Prefix => fzf_prefix_match_ascii(case_sensitive, text, pattern),
            Algorithm::Suffix => fzf_suffix_match_ascii(case_sensitive, text, pattern),
        }
    }

    fn assert_score(
        algorithm: Algorithm,
        case_sensitive: bool,
        text: &[u8],
        pattern: &[u8],
        expected_score: i32,
    ) {
        let result = run_match(algorithm, case_sensitive, text, pattern);
        assert_eq!(
            result.score,
            expected_score,
            "pattern: {:?}, text: {:?}, case_sensitive: {case_sensitive}",
            String::from_utf8_lossy(pattern),
            String::from_utf8_lossy(text)
        );
    }

    #[test]
    fn fuzzy_match_scores_match_fzf_cases() {
        for algorithm in [Algorithm::FuzzyV1, Algorithm::FuzzyV2] {
            assert_score(
                algorithm,
                false,
                b"fooBarbaz1",
                b"obz",
                SCORE_MATCH * 3 + CAMEL_CASE_BONUS + SCORE_GAP_START + SCORE_GAP_EXTENSION * 3,
            );
            assert_score(
                algorithm,
                false,
                b"foo bar baz",
                b"fbb",
                SCORE_MATCH * 3
                    + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                    + BOUNDARY_BONUS * 2
                    + 2 * SCORE_GAP_START
                    + 4 * SCORE_GAP_EXTENSION,
            );
            assert_score(
                algorithm,
                false,
                b"/AutomatorDocument.icns",
                b"rdoc",
                SCORE_MATCH * 4 + CAMEL_CASE_BONUS + BONUS_CONSECUTIVE * 2,
            );
            assert_score(
                algorithm,
                false,
                b"/man1/zshcompctl.1",
                b"zshc",
                SCORE_MATCH * 4 + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER + BOUNDARY_BONUS * 3,
            );
            assert_score(
                algorithm,
                false,
                b"/.oh-my-zsh/cache",
                b"zshc",
                SCORE_MATCH * 4
                    + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                    + BOUNDARY_BONUS * 2
                    + SCORE_GAP_START
                    + BOUNDARY_BONUS,
            );
            assert_score(
                algorithm,
                false,
                b"ab0123 456",
                b"12356",
                SCORE_MATCH * 5 + BONUS_CONSECUTIVE * 3 + SCORE_GAP_START + SCORE_GAP_EXTENSION,
            );
            assert_score(
                algorithm,
                false,
                b"foo/bar/baz",
                b"fbb",
                SCORE_MATCH * 3
                    + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                    + BOUNDARY_BONUS * 2
                    + 2 * SCORE_GAP_START
                    + 4 * SCORE_GAP_EXTENSION,
            );
            assert_score(
                algorithm,
                true,
                b"FooBarBaz",
                b"FBB",
                SCORE_MATCH * 3
                    + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                    + CAMEL_CASE_BONUS * 2
                    + SCORE_GAP_START * 2
                    + SCORE_GAP_EXTENSION * 2,
            );
            assert_score(algorithm, true, b"fooBarbaz", b"oBZ", 0);
            assert_score(algorithm, true, b"Foo Bar Baz", b"fbb", 0);
            assert_score(algorithm, true, b"fooBarbaz", b"fooBarbazz", 0);
        }
    }

    #[test]
    fn fuzzy_match_v1_forward_case_score_matches_fzf() {
        assert_score(
            Algorithm::FuzzyV1,
            false,
            b"foobar fb",
            b"fb",
            SCORE_MATCH * 2
                + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                + SCORE_GAP_START
                + SCORE_GAP_EXTENSION,
        );
    }

    #[test]
    fn exact_match_naive_scores_match_fzf_cases() {
        assert_score(Algorithm::Exact, true, b"fooBarbaz", b"oBA", 0);
        assert_score(Algorithm::Exact, true, b"fooBarbaz", b"fooBarbazz", 0);
        assert_score(
            Algorithm::Exact,
            false,
            b"fooBarbaz",
            b"oba",
            SCORE_MATCH * 3 + CAMEL_CASE_BONUS + BONUS_CONSECUTIVE,
        );
        assert_score(
            Algorithm::Exact,
            false,
            b"/AutomatorDocument.icns",
            b"rdoc",
            SCORE_MATCH * 4 + CAMEL_CASE_BONUS + BONUS_CONSECUTIVE * 2,
        );
        assert_score(
            Algorithm::Exact,
            false,
            b"/man1/zshcompctl.1",
            b"zshc",
            SCORE_MATCH * 4 + BOUNDARY_BONUS * (BONUS_FIRST_CHAR_MULTIPLIER + 3),
        );
        assert_score(
            Algorithm::Exact,
            false,
            b"/.oh-my-zsh/cache",
            b"zsh/c",
            SCORE_MATCH * 5 + BOUNDARY_BONUS * (BONUS_FIRST_CHAR_MULTIPLIER + 3) + BOUNDARY_BONUS,
        );
    }

    #[test]
    fn exact_match_naive_backward_case_score_matches_fzf() {
        assert_score(
            Algorithm::Exact,
            false,
            b"foobar foob",
            b"oo",
            SCORE_MATCH * 2 + BONUS_CONSECUTIVE,
        );
    }

    #[test]
    fn prefix_match_scores_match_fzf_cases() {
        let score =
            SCORE_MATCH * 3 + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER + BOUNDARY_BONUS * 2;

        assert_score(Algorithm::Prefix, true, b"fooBarbaz", b"Foo", 0);
        assert_score(Algorithm::Prefix, false, b"fooBarBaz", b"baz", 0);
        assert_score(Algorithm::Prefix, false, b"fooBarbaz", b"foo", score);
        assert_score(Algorithm::Prefix, false, b"foOBarBaZ", b"foo", score);
        assert_score(Algorithm::Prefix, false, b"f-oBarbaz", b"f-o", score);
        assert_score(Algorithm::Prefix, false, b" fooBar", b"foo", score);
        assert_score(Algorithm::Prefix, false, b" fooBar", b" fo", score);
        assert_score(Algorithm::Prefix, false, b"     fo", b"foo", 0);
    }

    #[test]
    fn suffix_match_scores_match_fzf_cases() {
        assert_score(Algorithm::Suffix, true, b"fooBarbaz", b"Baz", 0);
        assert_score(Algorithm::Suffix, false, b"fooBarbaz", b"foo", 0);
        assert_score(
            Algorithm::Suffix,
            false,
            b"fooBarbaz",
            b"baz",
            SCORE_MATCH * 3 + BONUS_CONSECUTIVE * 2,
        );
        assert_score(
            Algorithm::Suffix,
            false,
            b"fooBarBaZ",
            b"baz",
            (SCORE_MATCH + CAMEL_CASE_BONUS) * 3
                + CAMEL_CASE_BONUS * (BONUS_FIRST_CHAR_MULTIPLIER - 1),
        );
        assert_score(
            Algorithm::Suffix,
            false,
            b"fooBarbaz ",
            b"baz",
            SCORE_MATCH * 3 + BONUS_CONSECUTIVE * 2,
        );
        assert_score(
            Algorithm::Suffix,
            false,
            b"fooBarbaz ",
            b"baz ",
            SCORE_MATCH * 4 + BONUS_CONSECUTIVE * 2 + BOUNDARY_BONUS,
        );
    }

    #[test]
    fn empty_pattern_scores_match_fzf_cases() {
        for algorithm in [
            Algorithm::FuzzyV1,
            Algorithm::FuzzyV2,
            Algorithm::Exact,
            Algorithm::Prefix,
            Algorithm::Suffix,
        ] {
            assert_score(algorithm, true, b"foobar", b"", 0);
        }
    }

    #[test]
    fn long_string_match_past_u16_max_matches_fzf_case() {
        let mut text = vec![b'x'; u16::MAX as usize * 2];
        text.insert(u16::MAX as usize, b'z');

        let result = run_match(Algorithm::FuzzyV2, true, &text, b"zx");

        assert_eq!(result.start, u16::MAX as isize);
        assert_eq!(result.end, u16::MAX as isize + 2);
        assert_eq!(result.score, SCORE_MATCH * 2 + BONUS_CONSECUTIVE);
    }
}

#[cfg(test)]
mod cascadia_fzf_tests;

#[cfg(test)]
mod unicode_cascadia_fzf_tests;

}
