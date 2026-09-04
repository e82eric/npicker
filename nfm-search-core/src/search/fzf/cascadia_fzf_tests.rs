//! Non-Unicode score fixtures translated from TerminalAppUnitTests/FzfTests.cpp
//! in G:\src\terminal-retry. The C++ suite also checks highlighted runs; V4 is
//! currently score-only, so those assertions cannot be copied yet.

use super::*;

const MATCH: i32 = 16;
const GAP_START: i32 = -3;
const GAP_EXTENSION: i32 = -1;
const BOUNDARY: i32 = MATCH / 2;
const NON_WORD: i32 = MATCH / 2;
const CAMEL_123: i32 = BOUNDARY + GAP_EXTENSION;
const CONSECUTIVE: i32 = -(GAP_START + GAP_EXTENSION);
const FIRST: i32 = 2;

struct Case {
    name: &'static str,
    query: &'static str,
    text: &'static str,
    expected: i32,
}

fn score_term(term: &str, text: &str) -> Option<i32> {
    // Cascadia's cases are case-insensitive even when the query is uppercase.
    let term = term.to_ascii_lowercase();
    let pattern = AsciiPattern::parse(&term).expect("fixture query is ASCII");
    #[cfg(target_arch = "x86_64")]
    assert!(
        pattern.term_sets[0].terms[0].v4.is_some(),
        "fixture must exercise production V4"
    );
    pattern
        .score(text.as_bytes(), &mut MatchScratch::default())
        .map(|score| score as i32)
}

fn score_query(query: &str, text: &str) -> i32 {
    let mut total = 0;
    for term in query.split_ascii_whitespace() {
        let Some(score) = score_term(term, text) else {
            return 0;
        };
        total += score;
    }
    total
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "AllPatternCharsDoNotMatch",
            query: "fbb",
            text: "foo bar",
            expected: 0,
        },
        Case {
            name: "ConsecutiveChars",
            query: "oba",
            text: "foobar",
            expected: MATCH * 3 + CONSECUTIVE * 2,
        },
        Case {
            name: "ConsecutiveChars_FirstCharBonus",
            query: "foo",
            text: "foobar",
            expected: MATCH * 3 + BOUNDARY * FIRST + CONSECUTIVE * FIRST * 2,
        },
        Case {
            name: "NonWordBonusBoundary_ConsecutiveChars",
            query: "zshc",
            text: "/man1/zshcompctl.1",
            expected: MATCH * 4 + BOUNDARY * FIRST + FIRST * CONSECUTIVE * 3,
        },
        Case {
            name: "English_CaseMatch",
            query: "Newer",
            text: "Newer tab",
            expected: MATCH * 5 + BOUNDARY * FIRST + CONSECUTIVE * FIRST * 4,
        },
        Case {
            name: "English_CaseMisMatch",
            query: "newer",
            text: "Newer tab",
            expected: MATCH * 5 + BOUNDARY * FIRST + CONSECUTIVE * FIRST * 4,
        },
        Case {
            name: "MatchOnNonWordChars_CaseInSensitive",
            query: "foo-b",
            text: "xFoo-Bar Baz",
            expected: (MATCH + CAMEL_123 * FIRST)
                + (MATCH + CAMEL_123)
                + (MATCH + CAMEL_123)
                + (MATCH + BOUNDARY)
                + (MATCH + NON_WORD),
        },
        Case {
            name: "MatchOnNonWordCharsWithGap",
            query: "12356",
            text: "abc123 456",
            expected: (MATCH + CAMEL_123 * FIRST)
                + (MATCH + CAMEL_123)
                + (MATCH + CAMEL_123)
                + GAP_START
                + GAP_EXTENSION
                + MATCH
                + MATCH
                + CONSECUTIVE,
        },
        Case {
            name: "BonusForCamelCaseMatch",
            query: "def56",
            text: "abcDEF 456",
            expected: (MATCH + CAMEL_123 * FIRST)
                + (MATCH + CAMEL_123)
                + (MATCH + CAMEL_123)
                + GAP_START
                + GAP_EXTENSION
                + MATCH
                + MATCH
                + CONSECUTIVE,
        },
        Case {
            name: "BonusBoundaryAndFirstCharMultiplier",
            query: "fbb",
            text: "foo bar baz",
            expected: MATCH * 3
                + BOUNDARY * FIRST
                + BOUNDARY * 2
                + 2 * GAP_START
                + 4 * GAP_EXTENSION,
        },
        Case {
            name: "MatchesAreCaseInSensitive",
            query: "FBB",
            text: "foo bar baz",
            expected: MATCH * 3
                + BOUNDARY * FIRST
                + BOUNDARY * 2
                + 2 * GAP_START
                + 4 * GAP_EXTENSION,
        },
        Case {
            name: "MultipleTerms",
            query: "sp anta",
            text: "Split Pane, split: horizontal, profile: SSH: Antares",
            expected: (MATCH * 2 + BOUNDARY * FIRST + FIRST * CONSECUTIVE)
                + (MATCH * 4 + BOUNDARY * FIRST + (FIRST * CONSECUTIVE) * 3),
        },
        Case {
            name: "MultipleTerms_AllCharsMatch",
            query: "foo bar",
            text: "foo bar",
            expected: 2 * (MATCH * 3 + BOUNDARY * FIRST + FIRST * CONSECUTIVE * 2),
        },
        Case {
            name: "MultipleTerms_NotAllTermsMatch",
            query: "sp anta zz",
            text: "Split Pane, split: horizontal, profile: SSH: Antares",
            expected: 0,
        },
        Case {
            name: "MatchesAreCaseInSensitive_BonusBoundary",
            query: "fbb",
            text: "Foo Bar Baz",
            expected: MATCH * 3
                + BOUNDARY * FIRST
                + BOUNDARY * 2
                + 2 * GAP_START
                + 4 * GAP_EXTENSION,
        },
        Case {
            name: "TraceBackWillPickTheFirstMatchIfBothHaveTheSameScore",
            query: "bar",
            text: "Foo Bar Bar",
            expected: (MATCH + BOUNDARY * FIRST) + (MATCH + BOUNDARY) + (MATCH + BOUNDARY),
        },
        Case {
            name: "TraceBackWillPickTheMatchWithTheHighestScore",
            query: "bar",
            text: "Foo aBar Bar",
            expected: MATCH * 3 + BOUNDARY * FIRST * 2,
        },
        Case {
            name: "TraceBackWillPickTheMatchWithTheHighestScore_Gaps",
            query: "bar",
            text: "Boo Author Raz Bar",
            expected: MATCH * 3 + BOUNDARY * FIRST + CONSECUTIVE * FIRST * 2,
        },
        Case {
            name: "TraceBackWillPickEarlierCharsWhenNoBonus",
            query: "clts",
            text: "close all tabs after this",
            expected: MATCH * 4
                + BOUNDARY * FIRST
                + FIRST * CONSECUTIVE
                + GAP_START
                + GAP_EXTENSION * 7
                + BOUNDARY
                + GAP_START
                + GAP_EXTENSION,
        },
        Case {
            name: "Consecutive_NoBonus",
            query: "oob",
            text: "aoobar",
            expected: MATCH * 3 + CONSECUTIVE * 2,
        },
        Case {
            name: "Gapped_NoBonus",
            query: "oob",
            text: "aoaoabound",
            expected: MATCH * 3 + GAP_START * 2,
        },
        Case {
            name: "Consecutive_FirstCharBonus",
            query: "oob",
            text: "oobar",
            expected: MATCH * 3 + FIRST * BOUNDARY + FIRST * CONSECUTIVE * 2,
        },
        Case {
            name: "Gapped_FirstCharBonus",
            query: "oob",
            text: "oaoabound",
            expected: MATCH * 3 + BOUNDARY * FIRST + GAP_START * 2,
        },
        Case {
            name: "Consecutive_VersusBoundaryGap",
            query: "oob",
            text: "foobar",
            expected: MATCH * 3 + CONSECUTIVE * 2,
        },
        Case {
            name: "BoundaryGap_VersusConsecutive",
            query: "oob",
            text: "out-of-bound",
            expected: MATCH * 3
                + BOUNDARY * FIRST
                + BOUNDARY * 2
                + GAP_START
                + GAP_EXTENSION * 2
                + GAP_START
                + GAP_EXTENSION,
        },
        Case {
            name: "Consecutive_TwoChars",
            query: "ob",
            text: "aobar",
            expected: MATCH * 2 + CONSECUTIVE,
        },
        Case {
            name: "FirstCharBonusWithGap",
            query: "ob",
            text: "oabar",
            expected: MATCH * 2 + BOUNDARY * FIRST + GAP_START,
        },
        Case {
            name: "LongGap_TwoCharPattern",
            query: "ob",
            text: "oaaaaaaaaaaabar",
            expected: MATCH * 2 + BOUNDARY * FIRST + GAP_START + GAP_EXTENSION * 10,
        },
        Case {
            name: "LongGap_ThreeCharPattern",
            query: "oba",
            text: "oaaaaaaaaaaabar",
            expected: MATCH * 3 + BOUNDARY * FIRST + CONSECUTIVE + GAP_START + GAP_EXTENSION * 10,
        },
        Case {
            name: "FiveCharGap_NoConsecutive",
            query: "oba",
            text: "oaaabzzar",
            expected: MATCH * 3
                + BOUNDARY * FIRST
                + GAP_START
                + GAP_EXTENSION * 2
                + GAP_START
                + GAP_EXTENSION,
        },
        Case {
            name: "ThreeGaps_FourCharPattern",
            query: "obar",
            text: "oabzazr",
            expected: MATCH * 4 + BOUNDARY * FIRST + GAP_START * 3,
        },
    ]
}

fn assert_scores<'a>(cases: impl Iterator<Item = &'a Case>) {
    let failures = cases
        .filter_map(|case| {
            let actual = score_query(case.query, case.text);
            (actual != case.expected).then(|| {
                format!(
                    "{}: query={:?} text={:?}, expected={}, actual={}",
                    case.name, case.query, case.text, case.expected, actual
                )
            })
        })
        .collect::<Vec<_>>();

    assert!(
        failures.is_empty(),
        "V4 differs from the copied Cascadia FZF scores:\n{}",
        failures.join("\n")
    );
}

#[test]
fn v4_matches_all_non_unicode_cascadia_fzf_scores() {
    if !is_x86_feature_detected!("avx2") {
        return;
    }

    let cases = cases();
    assert_scores(cases.iter());
}


