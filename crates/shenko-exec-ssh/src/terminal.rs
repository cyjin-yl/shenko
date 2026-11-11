//! Terminal screen state machine for seat observation — docs/specs/remote-seats.md#spec-17-5, docs/specs/remote-seats.md#spec-35-5.
//!
//! A seat's screen is not a bag of text. `tmux capture-pane -e` and a real
//! PTY both deliver a byte stream whose meaning is carried by ANSI/VT
//! control sequences: SGR attributes (foreground/background/reverse/…),
//! cursor movement, erases, scroll regions and the alternate screen.
//! This module reconstructs the *screen* those bytes describe: a grid of
//! styled cells, the cursor, the size and a monotonic revision.
//!
//! Three facts the reconstruction deliberately does not invent:
//!
//! * **Cursor position is not a selection.** The cursor is reported as a
//!   coordinate; nothing here claims which option a TUI has highlighted.
//! * **Attributes are preserved, not summarised.** A reverse-video run stays
//!   a reverse-video run in the emitted text runs, so a caller can look for
//!   the highlighted option instead of guessing from option order.
//! * **Unknown state stays unknown.** Bytes that are not understood are
//!   consumed without inventing content, and the parser records that it saw
//!   something it did not model.
//!
//! The parser is a bounded subset of the xterm/VT100 repertoire that real
//! agent TUIs actually use. It is not a complete terminal emulator: it has no
//! scrollback history, no mouse reporting, no bracketed-paste buffer and no
//! ligature or grapheme clustering. Those limits are reported through
//! `TerminalLimits` so a caller can state what it did not observe.

use serde::{Deserialize, Serialize};

/// Colour of one cell, as the terminal's SGR state described it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TerminalColor {
    /// The terminal's own default foreground/background.
    Default,
    /// A palette index (0–255) from the 16/256 colour SGR forms.
    Indexed(u8),
    /// A direct 24-bit colour from the `38;2;r;g;b` SGR form.
    Rgb(u8, u8, u8),
}

/// The attributes a cell carries. Defaults are "no attribute set".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellStyle {
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub hidden: bool,
    pub strike: bool,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            foreground: TerminalColor::Default,
            background: TerminalColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            hidden: false,
            strike: false,
        }
    }
}

impl CellStyle {
    /// True when this style visually marks a run as selected/highlighted.
    ///
    /// Only attributes that actually invert or emphasise the glyph count.
    /// Bold text is not a selection: many TUIs bold their prompt.
    pub fn marks_selection(&self) -> bool {
        self.reverse || self.background != TerminalColor::Default
    }
}

/// One grid cell. A cell that continues a double-width glyph carries no
/// character of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenCell {
    pub ch: char,
    pub style: CellStyle,
    /// True for the second cell of a double-width glyph.
    pub continuation: bool,
}

impl ScreenCell {
    fn blank(style: CellStyle) -> Self {
        Self {
            ch: ' ',
            style,
            continuation: false,
        }
    }
}

/// A styled run of one line, addressed in **character** offsets of that
/// line's text (not screen columns), so a caller can slice the exact text it
/// saw highlighted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleRun {
    pub start: usize,
    pub end: usize,
    pub style: CellStyle,
}

/// One reconstructed screen line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenLine {
    pub text: String,
    pub runs: Vec<StyleRun>,
}

/// The reconstructed screen at one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenGrid {
    pub columns: u16,
    pub rows: u16,
    /// Monotonic within one `TerminalScreen`; bumped by any content or
    /// attribute change. It is *not* a question revision: an unrelated log
    /// line also moves it.
    pub revision: u64,
    pub cursor_column: u16,
    pub cursor_row: u16,
    pub cursor_visible: bool,
    /// True when the terminal was showing its alternate screen.
    pub alternate_screen: bool,
    pub lines: Vec<ScreenLine>,
}

impl ScreenGrid {
    /// The whole screen as plain text, one line per row, trailing blanks kept.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&line.text);
        }
        out
    }

    /// The screen with trailing blanks removed per line — the form that goes
    /// into a model's context. The styled grid stays available separately.
    pub fn trimmed_text(&self) -> String {
        let mut out = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(line.text.trim_end());
        }
        out
    }

    /// The character range of the first run in `line` that marks a selection,
    /// if the terminal showed one.
    pub fn highlighted_range(&self, line: usize) -> Option<(usize, usize)> {
        let runs = self.lines.get(line)?.runs.as_slice();
        let mut found: Option<(usize, usize)> = None;
        for run in runs {
            if run.style.marks_selection() {
                match found {
                    None => found = Some((run.start, run.end)),
                    Some((start, _)) if run.start != start => {
                        // Two separate highlighted runs in one line: the
                        // selection is not unambiguous.
                        return None;
                    }
                    Some(_) => {}
                }
            }
        }
        found
    }
}

/// One line that changed between two grid revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenLineDiff {
    pub row: u16,
    pub before: String,
    pub after: String,
}

/// Compare two grid revisions and return only the rows that changed.
pub fn diff(previous: &ScreenGrid, current: &ScreenGrid) -> Vec<ScreenLineDiff> {
    let mut out = Vec::new();
    let rows = current.lines.len().min(previous.lines.len());
    for row in 0..rows {
        let before = &previous.lines[row].text;
        let after = &current.lines[row].text;
        if before != after {
            out.push(ScreenLineDiff {
                row: row as u16,
                before: before.clone(),
                after: after.clone(),
            });
        }
    }
    for row in rows..current.lines.len() {
        out.push(ScreenLineDiff {
            row: row as u16,
            before: String::new(),
            after: current.lines[row].text.clone(),
        });
    }
    out
}

/// What this parser does not model. Reported so a caller can state the
/// observation's limits instead of implying a complete terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLimits {
    /// No scrollback history is retained; only the visible screen.
    pub no_scrollback: bool,
    /// Grapheme clusters are not combined; a combining mark is dropped.
    pub no_grapheme_clustering: bool,
    /// Mouse and bracketed-paste reports are consumed, not modelled.
    pub no_mouse_or_paste_model: bool,
}

impl Default for TerminalLimits {
    fn default() -> Self {
        Self {
            no_scrollback: true,
            no_grapheme_clustering: true,
            no_mouse_or_paste_model: true,
        }
    }
}

/// How a byte stream should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalMode {
    /// A live PTY stream: LF moves down without returning to column 0.
    Stream,
    /// A rendered screen dump (`tmux capture-pane -p -e`): every line is a
    /// complete row, so LF implies CR.
    ScreenDump,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    StringSequence,
    StringSequenceEscape,
    Charset,
}

/// A terminal screen reconstructed from a byte stream.
#[derive(Debug, Clone)]
pub struct TerminalScreen {
    columns: u16,
    rows: u16,
    cells: Vec<ScreenCell>,
    cursor_column: u16,
    cursor_row: u16,
    saved_cursor: Option<(u16, u16)>,
    current_style: CellStyle,
    scroll_top: u16,
    scroll_bottom: u16,
    alternate: bool,
    primary_cells: Option<Vec<ScreenCell>>,
    primary_cursor: Option<(u16, u16)>,
    cursor_visible: bool,
    autowrap: bool,
    revision: u64,
    parser: ParserState,
    csi_params: Vec<u8>,
    osc_buffer: Vec<u8>,
    utf8_pending: Vec<u8>,
    mode: TerminalMode,
    limits: TerminalLimits,
    /// Count of escape sequences this parser did not model. The byte stream
    /// is still consumed, so the screen stays usable; the count lets a
    /// caller report that part of the output was not interpreted.
    pub unmodelled_sequences: u64,
}

impl TerminalScreen {
    /// A fresh screen of the given size.
    pub fn new(columns: u16, rows: u16, mode: TerminalMode) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let cells = vec![ScreenCell::blank(CellStyle::default()); columns as usize * rows as usize];
        Self {
            columns,
            rows,
            cells,
            cursor_column: 0,
            cursor_row: 0,
            saved_cursor: None,
            current_style: CellStyle::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            alternate: false,
            primary_cells: None,
            primary_cursor: None,
            cursor_visible: true,
            autowrap: true,
            revision: 1,
            parser: ParserState::Ground,
            csi_params: Vec::new(),
            osc_buffer: Vec::new(),
            utf8_pending: Vec::new(),
            mode,
            limits: TerminalLimits::default(),
            unmodelled_sequences: 0,
        }
    }

    pub fn columns(&self) -> u16 {
        self.columns
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn limits(&self) -> TerminalLimits {
        self.limits
    }

    /// Feed terminal bytes. The screen is updated in place.
    pub fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.feed_byte(*byte);
        }
    }

    /// Feed a rendered screen dump (tmux `capture-pane -p -e`).
    pub fn feed_screen_dump(&mut self, bytes: &[u8]) {
        self.mode = TerminalMode::ScreenDump;
        // capture-pane terminates its last row with LF. That separator is
        // not a live terminal line-feed which may scroll the captured rows.
        self.feed(bytes.strip_suffix(b"\n").unwrap_or(bytes));
    }

    fn index(&self, row: u16, column: u16) -> usize {
        row as usize * self.columns as usize + column as usize
    }

    fn feed_byte(&mut self, byte: u8) {
        match self.parser {
            ParserState::Ground => self.ground(byte),
            ParserState::Escape => self.escape(byte),
            ParserState::Csi => self.csi(byte),
            ParserState::Osc => self.osc(byte),
            ParserState::OscEscape => {
                // ESC \ terminates the string; anything else ends it too.
                self.parser = ParserState::Ground;
            }
            ParserState::StringSequence => self.string_sequence(byte),
            ParserState::StringSequenceEscape => {
                self.parser = ParserState::Ground;
            }
            ParserState::Charset => {
                // Character-set designation: the designator itself is not
                // modelled, so the byte is consumed without effect.
                self.parser = ParserState::Ground;
            }
        }
    }

    fn ground(&mut self, byte: u8) {
        match byte {
            0x1b => {
                self.parser = ParserState::Escape;
            }
            0x07 => {}
            0x08 => {
                self.cursor_column = self.cursor_column.saturating_sub(1);
            }
            0x09 => {
                self.cursor_column = self.next_tab_stop(self.cursor_column);
            }
            0x0a | 0x0b | 0x0c => {
                self.line_feed();
                if self.mode == TerminalMode::ScreenDump {
                    self.cursor_column = 0;
                }
            }
            0x0d => {
                self.cursor_column = 0;
            }
            0x00..=0x1f => {}
            0x20..=0x7e => self.print(byte as char),
            _ => self.push_utf8(byte),
        }
    }

    fn escape(&mut self, byte: u8) {
        match byte {
            b'[' => {
                self.csi_params.clear();
                self.parser = ParserState::Csi;
            }
            b']' => {
                self.osc_buffer.clear();
                self.parser = ParserState::Osc;
            }
            b'P' | b'X' | b'^' | b'_' => {
                self.parser = ParserState::StringSequence;
            }
            b'(' | b')' | b'*' | b'+' => {
                self.parser = ParserState::Charset;
            }
            b'7' => {
                self.saved_cursor = Some((self.cursor_column, self.cursor_row));
                self.parser = ParserState::Ground;
            }
            b'8' => {
                if let Some((column, row)) = self.saved_cursor {
                    self.cursor_column = column;
                    self.cursor_row = row;
                }
                self.parser = ParserState::Ground;
            }
            b'D' => {
                self.line_feed();
                self.parser = ParserState::Ground;
            }
            b'E' => {
                self.line_feed();
                self.cursor_column = 0;
                self.parser = ParserState::Ground;
            }
            b'M' => {
                self.reverse_index();
                self.parser = ParserState::Ground;
            }
            b'c' => {
                self.reset();
                self.parser = ParserState::Ground;
            }
            _ => {
                self.unmodelled_sequences += 1;
                self.parser = ParserState::Ground;
            }
        }
    }

    fn csi(&mut self, byte: u8) {
        match byte {
            0x30..=0x3f | 0x20..=0x2f => self.csi_params.push(byte),
            0x40..=0x7e => {
                self.dispatch_csi(byte);
                self.parser = ParserState::Ground;
            }
            0x1b => {
                self.parser = ParserState::Escape;
            }
            _ => {
                // A control byte inside a sequence still executes.
                self.ground(byte);
            }
        }
    }

    fn osc(&mut self, byte: u8) {
        match byte {
            0x07 => {
                self.parser = ParserState::Ground;
            }
            0x1b => {
                self.parser = ParserState::OscEscape;
            }
            _ => {
                if self.osc_buffer.len() < 4096 {
                    self.osc_buffer.push(byte);
                } else {
                    self.unmodelled_sequences += 1;
                }
            }
        }
    }

    fn string_sequence(&mut self, byte: u8) {
        if byte == 0x1b {
            self.parser = ParserState::StringSequenceEscape;
        } else if byte == 0x07 {
            self.parser = ParserState::Ground;
        }
    }

    fn push_utf8(&mut self, byte: u8) {
        self.utf8_pending.push(byte);
        match std::str::from_utf8(&self.utf8_pending) {
            Ok(text) => {
                let decoded = text.to_owned();
                self.utf8_pending.clear();
                for ch in decoded.chars() {
                    self.print(ch);
                }
            }
            Err(error) if error.error_len().is_none() => {
                // Incomplete but still valid so far: wait for more bytes.
            }
            Err(_) => {
                // Invalid sequence: report it as unmodelled and continue with
                // the replacement character rather than inventing content.
                self.utf8_pending.clear();
                self.unmodelled_sequences += 1;
                self.print('\u{fffd}');
            }
        }
    }

    fn print(&mut self, ch: char) {
        let width = char_width(ch);
        if width == 0 {
            // Combining marks are not placed: this parser does not model
            // grapheme clusters (see `TerminalLimits`).
            self.unmodelled_sequences += 1;
            return;
        }
        if self.cursor_column as usize + width > self.columns as usize {
            if self.autowrap {
                self.line_feed();
                self.cursor_column = 0;
            } else {
                self.cursor_column = self.columns.saturating_sub(width as u16);
            }
        }
        let style = self.current_style;
        let row = self.cursor_row;
        let column = self.cursor_column;
        let index = self.index(row, column);
        self.cells[index] = ScreenCell {
            ch,
            style,
            continuation: false,
        };
        if width == 2 && column + 1 < self.columns {
            let next = self.index(row, column + 1);
            self.cells[next] = ScreenCell {
                ch: ' ',
                style,
                continuation: true,
            };
        }
        self.cursor_column += width as u16;
        self.revision += 1;
    }

    fn line_feed(&mut self) {
        if self.cursor_row >= self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
        self.revision += 1;
    }

    fn reverse_index(&mut self) {
        if self.cursor_row <= self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
        }
        self.revision += 1;
    }

    fn scroll_up(&mut self, count: u16) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        let height = bottom.saturating_sub(top) + 1;
        let count = (count as usize).min(height);
        if count == 0 {
            return;
        }
        for row in top..(bottom + 1 - count) {
            let source = self.index((row + count) as u16, 0);
            let target = self.index(row as u16, 0);
            for column in 0..self.columns as usize {
                let cell = self.cells[source + column].clone();
                self.cells[target + column] = cell;
            }
        }
        let blank = ScreenCell::blank(self.current_style);
        for row in (bottom + 1 - count)..=bottom {
            let target = self.index(row as u16, 0);
            for column in 0..self.columns as usize {
                self.cells[target + column] = blank.clone();
            }
        }
        self.revision += 1;
    }

    fn scroll_down(&mut self, count: u16) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        let height = bottom.saturating_sub(top) + 1;
        let count = (count as usize).min(height);
        if count == 0 {
            return;
        }
        for row in ((top + count)..=bottom).rev() {
            let source = self.index((row - count) as u16, 0);
            let target = self.index(row as u16, 0);
            for column in 0..self.columns as usize {
                let cell = self.cells[source + column].clone();
                self.cells[target + column] = cell;
            }
        }
        let blank = ScreenCell::blank(self.current_style);
        for row in top..(top + count) {
            let target = self.index(row as u16, 0);
            for column in 0..self.columns as usize {
                self.cells[target + column] = blank.clone();
            }
        }
        self.revision += 1;
    }

    fn next_tab_stop(&self, column: u16) -> u16 {
        let next = (column / 8 + 1) * 8;
        next.min(self.columns.saturating_sub(1))
    }

    fn previous_tab_stop(&self, column: u16) -> u16 {
        let previous = column.saturating_sub(1) / 8 * 8;
        previous
    }

    fn erase_in_line(&mut self, mode: u16) {
        let row = self.cursor_row;
        let column = self.cursor_column;
        let (start, end) = match mode {
            0 => (column, self.columns),
            1 => (0, (column + 1).min(self.columns)),
            _ => (0, self.columns),
        };
        let blank = ScreenCell::blank(self.current_style);
        for index in start..end {
            let target = self.index(row, index as u16);
            self.cells[target] = blank.clone();
        }
        self.revision += 1;
    }

    fn erase_in_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.erase_in_line(0);
                for row in (self.cursor_row + 1)..self.rows {
                    self.erase_row(row);
                }
            }
            1 => {
                self.erase_in_line(1);
                for row in 0..self.cursor_row {
                    self.erase_row(row);
                }
            }
            _ => {
                for row in 0..self.rows {
                    self.erase_row(row);
                }
            }
        }
        self.revision += 1;
    }

    fn erase_row(&mut self, row: u16) {
        let blank = ScreenCell::blank(self.current_style);
        for column in 0..self.columns {
            let target = self.index(row, column);
            self.cells[target] = blank.clone();
        }
    }

    fn insert_lines(&mut self, count: u16) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        let bottom = self.scroll_bottom as usize;
        let top = self.cursor_row as usize;
        let count = (count as usize).min(bottom + 1 - top);
        for row in (top..=bottom).rev() {
            if row + count > bottom {
                let blank = ScreenCell::blank(self.current_style);
                let target = self.index(row as u16, 0);
                for column in 0..self.columns as usize {
                    self.cells[target + column] = blank.clone();
                }
            } else {
                let source = self.index((row + count) as u16, 0);
                let target = self.index(row as u16, 0);
                for column in 0..self.columns as usize {
                    let cell = self.cells[source + column].clone();
                    self.cells[target + column] = cell;
                }
            }
        }
        self.revision += 1;
    }

    fn delete_lines(&mut self, count: u16) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        let bottom = self.scroll_bottom as usize;
        let top = self.cursor_row as usize;
        let count = (count as usize).min(bottom + 1 - top);
        for row in top..=bottom {
            if row + count > bottom {
                let blank = ScreenCell::blank(self.current_style);
                let target = self.index(row as u16, 0);
                for column in 0..self.columns as usize {
                    self.cells[target + column] = blank.clone();
                }
            } else {
                let source = self.index((row + count) as u16, 0);
                let target = self.index(row as u16, 0);
                for column in 0..self.columns as usize {
                    let cell = self.cells[source + column].clone();
                    self.cells[target + column] = cell;
                }
            }
        }
        self.revision += 1;
    }

    fn insert_characters(&mut self, count: u16) {
        let row = self.cursor_row;
        let column = self.cursor_column;
        let count = count.min(self.columns.saturating_sub(column));
        if count == 0 {
            return;
        }
        for index in (column..self.columns).rev() {
            let target = self.index(row, index);
            if index as u16 + count >= self.columns {
                self.cells[target] = ScreenCell::blank(self.current_style);
            } else {
                let source = self.index(row, index + count);
                let cell = self.cells[source].clone();
                self.cells[target] = cell;
            }
        }
        self.revision += 1;
    }

    fn delete_characters(&mut self, count: u16) {
        let row = self.cursor_row;
        let column = self.cursor_column;
        let count = count.min(self.columns.saturating_sub(column));
        if count == 0 {
            return;
        }
        for index in column..self.columns {
            let target = self.index(row, index);
            if index as u16 + count >= self.columns {
                self.cells[target] = ScreenCell::blank(self.current_style);
            } else {
                let source = self.index(row, index + count);
                let cell = self.cells[source].clone();
                self.cells[target] = cell;
            }
        }
        self.revision += 1;
    }

    fn erase_characters(&mut self, count: u16) {
        let row = self.cursor_row;
        let column = self.cursor_column;
        let count = count.min(self.columns.saturating_sub(column));
        let blank = ScreenCell::blank(self.current_style);
        for index in column..(column + count) {
            let target = self.index(row, index);
            self.cells[target] = blank.clone();
        }
        self.revision += 1;
    }

    fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        let top = top.saturating_sub(1);
        let bottom = bottom.saturating_sub(1);
        if top >= bottom || bottom >= self.rows {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        } else {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        }
        self.cursor_column = 0;
        self.cursor_row = self.scroll_top;
        self.revision += 1;
    }

    fn apply_sgr(&mut self, params: &[u16]) {
        if params.is_empty() {
            self.current_style = CellStyle::default();
            self.revision += 1;
            return;
        }
        let mut index = 0usize;
        while index < params.len() {
            let code = params[index];
            match code {
                0 => self.current_style = CellStyle::default(),
                1 => self.current_style.bold = true,
                2 => self.current_style.dim = true,
                3 => self.current_style.italic = true,
                4 => self.current_style.underline = true,
                7 => self.current_style.reverse = true,
                8 => self.current_style.hidden = true,
                9 => self.current_style.strike = true,
                21 | 22 => {
                    self.current_style.bold = false;
                    self.current_style.dim = false;
                }
                23 => self.current_style.italic = false,
                24 => self.current_style.underline = false,
                27 => self.current_style.reverse = false,
                28 => self.current_style.hidden = false,
                29 => self.current_style.strike = false,
                30..=37 => {
                    self.current_style.foreground = TerminalColor::Indexed((code - 30) as u8);
                }
                39 => self.current_style.foreground = TerminalColor::Default,
                40..=47 => {
                    self.current_style.background = TerminalColor::Indexed((code - 40) as u8);
                }
                49 => self.current_style.background = TerminalColor::Default,
                90..=97 => {
                    self.current_style.foreground = TerminalColor::Indexed((code - 90 + 8) as u8);
                }
                100..=107 => {
                    self.current_style.background = TerminalColor::Indexed((code - 100 + 8) as u8);
                }
                38 | 48 => {
                    let extended = params.get(index + 1).copied().unwrap_or(0);
                    let target_background = code == 48;
                    match extended {
                        5 => {
                            let value = params.get(index + 2).copied().unwrap_or(0) as u8;
                            let color = TerminalColor::Indexed(value);
                            if target_background {
                                self.current_style.background = color;
                            } else {
                                self.current_style.foreground = color;
                            }
                            index += 2;
                        }
                        2 => {
                            let red = params.get(index + 2).copied().unwrap_or(0) as u8;
                            let green = params.get(index + 3).copied().unwrap_or(0) as u8;
                            let blue = params.get(index + 4).copied().unwrap_or(0) as u8;
                            let color = TerminalColor::Rgb(red, green, blue);
                            if target_background {
                                self.current_style.background = color;
                            } else {
                                self.current_style.foreground = color;
                            }
                            index += 4;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            index += 1;
        }
        self.revision += 1;
    }

    fn dispatch_csi(&mut self, final_byte: u8) {
        let (private, params) = self.parse_csi_params();
        let first = params.first().copied().unwrap_or(0);
        let count = |value: u16| value.max(1);
        match final_byte {
            b'A' => {
                let amount = count(first).min(self.cursor_row.saturating_sub(self.scroll_top));
                self.cursor_row -= amount;
                self.revision += 1;
            }
            b'B' => {
                let amount = count(first).min(self.scroll_bottom.saturating_sub(self.cursor_row));
                self.cursor_row += amount;
                self.revision += 1;
            }
            b'C' => {
                let amount = count(first).min(self.columns.saturating_sub(self.cursor_column + 1));
                self.cursor_column += amount;
                self.revision += 1;
            }
            b'D' => {
                let amount = count(first).min(self.cursor_column);
                self.cursor_column -= amount;
                self.revision += 1;
            }
            b'E' => {
                let amount = count(first).min(self.scroll_bottom.saturating_sub(self.cursor_row));
                self.cursor_row += amount;
                self.cursor_column = 0;
                self.revision += 1;
            }
            b'F' => {
                let amount = count(first).min(self.cursor_row.saturating_sub(self.scroll_top));
                self.cursor_row -= amount;
                self.cursor_column = 0;
                self.revision += 1;
            }
            b'G' | b'`' => {
                self.cursor_column = first
                    .saturating_sub(1)
                    .min(self.columns.saturating_sub(1));
                self.revision += 1;
            }
            b'd' => {
                self.cursor_row = first.saturating_sub(1).min(self.rows - 1);
                self.revision += 1;
            }
            b'H' | b'f' => {
                let row = params.first().copied().unwrap_or(1).saturating_sub(1);
                let column = params.get(1).copied().unwrap_or(1).saturating_sub(1);
                self.cursor_row = row.min(self.rows - 1);
                self.cursor_column = column.min(self.columns.saturating_sub(1));
                self.revision += 1;
            }
            b'I' => {
                for _ in 0..count(first) {
                    self.cursor_column = self.next_tab_stop(self.cursor_column);
                }
                self.revision += 1;
            }
            b'Z' => {
                for _ in 0..count(first) {
                    self.cursor_column = self.previous_tab_stop(self.cursor_column);
                }
                self.revision += 1;
            }
            b'J' => self.erase_in_display(first),
            b'K' => self.erase_in_line(first),
            b'L' => self.insert_lines(count(first)),
            b'M' => self.delete_lines(count(first)),
            b'P' => self.delete_characters(count(first)),
            b'@' => self.insert_characters(count(first)),
            b'X' => self.erase_characters(count(first)),
            b'S' => self.scroll_up(count(first)),
            b'T' => self.scroll_down(count(first)),
            b'm' => self.apply_sgr(&params),
            b'r' => {
                let top = params.first().copied().unwrap_or(1);
                let bottom = params.get(1).copied().unwrap_or(self.rows);
                self.set_scroll_region(top, bottom);
            }
            b's' => {
                self.saved_cursor = Some((self.cursor_column, self.cursor_row));
            }
            b'u' => {
                if let Some((column, row)) = self.saved_cursor {
                    self.cursor_column = column;
                    self.cursor_row = row;
                    self.revision += 1;
                }
            }
            b'h' | b'l' => {
                let enable = final_byte == b'h';
                if private {
                    for mode in &params {
                        match *mode {
                            25 => self.cursor_visible = enable,
                            7 => self.autowrap = enable,
                            47 | 1047 | 1049 => self.set_alternate(enable),
                            _ => {}
                        }
                    }
                }
            }
            _ => {
                self.unmodelled_sequences += 1;
            }
        }
    }

    fn parse_csi_params(&self) -> (bool, Vec<u16>) {
        let mut private = false;
        let mut body: &[u8] = &self.csi_params;
        if let Some(&first) = body.first() {
            if matches!(first, b'?' | b'>' | b'<' | b'=') {
                private = first == b'?';
                body = &body[1..];
            }
        }
        let mut out = Vec::new();
        for part in body.split(|&byte| byte == b';') {
            let part = match part.iter().position(|&byte| byte == b':') {
                Some(index) => &part[..index],
                None => part,
            };
            if part.is_empty() {
                out.push(0);
                continue;
            }
            let mut value: u32 = 0;
            let mut valid = true;
            for &byte in part {
                if !byte.is_ascii_digit() {
                    valid = false;
                    break;
                }
                value = value.saturating_mul(10).saturating_add((byte - b'0') as u32);
            }
            out.push(if valid {
                value.min(u16::MAX as u32) as u16
            } else {
                0
            });
        }
        (private, out)
    }

    fn set_alternate(&mut self, on: bool) {
        if on == self.alternate {
            return;
        }
        if on {
            self.primary_cells = Some(self.cells.clone());
            self.primary_cursor = Some((self.cursor_column, self.cursor_row));
            for row in 0..self.rows {
                self.erase_row(row);
            }
            self.cursor_column = 0;
            self.cursor_row = 0;
            self.alternate = true;
        } else {
            if let Some(cells) = self.primary_cells.take() {
                self.cells = cells;
                if let Some((column, row)) = self.primary_cursor.take() {
                    self.cursor_column = column;
                    self.cursor_row = row;
                }
            }
            self.alternate = false;
        }
        self.revision += 1;
    }

    fn reset(&mut self) {
        for row in 0..self.rows {
            self.erase_row(row);
        }
        self.cursor_column = 0;
        self.cursor_row = 0;
        self.current_style = CellStyle::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.saved_cursor = None;
        self.alternate = false;
        self.primary_cells = None;
        self.primary_cursor = None;
        self.cursor_visible = true;
        self.autowrap = true;
        self.revision += 1;
    }

    /// Resize the screen. Content is kept top-left anchored; rows or columns
    /// that no longer exist are dropped, new ones are blank.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        let columns = columns.max(1);
        let rows = rows.max(1);
        if columns == self.columns && rows == self.rows {
            return;
        }
        let mut cells = vec![ScreenCell::blank(self.current_style); columns as usize * rows as usize];
        let copy_rows = rows.min(self.rows) as usize;
        let copy_columns = columns.min(self.columns) as usize;
        for row in 0..copy_rows {
            for column in 0..copy_columns {
                let source = self.index(row as u16, column as u16);
                let target = row * columns as usize + column;
                cells[target] = self.cells[source].clone();
            }
        }
        self.cells = cells;
        self.columns = columns;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.cursor_column = self.cursor_column.min(columns - 1);
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.revision += 1;
    }

    /// The reconstructed screen.
    pub fn grid(&self) -> ScreenGrid {
        let mut lines = Vec::with_capacity(self.rows as usize);
        for row in 0..self.rows {
            let mut text = String::new();
            let mut runs: Vec<StyleRun> = Vec::new();
            let mut current: Option<(usize, usize, CellStyle)> = None;
            let mut length = 0usize;
            for column in 0..self.columns {
                let cell = &self.cells[self.index(row, column)];
                if cell.continuation {
                    continue;
                }
                let style = cell.style;
                match current {
                    Some((_, _, existing)) if existing == style => {
                        if let Some((_, end, _)) = current.as_mut() {
                            *end = length + 1;
                        }
                    }
                    Some((start, end, existing)) => {
                        runs.push(StyleRun {
                            start,
                            end,
                            style: existing,
                        });
                        current = Some((length, length + 1, style));
                    }
                    None => current = Some((length, length + 1, style)),
                }
                text.push(cell.ch);
                length += 1;
            }
            if let Some((start, end, style)) = current {
                runs.push(StyleRun { start, end, style });
            }
            lines.push(ScreenLine { text, runs });
        }
        ScreenGrid {
            columns: self.columns,
            rows: self.rows,
            revision: self.revision,
            cursor_column: self.cursor_column,
            cursor_row: self.cursor_row,
            cursor_visible: self.cursor_visible,
            alternate_screen: self.alternate,
            lines,
        }
    }
}

/// Display width of one character in terminal cells.
///
/// The ranges below are the East Asian Wide/Fullwidth blocks plus the emoji
/// planes that agent TUIs render double-width. Zero-width covers the
/// combining marks and format characters that never advance the cursor.
/// Ambiguous-width characters are counted as one cell, which is the
/// behaviour of the terminals this product targets.
pub fn char_width(ch: char) -> usize {
    let code = ch as u32;
    if code == 0 {
        return 0;
    }
    if code < 0x20 || (0x7f..0xa0).contains(&code) {
        return 0;
    }
    if is_zero_width(code) {
        return 0;
    }
    if is_wide(code) {
        return 2;
    }
    1
}

fn is_zero_width(code: u32) -> bool {
    matches!(
        code,
        0x0300..=0x036f        // combining diacritical marks
            | 0x0483..=0x0489
            | 0x0591..=0x05bd
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x0670
            | 0x06d6..=0x06dc
            | 0x0730..=0x074a
            | 0x07a6..=0x07b0
            | 0x0900..=0x0903
            | 0x093a..=0x094f
            | 0x0951..=0x0957
            | 0x0e31
            | 0x0e34..=0x0e3a
            | 0x0e47..=0x0e4e
            | 0x200b..=0x200f    // zero width space/joiners and bidi marks
            | 0x2028..=0x202e
            | 0x2060..=0x2064
            | 0x20d0..=0x20f0
            | 0xfe00..=0xfe0f    // variation selectors
            | 0xfe20..=0xfe2f
            | 0xfeff
    )
}

fn is_wide(code: u32) -> bool {
    matches!(
        code,
        0x1100..=0x115f        // Hangul Jamo initial consonants
            | 0x2e80..=0x303e    // CJK radicals, Kangxi, CJK symbols
            | 0x3041..=0x33ff    // Hiragana .. CJK compatibility
            | 0x3400..=0x4dbf    // CJK extension A
            | 0x4e00..=0x9fff    // CJK unified ideographs
            | 0xa000..=0xa4cf    // Yi syllables
            | 0xa960..=0xa97f
            | 0xac00..=0xd7a3    // Hangul syllables
            | 0xf900..=0xfaff    // CJK compatibility ideographs
            | 0xfe10..=0xfe19    // vertical forms
            | 0xfe30..=0xfe6f    // CJK compatibility forms
            | 0xff00..=0xff60    // fullwidth forms
            | 0xffe0..=0xffe6
            | 0x16fe0..=0x16fe4
            | 0x17000..=0x18aff
            | 0x1b000..=0x1b2ff
            | 0x1f004
            | 0x1f0cf
            | 0x1f18e
            | 0x1f191..=0x1f19a
            | 0x1f200..=0x1f320
            | 0x1f32d..=0x1f335
            | 0x1f337..=0x1f37c
            | 0x1f37e..=0x1f393
            | 0x1f3a0..=0x1f3ca
            | 0x1f3cf..=0x1f3d3
            | 0x1f3e0..=0x1f3f0
            | 0x1f3f4
            | 0x1f3f8..=0x1f43e
            | 0x1f440
            | 0x1f442..=0x1f4fc
            | 0x1f4ff..=0x1f53d
            | 0x1f54b..=0x1f54e
            | 0x1f550..=0x1f567
            | 0x1f57a
            | 0x1f595..=0x1f596
            | 0x1f5a4
            | 0x1f5fb..=0x1f64f
            | 0x1f680..=0x1f6c5
            | 0x1f6cc
            | 0x1f6d0..=0x1f6d2
            | 0x1f6d5..=0x1f6d7
            | 0x1f6eb..=0x1f6ec
            | 0x1f6f4..=0x1f6fc
            | 0x1f7e0..=0x1f7eb
            | 0x1f90c..=0x1f93a
            | 0x1f93c..=0x1f945
            | 0x1f947..=0x1f9ff
            | 0x1fa70..=0x1faff
            | 0x20000..=0x2fffd
            | 0x30000..=0x3fffd
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_of(bytes: &[u8]) -> ScreenGrid {
        let mut screen = TerminalScreen::new(20, 4, TerminalMode::Stream);
        screen.feed(bytes);
        screen.grid()
    }

    #[test]
    fn plain_text_and_cursor_follow_the_stream() {
        let grid = grid_of(b"abc\r\ndef");
        assert_eq!(grid.lines[0].text.trim_end(), "abc");
        assert_eq!(grid.lines[1].text.trim_end(), "def");
        assert_eq!(grid.cursor_row, 1);
        assert_eq!(grid.cursor_column, 3);
    }

    #[test]
    fn sgr_attributes_survive_as_text_runs() {
        let grid = grid_of(b"\x1b[31mred\x1b[0m plain");
        let line = &grid.lines[0];
        assert_eq!(line.text.trim_end(), "red plain");
        let highlighted = line.runs.iter().find(|run| run.style.foreground
            == TerminalColor::Indexed(1));
        assert_eq!(highlighted.map(|run| (run.start, run.end)), Some((0, 3)));
    }

    #[test]
    fn erase_display_clears_the_screen() {
        let grid = grid_of(b"hello\x1b[2J");
        assert!(grid.plain_text().trim().is_empty());
    }

    #[test]
    fn scrolling_preserves_remaining_rows_and_blanks_a_full_region() {
        let mut screen = TerminalScreen::new(4, 2, TerminalMode::Stream);
        screen.feed(b"one\r\ntwo\r\nend");
        let grid = screen.grid();
        assert_eq!(grid.lines[0].text.trim_end(), "two");
        assert_eq!(grid.lines[1].text.trim_end(), "end");
        screen.feed(b"\x1b[99S");
        assert_eq!(screen.grid().lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>(), vec!["    ", "    "]);
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('a'), 1);
        let mut screen = TerminalScreen::new(10, 2, TerminalMode::Stream);
        screen.feed("中a".as_bytes());
        let grid = screen.grid();
        assert_eq!(grid.lines[0].text.trim_end(), "中a");
        assert_eq!(grid.cursor_column, 3);
    }

    #[test]
    fn alternate_screen_is_reported_and_restored() {
        let mut screen = TerminalScreen::new(10, 2, TerminalMode::Stream);
        screen.feed(b"main");
        screen.feed(b"\x1b[?1049h");
        screen.feed(b"alt");
        let grid = screen.grid();
        assert!(grid.alternate_screen);
        assert_eq!(grid.lines[0].text.trim_end(), "alt");
        screen.feed(b"\x1b[?1049l");
        let grid = screen.grid();
        assert!(!grid.alternate_screen);
        assert_eq!(grid.lines[0].text.trim_end(), "main");
    }

    #[test]
    fn screen_dump_mode_returns_to_column_zero_per_line() {
        let mut screen = TerminalScreen::new(3, 2, TerminalMode::ScreenDump);
        screen.feed_screen_dump(b"one\ntwo\n");
        let grid = screen.grid();
        assert_eq!(grid.lines[0].text.trim_end(), "one");
        assert_eq!(grid.lines[1].text.trim_end(), "two");
    }
}
