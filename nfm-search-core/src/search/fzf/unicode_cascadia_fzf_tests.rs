//! Unicode score and position fixtures translated from
//! TerminalAppUnitTests/FzfTests.cpp in G:\src\terminal-retry.

use super::*;

fn byte_positions(text: &str, character_positions: &[usize]) -> Vec<usize> {
    let offsets = text.char_indices().map(|(offset, _)| offset).collect::<Vec<_>>();
    character_positions
        .iter()
        .map(|&position| offsets[position])
        .collect()
}

fn assert_score_and_positions(
    query: &str,
    text: &str,
    expected_score: i32,
    expected_character_positions: &[usize],
) {
    // Cascadia's fixtures are case-insensitive even when the query contains
    // uppercase characters. Lowercasing selects NFM's case-insensitive mode.
    let query = query.to_lowercase();
    let pattern = SearchPattern::parse(&query);
    let score = pattern.score(
        text.as_bytes(),
        text.is_ascii(),
        &mut MatchScratch::default(),
    );
    assert_eq!(score, Some(expected_score as u32), "query={query:?} text={text:?}");

    let positions = pattern.positions(text, &mut MatchScratch::default());
    assert_eq!(
        positions,
        byte_positions(text, expected_character_positions),
        "query={query:?} text={text:?}"
    );
}

#[test]
fn russian_case_mismatch() {
    assert_score_and_positions(
        "новая",
        "Новая вкладка",
        SCORE_MATCH * 5
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 4,
        &[0, 1, 2, 3, 4],
    );
}

#[test]
fn russian_case_match() {
    assert_score_and_positions(
        "Новая",
        "Новая вкладка",
        SCORE_MATCH * 5
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 4,
        &[0, 1, 2, 3, 4],
    );
}

#[test]
fn german_case_match() {
    assert_score_and_positions(
        "fuß",
        "Fußball",
        SCORE_MATCH * 3
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 2,
        &[0, 1, 2],
    );
}

#[test]
#[ignore = "one-to-many Unicode case folding (ß to ss) is not implemented"]
fn german_case_mismatch_that_expands_when_folded() {
    assert_score_and_positions(
        "fuss",
        "Fußball",
        SCORE_MATCH * 4
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 3,
        &[0, 1, 2],
    );
}

#[test]
fn french_case_match() {
    assert_score_and_positions(
        "Éco",
        "École",
        SCORE_MATCH * 3
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 2,
        &[0, 1, 2],
    );
}

#[test]
fn french_case_mismatch() {
    assert_score_and_positions(
        "Éco",
        "école",
        SCORE_MATCH * 3
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 2,
        &[0, 1, 2],
    );
}

#[test]
fn greek_case_match() {
    assert_score_and_positions(
        "λόγος",
        "λόγος",
        SCORE_MATCH * 5
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 4,
        &[0, 1, 2, 3, 4],
    );
}

#[test]
#[ignore = "Unicode final-sigma case folding (ς to σ) is not implemented"]
fn greek_final_sigma_case_fold() {
    assert_score_and_positions(
        "λόγοσ",
        "λόγος",
        SCORE_MATCH * 5
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 4,
        &[0, 1, 2, 3, 4],
    );
}

#[test]
fn surrogate_pair() {
    assert_score_and_positions(
        "N😀ewer",
        "N😀ewer tab",
        SCORE_MATCH * 6
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 5,
        &[0, 1, 2, 3, 4, 5],
    );
}

#[test]
fn surrogate_pairs_convert_to_utf8_positions_for_consecutive_chars() {
    assert_score_and_positions(
        "N𠀋N😀𝄞e𐐷",
        "N𠀋N😀𝄞e𐐷 tab",
        SCORE_MATCH * 7
            + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
            + BONUS_CONSECUTIVE * BONUS_FIRST_CHAR_MULTIPLIER * 6,
        &[0, 1, 2, 3, 4, 5, 6],
    );
}

#[test]
fn surrogate_pairs_prefer_consecutive_chars() {
    assert_score_and_positions(
        "𠀋😀",
        "N𠀋😀wer 😀b𐐷 ",
        SCORE_MATCH * 2 + BONUS_CONSECUTIVE * 2,
        &[1, 2],
    );
}

#[test]
fn surrogate_pairs_with_gap_and_boundary_use_utf8_positions() {
    assert_score_and_positions(
        "𠀋😀",
        "N𠀋wer 😀b𐐷 ",
        SCORE_MATCH * 2 + SCORE_GAP_START + SCORE_GAP_EXTENSION * 3 + BOUNDARY_BONUS,
        &[1, 6],
    );
}
