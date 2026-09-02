//! The shell event loop and its input seam (M10.02, #457 — phase 2a).
//!
//! [`run`] is generic over both the terminal backend and the two sources of
//! effects: [`Inputs`] and [`DataPort`]. Tests therefore drive the exact loop
//! with finite scripts and a TestBackend—no raw terminal, socket, sleep, or
//! global state. A failed read is ordinary [`crate::app::Data`], folded into
//! the status line; only terminal/input failures end the loop with an error.

use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::app::App;
use crate::data::DataPort;
use crate::{keys, ui};

pub const TICK: Duration = Duration::from_millis(50);

pub enum Input {
    Key(KeyEvent),
    Resize,
    Tick,
}

pub trait Inputs {
    fn next(&mut self, timeout: Duration) -> Result<Input, String>;
}

pub struct CrosstermInputs;

impl Inputs for CrosstermInputs {
    fn next(&mut self, timeout: Duration) -> Result<Input, String> {
        if !event::poll(timeout).map_err(|error| error.to_string())? {
            return Ok(Input::Tick);
        }
        match event::read().map_err(|error| error.to_string())? {
            Event::Key(key) => Ok(Input::Key(key)),
            Event::Resize(_, _) => Ok(Input::Resize),
            _ => Ok(Input::Tick),
        }
    }
}

pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    inputs: &mut dyn Inputs,
    port: &mut dyn DataPort,
) -> Result<(), String> {
    for fetch in app.start() {
        port.request(fetch);
    }
    loop {
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map_err(|error| error.to_string())?;
        if app.quit {
            break;
        }
        match inputs.next(TICK)? {
            Input::Key(key) => {
                let action = if app.command_input.is_some() {
                    keys::dispatch_command(key)
                } else {
                    keys::dispatch(key, app.focus)
                };
                if let Some(action) = action {
                    for fetch in app.apply(action) {
                        port.request(fetch);
                    }
                }
            }
            Input::Tick => {
                for fetch in app.tick() {
                    port.request(fetch);
                }
            }
            Input::Resize => {}
        }
        while let Some(data) = port.poll() {
            app.receive(data);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as SizeCell;
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::rc::Rc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use git_vista_protocol::RepositoryDescriptor;
    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::layout::{Position, Size};

    use super::*;
    use crate::app::{Data, Fetch, Pane};

    const THREE: &str = r#"[
      {"repository":"r1","worktree":"w1","name":"alpha","kind":"main_worktree","read_only":false},
      {"repository":"r2","worktree":"w2","name":"beta","kind":"bare","read_only":true},
      {"repository":"r3","worktree":"w3","name":"gamma","kind":"linked_worktree","read_only":false}
    ]"#;

    fn catalog(wire: &str) -> Vec<RepositoryDescriptor> {
        serde_json::from_str(wire).expect("the wire literal is valid")
    }

    fn key(c: char) -> Input {
        Input::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    type ResizeSignal = (Rc<SizeCell<(u16, u16)>>, (u16, u16));

    struct Script {
        events: VecDeque<Result<Input, String>>,
        resize: Option<ResizeSignal>,
    }

    impl Script {
        fn new(events: impl IntoIterator<Item = Result<Input, String>>) -> Script {
            Script {
                events: events.into_iter().collect(),
                resize: None,
            }
        }

        fn resizing(
            events: impl IntoIterator<Item = Result<Input, String>>,
            size: Rc<SizeCell<(u16, u16)>>,
            target: (u16, u16),
        ) -> Script {
            Script {
                events: events.into_iter().collect(),
                resize: Some((size, target)),
            }
        }
    }

    impl Inputs for Script {
        fn next(&mut self, _timeout: Duration) -> Result<Input, String> {
            let event = self
                .events
                .pop_front()
                .unwrap_or_else(|| Err(String::from("script exhausted before the app quit")))?;
            if matches!(event, Input::Resize) {
                if let Some((size, target)) = &self.resize {
                    size.set(*target);
                }
            }
            Ok(event)
        }
    }

    #[derive(Default)]
    struct FakePort {
        seen: Vec<Fetch>,
        answers: VecDeque<Data>,
    }

    impl DataPort for FakePort {
        fn request(&mut self, fetch: Fetch) {
            self.seen.push(fetch);
        }

        fn poll(&mut self) -> Option<Data> {
            self.answers.pop_front()
        }
    }

    fn terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(80, 24)).expect("a test terminal")
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let mut out = String::new();
        for y in buffer.area.y..buffer.area.bottom() {
            for x in buffer.area.x..buffer.area.right() {
                out.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn start_requests_the_catalog_and_q_ends_the_loop_cleanly() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Ok(key('q'))]);
        let mut port = FakePort::default();

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        assert_eq!(port.seen, [Fetch::Catalog]);
        assert!(app.quit);
    }

    #[test]
    fn a_failed_read_shows_on_the_status_line_and_the_loop_keeps_running() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Ok(Input::Tick), Ok(key('q'))]);
        let mut port = FakePort {
            seen: Vec::new(),
            answers: VecDeque::from([Data::Catalog(Err(String::from(
                "GET /api/catalog answered 503: catalog rebuilding",
            )))]),
        };

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        assert!(app.quit, "the q after the failed read was still dispatched");
        assert!(buffer_text(terminal.backend().buffer()).contains("503"));
    }

    #[test]
    fn an_answer_is_folded_in_before_the_next_key_is_dispatched() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Ok(Input::Tick), Ok(key('j')), Ok(key('q'))]);
        let mut port = FakePort {
            seen: Vec::new(),
            answers: VecDeque::from([Data::Catalog(Ok(catalog(THREE)))]),
        };

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        assert_eq!(app.cursor(Pane::Repositories), 1);
        assert!(buffer_text(terminal.backend().buffer()).contains("beta"));
    }

    #[test]
    fn r_asks_the_port_again_once_the_first_answer_is_in() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Ok(Input::Tick), Ok(key('r')), Ok(key('q'))]);
        let mut port = FakePort {
            seen: Vec::new(),
            answers: VecDeque::from([Data::Catalog(Ok(Vec::new()))]),
        };

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        assert_eq!(port.seen, [Fetch::Catalog, Fetch::Catalog]);
    }

    #[test]
    fn an_input_failure_ends_the_loop_with_its_message_not_a_panic() {
        let mut terminal = terminal();
        let mut app = App::new();
        let mut script = Script::new([Err(String::from("the input stream vanished"))]);
        let mut port = FakePort::default();

        let error = run(&mut terminal, &mut app, &mut script, &mut port).unwrap_err();

        assert_eq!(error, "the input stream vanished");
        assert_eq!(port.seen, [Fetch::Catalog]);
    }

    struct ResizeBackend {
        inner: TestBackend,
        size: Rc<SizeCell<(u16, u16)>>,
    }

    impl Backend for ResizeBackend {
        type Error = Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.inner.draw(content)
        }

        fn append_lines(&mut self, count: u16) -> Result<(), Self::Error> {
            self.inner.append_lines(count)
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> Result<Size, Self::Error> {
            self.inner.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            let (width, height) = self.size.get();
            if self.inner.size()? != Size::new(width, height) {
                self.inner.resize(width, height);
            }
            self.inner.flush()
        }
    }

    #[test]
    fn a_resize_is_drawn_at_the_new_size() {
        let size = Rc::new(SizeCell::new((80, 24)));
        let backend = ResizeBackend {
            inner: TestBackend::new(80, 24),
            size: Rc::clone(&size),
        };
        let mut terminal = Terminal::new(backend).expect("a resizing test terminal");
        let mut app = App::new();
        let mut script =
            Script::resizing([Ok(Input::Resize), Ok(key('q'))], Rc::clone(&size), (30, 8));
        let mut port = FakePort::default();

        run(&mut terminal, &mut app, &mut script, &mut port).unwrap();

        let buffer = terminal.backend().inner.buffer();
        assert_eq!(buffer.area.as_size(), Size::new(30, 8));
        assert!(buffer_text(buffer).contains("40x10"));
    }
}
