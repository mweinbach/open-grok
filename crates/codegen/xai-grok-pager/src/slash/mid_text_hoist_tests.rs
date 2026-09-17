use pretty_assertions::assert_eq;

use super::hoist_mid_text_command;
use crate::slash::commands;
use crate::slash::registry::CommandRegistry;

fn hoist(text: &str) -> Option<String> {
    hoist_mid_text_command(text, &CommandRegistry::new(commands::builtin_commands()))
}

#[test]
fn hoists_first_btw_token_and_collapses_gap() {
    assert_eq!(
        Some("/btw explain the controller. what is a WBC".to_owned()),
        hoist("explain the controller. /btw what is a WBC")
    );
    assert_eq!(Some("/btw prose. q".to_owned()), hoist("prose. /btw   q"));
}

#[test]
fn token_at_end_trims_trailing_space() {
    assert_eq!(Some("/btw prose".to_owned()), hoist("prose /btw"));
    assert_eq!(Some("/btw prose".to_owned()), hoist("prose /btw   "));
}

#[test]
fn preserves_newline_before_token() {
    assert_eq!(
        Some("/btw line1\nq\nline2".to_owned()),
        hoist("line1\n/btw q\nline2")
    );
}

#[test]
fn second_token_stays_verbatim() {
    assert_eq!(Some("/btw a b /btw c".to_owned()), hoist("a /btw b /btw c"));
}

/// The caller keeps real leading invocations out; `[Image #1] /btw q` arrives here as ` /btw q`.
#[test]
fn leading_token_after_stripped_chip_hoists_in_place() {
    assert_eq!(Some("/btw q".to_owned()), hoist(" /btw q"));
    assert_eq!(Some("/btw q".to_owned()), hoist("/btw q"));
    assert_eq!(Some("/btw /nope hi q".to_owned()), hoist("/nope hi /btw q"));
}

#[test]
fn empty_question_is_none() {
    for text in ["/btw", " /btw", "/btw   "] {
        assert_eq!(None, hoist(text), "{text:?}");
    }
}

#[test]
fn punctuated_or_uppercase_token_misses() {
    for text in ["hi /btw, q", "hi /btw: q", "hi /BTW q"] {
        assert_eq!(None, hoist(text), "{text:?}");
    }
}

#[test]
fn path_and_url_slashes_miss() {
    for text in ["see docs/btw", "open https://x.ai/btw now"] {
        assert_eq!(None, hoist(text), "{text:?}");
    }
}

#[test]
fn non_opted_in_builtin_misses() {
    assert_eq!(None, hoist("great /compact go"));
}

#[test]
fn unknown_command_misses() {
    assert_eq!(None, hoist("hi /nope q"));
}
