//! Small, scrollback-native ANSI/VT presentation primitives.
//!
//! [`InlineTerminal`] deliberately retains only the current mutable tail. It
//! writes settled rows once to the main screen and borrows the alternate screen
//! only for explicitly modal surfaces.
#![forbid(unsafe_code)]

use std::io::{self, Write};

/// A terminal size in cells.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Size {
    /// Width in columns.
    pub width: u16,
    /// Height in rows.
    pub height: u16,
}

/// A terminal rectangle.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width in columns.
    pub width: u16,
    /// Height in rows.
    pub height: u16,
}

/// ANSI terminal colors used by the tea presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Color {
    Black,
    DarkGrey,
    Red,
    DarkRed,
    Green,
    DarkGreen,
    Yellow,
    DarkYellow,
    Blue,
    DarkBlue,
    Magenta,
    DarkMagenta,
    Cyan,
    DarkCyan,
    White,
    Grey,
}

/// The small style vocabulary needed by tea's line renderer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Style {
    /// Optional foreground color.
    pub foreground: Option<Color>,
    /// Optional background color.
    pub background: Option<Color>,
    /// Whether the text is bold.
    pub bold: bool,
}

/// One style run in a visible row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    /// Untrusted printable text. ANSI/VT controls are escaped by the renderer.
    pub text: String,
    /// Style applied to the span.
    pub style: Style,
}

impl Span {
    /// Construct a text span.
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// One semantic, physical terminal row.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StyledLine {
    spans: Vec<Span>,
}

impl StyledLine {
    /// Construct one uniformly styled row.
    pub fn plain(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![Span::new(text, style)],
        }
    }

    /// Construct a row from explicitly styled runs.
    pub fn from_spans(spans: Vec<Span>) -> Self {
        Self { spans }
    }

    /// Borrow the style runs in this row.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Append one style run, coalescing immediately adjacent equal styles.
    pub fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut().filter(|last| last.style == style) {
            last.text.push_str(&text);
        } else {
            self.spans.push(Span::new(text, style));
        }
    }

    /// Return visible text without terminal-control escaping.
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

/// A cursor location relative to the mutable live region.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cursor {
    /// Zero-based terminal column.
    pub column: u16,
    /// Zero-based row in the mutable live region.
    pub row: u16,
    /// Whether the cursor should be visible.
    pub visible: bool,
}

/// A compact ANSI/VT renderer with a permanent main-screen prefix and a
/// mutable inline tail. It does not retain a terminal-sized framebuffer.
pub struct InlineTerminal<W> {
    writer: W,
    live_rows: usize,
    cursor_row: usize,
    live_lines: Vec<StyledLine>,
    live_size: Option<Size>,
    live_cursor: Option<Cursor>,
    alternate_screen: bool,
    surface_lines: Vec<StyledLine>,
    surface_size: Option<Size>,
    surface_cursor: Option<Cursor>,
}

impl<W: Write> InlineTerminal<W> {
    /// Create an inline terminal over an owned writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            live_rows: 0,
            cursor_row: 0,
            live_lines: Vec::new(),
            live_size: None,
            live_cursor: None,
            alternate_screen: false,
            surface_lines: Vec::new(),
            surface_size: None,
            surface_cursor: None,
        }
    }

    /// Borrow the output writer.
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Whether an explicit modal surface currently owns the alternate screen.
    pub const fn alternate_screen_active(&self) -> bool {
        self.alternate_screen
    }

    /// Permanently append rows to native main-screen scrollback.
    ///
    /// Existing live content is first removed from its known origin. Once this
    /// method succeeds, the rows are never retained or redrawn by this type.
    pub fn commit(&mut self, lines: &[StyledLine]) -> io::Result<()> {
        self.ensure_main_screen()?;
        self.clear_live()?;
        for line in lines {
            self.write_line(line)?;
            // CR explicitly cancels a pending last-column wrap before LF
            // advances to the next committed physical row.
            self.writer.write_all(b"\r\n")?;
        }
        self.writer.write_all(b"\x1b[0m")?;
        self.writer.flush()
    }

    /// Repaint the bounded mutable main-screen tail.
    pub fn draw_live(
        &mut self,
        lines: &[StyledLine],
        size: Size,
        cursor: Option<Cursor>,
    ) -> io::Result<()> {
        self.ensure_main_screen()?;
        if size.width == 0 || size.height == 0 {
            if self.live_size == Some(size) && self.live_cursor.is_none() && self.live_rows == 0 {
                return Ok(());
            }
            self.clear_live()?;
            self.writer.write_all(b"\x1b[0m\x1b[?25l")?;
            self.live_size = Some(size);
            self.live_cursor = None;
            return self.writer.flush();
        }

        let count = lines.len().min(usize::from(size.height));
        let cursor = cursor.unwrap_or(Cursor {
            column: 0,
            row: count.saturating_sub(1) as u16,
            visible: false,
        });
        let cursor = Cursor {
            row: cursor.row.min(count.saturating_sub(1) as u16),
            ..cursor
        };
        let visible = &lines[..count];
        if self.live_size == Some(size)
            && self.live_cursor == Some(cursor)
            && self.live_lines == visible
        {
            return Ok(());
        }
        self.clear_live()?;
        for line in &lines[..count] {
            self.write_line_clipped(line, size.width)?;
            self.writer.write_all(b"\r\n")?;
        }
        self.live_rows = count;
        let target_row = usize::from(cursor.row).min(count.saturating_sub(1));
        let up = count.saturating_sub(target_row);
        if up != 0 {
            write!(self.writer, "\x1b[{up}A")?;
        }
        write!(
            self.writer,
            "\r\x1b[{}G\x1b[0m\x1b[?25{}",
            cursor.column.saturating_add(1),
            if cursor.visible { 'h' } else { 'l' }
        )?;
        self.cursor_row = target_row;
        self.live_lines = visible.to_vec();
        self.live_size = Some(size);
        self.live_cursor = Some(cursor);
        self.writer.flush()
    }

    /// Enter the alternate screen for a temporary full-screen surface.
    pub fn enter_surface(&mut self) -> io::Result<()> {
        if !self.alternate_screen {
            self.writer.write_all(b"\x1b[?1049h")?;
            self.alternate_screen = true;
        }
        Ok(())
    }

    /// Render a temporary full-screen surface. Main-screen history is untouched.
    pub fn draw_surface(
        &mut self,
        lines: &[StyledLine],
        size: Size,
        cursor: Option<Cursor>,
    ) -> io::Result<()> {
        self.enter_surface()?;
        let cursor = cursor.unwrap_or(Cursor {
            column: 0,
            row: 0,
            visible: false,
        });
        if self.surface_size == Some(size)
            && self.surface_cursor == Some(cursor)
            && self.surface_lines == lines
        {
            return Ok(());
        }
        self.writer.write_all(b"\x1b[H\x1b[2J")?;
        let row_count = lines.len().min(usize::from(size.height));
        if size.width != 0 {
            for (row, line) in lines[..row_count].iter().enumerate() {
                write!(self.writer, "\x1b[{};1H", row.saturating_add(1))?;
                self.write_line_clipped(line, size.width)?;
            }
        }
        write!(
            self.writer,
            "\x1b[{};{}H\x1b[0m\x1b[?25{}",
            cursor.row.saturating_add(1),
            cursor.column.saturating_add(1),
            if cursor.visible { 'h' } else { 'l' }
        )?;
        self.surface_lines = lines.to_vec();
        self.surface_size = Some(size);
        self.surface_cursor = Some(cursor);
        self.writer.flush()
    }

    /// Return from a temporary full-screen surface to the untouched main screen.
    pub fn leave_surface(&mut self) -> io::Result<()> {
        if self.alternate_screen {
            self.writer.write_all(b"\x1b[0m\x1b[?1049l")?;
            self.alternate_screen = false;
            self.surface_lines.clear();
            self.surface_size = None;
            self.surface_cursor = None;
        }
        Ok(())
    }

    /// Clear only the known mutable region, then reset terminal styling.
    pub fn finish(&mut self) -> io::Result<()> {
        self.leave_surface()?;
        self.clear_live()?;
        self.writer.write_all(b"\x1b[0m\x1b[?25h")?;
        self.writer.flush()
    }

    /// Forget the mutable-tail position after an external program owned the terminal.
    pub fn invalidate_live_region(&mut self) {
        self.live_rows = 0;
        self.cursor_row = 0;
        self.live_lines.clear();
        self.live_size = None;
        self.live_cursor = None;
    }

    fn ensure_main_screen(&mut self) -> io::Result<()> {
        self.leave_surface()
    }

    fn clear_live(&mut self) -> io::Result<()> {
        if self.live_rows == 0 {
            return Ok(());
        }
        if self.cursor_row != 0 {
            write!(self.writer, "\x1b[{}A", self.cursor_row)?;
        }
        // This begins at the live origin, never at the top of the main screen.
        self.writer.write_all(b"\r\x1b[J\x1b[0m")?;
        self.live_rows = 0;
        self.cursor_row = 0;
        self.live_lines.clear();
        self.live_size = None;
        self.live_cursor = None;
        Ok(())
    }

    fn write_line(&mut self, line: &StyledLine) -> io::Result<()> {
        for span in line.spans() {
            write_style(&mut self.writer, span.style)?;
            write_escaped(&mut self.writer, &span.text)?;
        }
        self.writer.write_all(b"\x1b[0m")
    }

    fn write_line_clipped(&mut self, line: &StyledLine, width: u16) -> io::Result<()> {
        let mut available = usize::from(width);
        for span in line.spans() {
            if available == 0 {
                break;
            }
            write_style(&mut self.writer, span.style)?;
            available = write_escaped_clipped(&mut self.writer, &span.text, available)?;
        }
        self.writer.write_all(b"\x1b[0m")
    }
}

fn write_style(writer: &mut impl Write, style: Style) -> io::Result<()> {
    writer.write_all(b"\x1b[0m")?;
    if let Some(foreground) = style.foreground {
        write!(writer, "\x1b[38;5;{}m", color_index(foreground))?;
    }
    if let Some(background) = style.background {
        write!(writer, "\x1b[48;5;{}m", color_index(background))?;
    }
    if style.bold {
        writer.write_all(b"\x1b[1m")?;
    }
    Ok(())
}

fn write_escaped(writer: &mut impl Write, text: &str) -> io::Result<()> {
    for character in text.chars() {
        write_printable(writer, character)?;
    }
    Ok(())
}

fn write_escaped_clipped(
    writer: &mut impl Write,
    text: &str,
    mut available: usize,
) -> io::Result<usize> {
    for character in text.chars() {
        let width = display_width(character);
        if width > available {
            break;
        }
        write_printable(writer, character)?;
        available = available.saturating_sub(width);
    }
    Ok(available)
}

fn write_printable(writer: &mut impl Write, character: char) -> io::Result<()> {
    match character {
        // Text APIs are line-oriented; all C0/C1 controls are rendered visibly
        // so untrusted model or repository text cannot escape this renderer.
        '\u{1b}' => writer.write_all("␛".as_bytes()),
        '\0'..='\u{1f}' | '\u{7f}'..='\u{9f}' => writer.write_all("�".as_bytes()),
        character => write!(writer, "{character}"),
    }
}

const fn color_index(color: Color) -> u8 {
    match color {
        Color::Black => 0,
        Color::DarkGrey => 8,
        Color::Red => 9,
        Color::DarkRed => 1,
        Color::Green => 10,
        Color::DarkGreen => 2,
        Color::Yellow => 11,
        Color::DarkYellow => 3,
        Color::Blue => 12,
        Color::DarkBlue => 4,
        Color::Magenta => 13,
        Color::DarkMagenta => 5,
        Color::Cyan => 14,
        Color::DarkCyan => 6,
        Color::White => 15,
        Color::Grey => 7,
    }
}

fn display_width(symbol: char) -> usize {
    if symbol == '\u{200d}'
        || matches!(symbol, '\u{fe0e}' | '\u{fe0f}')
        || ('\u{300}'..='\u{36f}').contains(&symbol)
        || ('\u{1ab0}'..='\u{1aff}').contains(&symbol)
        || ('\u{20d0}'..='\u{20ff}').contains(&symbol)
        || ('\u{fe20}'..='\u{fe2f}').contains(&symbol)
    {
        0
    } else if matches!(
        symbol,
        '\u{1100}'..='\u{115f}'
            | '\u{2329}'..='\u{232a}'
            | '\u{2e80}'..='\u{a4cf}'
            | '\u{ac00}'..='\u{d7a3}'
            | '\u{f900}'..='\u{faff}'
            | '\u{fe10}'..='\u{fe19}'
            | '\u{fe30}'..='\u{fe6f}'
            | '\u{ff00}'..='\u{ff60}'
            | '\u{ffe0}'..='\u{ffe6}'
            | '\u{1f300}'..='\u{1faff}'
    ) {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> StyledLine {
        StyledLine::plain(text, Style::default())
    }

    #[test]
    fn commits_are_main_screen_rows_written_once() {
        let mut output = Vec::new();
        let mut terminal = InlineTerminal::new(&mut output);
        terminal.commit(&[line("settled"), line("history")]).unwrap();
        terminal.draw_live(&[line("composer")], Size { width: 20, height: 5 }, None).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("settled").count(), 1);
        assert_eq!(output.matches("history").count(), 1);
        assert!(!output.contains("\x1b[2J"));
        assert!(!output.contains("?1049h"));
        assert!(output.contains("settled\x1b[0m\r\n"));
    }

    #[test]
    fn live_redraw_returns_to_its_origin_without_duplicate_history() {
        let mut output = Vec::new();
        let mut terminal = InlineTerminal::new(&mut output);
        terminal.draw_live(&[line("first"), line("tail")], Size { width: 20, height: 5 }, Some(Cursor { column: 0, row: 1, visible: true })).unwrap();
        terminal.draw_live(&[line("second")], Size { width: 20, height: 5 }, Some(Cursor { column: 0, row: 0, visible: true })).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("first").count(), 1);
        assert_eq!(output.matches("second").count(), 1);
        assert!(output.contains("\x1b[1A\r\x1b[J"));
        assert!(!output.contains("\x1b[2J"));
    }

    #[test]
    fn tiny_sizes_and_full_width_rows_are_safe() {
        let mut output = Vec::new();
        let mut terminal = InlineTerminal::new(&mut output);
        terminal.draw_live(&[line("abcdef")], Size { width: 0, height: 0 }, None).unwrap();
        terminal.draw_live(&[line("abcd")], Size { width: 4, height: 1 }, Some(Cursor { column: 3, row: 0, visible: true })).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("abcd\x1b[0m\r\n"));
        assert!(!output.contains("?7h"));
        assert!(!output.contains("?7l"));
        assert!(!output.contains("[1;1r"));
    }

    #[test]
    fn modal_surface_pairs_alternate_screen_without_touching_main_mode() {
        let mut output = Vec::new();
        let mut terminal = InlineTerminal::new(&mut output);
        terminal.draw_live(&[line("main")], Size { width: 20, height: 5 }, None).unwrap();
        terminal.draw_surface(&[line("modal")], Size { width: 20, height: 5 }, None).unwrap();
        assert!(terminal.alternate_screen_active());
        terminal.leave_surface().unwrap();
        assert!(!terminal.alternate_screen_active());
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("?1049h").count(), 1);
        assert_eq!(output.matches("?1049l").count(), 1);
        assert!(!output.contains("?7h"));
        assert!(!output.contains("?7l"));
    }

    #[test]
    fn untrusted_controls_are_not_emitted() {
        let mut output = Vec::new();
        let mut terminal = InlineTerminal::new(&mut output);
        terminal.commit(&[line("safe\x1b[2J\ntext")]).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("safe\x1b[2J"));
        assert!(output.contains("safe␛[2J�text"));
    }
}
