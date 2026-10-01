//! Delivers a board decision (approve/reject/answer) into a real, live
//! `claude` CLI process by writing raw bytes straight to its controlling
//! terminal device (`SessionTtys`, captured by `hook-bridge` at hook time —
//! see its `detect_tty` doc comment). Used as the fallback when a session has
//! no embedded pty the board opened itself (`terminal::TerminalManager`,
//! tried first in `api::deliver_and_resolve`) — the common case for a
//! session running in an ordinary external terminal. There is no Claude Code
//! hook that lets a third party answer a prompt already on screen: hooks are
//! fire-and-forget observers (see `orchestrator.rs`'s notes on
//! `PermissionRequest`), so this is the only channel that reaches such a
//! session at all.
//!
//! Key sequences here are reverse-engineered against a real running Claude
//! Code CLI (2.1.x), not documented anywhere:
//! - A numbered menu (permission prompts, single-select `AskUserQuestion`
//!   options) selects **immediately** on the digit keypress — no trailing
//!   Enter, confirmed live (pressing "1" on a real "Do you want to proceed?"
//!   prompt ran the command right away).
//! - The list does not wrap: pressing Down past the last item cycles back to
//!   the first (confirmed live), so overshooting to "definitely land on the
//!   last item" is unsafe. Reject therefore uses Escape ("Esc to cancel",
//!   shown in every permission prompt's own footer) rather than trying to
//!   select a numbered "No" whose position varies by prompt (2, 3, or 4
//!   items depending on which "Yes, and…" variants are offered).
//! - Multi-select `AskUserQuestion` (checkboxes) is *not* live-verified —
//!   the toggle-then-Enter-to-confirm behavior below follows the standard
//!   convention for this style of terminal prompt, but hasn't been tested
//!   against a real multi-select question.

use std::fs::OpenOptions;
use std::io::Write;

use crate::engine::WaitingQuestion;

/// Opens `tty` (e.g. `/dev/ttys003`) and writes `bytes` to it directly — not
/// through a shell — so they land in the terminal's input stream exactly as
/// if typed there. Fails harmlessly (caller surfaces it as "couldn't reach
/// the session") if the device is gone, e.g. the terminal window was closed
/// since the tty was captured.
fn write_to_tty(tty: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = OpenOptions::new().write(true).open(tty)?;
    f.write_all(bytes)?;
    f.flush()
}

const ESC: u8 = 0x1b;
const CR: u8 = b'\r';

/// Selects a menu item by its 1-based position. Real Claude Code menus only
/// ever seem to need 1-9 (permission prompts top out around 4, and
/// `AskUserQuestion` options are capped much lower than 9 by the tool
/// itself), so the direct digit keypress covers every real case. Beyond 9,
/// falls back to arrow-key navigation from the top (Home isn't a safe bet in
/// raw terminal input, and the list is confirmed non-wrapping downward from
/// item 1) — unverified, since no real prompt has ever needed it.
fn select_index_bytes(index_1based: usize) -> Vec<u8> {
    if (1..=9).contains(&index_1based) {
        return index_1based.to_string().into_bytes();
    }
    let mut bytes = Vec::new();
    for _ in 0..index_1based.saturating_sub(1) {
        bytes.extend_from_slice(&[ESC, b'[', b'B']);
    }
    bytes.push(CR);
    bytes
}

/// Approve a permission prompt: "1. Yes" is always the first item — direct
/// digit keypress, confirmed live to run immediately.
pub fn approve_bytes() -> Vec<u8> {
    b"1".to_vec()
}

/// Reject a permission prompt via Escape ("Esc to cancel") rather than a
/// numbered "No" — see this module's doc comment for why the numbered
/// position isn't safe to guess.
pub fn reject_bytes() -> Vec<u8> {
    vec![ESC]
}

/// Types free text into whatever the terminal is currently reading a line
/// from, then submits it with Enter. Most reliable when the session is
/// genuinely sitting at a plain input prompt (not mid numbered-menu, which
/// ignores typed text) — used for the drawer's general "reply" action on a
/// plain wait, and for `AskUserQuestion`'s own free-text option (see
/// `answer_question_bytes`).
fn free_text_bytes(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(CR);
    bytes
}

pub fn reply_bytes(text: &str) -> Vec<u8> {
    free_text_bytes(text)
}

/// Builds the key sequence that answers a captured `AskUserQuestion`
/// (`question`) with either a set of chosen option labels (`selected`) or
/// free text (`free_text` — the tool's own "Type something." escape hatch,
/// always the implicit item right after the declared options).
///
/// - Free text takes priority when given: select the "type something" slot
///   (`options.len() + 1`), type the text, submit with Enter.
/// - Single-select: select the one matching option by its declared
///   position — immediate, like a permission prompt's numbered menu.
/// - Multi-select: toggle every matching option in turn, then confirm with
///   Enter (unverified against a real multi-select prompt — see this
///   module's doc comment).
///
/// A `selected` label that doesn't match any of `question.options` is
/// silently skipped rather than guessing — better to under-deliver than to
/// select the wrong option.
pub fn answer_question_bytes(question: &WaitingQuestion, selected: &[String], free_text: Option<&str>) -> Vec<u8> {
    if let Some(text) = free_text {
        let mut bytes = select_index_bytes(question.options.len() + 1);
        bytes.extend(free_text_bytes(text));
        return bytes;
    }

    let mut bytes = Vec::new();
    for label in selected {
        if let Some(pos) = question.options.iter().position(|o| &o.label == label) {
            bytes.extend(select_index_bytes(pos + 1));
        }
    }
    if question.multi_select {
        bytes.push(CR);
    }
    bytes
}

/// Writes `bytes` to `tty`. Thin wrapper kept separate from the byte-sequence
/// builders above so `api.rs` can unit-test sequence construction without
/// touching a real device, while still having one obvious place that
/// performs the actual I/O.
pub fn deliver(tty: &str, bytes: &[u8]) -> std::io::Result<()> {
    write_to_tty(tty, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::WaitingOption;

    #[test]
    fn approve_is_a_bare_digit_one() {
        assert_eq!(approve_bytes(), b"1");
    }

    #[test]
    fn reject_is_escape() {
        assert_eq!(reject_bytes(), vec![ESC]);
    }

    fn animal_question(multi_select: bool) -> WaitingQuestion {
        WaitingQuestion {
            question: "Which animal do you pick?".into(),
            header: Some("Animal".into()),
            multi_select,
            options: vec![
                WaitingOption { label: "Dog".into(), description: Some("Loyal and friendly".into()) },
                WaitingOption { label: "Cat".into(), description: Some("Independent and curious".into()) },
                WaitingOption { label: "Owl".into(), description: Some("Wise night hunter".into()) },
                WaitingOption { label: "Octopus".into(), description: Some("Clever eight-armed swimmer".into()) },
            ],
        }
    }

    #[test]
    fn single_select_picks_the_matching_option_by_position() {
        let q = animal_question(false);
        assert_eq!(answer_question_bytes(&q, &["Cat".into()], None), b"2");
        assert_eq!(answer_question_bytes(&q, &["Octopus".into()], None), b"4");
    }

    #[test]
    fn free_text_selects_the_slot_after_the_last_option_then_types_and_submits() {
        let q = animal_question(false);
        let bytes = answer_question_bytes(&q, &[], Some("Parrot"));
        // 4 options -> "type something" is item 5.
        assert_eq!(bytes, b"5Parrot\r");
    }

    #[test]
    fn multi_select_toggles_each_choice_then_confirms_with_enter() {
        let q = animal_question(true);
        let bytes = answer_question_bytes(&q, &["Dog".into(), "Owl".into()], None);
        assert_eq!(bytes, b"13\r");
    }

    #[test]
    fn an_unknown_label_is_skipped_rather_than_guessed() {
        let q = animal_question(false);
        assert_eq!(answer_question_bytes(&q, &["Dragon".into()], None), b"");
    }
}
