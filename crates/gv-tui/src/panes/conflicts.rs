//! Conflict inspection and resolution in the terminal (M10.07, #462).
//!
//! Pure. No ratatui, no socket, no thread: [`ConflictsPane`] holds what the
//! overlay shows, [`ConflictsPane::apply`] folds one [`Act`] in and returns
//! the [`Request`]s the shell must dispatch, and [`ConflictsPane::receive_*`]
//! fold answers back. Every rule below is therefore an ordinary `#[test]`
//! with nothing but a struct in sight — the same placement argument
//! `git-vista-conflicts` makes for the model this module draws.
//!
//! # It draws the M4.31 model; it does not re-decide anything
//!
//! #462's governing sentence is "using the same model M4.31 shipped — not a
//! second conflict implementation", so every judgement below is delegated:
//!
//! | question | who answers |
//! |---|---|
//! | what state is this pane in | [`PaneState::for_stage`], [`PaneState::with_content`], [`result_pane_state`] |
//! | what does that state say to a user | [`PaneState::describe`] |
//! | what is this pane called | [`View::label`] |
//! | may `Take ours` be offered, and if not why | [`ResolutionSurface::take_ours`] and [`Withheld::describe`] |
//! | what shape of conflict is this | [`ResolutionSurface::note`] |
//! | may a line-level resolver open at all | [`ResolutionSurface::text_resolution_allowed`] |
//! | what blocks does the marker file hold | [`markers::parse`] |
//! | what content does a set of choices produce | [`markers::compose`] |
//!
//! Nothing in this file recomputes any of those. In particular the eligibility
//! predicate is **read**, never re-derived: `text_resolution_allowed` traces
//! back to `ConflictedFile::text_resolvable`, the same call the server makes
//! before executing a content resolution. #430 shipped a wrong sentence
//! because that rule had two implementations; there is still one.
//!
//! # Why the four panes are a summary strip plus one body, not a 2×2 grid
//!
//! The frame's floor is 40 columns ([`crate::layout::MIN_WIDTH`]). Quartered,
//! that is 20 columns of source per pane — narrow enough that every line of
//! every version is truncated, and the user is reading the *shape* of four
//! boxes rather than their contents. That is the same failure as a pane that
//! draws an empty box: it looks like inspection and is not.
//!
//! So all four panes are always **stated** — one row each, carrying that
//! pane's own [`PaneState::describe`] sentence, so "there is no ancestor" is
//! on screen the whole time — and the focused one is shown full width.
//! `Tab` and `1`–`4` move between them. Every pane stays reachable, which is
//! what #428's first criterion asks, and none of them is ever a blank box.
//!
//! # An absent thing says so, everywhere, including inside the editor
//!
//! A `Block::Conflict` whose `base` is `None` is a merge-style marker file
//! with no recorded ancestor. It renders as that sentence, never as an empty
//! ancestor section — the same distinction ADR 0063 draws for stages, applied
//! one layer in.

use git_vista_conflicts::core::{
    result_pane_state, ConflictPanes, Pane as View, PaneState, ResolutionSurface, ResultRead,
};
use git_vista_conflicts::markers::{self, Block, Choice};
use git_vista_core::diff::BlobContent;
use git_vista_protocol::conflict::{ConflictedFile, Resolution};
use git_vista_protocol::{CommitOid, ConflictSource, GenerationToken};

/// Which screen of the overlay is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    /// Every conflicted path.
    #[default]
    List,
    /// One path's four panes and the resolutions offered for it.
    Inspect,
    /// One path's marker blocks, the composed result, and the free-text edit.
    Editor,
}

/// What key dispatch needs to know, and nothing else.
///
/// A separate, tiny enum rather than handing `keys.rs` the whole pane: the
/// only thing a keymap may depend on is which bindings are live, and
/// [`KeyMode::Insert`] is the one that matters most — while it is on, `q` is
/// a letter the user typed, not a request to quit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    List,
    Inspect,
    Editor,
    Insert,
}

/// A read or write the shell must dispatch. The pane names the path; the
/// shell adds the repository, which is the shell's to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `GET /api/conflicts`.
    Conflicts,
    /// `GET /api/blob/{oid}` for one stage of one path.
    Stage {
        path: String,
        pane: View,
        oid: String,
    },
    /// `GET /api/worktree-file/{*path}` — the result pane.
    Result { path: String },
    /// `GET /api/conflict-source/{*path}` — the marker file and its token.
    Source { path: String },
    /// `POST /api/resolve-conflict` — a whole side, or the deletion.
    ResolveWholeFile { path: String, resolution: Resolution },
    /// `POST /api/resolve-conflict-content` (ADR 0069).
    ResolveContent {
        path: String,
        expected_stages: [Option<CommitOid>; 3],
        expected_source: GenerationToken,
        content: String,
    },
}

/// One key press, after `keys.rs` has translated it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Close,
    Back,
    Refresh,
    Down,
    Up,
    Open,
    FocusPane(View),
    NextPane,
    Take(Resolution),
    OpenEditor,
    Choose(Choice),
    BeginEdit,
    EndEdit,
    Apply,
    Type(char),
    Backspace,
    Newline,
    CaretLeft,
    CaretRight,
}

/// How one row of the overlay body is styled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Heading,
    Plain,
    /// A sentence the model produced about state or about what is on offer:
    /// "Not present on this side", a conflict's shape note, a withheld
    /// control's reason.
    ///
    /// Its own tone rather than sharing [`Tone::Muted`], and the difference is
    /// not cosmetic. These sentences ARE the content on the screens where they
    /// appear — "there is no ancestor" is what criterion 1 asks the terminal to
    /// say — so they are drawn at full weight. Dimming them would leave the
    /// most important line on the pane as the faintest thing on it.
    State,
    /// Genuinely secondary text: file context between conflicts, the ancestor
    /// body.
    Muted,
    /// Something that failed — an unreadable stage, a refused write.
    Fault,
    Ours,
    Theirs,
}

/// One logical row. Only the visible window of these is ever built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub tone: Tone,
    /// The cursor sits on this row: the list selection, or the block the
    /// editor's choice keys act on.
    pub selected: bool,
    /// Column of the text caret on this row, while hand-editing.
    pub caret: Option<usize>,
}

fn row(text: impl Into<String>, tone: Tone) -> Row {
    Row {
        text: text.into(),
        tone,
        selected: false,
        caret: None,
    }
}

fn selected_row(text: impl Into<String>, tone: Tone) -> Row {
    Row {
        selected: true,
        ..row(text, tone)
    }
}

/// A minimal multi-line text buffer: what the user typed, and where the
/// caret is.
///
/// `caret` is a **byte** offset that is always on a `char` boundary — every
/// mutation and every move goes through this type, and none of them can
/// produce an interior index. A `usize` counting characters would be simpler
/// to move but would make every insert an O(n) scan and every render a second
/// one; this way the slice operations are the cheap ones and only the
/// vertical moves walk the line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextEdit {
    text: String,
    caret: usize,
}

impl TextEdit {
    pub fn new(text: String) -> TextEdit {
        let caret = text.len();
        TextEdit { text, caret }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// `(line, column)` of the caret, both zero-based and both in characters
    /// — a renderer places a cell, not a byte.
    pub fn position(&self) -> (usize, usize) {
        let before = &self.text[..self.caret];
        let line = before.matches('\n').count();
        let column = before
            .rfind('\n')
            .map_or(before, |nl| &before[nl + 1..])
            .chars()
            .count();
        (line, column)
    }

    pub fn insert(&mut self, ch: char) {
        self.text.insert(self.caret, ch);
        self.caret += ch.len_utf8();
    }

    /// Delete the character before the caret. At the very start this does
    /// nothing rather than wrapping — there is nothing before the first byte.
    pub fn backspace(&mut self) {
        let Some((at, ch)) = self.text[..self.caret].char_indices().next_back() else {
            return;
        };
        self.text.replace_range(at..at + ch.len_utf8(), "");
        self.caret = at;
    }

    pub fn left(&mut self) {
        if let Some((at, _)) = self.text[..self.caret].char_indices().next_back() {
            self.caret = at;
        }
    }

    pub fn right(&mut self) {
        if let Some(ch) = self.text[self.caret..].chars().next() {
            self.caret += ch.len_utf8();
        }
    }

    /// Move one display line, keeping the column where it can. `delta` is
    /// -1 or 1; anything past the first or last line stays put.
    pub fn vertical(&mut self, delta: isize) {
        let (line, column) = self.position();
        let Some(target) = line.checked_add_signed(delta) else {
            return;
        };
        let lines: Vec<&str> = self.line_slices();
        let Some(row) = lines.get(target) else {
            return;
        };
        let start = row.as_ptr() as usize - self.text.as_ptr() as usize;
        let offset = row
            .char_indices()
            .nth(column)
            .map_or(row.len(), |(index, _)| index);
        self.caret = start + offset;
    }

    /// The buffer's lines as slices of it, with a trailing empty line when
    /// the text ends in a newline — a caret can legitimately sit there, and a
    /// splitter that dropped it would refuse to move onto the last line the
    /// user just made.
    fn line_slices(&self) -> Vec<&str> {
        let mut lines: Vec<&str> = self.text.split('\n').collect();
        if lines.is_empty() {
            lines.push("");
        }
        lines
    }
}

/// A path's four panes, once one has been opened for inspection.
#[derive(Clone, Debug)]
struct Inspect {
    path: String,
    panes: ConflictPanes,
    focus: View,
}

/// The line-level resolver's state for one path.
#[derive(Clone, Debug)]
struct Editor {
    path: String,
    source: ConflictSource,
    blocks: Vec<Block>,
    choices: Vec<Choice>,
    /// Which conflict ordinal the choice keys act on.
    block: usize,
    /// The free-text buffer. `Some` from the moment the user opens it.
    buffer: Option<TextEdit>,
    /// True once a keystroke actually changed the buffer.
    ///
    /// Separate from `buffer.is_some()` deliberately: merely *looking* at the
    /// composed text must not freeze the block buttons, and a typed character
    /// must. Criterion 4 lives on this flag.
    hand_edited: bool,
    /// Insert mode: every printable key is a character, not a command.
    inserting: bool,
}

/// The conflict overlay's whole state.
#[derive(Debug, Default)]
pub struct ConflictsPane {
    open: bool,
    repo: Option<String>,
    screen: Screen,
    /// `None` while the first read is out.
    files: Option<Result<Vec<ConflictedFile>, String>>,
    cursor: usize,
    scroll: usize,
    inspect: Option<Inspect>,
    editor: Option<Editor>,
    /// A write is out; the controls are inert until it answers.
    busy: bool,
    /// The last thing worth saying: a refusal, a server sentence, a success.
    message: Option<(String, bool)>,
}

impl ConflictsPane {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn key_mode(&self) -> KeyMode {
        match self.screen {
            Screen::List => KeyMode::List,
            Screen::Inspect => KeyMode::Inspect,
            Screen::Editor if self.editor.as_ref().is_some_and(|e| e.inserting) => KeyMode::Insert,
            Screen::Editor => KeyMode::Editor,
        }
    }

    /// Open the overlay on `repo` and ask for its conflicts.
    pub fn open(&mut self, repo: String) -> Vec<Request> {
        let same_repo = self.repo.as_deref() == Some(repo.as_str());
        self.open = true;
        self.screen = Screen::List;
        self.message = None;
        self.busy = false;
        self.scroll = 0;
        if !same_repo {
            self.cursor = 0;
            self.inspect = None;
            self.editor = None;
        }
        self.repo = Some(repo);
        self.files = None;
        vec![Request::Conflicts]
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// The repository this view is bound to. An answer for any other one is
    /// dropped — the same request-key discipline `DetailPane` uses.
    pub fn repo(&self) -> Option<&str> {
        self.repo.as_deref()
    }

    pub fn message(&self) -> Option<(&str, bool)> {
        self.message.as_ref().map(|(t, e)| (t.as_str(), *e))
    }

    /// The keys live on the current screen, for the status line.
    pub fn hint(&self) -> String {
        match self.key_mode() {
            KeyMode::List => {
                String::from("j/k move · Enter inspect · r refresh · Esc close")
            }
            KeyMode::Inspect => String::from(
                "Tab/1-4 pane · j/k scroll · o ours · t theirs · d delete · e line editor · Esc back",
            ),
            KeyMode::Editor => String::from(
                "j/k block · o/t/b choose · i edit text · Enter apply · Esc back",
            ),
            KeyMode::Insert => String::from("typing — Esc leaves the text buffer"),
        }
    }

    // ---- folding answers in ------------------------------------------

    pub fn receive_conflicts(&mut self, repo: &str, result: Result<Vec<ConflictedFile>, String>) {
        if self.repo.as_deref() != Some(repo) {
            return;
        }
        if let Err(message) = &result {
            self.message = Some((message.clone(), true));
        }
        self.files = Some(result);
        self.clamp_cursor();
    }

    pub fn receive_stage(
        &mut self,
        repo: &str,
        path: &str,
        pane: View,
        result: Result<BlobContent, String>,
    ) {
        if self.repo.as_deref() != Some(repo) {
            return;
        }
        let Some(inspect) = self.inspect.as_mut().filter(|i| i.path == path) else {
            return;
        };
        let slot = inspect.panes.pane_mut(pane);
        *slot = slot.clone().with_content(result);
    }

    pub fn receive_result(&mut self, repo: &str, path: &str, read: ResultRead) {
        if self.repo.as_deref() != Some(repo) {
            return;
        }
        let Some(inspect) = self.inspect.as_mut().filter(|i| i.path == path) else {
            return;
        };
        inspect.panes.result = result_pane_state(read);
    }

    pub fn receive_source(
        &mut self,
        repo: &str,
        path: &str,
        result: Result<ConflictSource, String>,
    ) {
        if self.repo.as_deref() != Some(repo) {
            return;
        }
        if self.inspect.as_ref().map(|i| i.path.as_str()) != Some(path) {
            return;
        }
        self.busy = false;
        match result {
            Err(message) => self.message = Some((message, true)),
            Ok(source) => {
                let blocks = markers::parse(&source.content);
                let choices = vec![Choice::Unchosen; markers::conflict_count(&blocks)];
                self.editor = Some(Editor {
                    path: path.to_string(),
                    source,
                    blocks,
                    choices,
                    block: 0,
                    buffer: None,
                    hand_edited: false,
                    inserting: false,
                });
                self.screen = Screen::Editor;
                self.scroll = 0;
                self.message = None;
            }
        }
    }

    /// A resolution came back. Success sends the view back to a fresh list —
    /// the path it was showing is, by construction, no longer conflicted.
    pub fn receive_resolved(&mut self, repo: &str, path: &str, result: Result<(), String>) -> bool {
        if self.repo.as_deref() != Some(repo) {
            return false;
        }
        self.busy = false;
        match result {
            Err(message) => {
                // The server's own sentence, kept whole. The four content
                // refusals name four different things that moved, and
                // collapsing them into "it failed" throws away the only part
                // that says what to do next.
                self.message = Some((message, true));
                false
            }
            Ok(()) => {
                self.message = Some((format!("resolved {path}"), false));
                self.inspect = None;
                self.editor = None;
                self.screen = Screen::List;
                self.scroll = 0;
                self.files = None;
                true
            }
        }
    }

    // ---- folding key presses in --------------------------------------

    pub fn apply(&mut self, act: Act) -> Vec<Request> {
        match act {
            Act::Close => {
                self.close();
                Vec::new()
            }
            Act::Back => self.back(),
            Act::Refresh => {
                self.files = None;
                self.message = None;
                vec![Request::Conflicts]
            }
            Act::Down => self.move_cursor(1),
            Act::Up => self.move_cursor(-1),
            Act::Open => self.open_selected(),
            Act::NextPane => {
                if let Some(inspect) = self.inspect.as_mut() {
                    let next = (View::ALL
                        .iter()
                        .position(|p| *p == inspect.focus)
                        .expect("every pane is in ALL")
                        + 1)
                        % View::ALL.len();
                    inspect.focus = View::ALL[next];
                    self.scroll = 0;
                }
                Vec::new()
            }
            Act::FocusPane(pane) => {
                if let Some(inspect) = self.inspect.as_mut() {
                    inspect.focus = pane;
                    self.scroll = 0;
                }
                Vec::new()
            }
            Act::Take(resolution) => self.take(resolution),
            Act::OpenEditor => self.open_editor(),
            Act::Choose(choice) => {
                self.choose(choice);
                Vec::new()
            }
            Act::BeginEdit => {
                self.begin_edit();
                Vec::new()
            }
            Act::EndEdit => {
                if let Some(editor) = self.editor.as_mut() {
                    editor.inserting = false;
                }
                Vec::new()
            }
            Act::Apply => self.apply_content(),
            Act::Type(ch) => {
                self.edit(|buffer| buffer.insert(ch));
                Vec::new()
            }
            Act::Newline => {
                self.edit(|buffer| buffer.insert('\n'));
                Vec::new()
            }
            Act::Backspace => {
                self.edit(TextEdit::backspace);
                Vec::new()
            }
            Act::CaretLeft => {
                self.move_caret(TextEdit::left);
                Vec::new()
            }
            Act::CaretRight => {
                self.move_caret(TextEdit::right);
                Vec::new()
            }
        }
    }

    fn back(&mut self) -> Vec<Request> {
        match self.screen {
            Screen::List => self.close(),
            Screen::Inspect => {
                self.screen = Screen::List;
                self.scroll = 0;
            }
            Screen::Editor => {
                self.editor = None;
                self.screen = Screen::Inspect;
                self.scroll = 0;
            }
        }
        Vec::new()
    }

    fn move_cursor(&mut self, delta: isize) -> Vec<Request> {
        match self.screen {
            Screen::List => {
                let last = self.files_ref().map_or(0, <[_]>::len).saturating_sub(1);
                self.cursor = self.cursor.saturating_add_signed(delta).min(last);
            }
            Screen::Inspect => {
                self.scroll = self.scroll.saturating_add_signed(delta);
            }
            Screen::Editor => {
                if let Some(editor) = self.editor.as_mut() {
                    if editor.inserting {
                        if let Some(buffer) = editor.buffer.as_mut() {
                            buffer.vertical(delta);
                        }
                        return Vec::new();
                    }
                    let last = editor.choices.len().saturating_sub(1);
                    editor.block = editor.block.saturating_add_signed(delta).min(last);
                }
            }
        }
        Vec::new()
    }

    fn open_selected(&mut self) -> Vec<Request> {
        if self.screen == Screen::Editor {
            return self.apply_content();
        }
        if self.screen != Screen::List {
            return Vec::new();
        }
        let Some(file) = self
            .files_ref()
            .and_then(|files| files.get(self.cursor))
            .cloned()
        else {
            return Vec::new();
        };
        let panes = ConflictPanes::open(&file);
        // Only a pane the model says is awaiting content is fetched. A binary
        // or absent side resolves from metadata alone, so a conflict with a
        // 200 MB binary side costs one listing, not a download — the same rule
        // the browser client's assembler follows.
        let mut requests = vec![Request::Result {
            path: file.path.clone(),
        }];
        for pane in [View::Base, View::Ours, View::Theirs] {
            if let PaneState::AwaitingContent { oid } = panes.pane(pane) {
                requests.push(Request::Stage {
                    path: file.path.clone(),
                    pane,
                    oid: oid.clone(),
                });
            }
        }
        self.inspect = Some(Inspect {
            path: file.path.clone(),
            panes,
            focus: View::Ours,
            });
        self.editor = None;
        self.screen = Screen::Inspect;
        self.scroll = 0;
        self.message = None;
        requests
    }

    /// A whole-side resolution, or the deletion.
    ///
    /// Refused here when the model withheld the control, in the model's own
    /// words. The row for a withheld control carries no key, so this is the
    /// second half of the same answer rather than the only one: a user who
    /// presses the key anyway is told why, not walked into a 409.
    fn take(&mut self, resolution: Resolution) -> Vec<Request> {
        if self.busy {
            return Vec::new();
        }
        let Some(inspect) = self.inspect.as_ref() else {
            return Vec::new();
        };
        let offer = match resolution {
            Resolution::TakeOurs => &inspect.panes.surface.take_ours,
            Resolution::TakeTheirs => &inspect.panes.surface.take_theirs,
            Resolution::TakeDeletion => &inspect.panes.surface.take_deletion,
        };
        if let Err(withheld) = offer {
            self.message = Some((withheld.describe(), true));
            return Vec::new();
        }
        let path = inspect.path.clone();
        self.busy = true;
        self.message = None;
        vec![Request::ResolveWholeFile { path, resolution }]
    }

    fn open_editor(&mut self) -> Vec<Request> {
        if self.busy {
            return Vec::new();
        }
        let Some(inspect) = self.inspect.as_ref() else {
            return Vec::new();
        };
        // Read from the model, never recomputed. This flag is
        // `ConflictedFile::text_resolvable` — the identical question the
        // server asks before executing a content resolution — so the editor
        // cannot open on a file the executor would refuse.
        if !inspect.panes.surface.text_resolution_allowed {
            self.message = Some((
                inspect.panes.surface.note.clone().unwrap_or_else(|| {
                    String::from("This file cannot be resolved line by line.")
                }),
                true,
            ));
            return Vec::new();
        }
        let path = inspect.path.clone();
        self.busy = true;
        self.message = None;
        vec![Request::Source { path }]
    }

    /// Choose a side for the block under the cursor.
    ///
    /// **A no-op once the text has been hand-edited**, which is criterion 4:
    /// re-composing from the buttons would throw the user's typing away
    /// without a word, and silently discarding what somebody typed is the
    /// worst failure available in an editor. The buttons go inert instead,
    /// and the row says so.
    fn choose(&mut self, choice: Choice) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if editor.hand_edited {
            self.message = Some((
                String::from(
                    "the text has been edited by hand — block choices no longer change it",
                ),
                false,
            ));
            return;
        }
        if let Some(slot) = editor.choices.get_mut(editor.block) {
            *slot = choice;
        }
    }

    fn begin_edit(&mut self) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        editor.inserting = true;
        // Re-seeded on every entry UNTIL the first real edit, and never after
        // it. Before a hand-edit the buffer is only a view of the current
        // composition, so opening it again after changing a block must show
        // that change; after one, it is the user's own text and re-seeding it
        // would be the silent discard criterion 4 exists to forbid.
        if editor.hand_edited {
            return;
        }
        // Seeded from the composition when there is one, and otherwise from
        // the marker file git actually wrote. Never from empty: an empty
        // buffer would offer "delete everything in this file" as the starting
        // point of a resolution.
        let seed = markers::compose(&editor.blocks, &editor.choices)
            .unwrap_or_else(|| editor.source.content.clone());
        editor.buffer = Some(TextEdit::new(seed));
    }

    fn edit(&mut self, change: impl FnOnce(&mut TextEdit)) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if !editor.inserting {
            return;
        }
        let Some(buffer) = editor.buffer.as_mut() else {
            return;
        };
        change(buffer);
        editor.hand_edited = true;
    }

    fn move_caret(&mut self, change: impl FnOnce(&mut TextEdit)) {
        let Some(buffer) = self
            .editor
            .as_mut()
            .filter(|editor| editor.inserting)
            .and_then(|editor| editor.buffer.as_mut())
        else {
            return;
        };
        change(buffer);
    }

    /// Submit the line-level resolution.
    ///
    /// `expected_stages` and `expected_source` are echoed back exactly as the
    /// server served them and are never recomputed here — a client that
    /// computed its own could only ever agree with itself, and ADR 0069's
    /// third and fourth gates would then prove nothing.
    fn apply_content(&mut self) -> Vec<Request> {
        if self.busy {
            return Vec::new();
        }
        let Some(editor) = self.editor.as_ref() else {
            return Vec::new();
        };
        let composed = if editor.hand_edited {
            editor.buffer.as_ref().map(|buffer| buffer.text().to_string())
        } else {
            markers::compose(&editor.blocks, &editor.choices)
        };
        let Some(content) = composed else {
            let open = markers::unchosen(&editor.blocks, &editor.choices).len();
            self.message = Some((
                format!("{open} conflict(s) still need a choice before this can be applied"),
                true,
            ));
            return Vec::new();
        };
        let request = Request::ResolveContent {
            path: editor.path.clone(),
            expected_stages: editor.source.stages.clone(),
            expected_source: editor.source.source.clone(),
            content,
        };
        self.busy = true;
        self.message = None;
        vec![request]
    }

    fn files_ref(&self) -> Option<&[ConflictedFile]> {
        match self.files.as_ref() {
            Some(Ok(files)) => Some(files),
            _ => None,
        }
    }

    fn clamp_cursor(&mut self) {
        let last = self.files_ref().map_or(0, <[_]>::len).saturating_sub(1);
        self.cursor = self.cursor.min(last);
    }
}

// ---- the row projection ----------------------------------------------
//
// Rows are produced through a visitor and only the visible window of them is
// ever materialized, the same shape `DetailPane` uses and for the same
// reason: a conflict side is bounded by the server's content cap, not by the
// height of a terminal, and building thirty thousand `String`s per redraw to
// show forty of them is how a viewer becomes unusable on exactly the file
// that matters most.
//
// `row_count` is therefore arithmetic rather than a counting visit. That is a
// second implementation of the layout, and the honest way to hold two
// implementations of one thing is to pin them against each other:
// `row_count_agrees_with_the_rows_actually_emitted` fails the moment they
// drift, on every screen.

impl ConflictsPane {
    /// How many rows the body has, for scroll clamping.
    pub fn row_count(&self) -> usize {
        match self.screen {
            Screen::List => 1 + self.list_body_len(),
            Screen::Inspect => self.inspect.as_ref().map_or(1, inspect_row_count),
            Screen::Editor => self.editor.as_ref().map_or(1, editor_row_count),
        }
    }

    /// The rows in `[offset, offset + limit)`.
    pub fn window(&self, offset: usize, limit: usize) -> Vec<Row> {
        let mut window = RowWindow::new(offset, limit);
        self.visit_rows(|row| window.push(row));
        window.rows
    }

    /// Where the body should be scrolled to for a `height`-row viewport:
    /// the user's own scroll, clamped to the end, and pulled far enough to
    /// keep the text caret on screen while they are typing.
    ///
    /// A caret the viewport has scrolled away from is a caret the user cannot
    /// see, and an invisible caret in a buffer that accepts every keystroke is
    /// a way to edit the wrong line of a file you are about to write.
    pub fn view_offset(&self, height: usize) -> usize {
        let max = self.row_count().saturating_sub(height);
        let offset = self.scroll.min(max);
        let Some(caret) = self.caret_row() else {
            return offset;
        };
        if height == 0 {
            return offset;
        }
        if caret < offset {
            caret
        } else if caret >= offset + height {
            caret + 1 - height
        } else {
            offset
        }
    }

    /// The absolute row index the text caret sits on, while hand-editing.
    fn caret_row(&self) -> Option<usize> {
        let editor = self.editor.as_ref()?;
        if !editor.inserting {
            return None;
        }
        let (line, _) = editor.buffer.as_ref()?.position();
        Some(editor_result_first_row(editor) + line)
    }

    fn list_body_len(&self) -> usize {
        match self.files.as_ref() {
            None | Some(Err(_)) => 1,
            Some(Ok(files)) if files.is_empty() => 1,
            Some(Ok(files)) => files.len(),
        }
    }

    fn visit_rows(&self, mut emit: impl FnMut(Row) -> bool) {
        match self.screen {
            Screen::List => self.visit_list(&mut emit),
            Screen::Inspect => match self.inspect.as_ref() {
                Some(inspect) => visit_inspect(inspect, &mut emit),
                None => {
                    let _ = emit(row("No conflict is open.", Tone::Muted));
                }
            },
            Screen::Editor => match self.editor.as_ref() {
                Some(editor) => visit_editor(editor, &mut emit),
                None => {
                    let _ = emit(row("No conflict is open.", Tone::Muted));
                }
            },
        }
    }

    fn visit_list(&self, emit: &mut impl FnMut(Row) -> bool) {
        let heading = match self.files.as_ref() {
            None => String::from("Conflicts — loading…"),
            Some(Err(_)) => String::from("Conflicts — could not be read"),
            Some(Ok(files)) => format!(
                "Conflicts — {} path{}",
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            ),
        };
        if !emit(row(heading, Tone::Heading)) {
            return;
        }
        match self.files.as_ref() {
            None => {
                let _ = emit(row("Reading the index…", Tone::State));
            }
            Some(Err(message)) => {
                let _ = emit(row(message.clone(), Tone::Fault));
            }
            Some(Ok(files)) if files.is_empty() => {
                // Said plainly rather than left blank: an empty body would be
                // indistinguishable from a list that failed to draw.
                let _ = emit(row(
                    "Nothing is conflicted in this repository.",
                    Tone::State,
                ));
            }
            Some(Ok(files)) => {
                for (index, file) in files.iter().enumerate() {
                    // The note is the model's own sentence about this
                    // conflict's shape — binary, delete/modify, or a side that
                    // could not be read. `None` for an ordinary text conflict,
                    // deliberately: a note on every row would train the eye to
                    // skip the rows that have something to say.
                    let surface = ResolutionSurface::of(file);
                    let text = match surface.note {
                        Some(note) => format!("{}  —  {note}", file.path),
                        None => file.path.clone(),
                    };
                    let line = if index == self.cursor {
                        selected_row(text, Tone::Plain)
                    } else {
                        row(text, Tone::Plain)
                    };
                    if !emit(line) {
                        return;
                    }
                }
            }
        }
    }
}

fn inspect_row_count(inspect: &Inspect) -> usize {
    // path + [note] + blank + four summary rows + blank + focus heading
    // + content + blank + "Resolutions" + three controls + the editor row.
    let note = usize::from(inspect.panes.surface.note.is_some());
    1 + note + 1 + 4 + 1 + 1 + content_row_count(inspect.panes.pane(inspect.focus)) + 1 + 1 + 3 + 1
}

/// How many rows one pane's body takes.
///
/// Every non-`Text` state is exactly one row — its own sentence. `Text` with
/// no content is also one row, and it says the version is empty rather than
/// drawing a blank: `Text { content: "" }` is the one state entitled to claim
/// "this version was blank" (ADR 0063), and a claim nobody can see is not
/// made.
fn content_row_count(state: &PaneState) -> usize {
    match state {
        PaneState::Text { content, .. } if !content.is_empty() => content.lines().count().max(1),
        _ => 1,
    }
}

fn visit_content(state: &PaneState, emit: &mut impl FnMut(Row) -> bool) -> bool {
    match state {
        PaneState::Text { content, .. } if !content.is_empty() => {
            for line in content.lines() {
                if !emit(row(line, Tone::Plain)) {
                    return false;
                }
            }
            true
        }
        PaneState::Text { .. } => emit(row("(this version is empty)", Tone::State)),
        // Absent, Unreadable, Binary, AwaitingContent and ContentUnavailable
        // each say what they are, in the model's own words. The two that are
        // faults rather than facts are marked as such.
        other => {
            let tone = match other {
                PaneState::Unreadable { .. } | PaneState::ContentUnavailable { .. } => Tone::Fault,
                _ => Tone::State,
            };
            emit(row(other.describe(), tone))
        }
    }
}

fn visit_inspect(inspect: &Inspect, emit: &mut impl FnMut(Row) -> bool) {
    if !emit(row(inspect.path.clone(), Tone::Heading)) {
        return;
    }
    if let Some(note) = &inspect.panes.surface.note {
        if !emit(row(note.clone(), Tone::State)) {
            return;
        }
    }
    if !emit(row("", Tone::Plain)) {
        return;
    }

    // All four panes are stated at once, whichever one is being read. This is
    // where "there is no ancestor" stays on screen instead of being something
    // the user has to go and look for.
    for pane in View::ALL {
        let state = inspect.panes.pane(pane);
        let text = format!("{:<18} {}", pane.label(), state.describe());
        let tone = match state {
            PaneState::Unreadable { .. } | PaneState::ContentUnavailable { .. } => Tone::Fault,
            _ => Tone::State,
        };
        let line = if pane == inspect.focus {
            selected_row(text, tone)
        } else {
            row(text, tone)
        };
        if !emit(line) {
            return;
        }
    }

    if !emit(row("", Tone::Plain)) {
        return;
    }
    if !emit(row(inspect.focus.label(), Tone::Heading)) {
        return;
    }
    if !visit_content(inspect.panes.pane(inspect.focus), emit) {
        return;
    }
    if !emit(row("", Tone::Plain)) {
        return;
    }
    if !emit(row("Resolutions", Tone::Heading)) {
        return;
    }

    // A withheld control is drawn with NO key beside it — that is what "not
    // offered" looks like on a keyboard — and carries the model's sentence
    // saying why. Pressing the key anyway is refused in `take`, with the same
    // sentence; the two halves agree because both read this one `Result`.
    let surface = &inspect.panes.surface;
    for (key, label, offer) in [
        ('o', "Take ours", &surface.take_ours),
        ('t', "Take theirs", &surface.take_theirs),
        ('d', "Delete the file", &surface.take_deletion),
    ] {
        let line = match offer {
            Ok(()) => row(format!("  {key}   {label}"), Tone::Plain),
            Err(withheld) => row(
                format!("      {label} — {}", withheld.describe()),
                Tone::State,
            ),
        };
        if !emit(line) {
            return;
        }
    }

    // The line-level editor's own offer, from the same flag the server asks
    // before it will execute one.
    let editor_row = if surface.text_resolution_allowed {
        row("  e   Resolve line by line", Tone::Plain)
    } else {
        row(
            "      Resolve line by line — not available for this file",
            Tone::State,
        )
    };
    let _ = emit(editor_row);
}

/// The composed text a submission would carry, or `None` while a block is
/// still unchosen and nothing has been typed.
fn editor_result(editor: &Editor) -> Option<String> {
    if editor.hand_edited || editor.inserting {
        if let Some(buffer) = editor.buffer.as_ref() {
            return Some(buffer.text().to_string());
        }
    }
    markers::compose(&editor.blocks, &editor.choices)
}

fn editor_state_sentence(editor: &Editor) -> String {
    if editor.hand_edited {
        return String::from("edited by hand — block choices no longer change this text");
    }
    let open = markers::unchosen(&editor.blocks, &editor.choices).len();
    if open == 0 {
        String::from("every conflict chosen")
    } else {
        format!(
            "{open} of {} conflict(s) still need a choice",
            editor.choices.len()
        )
    }
}

fn block_row_count(block: &Block) -> usize {
    match block {
        Block::Context { text } => text.lines().count().max(1),
        Block::Conflict { ours, theirs, base } => {
            // heading + the ancestor (its lines, or the one sentence saying
            // there is none) + both sides.
            1 + base
                .as_ref()
                .map_or(1, |text| text.lines().count().max(1))
                + ours.lines().count().max(1)
                + theirs.lines().count().max(1)
        }
    }
}

/// The absolute index of the first row of the Result section — what the
/// caret's line number is measured from.
fn editor_result_first_row(editor: &Editor) -> usize {
    // path + state sentence + blank + "Blocks" + every block + blank + "Result"
    4 + editor.blocks.iter().map(block_row_count).sum::<usize>() + 2
}

fn editor_row_count(editor: &Editor) -> usize {
    let body = match editor_result(editor) {
        Some(text) => text.lines().count().max(1),
        None => 1,
    };
    editor_result_first_row(editor) + body
}

fn visit_editor(editor: &Editor, emit: &mut impl FnMut(Row) -> bool) {
    for head in [
        row(editor.path.clone(), Tone::Heading),
        row(editor_state_sentence(editor), Tone::State),
        row("", Tone::Plain),
        row("Blocks", Tone::Heading),
    ] {
        if !emit(head) {
            return;
        }
    }

    let total = editor.choices.len();
    let mut nth = 0usize;
    for block in &editor.blocks {
        match block {
            Block::Context { text } => {
                for line in lines_or_one(text) {
                    if !emit(row(format!("  {line}"), Tone::Muted)) {
                        return;
                    }
                }
            }
            Block::Conflict { ours, theirs, base } => {
                let index = nth;
                nth += 1;
                let choice = editor.choices.get(index).copied().unwrap_or(Choice::Unchosen);
                let heading = format!(
                    "Conflict {} of {total} — {}",
                    index + 1,
                    choice.describe()
                );
                let line = if index == editor.block {
                    selected_row(heading, Tone::Heading)
                } else {
                    row(heading, Tone::Heading)
                };
                if !emit(line) {
                    return;
                }
                // An absent ancestor says so. Git omits it under the default
                // merge style, and an empty ancestor section would claim a
                // common ancestor existed and was blank.
                match base {
                    Some(text) => {
                        for line in lines_or_one(text) {
                            if !emit(row(format!("| {line}"), Tone::Muted)) {
                                return;
                            }
                        }
                    }
                    None => {
                        if !emit(row(
                            "| no recorded ancestor in this marker file",
                            Tone::State,
                        )) {
                            return;
                        }
                    }
                }
                for line in lines_or_one(ours) {
                    if !emit(row(format!("< {line}"), Tone::Ours)) {
                        return;
                    }
                }
                for line in lines_or_one(theirs) {
                    if !emit(row(format!("> {line}"), Tone::Theirs)) {
                        return;
                    }
                }
            }
        }
    }

    if !emit(row("", Tone::Plain)) {
        return;
    }
    if !emit(row("Result", Tone::Heading)) {
        return;
    }
    let caret = editor
        .inserting
        .then(|| editor.buffer.as_ref().map(TextEdit::position))
        .flatten();
    match editor_result(editor) {
        None => {
            let _ = emit(row(editor_state_sentence(editor), Tone::State));
        }
        Some(text) => {
            for (index, line) in lines_or_one(&text).into_iter().enumerate() {
                let mut line = row(line, Tone::Plain);
                if let Some((caret_line, column)) = caret {
                    if caret_line == index {
                        line.caret = Some(column);
                    }
                }
                if !emit(line) {
                    return;
                }
            }
        }
    }
}

/// A text's lines, or one empty line when it has none — so a body always
/// occupies the row `block_row_count` reserved for it.
fn lines_or_one(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        vec![""]
    } else {
        lines
    }
}

struct RowWindow {
    start: usize,
    limit: usize,
    rows: Vec<Row>,
}

impl RowWindow {
    fn new(start: usize, limit: usize) -> RowWindow {
        RowWindow {
            start,
            limit,
            rows: Vec::with_capacity(limit.min(256)),
        }
    }

    /// `false` means the window is full and projection can stop.
    fn push(&mut self, row: Row) -> bool {
        if self.limit == 0 {
            return false;
        }
        if self.start > 0 {
            self.start -= 1;
            return true;
        }
        if self.rows.len() < self.limit {
            self.rows.push(row);
        }
        self.rows.len() < self.limit
    }
}
