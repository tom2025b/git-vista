//! Key dispatch (M10.02, #457 — phase 2a): one crossterm [`KeyEvent`] in,
//! at most one [`Action`] out. Pure and host-tested; the event loop calls it
//! and never interprets a key itself.
//!
//! # The bindings
//!
//! | Key | Action |
//! |---|---|
//! | `q`, `Ctrl-C` | quit |
//! | `Tab`, `l`, `→` | focus the next pane |
//! | `BackTab`, `h`, `←` | focus the previous pane |
//! | `1`–`4` | focus that pane |
//! | `j`, `↓` | cursor down |
//! | `k`, `↑` | cursor up |
//! | `r`, `F5` | refresh |
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

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{Action, Pane};

/// Translate one key event. `None` means the key is unbound.
pub fn dispatch(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => Some(Action::Quit),
        _ if !plain => None,
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => Some(Action::FocusNext),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => Some(Action::FocusPrev),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::CursorDown),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::CursorUp),
        KeyCode::F(5) | KeyCode::Char('r') => Some(Action::Refresh),
        KeyCode::Char(d @ '1'..='9') => Pane::from_number(d.to_digit(10)? as u8).map(Action::Focus),
        _ => None,
    }
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

    #[test]
    fn q_and_ctrl_c_quit() {
        assert_eq!(dispatch(press(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(dispatch(ctrl('c')), Some(Action::Quit));
    }

    #[test]
    fn tab_and_back_tab_move_focus_and_so_do_h_l_and_the_side_arrows() {
        for key in [
            press(KeyCode::Tab),
            press(KeyCode::Char('l')),
            press(KeyCode::Right),
        ] {
            assert_eq!(dispatch(key), Some(Action::FocusNext), "{key:?}");
        }
        for key in [
            press(KeyCode::BackTab),
            press(KeyCode::Char('h')),
            press(KeyCode::Left),
        ] {
            assert_eq!(dispatch(key), Some(Action::FocusPrev), "{key:?}");
        }
    }

    #[test]
    fn digits_one_to_four_focus_that_pane_and_other_digits_do_nothing() {
        assert_eq!(
            dispatch(press(KeyCode::Char('1'))),
            Some(Action::Focus(Pane::Repositories))
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('2'))),
            Some(Action::Focus(Pane::Branches))
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('3'))),
            Some(Action::Focus(Pane::Commits))
        );
        assert_eq!(
            dispatch(press(KeyCode::Char('4'))),
            Some(Action::Focus(Pane::Main))
        );
        assert_eq!(dispatch(press(KeyCode::Char('0'))), None);
        assert_eq!(dispatch(press(KeyCode::Char('5'))), None);
        assert_eq!(dispatch(press(KeyCode::Char('9'))), None);
    }

    #[test]
    fn j_k_and_the_vertical_arrows_move_the_cursor() {
        for key in [press(KeyCode::Char('j')), press(KeyCode::Down)] {
            assert_eq!(dispatch(key), Some(Action::CursorDown), "{key:?}");
        }
        for key in [press(KeyCode::Char('k')), press(KeyCode::Up)] {
            assert_eq!(dispatch(key), Some(Action::CursorUp), "{key:?}");
        }
    }

    #[test]
    fn r_and_f5_refresh() {
        assert_eq!(dispatch(press(KeyCode::Char('r'))), Some(Action::Refresh));
        assert_eq!(dispatch(press(KeyCode::F(5))), Some(Action::Refresh));
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
            dispatch(release),
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
            dispatch(repeat),
            Some(Action::CursorDown),
            "a held key keeps scrolling"
        );
    }

    #[test]
    fn a_modified_letter_is_not_its_plain_binding() {
        // `Ctrl-q`, `Alt-j`: unbound, so a chord meant for the terminal or
        // the multiplexer never leaks into the app as the bare letter.
        assert_eq!(dispatch(ctrl('q')), None);
        assert_eq!(
            dispatch(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
            None
        );
    }

    #[test]
    fn an_unbound_key_is_none() {
        for key in [
            press(KeyCode::Char('x')),
            press(KeyCode::Enter),
            press(KeyCode::Esc),
            press(KeyCode::Char(' ')),
            press(KeyCode::F(1)),
        ] {
            assert_eq!(dispatch(key), None, "{key:?}");
        }
    }
}
