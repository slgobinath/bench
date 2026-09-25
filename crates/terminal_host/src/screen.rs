use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line},
    term::{Config, Term, TermMode, cell::Cell, cell::Flags},
    vte::ansi::{Color, NamedColor},
};
use std::{
    fmt::Write as _,
    sync::{Arc, Mutex},
};
use vte::ansi::{Processor, StdSyncHandler};

/// Enough to restore what a user scrolls back through after a restart,
/// without letting a long-lived noisy session grow the daemon without bound.
const SCROLLBACK_LINES: usize = 10_000;

/// A session's screen, kept by feeding it everything the shell writes, so a
/// client that attaches later can be repainted to the same state.
pub(crate) struct Screen {
    term: Term<TitleListener>,
    parser: Processor<StdSyncHandler>,
    title: Arc<Mutex<Option<String>>>,
    /// While a program has the alternate screen, the drawing of the normal
    /// screen and its scrollback as they were when it switched. Alacritty
    /// keeps the inactive grid private, so this is the only way to repaint
    /// what the user will return to.
    normal_screen: Option<String>,
}

/// Switching to the alternate screen, and back. Alacritty, and so every
/// terminal Bench draws, supports no other spelling of the switch.
const ALTERNATE_SCREEN_TOGGLES: [(&[u8], bool); 2] =
    [(b"\x1b[?1049h", true), (b"\x1b[?1049l", false)];

struct Size {
    columns: usize,
    lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// The title is the only event worth keeping. Replies to queries such as
/// cursor position reports are left to the attached client's emulator, so the
/// shell does not get two answers.
struct TitleListener(Arc<Mutex<Option<String>>>);

impl EventListener for TitleListener {
    fn send_event(&self, event: Event) {
        let title = match event {
            Event::Title(title) => Some(title),
            Event::ResetTitle => None,
            _ => return,
        };
        if let Ok(mut current) = self.0.lock() {
            *current = title;
        }
    }
}

impl Screen {
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        let title = Arc::new(Mutex::new(None));
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size(cols, rows), TitleListener(title.clone())),
            parser: Processor::default(),
            title,
            normal_screen: None,
        }
    }

    /// A toggle split across two reads goes unnoticed, which only means that
    /// scrollback is missing from a repaint made while that program runs.
    pub(crate) fn advance(&mut self, mut bytes: &[u8]) {
        while let Some((start, len, enters)) = next_alternate_screen_toggle(bytes) {
            self.parser.advance(&mut self.term, &bytes[..start]);
            if enters && !self.is_alternate_screen() {
                let mut normal_screen = String::new();
                self.draw(&mut normal_screen);
                self.normal_screen = Some(normal_screen);
            }
            self.parser.advance(&mut self.term, &bytes[start..start + len]);
            bytes = &bytes[start + len..];
        }
        self.parser.advance(&mut self.term, bytes);
        if !self.is_alternate_screen() {
            self.normal_screen = None;
        }
    }

    fn is_alternate_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(size(cols, rows));
    }

    /// Escape sequences that draw this screen, its scrollback and its modes
    /// onto a freshly reset terminal of the same size.
    ///
    /// Scrollback is drawn as ordinary lines that scroll off the top, so it
    /// lands in the client's own scrollback. On the alternate screen, the
    /// normal screen saved when the program switched is drawn first, so the
    /// client has it to return to when the program exits.
    pub(crate) fn repaint(&self) -> Vec<u8> {
        let mut out = String::new();
        // Synchronized output, so the client paints the result once instead of
        // scrolling through the whole history.
        out.push_str("\x1b[?2026h");
        if self.is_alternate_screen() {
            if let Some(normal_screen) = &self.normal_screen {
                out.push_str(normal_screen);
            }
            out.push_str("\x1b[?1049h\x1b[H\x1b[2J");
        }
        self.draw(&mut out);
        push_modes(*self.term.mode(), &mut out);
        if let Ok(title) = self.title.lock()
            && let Some(title) = title.as_ref()
        {
            write!(out, "\x1b]0;{title}\x07").ok();
        }
        let cursor = self.term.grid().cursor.point;
        write!(out, "\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).ok();
        out.push_str("\x1b[?2026l");
        out.into_bytes()
    }

    /// Draws the active grid, with its scrollback unless that is the
    /// alternate screen, and leaves the cursor where the grid has it.
    fn draw(&self, out: &mut String) {
        let grid = self.term.grid();
        let columns = grid.columns();
        let screen_lines = grid.screen_lines() as i32;
        let first_line = if self.is_alternate_screen() {
            0
        } else {
            -(grid.history_size() as i32)
        };
        let mut pen = Pen::default();
        for line in first_line..screen_lines {
            let row = &grid[Line(line)];
            let wrapped = columns > 0 && row[Column(columns - 1)].flags.contains(Flags::WRAPLINE);
            let mut end = columns;
            // A wrapped line has to fill its row so the client wraps it too.
            if !wrapped {
                while end > 0 && is_blank(&row[Column(end - 1)]) {
                    end -= 1;
                }
            }
            for column in 0..end {
                let cell = &row[Column(column)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                pen.draw(cell, out);
                out.push(cell.c);
                if let Some(zerowidth) = cell.zerowidth() {
                    out.extend(zerowidth);
                }
            }
            if line + 1 < screen_lines && !wrapped {
                pen.reset(out);
                out.push_str("\r\n");
            }
        }
        out.push_str("\x1b[0m");
        let cursor = grid.cursor.point;
        write!(out, "\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).ok();
    }
}

/// The first alternate screen toggle in `bytes`: where it starts, how long it
/// is, and whether it enters the alternate screen.
fn next_alternate_screen_toggle(bytes: &[u8]) -> Option<(usize, usize, bool)> {
    ALTERNATE_SCREEN_TOGGLES
        .iter()
        .filter_map(|(sequence, enters)| {
            bytes
                .windows(sequence.len())
                .position(|window| window == *sequence)
                .map(|start| (start, sequence.len(), *enters))
        })
        .min_by_key(|(start, _, _)| *start)
}

fn size(cols: u16, rows: u16) -> Size {
    Size {
        columns: usize::from(cols.max(1)),
        lines: usize::from(rows.max(1)),
    }
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
        && (cell.flags - Flags::WRAPLINE).is_empty()
        && cell.zerowidth().is_none()
}

fn push_modes(mode: TermMode, out: &mut String) {
    let private_modes = [
        (TermMode::APP_CURSOR, 1),
        (TermMode::MOUSE_REPORT_CLICK, 1000),
        (TermMode::MOUSE_DRAG, 1002),
        (TermMode::MOUSE_MOTION, 1003),
        (TermMode::FOCUS_IN_OUT, 1004),
        (TermMode::UTF8_MOUSE, 1005),
        (TermMode::SGR_MOUSE, 1006),
        (TermMode::BRACKETED_PASTE, 2004),
    ];
    for (flag, code) in private_modes {
        if mode.contains(flag) {
            write!(out, "\x1b[?{code}h").ok();
        }
    }
    if !mode.contains(TermMode::LINE_WRAP) {
        out.push_str("\x1b[?7l");
    }
    if !mode.contains(TermMode::SHOW_CURSOR) {
        out.push_str("\x1b[?25l");
    }
    if mode.contains(TermMode::APP_KEYPAD) {
        out.push_str("\x1b=");
    }
    if mode.contains(TermMode::INSERT) {
        out.push_str("\x1b[4h");
    }
    if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
        out.push_str("\x1b[20h");
    }
    let keyboard_flags = [
        (TermMode::DISAMBIGUATE_ESC_CODES, 1),
        (TermMode::REPORT_EVENT_TYPES, 2),
        (TermMode::REPORT_ALTERNATE_KEYS, 4),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16),
    ]
    .into_iter()
    .filter(|(flag, _)| mode.contains(*flag))
    .fold(0, |flags, (_, bit)| flags | bit);
    if keyboard_flags != 0 {
        write!(out, "\x1b[={keyboard_flags};1u").ok();
    }
}

/// The graphic rendition last written, so a run of cells in the same style
/// costs one escape sequence.
#[derive(Default, PartialEq)]
struct Pen {
    style: Option<(Color, Color, Flags)>,
}

impl Pen {
    fn draw(&mut self, cell: &Cell, out: &mut String) {
        let flags = cell.flags
            & (Flags::BOLD
                | Flags::DIM
                | Flags::ITALIC
                | Flags::ALL_UNDERLINES
                | Flags::INVERSE
                | Flags::HIDDEN
                | Flags::STRIKEOUT);
        let style = (cell.fg, cell.bg, flags);
        if self.style == Some(style) {
            return;
        }
        out.push_str("\x1b[0");
        let attributes = [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ];
        for (flag, code) in attributes {
            if flags.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        push_color(cell.fg, false, out);
        push_color(cell.bg, true, out);
        out.push('m');
        self.style = Some(style);
    }

    fn reset(&mut self, out: &mut String) {
        let default = (
            Color::Named(NamedColor::Foreground),
            Color::Named(NamedColor::Background),
            Flags::empty(),
        );
        if self.style != Some(default) {
            out.push_str("\x1b[0m");
            self.style = Some(default);
        }
    }
}

fn push_color(color: Color, background: bool, out: &mut String) {
    let (normal_base, bright_base, extended) = if background {
        (40, 100, 48)
    } else {
        (30, 90, 38)
    };
    match color {
        Color::Spec(rgb) => {
            write!(out, ";{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b).ok();
        }
        Color::Indexed(index) => {
            write!(out, ";{extended};5;{index}").ok();
        }
        Color::Named(named) => {
            let index = named as usize;
            let dim_black = NamedColor::DimBlack as usize;
            let dim_white = NamedColor::DimWhite as usize;
            if index < 8 {
                write!(out, ";{}", normal_base + index).ok();
            } else if index < 16 {
                write!(out, ";{}", bright_base + index - 8).ok();
            } else if (dim_black..=dim_white).contains(&index) {
                write!(out, ";{}", normal_base + index - dim_black).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay(bytes: &[u8], cols: u16, rows: u16) -> Screen {
        let mut screen = Screen::new(cols, rows);
        screen.advance(bytes);
        let mut copy = Screen::new(cols, rows);
        copy.advance(&screen.repaint());
        copy
    }

    fn visible_text(screen: &Screen) -> Vec<String> {
        let grid = screen.term.grid();
        (0..grid.screen_lines() as i32)
            .map(|line| {
                let row = &grid[Line(line)];
                (0..grid.columns())
                    .map(|column| row[Column(column)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn repaint_restores_text_cursor_and_style() {
        let original = b"one\r\ntwo \x1b[1;31mred\x1b[0m\r\nthree";
        let mut screen = Screen::new(20, 4);
        screen.advance(original);
        let copy = replay(original, 20, 4);

        assert_eq!(visible_text(&copy), visible_text(&screen));
        assert_eq!(copy.term.grid().cursor.point, screen.term.grid().cursor.point);
        let red = &copy.term.grid()[Line(1)][Column(4)];
        assert_eq!(red.c, 'r');
        assert!(red.flags.contains(Flags::BOLD));
        assert_eq!(red.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn repaint_restores_scrollback() {
        let mut text = String::new();
        for line in 0..10 {
            write!(text, "line {line}\r\n").ok();
        }
        let copy = replay(text.as_bytes(), 20, 3);
        assert_eq!(copy.term.grid().history_size(), 8);
        assert_eq!(visible_text(&copy), vec!["line 8", "line 9", ""]);
    }

    #[test]
    fn repaint_restores_the_alternate_screen_and_modes() {
        let copy = replay(b"shell\x1b[?1049h\x1b[?2004h\x1b[2;3Hfull screen", 20, 4);
        let mode = *copy.term.mode();
        assert!(mode.contains(TermMode::ALT_SCREEN));
        assert!(mode.contains(TermMode::BRACKETED_PASTE));
        assert_eq!(visible_text(&copy)[1], "  full screen");
    }

    #[test]
    fn repaint_keeps_the_normal_screen_behind_the_alternate_screen() {
        let mut text = String::new();
        for line in 0..10 {
            write!(text, "line {line}\r\n").ok();
        }
        text.push_str("$ vim\x1b[?1049h\x1b[Hediting");
        let mut copy = replay(text.as_bytes(), 20, 3);
        assert_eq!(visible_text(&copy)[0], "editing");

        copy.advance(b"\x1b[?1049l");
        assert_eq!(visible_text(&copy), vec!["line 8", "line 9", "$ vim"]);
        assert_eq!(copy.term.grid().history_size(), 8);
        assert_eq!(copy.term.grid().cursor.point.column.0, 5);
    }

    #[test]
    fn leaving_the_alternate_screen_drops_the_saved_normal_screen() {
        let mut screen = Screen::new(20, 3);
        screen.advance(b"shell\x1b[?1049hvim\x1b[?1049l");
        assert!(screen.normal_screen.is_none());
        screen.advance(b"\x1b[?1049h");
        assert!(screen.normal_screen.is_some());
    }

    #[test]
    fn repaint_keeps_wrapped_lines_wrapped() {
        let copy = replay(b"abcdefghij", 6, 3);
        assert_eq!(visible_text(&copy), vec!["abcdef", "ghij", ""]);
        assert!(copy.term.grid()[Line(0)][Column(5)]
            .flags
            .contains(Flags::WRAPLINE));
    }
}
