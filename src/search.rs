//! Less-style search over the rendered document: the pattern being typed at
//! the prompt, the compiled pattern, which line it finds next, and which
//! columns to highlight.
//!
//! Assumes a pattern is matched against each visual line's displayed text,
//! so what you can search is exactly what you see — resolved link paths,
//! table borders and blockquote bars included — and a match never spans a
//! line break the renderer introduced by wrapping.
//!
//! Gotcha: like less, searching moves by line, not by match: repeating a
//! search goes to the next *line* holding a match, however many matches the
//! current line has.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::document::{DocLine, Document};

/// Which way a search looks from where it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

impl Direction {
    pub fn reversed(self) -> Self {
        match self {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        }
    }

    /// The key that starts a search this way, shown in front of the pattern.
    fn prompt(self) -> char {
        match self {
            Direction::Forward => '/',
            Direction::Backward => '?',
        }
    }
}

/// A compiled pattern and the direction it was entered in.
#[derive(Clone, Debug)]
pub struct Search {
    regex: Regex,
    direction: Direction,
}

impl Search {
    /// Compiles `pattern` as a regular expression. The error is a one-line
    /// message sized for the prompt row.
    pub fn new(pattern: &str, direction: Direction) -> Result<Self, String> {
        Regex::new(pattern)
            .map(|regex| Self { regex, direction })
            .map_err(|err| invalid_pattern(&err))
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The same pattern as if it had been entered in `direction`.
    pub fn toward(self, direction: Direction) -> Self {
        Self { direction, ..self }
    }

    /// The nearest line past `anchor` in `direction` that holds a match.
    ///
    /// `anchor` itself is never a candidate, so searching again from the line
    /// a search landed on always moves on.
    pub fn find(&self, doc: &Document, anchor: usize, direction: Direction) -> Option<usize> {
        let hit = |line: &DocLine| self.regex.is_match(&line.text());
        let lines = doc.lines().iter();
        match direction {
            Direction::Forward => lines
                .enumerate()
                .skip(anchor.saturating_add(1))
                .find_map(|(index, line)| hit(line).then_some(index)),
            Direction::Backward => lines.take(anchor).rposition(hit),
        }
    }

    /// The display columns of every match on `line`, in order. Empty matches
    /// (`^`, `x*`) are left out: they find a line but cover no cells.
    pub fn highlights(&self, line: &DocLine) -> Vec<Range<usize>> {
        let text = line.text();
        self.regex
            .find_iter(&text)
            .filter(|found| !found.is_empty())
            .map(|found| {
                let start = text[..found.start()].width();
                start..start + found.as_str().width()
            })
            .collect()
    }
}

/// Condenses a compile error to the one line the prompt row has room for:
/// the reason, without the pattern-and-caret diagram the regex crate draws
/// above it.
fn invalid_pattern(err: &regex::Error) -> String {
    let detail = err.to_string();
    let reason = detail
        .lines()
        .find_map(|line| line.strip_prefix("error: "))
        .map_or_else(
            || detail.split_whitespace().collect::<Vec<_>>().join(" "),
            str::to_string,
        );
    format!("Invalid pattern: {reason}")
}

/// A pattern being typed at the prompt.
///
/// The text is kept split at the cursor, so the cursor can only ever sit
/// between two graphemes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchInput {
    direction: Direction,
    before: String,
    after: String,
}

/// What one keypress did to a pattern being typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Still typing.
    Typing(SearchInput),
    /// Enter: search `direction` for `pattern`. An empty pattern means
    /// "the last one again", as in less.
    Submit {
        direction: Direction,
        pattern: String,
    },
    /// Esc, ^C, or Backspace on an empty line: back to paging.
    Cancel,
}

impl SearchInput {
    pub fn new(direction: Direction) -> Self {
        Self {
            direction,
            before: String::new(),
            after: String::new(),
        }
    }

    /// Applies one keypress. Unknown keys leave the input as it was.
    pub fn key(mut self, key: KeyEvent) -> Edit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match (key.code, ctrl) {
            (KeyCode::Enter, _) => {
                return Edit::Submit {
                    direction: self.direction,
                    pattern: self.before + &self.after,
                };
            }
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => return Edit::Cancel,
            (KeyCode::Backspace, _) | (KeyCode::Char('h'), true) => {
                if self.before.is_empty() && self.after.is_empty() {
                    return Edit::Cancel;
                }
                pop_grapheme(&mut self.before);
            }
            (KeyCode::Delete, _) => {
                shift_grapheme(&mut self.after);
            }
            (KeyCode::Left, _) => {
                if let Some(grapheme) = pop_grapheme(&mut self.before) {
                    self.after.insert_str(0, &grapheme);
                }
            }
            (KeyCode::Right, _) => {
                if let Some(grapheme) = shift_grapheme(&mut self.after) {
                    self.before.push_str(&grapheme);
                }
            }
            (KeyCode::Home, _) => {
                let before = std::mem::take(&mut self.before);
                self.after.insert_str(0, &before);
            }
            (KeyCode::End, _) => {
                let after = std::mem::take(&mut self.after);
                self.before.push_str(&after);
            }
            (KeyCode::Char('u'), true) => {
                self.before.clear();
                self.after.clear();
            }
            (KeyCode::Char(c), false) if !alt => self.before.push(c),
            _ => {}
        }
        Edit::Typing(self)
    }

    /// The prompt row: `/` or `?`, then the pattern.
    pub fn line(&self) -> String {
        format!("{}{}{}", self.direction.prompt(), self.before, self.after)
    }

    /// The cursor's display column within [`line`](Self::line).
    pub fn cursor_column(&self) -> usize {
        1 + self.before.width()
    }
}

/// Removes and returns the last grapheme of `text`.
fn pop_grapheme(text: &mut String) -> Option<String> {
    let (start, _) = text.grapheme_indices(true).next_back()?;
    Some(text.split_off(start))
}

/// Removes and returns the first grapheme of `text`.
fn shift_grapheme(text: &mut String) -> Option<String> {
    let len = text.graphemes(true).next()?.len();
    let rest = text.split_off(len);
    Some(std::mem::replace(text, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Run;
    use ratatui::style::Style;

    fn document(lines: &[&str]) -> Document {
        Document::new(
            lines
                .iter()
                .map(|text| DocLine::from_runs(vec![Run::plain(*text, Style::default())]))
                .collect(),
        )
    }

    fn search(pattern: &str) -> Search {
        Search::new(pattern, Direction::Forward).expect("valid pattern")
    }

    #[test]
    fn find_skips_the_anchor_line_in_both_directions() {
        let doc = document(&["cat", "dog", "cat", "dog", "cat"]);
        let cat = search("cat");

        assert_eq!(cat.find(&doc, 0, Direction::Forward), Some(2));
        assert_eq!(cat.find(&doc, 2, Direction::Forward), Some(4));
        assert_eq!(cat.find(&doc, 4, Direction::Forward), None);
        assert_eq!(cat.find(&doc, 4, Direction::Backward), Some(2));
        assert_eq!(cat.find(&doc, 2, Direction::Backward), Some(0));
        assert_eq!(cat.find(&doc, 0, Direction::Backward), None);
    }

    #[test]
    fn find_tolerates_anchors_outside_the_document() {
        let doc = document(&["cat"]);
        let cat = search("cat");

        assert_eq!(cat.find(&doc, usize::MAX, Direction::Forward), None);
        assert_eq!(cat.find(&doc, usize::MAX, Direction::Backward), Some(0));
        assert_eq!(cat.find(&Document::default(), 0, Direction::Forward), None);
        assert_eq!(cat.find(&Document::default(), 0, Direction::Backward), None);
    }

    #[test]
    fn find_matches_across_styled_runs() {
        let line = DocLine::from_runs(vec![
            Run::plain("ne", Style::default()),
            Run::plain("edle", Style::default().fg(ratatui::style::Color::Red)),
        ]);
        let doc = Document::new(vec![DocLine::default(), line]);

        assert_eq!(search("needle").find(&doc, 0, Direction::Forward), Some(1));
    }

    #[test]
    fn highlights_are_display_columns() {
        let line = DocLine::from_runs(vec![Run::plain("界 ab 界ab", Style::default())]);

        assert_eq!(search("ab").highlights(&line), [3..5, 8..10]);
        assert_eq!(search("界.").highlights(&line), [0..3, 6..9]);
    }

    #[test]
    fn empty_matches_are_not_highlighted() {
        let line = DocLine::from_runs(vec![Run::plain("abc", Style::default())]);

        assert!(search("x*").highlights(&line).is_empty());
        assert_eq!(
            search("x*").find(&document(&["a", "b"]), 0, Direction::Forward),
            Some(1)
        );
    }

    #[test]
    fn invalid_patterns_explain_themselves_on_one_line() {
        let err = Search::new("(open", Direction::Forward).expect_err("invalid");

        assert!(err.starts_with("Invalid pattern: "), "{err}");
        assert!(!err.contains('\n'), "{err}");
        assert!(err.len() > "Invalid pattern: ".len(), "{err}");
    }

    #[test]
    fn toward_keeps_the_pattern_and_changes_the_direction() {
        let back = search("cat").toward(Direction::Backward);

        assert_eq!(back.direction(), Direction::Backward);
        assert_eq!(
            back.find(&document(&["cat", "dog"]), 1, back.direction()),
            Some(0)
        );
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Feeds `keys` to `input`, which must still be typing afterwards.
    fn typed(input: SearchInput, keys: impl IntoIterator<Item = KeyEvent>) -> SearchInput {
        keys.into_iter()
            .fold(input, |input, key| match input.key(key) {
                Edit::Typing(input) => input,
                other => panic!("stopped typing at {key:?}: {other:?}"),
            })
    }

    fn chars(text: &str) -> Vec<KeyEvent> {
        text.chars().map(|c| press(KeyCode::Char(c))).collect()
    }

    #[test]
    fn typing_builds_the_prompt_line() {
        let input = typed(SearchInput::new(Direction::Forward), chars("needle"));

        assert_eq!(input.line(), "/needle");
        assert_eq!(input.cursor_column(), 7);
        assert_eq!(
            input.key(press(KeyCode::Enter)),
            Edit::Submit {
                direction: Direction::Forward,
                pattern: String::from("needle"),
            }
        );
    }

    #[test]
    fn backward_input_is_prompted_with_a_question_mark() {
        let input = typed(SearchInput::new(Direction::Backward), chars("q"));

        assert_eq!(input.line(), "?q");
    }

    #[test]
    fn cursor_keys_edit_in_the_middle() {
        let input = typed(
            SearchInput::new(Direction::Forward),
            [
                chars("acd"),
                vec![press(KeyCode::Left), press(KeyCode::Left)],
                chars("b"),
                vec![press(KeyCode::Delete)],
            ]
            .concat(),
        );
        assert_eq!(input.line(), "/abd");
        assert_eq!(input.cursor_column(), 3);

        let input = typed(input, [press(KeyCode::Home)]);
        assert_eq!(input.cursor_column(), 1);
        let input = typed(input, [press(KeyCode::Backspace), press(KeyCode::Right)]);
        assert_eq!((input.line().as_str(), input.cursor_column()), ("/abd", 2));
        let input = typed(input, [press(KeyCode::End)]);
        assert_eq!(input.cursor_column(), 4);
    }

    #[test]
    fn edits_move_by_grapheme() {
        let input = typed(
            SearchInput::new(Direction::Forward),
            [chars("xe\u{301}界"), vec![press(KeyCode::Left)]].concat(),
        );
        assert_eq!(input.cursor_column(), 3);

        let input = typed(input, [press(KeyCode::Backspace)]);
        assert_eq!(input.line(), "/x界");
        assert_eq!(input.cursor_column(), 2);
    }

    #[test]
    fn ctrl_u_clears_and_ctrl_h_deletes() {
        let input = typed(SearchInput::new(Direction::Forward), chars("abc"));

        assert_eq!(typed(input.clone(), [ctrl('h')]).line(), "/ab");
        assert_eq!(typed(input, [ctrl('u')]).line(), "/");
    }

    #[test]
    fn escape_ctrl_c_and_backspacing_past_the_start_cancel() {
        let input = typed(SearchInput::new(Direction::Forward), chars("a"));

        assert_eq!(input.clone().key(press(KeyCode::Esc)), Edit::Cancel);
        assert_eq!(input.clone().key(ctrl('c')), Edit::Cancel);
        let emptied = typed(input, [press(KeyCode::Backspace)]);
        assert_eq!(emptied.key(press(KeyCode::Backspace)), Edit::Cancel);
    }

    #[test]
    fn backspace_at_the_start_of_a_non_empty_line_keeps_it() {
        let input = typed(
            SearchInput::new(Direction::Forward),
            [
                chars("ab"),
                vec![press(KeyCode::Home), press(KeyCode::Backspace)],
            ]
            .concat(),
        );

        assert_eq!(input.line(), "/ab");
    }

    #[test]
    fn control_and_alt_chords_are_not_typed() {
        let input = typed(
            SearchInput::new(Direction::Forward),
            [
                ctrl('x'),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
                press(KeyCode::Tab),
            ],
        );

        assert_eq!(input.line(), "/");
    }

    #[test]
    fn shifted_characters_are_typed() {
        let input = typed(
            SearchInput::new(Direction::Forward),
            [KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)],
        );

        assert_eq!(input.line(), "/A");
    }
}
