//! Less-style full-screen viewer for pre-rendered text.
//!
//! Assumes the caller can re-render the document at any column width: the
//! pager owns the terminal, tracks the scroll position, and asks for a fresh
//! render whenever the width it follows changes. Everything that decides
//! *where* the viewport goes is a pure value transform on [`Viewport`], so the
//! event loop is only glue between crossterm events and those transforms.
//!
//! Links are clickable: after every frame the link cells on screen are
//! re-printed inside OSC 8 escapes (see [`crate::hyperlinks`]). Mouse capture
//! stays off, so clicks and text selection belong to the terminal, and the
//! wheel arrives as the arrow keys the terminal translates it into.
//!
//! Search (`/`, `?`, `n`, `N`) follows less: the matching line moves to the
//! top of the screen and every match on screen is shown in reverse video.
//! Matching itself lives in [`crate::search`]; the pager only decides which
//! line a search starts from.
//!
//! Gotcha: a fixed `-w` wider than the terminal is honored, not clamped, so
//! the right edge is clipped exactly like `less -S`. Auto width re-wraps on
//! every resize and keeps the reader at the same proportional position rather
//! than the same line index, since line indices shift when text reflows.

use std::io;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::document::Document;
use crate::hyperlinks::{Paint, Painter, PlacedLink};
use crate::search::{Direction, Edit, Search, SearchInput};

/// How search matches are marked on screen: reverse video, like less.
const HIGHLIGHT: Style = Style::new().add_modifier(Modifier::REVERSED);

/// Less's messages for a search that finds nothing, and for `n` before any
/// pattern was entered.
const NOT_FOUND: &str = "Pattern not found";
const NO_PREVIOUS: &str = "No previous regular expression";

/// Where the render width comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// Always this many columns, regardless of the terminal.
    Fixed(NonZeroUsize),
    /// The terminal's current width, re-rendered on resize.
    Auto,
}

impl Width {
    /// The column count to render at for a terminal `terminal_cols` wide.
    pub fn columns(self, terminal_cols: u16) -> usize {
        match self {
            Width::Fixed(cols) => cols.get(),
            Width::Auto => usize::from(terminal_cols).max(1),
        }
    }
}

/// How far one scroll command moves, in terms of the visible height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Line,
    HalfPage,
    Page,
}

/// A change of reading position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Down(Step),
    Up(Step),
    Top,
    Bottom,
}

/// Which way `n` and `N` go, relative to the direction the pattern was
/// entered in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    Same,
    Reverse,
}

impl Repeat {
    fn direction(self, entered: Direction) -> Direction {
        match self {
            Repeat::Same => entered,
            Repeat::Reverse => entered.reversed(),
        }
    }
}

/// Everything a keypress can ask the pager to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Move(Motion),
    /// Read a pattern at the prompt, then search for it.
    Search(Direction),
    /// Search for the last pattern again.
    RepeatSearch(Repeat),
    Quit,
}

/// Maps a key to a command using less/vi conventions; unknown keys are ignored.
pub fn command_for(key: KeyEvent) -> Option<Command> {
    use KeyCode::{Char, Down, End, Enter, Esc, Home, PageDown, PageUp, Up};

    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    let command = match (key.code, ctrl) {
        (Char('q') | Char('Q') | Esc, false) | (Char('c'), true) => Command::Quit,

        (Char('j') | Down | Enter, false) | (Char('e') | Char('n') | Char('j'), true) => {
            Command::Move(Motion::Down(Step::Line))
        }
        (Char('k') | Up, false) | (Char('y') | Char('p') | Char('k'), true) => {
            Command::Move(Motion::Up(Step::Line))
        }

        (Char('J') | Char('d'), false) | (Char('d'), true) => {
            Command::Move(Motion::Down(Step::HalfPage))
        }
        (Char('K') | Char('u'), false) | (Char('u'), true) => {
            Command::Move(Motion::Up(Step::HalfPage))
        }

        (Char('f') | Char(' ') | PageDown, false) | (Char('f') | Char('v'), true) => {
            Command::Move(Motion::Down(Step::Page))
        }
        (Char('b') | PageUp, false) | (Char('b'), true) => Command::Move(Motion::Up(Step::Page)),

        (Char('g') | Char('<') | Home, false) => Command::Move(Motion::Top),
        (Char('G') | Char('>') | End, false) => Command::Move(Motion::Bottom),

        (Char('/'), false) => Command::Search(Direction::Forward),
        (Char('?'), false) => Command::Search(Direction::Backward),
        (Char('n'), false) => Command::RepeatSearch(Repeat::Same),
        (Char('N'), false) => Command::RepeatSearch(Repeat::Reverse),

        _ => return None,
    };
    Some(command)
}

/// The visible window over a column of rendered lines.
///
/// Every method returns a new value with the offset clamped into
/// `0..=max_offset`, so a `Viewport` can never point past the content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    offset: usize,
    content_lines: usize,
    height: usize,
}

impl Viewport {
    /// A viewport parked at the top of `content_lines` lines, `height` tall.
    pub fn new(content_lines: usize, height: usize) -> Self {
        Self {
            offset: 0,
            content_lines,
            height,
        }
    }

    /// Index of the first visible line.
    pub fn offset(self) -> usize {
        self.offset
    }

    /// The largest offset that still fills the viewport (or 0 when the
    /// content fits).
    pub fn max_offset(self) -> usize {
        self.content_lines.saturating_sub(self.height)
    }

    /// Whether the last content line is on screen.
    pub fn at_bottom(self) -> bool {
        self.offset >= self.max_offset()
    }

    /// The half-open range of line indices currently visible.
    pub fn visible_range(self) -> std::ops::Range<usize> {
        let end = self
            .offset
            .saturating_add(self.height)
            .min(self.content_lines);
        self.offset..end
    }

    /// Same content and height, parked at `offset` (clamped).
    pub fn with_offset(self, offset: usize) -> Self {
        Self {
            offset: offset.min(self.max_offset()),
            ..self
        }
    }

    /// Same content and offset, with a new visible height (offset re-clamped).
    pub fn with_height(self, height: usize) -> Self {
        Self { height, ..self }.with_offset(self.offset)
    }

    /// Swaps in a re-rendered document of `content_lines` lines, keeping the
    /// reader at the same proportional position through the text.
    ///
    /// A reader parked at the very bottom stays at the bottom, so a resize
    /// while reading the end of a file does not strand them mid-document.
    pub fn with_content(self, content_lines: usize) -> Self {
        let next = Self {
            content_lines,
            ..self
        };
        let offset = if self.content_lines == 0 {
            0
        } else if self.at_bottom() && self.offset > 0 {
            next.max_offset()
        } else {
            // u128 keeps the product from overflowing on absurdly long files.
            let scaled = u128::from(self.offset as u64) * u128::from(content_lines as u64)
                / u128::from(self.content_lines as u64);
            usize::try_from(scaled).unwrap_or(usize::MAX)
        };
        next.with_offset(offset)
    }

    /// How many lines `step` moves at the current height; never zero, so a
    /// keypress always makes progress even in a one-row terminal.
    fn lines_for(self, step: Step) -> usize {
        match step {
            Step::Line => 1,
            Step::HalfPage => (self.height / 2).max(1),
            Step::Page => self.height.max(1),
        }
    }

    /// Applies one motion.
    pub fn moved(self, motion: Motion) -> Self {
        match motion {
            Motion::Down(step) => {
                self.with_offset(self.offset.saturating_add(self.lines_for(step)))
            }
            Motion::Up(step) => self.with_offset(self.offset.saturating_sub(self.lines_for(step))),
            Motion::Top => self.with_offset(0),
            Motion::Bottom => self.with_offset(self.max_offset()),
        }
    }
}

/// Runs the interactive pager until the reader quits.
///
/// `render` is called with a column count and must return the document
/// wrapped to that width; it runs once up front and again after any resize
/// that changes the render width.
pub fn run(title: &str, width: Width, render: impl Fn(usize) -> Document) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, title, width, render);
    ratatui::restore();
    result
}

/// The rendered document plus the geometry it was rendered for.
struct Rendered {
    doc: Document,
    columns: usize,
    /// The line the last search landed on. Line indices mean nothing in a
    /// render at another width, so every fresh render starts without one.
    last_match: Option<usize>,
}

impl Rendered {
    fn new(doc: Document, columns: usize) -> Self {
        Self {
            doc,
            columns,
            last_match: None,
        }
    }
}

/// Whether the event loop keeps going after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

/// What the bottom row is doing between keypresses.
///
/// One enum rather than flags because the states exclude each other: the
/// title lasts until the first keypress, a message until the next one, and
/// while a pattern is being typed keys edit it instead of scrolling.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    /// The file name, on the first screen only, like less.
    Title,
    /// The ordinary prompt: `:` or `(END)`.
    Paging,
    /// What went wrong with the last command, e.g. `Pattern not found`.
    Message(String),
    /// Reading a search pattern.
    Typing(SearchInput),
}

/// Everything the event loop mutates between frames.
struct Session<F> {
    width: Width,
    render: F,
    rendered: Rendered,
    viewport: Viewport,
    /// The last pattern entered; it stays highlighted until another replaces it.
    search: Option<Search>,
    status: Status,
}

impl<F: Fn(usize) -> Document> Session<F> {
    fn new(width: Width, render: F, terminal_cols: u16, terminal_rows: u16) -> Self {
        let columns = width.columns(terminal_cols);
        let rendered = Rendered::new(render(columns), columns);
        let viewport = Viewport::new(rendered.doc.lines().len(), body_height(terminal_rows));
        Self {
            width,
            render,
            rendered,
            viewport,
            search: None,
            status: Status::Title,
        }
    }

    /// Applies one terminal event without drawing.
    fn apply(&mut self, event: Event) -> Flow {
        match event {
            // Releases are dropped here, not just unmapped, so one cannot
            // dismiss the message its own press put up.
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                return self.key(key);
            }
            Event::Resize(cols, rows) => {
                let columns = self.width.columns(cols);
                if columns != self.rendered.columns {
                    self.rendered = Rendered::new((self.render)(columns), columns);
                    self.viewport = self.viewport.with_content(self.rendered.doc.lines().len());
                }
                self.viewport = self.viewport.with_height(body_height(rows));
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Applies one keypress. Any key ends the title screen or dismisses a
    /// message; while a pattern is being typed, keys edit it instead.
    fn key(&mut self, key: KeyEvent) -> Flow {
        if let Status::Typing(input) = std::mem::replace(&mut self.status, Status::Paging) {
            self.status = match input.key(key) {
                Edit::Typing(input) => Status::Typing(input),
                Edit::Cancel => Status::Paging,
                Edit::Submit { direction, pattern } => self.submit(direction, &pattern),
            };
            return Flow::Continue;
        }
        match command_for(key) {
            Some(Command::Quit) => return Flow::Quit,
            Some(Command::Move(motion)) => self.viewport = self.viewport.moved(motion),
            Some(Command::Search(direction)) => {
                self.status = Status::Typing(SearchInput::new(direction));
            }
            Some(Command::RepeatSearch(repeat)) => self.status = self.repeat_search(repeat),
            None => {}
        }
        Flow::Continue
    }

    /// Makes a typed pattern the current search and runs it. An empty pattern
    /// runs the last one again in the newly typed direction, as in less; a
    /// pattern that does not compile leaves the last one in place.
    fn submit(&mut self, direction: Direction, pattern: &str) -> Status {
        if pattern.is_empty() {
            let Some(last) = self.search.take() else {
                return Status::Message(NO_PREVIOUS.into());
            };
            self.search = Some(last.toward(direction));
        } else {
            match Search::new(pattern, direction) {
                Ok(search) => {
                    self.search = Some(search);
                    // A new pattern starts from the screen, not from wherever
                    // the old one left off.
                    self.rendered.last_match = None;
                }
                Err(message) => return Status::Message(message),
            }
        }
        self.repeat_search(Repeat::Same)
    }

    /// Moves the next line holding a match to the top of the screen.
    fn repeat_search(&mut self, repeat: Repeat) -> Status {
        let Some(search) = &self.search else {
            return Status::Message(NO_PREVIOUS.into());
        };
        let direction = repeat.direction(search.direction());
        match search.find(&self.rendered.doc, self.anchor(), direction) {
            Some(line) => {
                self.rendered.last_match = Some(line);
                self.viewport = self.viewport.with_offset(line);
                Status::Paging
            }
            None => Status::Message(NOT_FOUND.into()),
        }
    }

    /// The line a search starts past: the top line, as in less, unless the
    /// last match is still on screen below it. That happens near the end of
    /// the file, where the viewport cannot scroll a match up to the top, and
    /// starting from the top again would find the same match forever.
    fn anchor(&self) -> usize {
        self.rendered
            .last_match
            .filter(|line| self.viewport.visible_range().contains(line))
            .unwrap_or(self.viewport.offset())
    }

    /// What the bottom row shows for the current state.
    fn prompt(&self, title: &str) -> Prompt {
        let at_bottom = self.viewport.at_bottom();
        match &self.status {
            Status::Title => Prompt::for_screen(title, true, at_bottom),
            Status::Paging => Prompt::for_screen(title, false, at_bottom),
            Status::Message(message) => Prompt::Standout(message.clone()),
            Status::Typing(input) => Prompt::Input {
                text: input.line(),
                cursor: input.cursor_column(),
            },
        }
    }

    /// Blocks for one event, then applies it and every event already queued
    /// behind it.
    ///
    /// A mouse-wheel notch reaches us as a burst of arrow keys (the terminal
    /// translates the wheel because mouse capture is off), and a resize drag
    /// as a stream of resizes. Drawing once per event made the screen fall
    /// further behind the input the longer the burst ran; draining the queue
    /// first means each frame shows where the reader actually is.
    fn apply_pending(&mut self) -> io::Result<Flow> {
        let mut event = event::read()?;
        loop {
            if self.apply(event) == Flow::Quit {
                return Ok(Flow::Quit);
            }
            if !event::poll(Duration::ZERO)? {
                return Ok(Flow::Continue);
            }
            event = event::read()?;
        }
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    title: &str,
    width: Width,
    render: impl Fn(usize) -> Document,
) -> io::Result<()> {
    let size = terminal.size()?;
    let mut session = Session::new(width, render, size.width, size.height);
    let mut painter = Painter::default();

    loop {
        let mut paint = Paint::default();
        terminal.draw(|frame| {
            let links = draw(
                frame,
                &session.rendered.doc,
                session.viewport,
                session.prompt(title),
                session.search.as_ref(),
            );
            paint = painter.plan(frame.buffer_mut(), &links);
        })?;
        paint.apply(terminal.backend_mut())?;

        if session.apply_pending()? == Flow::Quit {
            return Ok(());
        }
    }
}

/// Rows left for content once the prompt line takes the bottom row.
fn body_height(terminal_rows: u16) -> usize {
    usize::from(terminal_rows.saturating_sub(1))
}

/// What the bottom row says, following less's default prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// A bare `:`, printed plainly: mid-file, after the first screen.
    Colon,
    /// A message printed in reverse video: the file name on the first
    /// screen and/or `(END)` once the last line is visible, or what went
    /// wrong with the last command.
    Standout(String),
    /// A search pattern being typed, with the cursor `cursor` columns in.
    Input { text: String, cursor: usize },
}

impl Prompt {
    /// The prompt for a screen: `title`, then `(END)`, whichever apply.
    pub fn for_screen(title: &str, first_screen: bool, at_bottom: bool) -> Self {
        let parts: Vec<&str> = [(first_screen, title), (at_bottom, "(END)")]
            .into_iter()
            .filter_map(|(show, part)| show.then_some(part))
            .collect();
        if parts.is_empty() {
            Prompt::Colon
        } else {
            Prompt::Standout(parts.join(" "))
        }
    }

    fn render(self, frame: &mut ratatui::Frame, area: Rect) {
        match self {
            Prompt::Colon => frame.render_widget(Paragraph::new(":"), area),
            // Through a Paragraph so only the text is in reverse video, not
            // the rest of the row, as in less.
            Prompt::Standout(text) => {
                frame.render_widget(Paragraph::new(Line::styled(text, HIGHLIGHT)), area);
            }
            Prompt::Input { text, cursor } => {
                // A pattern longer than the row scrolls left under the
                // cursor, keeping the cursor on screen.
                let cursor = u16::try_from(cursor).unwrap_or(u16::MAX);
                let scroll = cursor.saturating_sub(area.width.saturating_sub(1));
                frame.render_widget(Paragraph::new(text).scroll((0, scroll)), area);
                frame.set_cursor_position((area.x.saturating_add(cursor - scroll), area.y));
            }
        }
    }
}

/// Draws the visible window and prompt, marking every match of `search`,
/// and returns where each link landed so its cells can be made clickable.
fn draw(
    frame: &mut ratatui::Frame,
    doc: &Document,
    viewport: Viewport,
    prompt: Prompt,
    search: Option<&Search>,
) -> Vec<PlacedLink> {
    let [body, prompt_row] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    // A line's column range, clipped to the body; `None` once nothing of it
    // is left on screen.
    let on_screen = |columns: &Range<usize>| {
        let clip = |column: usize| u16::try_from(column).unwrap_or(u16::MAX).min(body.width);
        let x = clip(columns.start)..clip(columns.end);
        (!x.is_empty()).then_some(x)
    };

    // Each visible line is written straight into the frame by reference;
    // cloning a screenful of styled spans per frame is wasted work when the
    // frame is redrawn for every scroll step.
    let visible = &doc.lines()[viewport.visible_range()];
    let rows = body.rows().take(usize::from(body.height));
    let mut links = Vec::new();
    for (row, line) in rows.zip(visible) {
        frame.render_widget(line.line(), row);
        let matches = search
            .into_iter()
            .flat_map(|search| search.highlights(line));
        for x in matches.filter_map(|columns| on_screen(&columns)) {
            let area = Rect::new(x.start, row.y, x.end - x.start, 1);
            frame.buffer_mut().set_style(area, HIGHLIGHT);
        }
        links.extend(line.links().iter().filter_map(|link| {
            on_screen(&link.columns).map(|x| PlacedLink {
                y: row.y,
                x,
                target: link.target.clone(),
            })
        }));
    }
    // Rows past the end of the file are marked with `~`, as in less.
    let filler = Line::from("~");
    for row in body.rows().skip(visible.len()) {
        frame.render_widget(&filler, row);
    }

    prompt.render(frame, prompt_row);
    links
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn less_style_keys_map_to_motions() {
        let down = Some(Command::Move(Motion::Down(Step::Line)));
        assert_eq!(command_for(press(KeyCode::Char('j'))), down);
        assert_eq!(command_for(press(KeyCode::Down)), down);
        assert_eq!(command_for(press(KeyCode::Enter)), down);
        assert_eq!(command_for(ctrl('e')), down);

        let up = Some(Command::Move(Motion::Up(Step::Line)));
        assert_eq!(command_for(press(KeyCode::Char('k'))), up);
        assert_eq!(command_for(press(KeyCode::Up)), up);

        let half_down = Some(Command::Move(Motion::Down(Step::HalfPage)));
        assert_eq!(command_for(press(KeyCode::Char('J'))), half_down);
        assert_eq!(command_for(press(KeyCode::Char('d'))), half_down);
        assert_eq!(command_for(ctrl('d')), half_down);
        let half_up = Some(Command::Move(Motion::Up(Step::HalfPage)));
        assert_eq!(command_for(press(KeyCode::Char('K'))), half_up);
        assert_eq!(command_for(ctrl('u')), half_up);

        let page_down = Some(Command::Move(Motion::Down(Step::Page)));
        assert_eq!(command_for(press(KeyCode::Char(' '))), page_down);
        assert_eq!(command_for(press(KeyCode::PageDown)), page_down);
        assert_eq!(command_for(ctrl('f')), page_down);
        let page_up = Some(Command::Move(Motion::Up(Step::Page)));
        assert_eq!(command_for(press(KeyCode::Char('b'))), page_up);
        assert_eq!(command_for(press(KeyCode::PageUp)), page_up);

        assert_eq!(
            command_for(press(KeyCode::Char('g'))),
            Some(Command::Move(Motion::Top))
        );
        assert_eq!(
            command_for(press(KeyCode::End)),
            Some(Command::Move(Motion::Bottom))
        );
    }

    #[test]
    fn quit_keys() {
        assert_eq!(command_for(press(KeyCode::Char('q'))), Some(Command::Quit));
        assert_eq!(command_for(press(KeyCode::Esc)), Some(Command::Quit));
        assert_eq!(command_for(ctrl('c')), Some(Command::Quit));
    }

    #[test]
    fn search_keys() {
        assert_eq!(
            command_for(press(KeyCode::Char('/'))),
            Some(Command::Search(Direction::Forward))
        );
        assert_eq!(
            command_for(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            Some(Command::Search(Direction::Backward))
        );
        assert_eq!(
            command_for(press(KeyCode::Char('n'))),
            Some(Command::RepeatSearch(Repeat::Same))
        );
        assert_eq!(
            command_for(press(KeyCode::Char('N'))),
            Some(Command::RepeatSearch(Repeat::Reverse))
        );
        assert_eq!(
            command_for(ctrl('n')),
            Some(Command::Move(Motion::Down(Step::Line)))
        );
    }

    #[test]
    fn unknown_keys_and_releases_are_ignored() {
        assert_eq!(command_for(press(KeyCode::Char('x'))), None);
        let mut release = press(KeyCode::Char('j'));
        release.kind = KeyEventKind::Release;
        assert_eq!(command_for(release), None);
    }

    #[test]
    fn motions_clamp_to_the_content() {
        let view = Viewport::new(100, 10);
        assert_eq!(view.moved(Motion::Up(Step::Page)).offset(), 0);
        assert_eq!(view.moved(Motion::Down(Step::Line)).offset(), 1);
        assert_eq!(view.moved(Motion::Down(Step::HalfPage)).offset(), 5);
        assert_eq!(view.moved(Motion::Down(Step::Page)).offset(), 10);
        assert_eq!(view.moved(Motion::Bottom).offset(), 90);
        assert_eq!(
            view.moved(Motion::Bottom)
                .moved(Motion::Down(Step::Page))
                .offset(),
            90
        );
        assert!(view.moved(Motion::Bottom).at_bottom());
        assert_eq!(view.moved(Motion::Bottom).moved(Motion::Top).offset(), 0);
    }

    #[test]
    fn content_that_fits_never_scrolls() {
        let view = Viewport::new(5, 10);
        assert_eq!(view.max_offset(), 0);
        assert_eq!(view.moved(Motion::Down(Step::Page)).offset(), 0);
        assert!(view.at_bottom());
        assert_eq!(view.visible_range(), 0..5);
    }

    #[test]
    fn steps_make_progress_in_a_tiny_terminal() {
        let view = Viewport::new(10, 1);
        assert_eq!(view.moved(Motion::Down(Step::HalfPage)).offset(), 1);
        assert_eq!(view.moved(Motion::Down(Step::Page)).offset(), 1);
        let zero = Viewport::new(10, 0);
        assert_eq!(zero.moved(Motion::Down(Step::Page)).offset(), 1);
    }

    #[test]
    fn height_change_reclamps_the_offset() {
        let view = Viewport::new(100, 10).moved(Motion::Bottom);
        assert_eq!(view.with_height(50).offset(), 50);
        assert_eq!(view.with_height(200).offset(), 0);
    }

    #[test]
    fn reflow_keeps_the_proportional_position() {
        let view = Viewport::new(100, 10).with_offset(40);
        assert_eq!(view.with_content(200).offset(), 80);
        assert_eq!(view.with_content(50).offset(), 20);
    }

    #[test]
    fn reflow_keeps_a_reader_at_the_bottom_at_the_bottom() {
        let view = Viewport::new(100, 10).moved(Motion::Bottom);
        assert!(view.with_content(300).at_bottom());
        assert!(view.with_content(30).at_bottom());
    }

    #[test]
    fn reflow_from_the_top_stays_at_the_top() {
        let view = Viewport::new(100, 10);
        assert_eq!(view.with_content(300).offset(), 0);
        assert_eq!(Viewport::new(0, 10).with_content(300).offset(), 0);
    }

    #[test]
    fn prompt_follows_less() {
        assert_eq!(
            Prompt::for_screen("README.md", true, false),
            Prompt::Standout(String::from("README.md"))
        );
        assert_eq!(
            Prompt::for_screen("README.md", true, true),
            Prompt::Standout(String::from("README.md (END)"))
        );
        assert_eq!(Prompt::for_screen("README.md", false, false), Prompt::Colon);
        assert_eq!(
            Prompt::for_screen("README.md", false, true),
            Prompt::Standout(String::from("(END)"))
        );
    }

    /// `lines` lines of text, each `link_from..` columns of which is a link.
    fn document(lines: usize, link_from: usize) -> Document {
        use crate::document::{DocLine, Run};
        use std::rc::Rc;

        Document::new(
            (0..lines)
                .map(|i| {
                    let text = format!("line {i} is wider than the screen");
                    let (plain, linked) = text.split_at(link_from);
                    DocLine::from_runs(vec![
                        Run::plain(plain, Style::default()),
                        Run::new(
                            linked,
                            Style::default(),
                            Some(Rc::from(format!("file:///{i}"))),
                        ),
                    ])
                })
                .collect(),
        )
    }

    /// Draws `lines` lines of text scrolled to `offset` on a 10x5 test
    /// terminal and returns the rows as strings.
    fn draw_rows(lines: usize, offset: usize, prompt: Prompt) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let doc = document(lines, 0);
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).expect("test terminal");
        let viewport = Viewport::new(lines, 4).with_offset(offset);
        terminal
            .draw(|frame| {
                draw(frame, &doc, viewport, prompt, None);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        assert!(buffer[(0, 4)].modifier.contains(Modifier::REVERSED));
        assert!(
            !buffer[(9, 4)].modifier.contains(Modifier::REVERSED),
            "only the prompt text stands out, not the whole row"
        );
        (0..5)
            .map(|y| (0..10).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn draw_shows_the_visible_window_truncated_to_the_width() {
        let end = Prompt::Standout("(END)".into());
        assert_eq!(
            draw_rows(6, 2, end),
            [
                "line 2 is ",
                "line 3 is ",
                "line 4 is ",
                "line 5 is ",
                "(END)     "
            ]
        );
    }

    #[test]
    fn draw_places_visible_links_by_screen_row_clipped_to_the_width() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let doc = document(6, 5);
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).expect("test terminal");
        let viewport = Viewport::new(6, 4).with_offset(2);
        let mut placed = Vec::new();
        terminal
            .draw(|frame| placed = draw(frame, &doc, viewport, Prompt::Colon, None))
            .expect("draw");

        let summary: Vec<(u16, Range<u16>, String)> = placed
            .iter()
            .map(|l| (l.y, l.x.clone(), l.target.to_string()))
            .collect();
        assert_eq!(
            summary,
            [
                (0, 5..10, String::from("file:///2")),
                (1, 5..10, String::from("file:///3")),
                (2, 5..10, String::from("file:///4")),
                (3, 5..10, String::from("file:///5")),
            ]
        );
    }

    #[test]
    fn draw_fills_rows_past_the_end_with_tildes() {
        let end = Prompt::Standout("(END)".into());
        assert_eq!(
            draw_rows(2, 0, end),
            [
                "line 0 is ",
                "line 1 is ",
                "~         ",
                "~         ",
                "(END)     "
            ]
        );
    }

    #[test]
    fn fixed_width_ignores_the_terminal() {
        let fixed = Width::Fixed(NonZeroUsize::new(72).expect("non-zero"));
        assert_eq!(fixed.columns(200), 72);
        assert_eq!(fixed.columns(10), 72);
        assert_eq!(Width::Auto.columns(120), 120);
        assert_eq!(Width::Auto.columns(0), 1);
    }

    /// One unstyled line per entry.
    fn plain(lines: &[&str]) -> Document {
        use crate::document::{DocLine, Run};

        Document::new(
            lines
                .iter()
                .map(|text| DocLine::from_runs(vec![Run::plain(*text, Style::default())]))
                .collect(),
        )
    }

    /// Twenty numbered lines: `N needle` on the lines in `needles`, `N hay`
    /// everywhere else.
    fn haystack(needles: &[usize]) -> Document {
        let lines: Vec<String> = (0..20)
            .map(|i| {
                let word = if needles.contains(&i) {
                    "needle"
                } else {
                    "hay"
                };
                format!("{i} {word}")
            })
            .collect();
        plain(&lines.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// A pager over `doc` in a 20x5 terminal: four body rows over the prompt.
    fn session(doc: Document) -> Session<impl Fn(usize) -> Document> {
        Session::new(Width::Auto, move |_| doc.clone(), 20, 5)
    }

    /// Presses each character of `typed` in turn (`\n` is Enter), none of
    /// which may quit.
    fn keys(session: &mut Session<impl Fn(usize) -> Document>, typed: &str) {
        for c in typed.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                c => KeyCode::Char(c),
            };
            assert_eq!(
                session.apply(Event::Key(press(code))),
                Flow::Continue,
                "{c:?}"
            );
        }
    }

    fn message(text: &str) -> Status {
        Status::Message(String::from(text))
    }

    #[test]
    fn search_puts_each_matching_line_at_the_top() {
        let mut pager = session(haystack(&[5, 9, 12]));

        keys(&mut pager, "/needle\n");
        assert_eq!(pager.viewport.offset(), 5);
        assert_eq!(pager.status, Status::Paging);
        keys(&mut pager, "n");
        assert_eq!(pager.viewport.offset(), 9);
        keys(&mut pager, "n");
        assert_eq!(pager.viewport.offset(), 12);
        keys(&mut pager, "N");
        assert_eq!(pager.viewport.offset(), 9);
        keys(&mut pager, "N");
        assert_eq!(pager.viewport.offset(), 5);
    }

    #[test]
    fn search_stops_at_the_last_match_until_the_next_key() {
        let mut pager = session(haystack(&[5]));

        keys(&mut pager, "/needle\nn");
        assert_eq!(pager.status, message(NOT_FOUND));
        assert_eq!(pager.viewport.offset(), 5);
        assert_eq!(pager.prompt("f"), Prompt::Standout(String::from(NOT_FOUND)));

        keys(&mut pager, "j");
        assert_eq!(pager.status, Status::Paging);
        assert_eq!(pager.viewport.offset(), 6);
    }

    #[test]
    fn matches_on_the_last_screen_are_visited_in_turn() {
        // The last screen is lines 16..20, so neither match can reach the top.
        let mut pager = session(haystack(&[17, 18]));

        keys(&mut pager, "/needle\n");
        assert_eq!(
            (pager.viewport.offset(), pager.rendered.last_match),
            (16, Some(17))
        );
        keys(&mut pager, "n");
        assert_eq!(pager.rendered.last_match, Some(18));
        keys(&mut pager, "n");
        assert_eq!(pager.status, message(NOT_FOUND));
        keys(&mut pager, "N");
        assert_eq!(pager.rendered.last_match, Some(17));
        assert_eq!(pager.status, Status::Paging);
    }

    #[test]
    fn question_mark_searches_backward_and_n_keeps_going_that_way() {
        let mut pager = session(haystack(&[2, 9]));

        keys(&mut pager, "G?needle\n");
        assert_eq!(pager.viewport.offset(), 9);
        keys(&mut pager, "n");
        assert_eq!(pager.viewport.offset(), 2);
        keys(&mut pager, "N");
        assert_eq!(pager.viewport.offset(), 9);
    }

    #[test]
    fn empty_pattern_repeats_the_last_one_in_the_new_direction() {
        let mut pager = session(haystack(&[2, 9]));

        keys(&mut pager, "/needle\n/\n");
        assert_eq!(pager.viewport.offset(), 9);
        keys(&mut pager, "?\n");
        assert_eq!(pager.viewport.offset(), 2);
        keys(&mut pager, "n");
        assert_eq!(pager.status, message(NOT_FOUND));
    }

    #[test]
    fn repeating_before_any_pattern_says_so() {
        let mut pager = session(haystack(&[2]));

        keys(&mut pager, "n");
        assert_eq!(pager.status, message(NO_PREVIOUS));
        keys(&mut pager, "?\n");
        assert_eq!(pager.status, message(NO_PREVIOUS));
        assert_eq!(pager.viewport.offset(), 0);
    }

    #[test]
    fn invalid_pattern_reports_and_keeps_the_last_search() {
        let mut pager = session(haystack(&[2, 9]));

        keys(&mut pager, "/needle\n/(\n");
        assert!(
            matches!(&pager.status, Status::Message(m) if m.starts_with("Invalid pattern")),
            "{:?}",
            pager.status
        );
        keys(&mut pager, "n");
        assert_eq!(pager.viewport.offset(), 9);
    }

    #[test]
    fn a_new_pattern_searches_from_the_screen_not_the_last_match() {
        let mut pager = session(haystack(&[16, 18]));

        keys(&mut pager, "/needle\nn");
        assert_eq!(pager.rendered.last_match, Some(18));
        keys(&mut pager, "/hay\n");
        assert_eq!(pager.rendered.last_match, Some(17));
    }

    #[test]
    fn keys_typed_into_a_pattern_neither_scroll_nor_quit() {
        let mut pager = session(haystack(&[]));

        keys(&mut pager, "/qj");
        assert_eq!(
            pager.prompt("f"),
            Prompt::Input {
                text: String::from("/qj"),
                cursor: 3
            }
        );
        assert_eq!(pager.apply(Event::Key(press(KeyCode::Esc))), Flow::Continue);
        assert_eq!(pager.status, Status::Paging);
        assert_eq!(pager.viewport.offset(), 0);

        keys(&mut pager, "/");
        assert_eq!(pager.apply(Event::Key(ctrl('c'))), Flow::Continue);
        assert_eq!(pager.status, Status::Paging);
        assert!(pager.search.is_none());
    }

    #[test]
    fn key_releases_change_nothing() {
        let mut pager = session(haystack(&[]));
        let mut release = press(KeyCode::Char('n'));
        release.kind = KeyEventKind::Release;

        pager.apply(Event::Key(release));
        assert_eq!(pager.status, Status::Title);
        keys(&mut pager, "n");
        pager.apply(Event::Key(release));
        assert_eq!(pager.status, message(NO_PREVIOUS));
    }

    #[test]
    fn rewrapping_forgets_the_last_match_but_a_height_change_does_not() {
        let mut pager = session(haystack(&[17, 18]));
        keys(&mut pager, "/needle\n");

        pager.apply(Event::Resize(20, 6));
        assert_eq!(pager.rendered.last_match, Some(17));
        pager.apply(Event::Resize(30, 6));
        assert_eq!(pager.rendered.last_match, None);
    }

    /// Draws onto a 10x5 test terminal and hands it back for inspection.
    fn drawn(
        doc: &Document,
        viewport: Viewport,
        prompt: Prompt,
        search: Option<&Search>,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let backend = ratatui::backend::TestBackend::new(10, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                draw(frame, doc, viewport, prompt, search);
            })
            .expect("draw");
        terminal
    }

    /// Row `y`'s columns shown in reverse video.
    fn reversed(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, y: u16) -> Vec<u16> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .filter(|&x| buffer[(x, y)].modifier.contains(Modifier::REVERSED))
            .collect()
    }

    #[test]
    fn draw_highlights_matches_clipped_to_the_width() {
        let doc = plain(&["a needle b", "needle", "12345678needle", "123456789 needle"]);
        let search = Search::new("needle", Direction::Forward).expect("pattern");

        let terminal = drawn(&doc, Viewport::new(4, 4), Prompt::Colon, Some(&search));

        assert_eq!(reversed(&terminal, 0), [2, 3, 4, 5, 6, 7]);
        assert_eq!(reversed(&terminal, 1), [0, 1, 2, 3, 4, 5]);
        assert_eq!(reversed(&terminal, 2), [8, 9]);
        assert!(reversed(&terminal, 3).is_empty());
        assert!(reversed(&terminal, 4).is_empty(), "the `:` prompt is plain");
    }

    #[test]
    fn draw_without_a_search_highlights_nothing() {
        let terminal = drawn(
            &plain(&["needle"]),
            Viewport::new(1, 4),
            Prompt::Colon,
            None,
        );

        assert!(reversed(&terminal, 0).is_empty());
    }

    #[test]
    fn typed_pattern_shows_with_the_cursor_after_it() {
        let input = Prompt::Input {
            text: String::from("/needle"),
            cursor: 7,
        };
        let mut terminal = drawn(&Document::default(), Viewport::new(0, 4), input, None);

        let row: String = (0..10)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol().to_string())
            .collect();
        assert_eq!(row, "/needle   ");
        assert_eq!(
            terminal.get_cursor_position().expect("cursor"),
            ratatui::layout::Position::new(7, 4)
        );
    }

    #[test]
    fn a_long_typed_pattern_scrolls_to_keep_the_cursor_on_screen() {
        let input = Prompt::Input {
            text: String::from("/abcdefghijklmnop"),
            cursor: 17,
        };
        let mut terminal = drawn(&Document::default(), Viewport::new(0, 4), input, None);

        let row: String = (0..10)
            .map(|x| terminal.backend().buffer()[(x, 4)].symbol().to_string())
            .collect();
        assert_eq!(row, "hijklmnop ");
        assert_eq!(
            terminal.get_cursor_position().expect("cursor"),
            ratatui::layout::Position::new(9, 4)
        );
    }
}
