//! Key dispatch (M10.02, #457 — phase 2a): one crossterm [`KeyEvent`] in,
//! at most one [`Action`] out. Pure and host-tested; the event loop calls it
//! and never interprets a key itself.
//!
//! # The bindings
//!
//! | Key | Action |
//! |---|---|
//! | `q`, `Ctrl-C` | quit |
//! | `Tab`, `l` | focus the next pane |
//! | `BackTab`, `h` | focus the previous pane |
//! | `←` / `→` | focus outside Main; horizontal scroll in Main |
//! | `1`–`4` | focus that pane |
//! | `j`, `↓` | cursor down |
//! | `k`, `↑` | cursor up |
//! | `Enter` | load a repository, open a commit, or follow a parent |
//! | `[` / `]` | select the previous/next parent in Main |
//! | `r`, `F5` | refresh |
//! | `x` | open the conflict overlay (M10.07, #462) |
//!
//! The vi-shaped `hjkl` set is lazygit's, and lazygit is the interface this
//! milestone is modelled on; the arrow and Tab set is for everyone else.
//!
//! # Only a press counts
//!
//! Terminals that report key *releases* (Windows, and any terminal with the
//! kitty keyboard protocol on) deliver two events per keystroke. Dispatch
//! ignores `Release` and `Repeat` is treated as a press — so `q` quits once,
//! not twice, and a held `j` still scrolls.
//!
//! # Where per-pane keys will go
//!
//! Phase 2a's bindings are the same in every pane. The first pane-specific
//! key (#459's `s` to stage, say) adds a layer here that consults the
//! focused pane; the signature grows a `Pane` when that key exists, not
//! before.
//!
//! # The conflict overlay has its own keymap, and that is not a style choice
//!
//! [`dispatch_conflict`] is a second table rather than more arms in
//! [`dispatch`], because the overlay's editor accepts **every printable
//! character** as text. Under one shared table, typing `q` into a file you
//! were resolving would quit the program and throw the edit away, and `j`
//! would scroll instead of appearing in the line. So while the overlay is up
//! it owns the keyboard, and [`KeyMode::Insert`] is the mode where the
//! ordinary meanings of letters are suspended entirely.
//!
//! `x` opens it. `c` was the obvious mnemonic and is deliberately left alone:
//! the working-tree slice (#459) is the natural owner of a commit key, and
//! taking it here would mean renaming a binding somebody had already learned.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use git_vista_conflicts::core::Pane as ConflictView;
use git_vista_conflicts::markers::Choice;
use git_vista_protocol::conflict::Resolution;

use crate::app::{Action, Pane};
use crate::panes::conflicts::{Act, KeyMode};

/// Translate one key event. `None` means the key is unbound.
pub fn dispatch(key: KeyEvent, pane: Pane) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => Some(Action::Quit),
        _ if !plain => None,
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Tab | KeyCode::Char('l') => Some(Action::FocusNext),
        KeyCode::BackTab | KeyCode::Char('h') => Some(Action::FocusPrev),
        KeyCode::Right if pane == Pane::Main => Some(Action::HorizontalRight),
        KeyCode::Left if pane == Pane::Main => Some(Action::HorizontalLeft),
        KeyCode::Right => Some(Action::FocusNext),
        KeyCode::Left => Some(Action::FocusPrev),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::CursorDown),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::CursorUp),
        KeyCode::Enter if pane != Pane::Branches => Some(Action::Activate),
        KeyCode::Char('[') if pane == Pane::Main => Some(Action::ParentPrev),
        KeyCode::Char(']') if pane == Pane::Main => Some(Action::ParentNext),
        KeyCode::F(5) | KeyCode::Char('r') => Some(Action::Refresh),
        KeyCode::Char('x') => Some(Action::OpenConflicts),
        KeyCode::Char(d @ '1'..='9') => Pane::from_number(d.to_digit(10)? as u8).map(Action::Focus),
        _ => None,
    }
}

/// Translate one key event for the conflict overlay. `None` means unbound.
///
/// `Ctrl-C` still quits from every mode including [`KeyMode::Insert`] — a
/// terminal program that cannot be interrupted is a terminal program somebody
/// has to kill from another window. Nothing else survives insert mode.
pub fn dispatch_conflict(key: KeyEvent, mode: KeyMode) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return Some(Action::Quit);
    }
    if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
        return None;
    }

    // Insert mode first and on its own, so no later arm can claim a character
    // out from under the buffer. Every printable key is text here.
    if mode == KeyMode::Insert {
        return Some(Action::Conflict(match key.code {
            KeyCode::Esc => Act::EndEdit,
            KeyCode::Enter => Act::Newline,
            KeyCode::Backspace => Act::Backspace,
            KeyCode::Left => Act::CaretLeft,
            KeyCode::Right => Act::CaretRight,
            KeyCode::Up => Act::Up,
            KeyCode::Down => Act::Down,
            KeyCode::Char(ch) => Act::Type(ch),
            KeyCode::Tab => Act::Type('\t'),
            _ => return None,
        }));
    }

    let act = match (mode, key.code) {
        (_, KeyCode::Esc) => Act::Back,
        (_, KeyCode::Char('q')) => Act::Close,
        (_, KeyCode::Down | KeyCode::Char('j')) => Act::Down,
        (_, KeyCode::Up | KeyCode::Char('k')) => Act::Up,

        (KeyMode::List, KeyCode::Enter) => Act::Open,
        (KeyMode::List, KeyCode::Char('r') | KeyCode::F(5)) => Act::Refresh,

        (KeyMode::Inspect, KeyCode::Tab) => Act::NextPane,
        (KeyMode::Inspect, KeyCode::Char(d @ '1'..='4')) => {
            let index = d.to_digit(10)? as usize - 1;
            Act::FocusPane(*ConflictView::ALL.get(index)?)
        }
        (KeyMode::Inspect, KeyCode::Char('o')) => Act::Take(Resolution::TakeOurs),
        (KeyMode::Inspect, KeyCode::Char('t')) => Act::Take(Resolution::TakeTheirs),
        (KeyMode::Inspect, KeyCode::Char('d')) => Act::Take(Resolution::TakeDeletion),
        (KeyMode::Inspect, KeyCode::Char('e')) => Act::OpenEditor,

        (KeyMode::Editor, KeyCode::Char('o')) => Act::Choose(Choice::Ours),
        (KeyMode::Editor, KeyCode::Char('t')) => Act::Choose(Choice::Theirs),
        (KeyMode::Editor, KeyCode::Char('b')) => Act::Choose(Choice::Both),
        (KeyMode::Editor, KeyCode::Char('i')) => Act::BeginEdit,
        (KeyMode::Editor, KeyCode::Enter) => Act::Apply,

        _ => return None,
    };
    Some(Action::Conflict(act))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Pane;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn global(key: KeyEvent) -> Option<Action> {
        dispatch(key, Pane::Repositories)
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        assert_eq!(global(press(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(global(ctrl('c')), Some(Action::Quit));
    }

    #[test]
    fn tab_and_back_tab_move_focus_and_so_do_h_l_and_the_side_arrows() {
        for key in [
            press(KeyCode::Tab),
            press(KeyCode::Char('l')),
            press(KeyCode::Right),
        ] {
            assert_eq!(global(key), Some(Action::FocusNext), "{key:?}");
        }
        for key in [
            press(KeyCode::BackTab),
            press(KeyCode::Char('h')),
            press(KeyCode::Left),
        ] {
            assert_eq!(global(key), Some(Action::FocusPrev), "{key:?}");
        }
    }

    #[test]
    fn digits_one_to_four_focus_that_pane_and_other_digits_do_nothing() {
        assert_eq!(
            global(press(KeyCode::Char('1'))),
            Some(Action::Focus(Pane::Repositories))
        );
        assert_eq!(
            global(press(KeyCode::Char('2'))),
            Some(Action::Focus(Pane::Branches))
        );
        assert_eq!(
            global(press(KeyCode::Char('3'))),
            Some(Action::Focus(Pane::Commits))
        );
        assert_eq!(
            global(press(KeyCode::Char('4'))),
            Some(Action::Focus(Pane::Main))
        );
        assert_eq!(global(press(KeyCode::Char('0'))), None);
        assert_eq!(global(press(KeyCode::Char('5'))), None);
        assert_eq!(global(press(KeyCode::Char('9'))), None);
    }

    #[test]
    fn j_k_and_the_vertical_arrows_move_the_cursor() {
        for key in [press(KeyCode::Char('j')), press(KeyCode::Down)] {
            assert_eq!(global(key), Some(Action::CursorDown), "{key:?}");
        }
        for key in [press(KeyCode::Char('k')), press(KeyCode::Up)] {
            assert_eq!(global(key), Some(Action::CursorUp), "{key:?}");
        }
    }

    #[test]
    fn r_and_f5_refresh() {
        assert_eq!(global(press(KeyCode::Char('r'))), Some(Action::Refresh));
        assert_eq!(global(press(KeyCode::F(5))), Some(Action::Refresh));
    }

    #[test]
    fn a_key_release_is_ignored_even_for_q_but_a_repeat_still_counts() {
        let release = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(
            global(release),
            None,
            "a release after the press must not quit twice"
        );
        let repeat = KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        };
        assert_eq!(
            global(repeat),
            Some(Action::CursorDown),
            "a held key keeps scrolling"
        );
    }

    #[test]
    fn a_modified_letter_is_not_its_plain_binding() {
        // `Ctrl-q`, `Alt-j`: unbound, so a chord meant for the terminal or
        // the multiplexer never leaks into the app as the bare letter.
        assert_eq!(global(ctrl('q')), None);
        assert_eq!(
            global(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
            None
        );
    }

    #[test]
    fn an_unbound_key_is_none() {
        // `x` used to be in this list and is now the conflict overlay
        // (M10.07, #462) — this test is what noticed, which is the whole point
        // of keeping an explicit unbound set rather than trusting the match's
        // fall-through.
        for key in [
            press(KeyCode::Char('z')),
            press(KeyCode::Esc),
            press(KeyCode::Char(' ')),
            press(KeyCode::F(1)),
        ] {
            assert_eq!(global(key), None, "{key:?}");
        }
    }

    #[test]
    fn x_opens_the_conflict_overlay_from_every_pane() {
        for pane in Pane::ALL {
            assert_eq!(
                dispatch(press(KeyCode::Char('x')), pane),
                Some(Action::OpenConflicts),
                "{pane:?}"
            );
        }
    }

    // ---- the overlay's own keymap (M10.07, #462) ------------------------

    #[test]
    fn insert_mode_types_the_letters_that_are_commands_everywhere_else() {
        // The reason `dispatch_conflict` is a second table at all. Under one
        // shared keymap, typing `q` into a file you were resolving would quit
        // the program and lose the edit.
        //
        // MUTATION: move the insert-mode block below the shared `q`/`j`/`k`
        // arms. Every other key test still passes and this one fails.
        for (ch, _) in [('q', ()), ('j', ()), ('k', ()), ('o', ()), ('e', ()), ('i', ())] {
            assert_eq!(
                dispatch_conflict(press(KeyCode::Char(ch)), KeyMode::Insert),
                Some(Action::Conflict(Act::Type(ch))),
                "{ch} was claimed as a command inside the text buffer"
            );
        }
        assert_eq!(
            dispatch_conflict(press(KeyCode::Enter), KeyMode::Insert),
            Some(Action::Conflict(Act::Newline))
        );
        assert_eq!(
            dispatch_conflict(press(KeyCode::Esc), KeyMode::Insert),
            Some(Action::Conflict(Act::EndEdit))
        );
    }

    #[test]
    fn ctrl_c_still_quits_from_inside_the_text_buffer() {
        // A terminal program you cannot interrupt is one somebody has to kill
        // from another window.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for mode in [
            KeyMode::List,
            KeyMode::Inspect,
            KeyMode::Editor,
            KeyMode::Insert,
        ] {
            assert_eq!(
                dispatch_conflict(ctrl_c, mode),
                Some(Action::Quit),
                "{mode:?}"
            );
        }
        // …and `c` on its own is a character there, not a quit.
        assert_eq!(
            dispatch_conflict(press(KeyCode::Char('c')), KeyMode::Insert),
            Some(Action::Conflict(Act::Type('c')))
        );
    }

    #[test]
    fn the_resolution_keys_are_live_only_on_the_screen_that_offers_them() {
        assert_eq!(
            dispatch_conflict(press(KeyCode::Char('o')), KeyMode::Inspect),
            Some(Action::Conflict(Act::Take(Resolution::TakeOurs)))
        );
        assert_eq!(
            dispatch_conflict(press(KeyCode::Char('d')), KeyMode::Inspect),
            Some(Action::Conflict(Act::Take(Resolution::TakeDeletion)))
        );
        // The same letter is a BLOCK choice one screen further in, and must
        // never reach `Take` there — that would resolve a whole file while the
        // user was picking one hunk of it.
        assert_eq!(
            dispatch_conflict(press(KeyCode::Char('o')), KeyMode::Editor),
            Some(Action::Conflict(Act::Choose(Choice::Ours)))
        );
        assert_eq!(
            dispatch_conflict(press(KeyCode::Char('d')), KeyMode::Editor),
            None
        );
        assert_eq!(
            dispatch_conflict(press(KeyCode::Char('o')), KeyMode::List),
            None
        );
    }

    #[test]
    fn a_key_release_is_ignored_in_the_overlay_too() {
        // Terminals with the kitty protocol on deliver two events per
        // keystroke; in the text buffer that would double every character.
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(dispatch_conflict(release, KeyMode::Insert), None);
        assert_eq!(dispatch_conflict(release, KeyMode::List), None);
    }

    #[test]
    fn enter_activates_rows_that_open_something_but_not_the_branches_placeholder() {
        let enter = press(KeyCode::Enter);
        for pane in [Pane::Repositories, Pane::Commits, Pane::Main] {
            assert_eq!(dispatch(enter, pane), Some(Action::Activate), "{pane:?}");
        }
        assert_eq!(dispatch(enter, Pane::Branches), None);
    }

    #[test]
    fn main_arrows_scroll_horizontally_while_tab_and_h_l_still_move_focus() {
        assert_eq!(
            dispatch(press(KeyCode::Left), Pane::Main),
            Some(Action::HorizontalLeft)
        );
        assert_eq!(
            dispatch(press(KeyCode::Right), Pane::Main),
            Some(Action::HorizontalRight)
        );
        assert_eq!(
            dispatch(press(KeyCode::Left), Pane::Commits),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('h')), Pane::Main),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('l')), Pane::Main),
            Some(Action::FocusNext)
        );
    }

    #[test]
    fn brackets_select_parents_only_in_the_main_pane() {
        assert_eq!(
            dispatch(press(KeyCode::Char('[')), Pane::Main),
            Some(Action::ParentPrev)
        );
        assert_eq!(
            dispatch(press(KeyCode::Char(']')), Pane::Main),
            Some(Action::ParentNext)
        );
        assert_eq!(dispatch(press(KeyCode::Char('[')), Pane::Commits), None);
        assert_eq!(
            dispatch(press(KeyCode::Char(']')), Pane::Repositories),
            None
        );
    }
}
