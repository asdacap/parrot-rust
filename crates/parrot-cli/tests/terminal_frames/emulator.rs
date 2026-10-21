use parrot_cli::terminal_layout::TerminalText;
use unicode_segmentation::UnicodeSegmentation;

/// Deliberately bounded VT interpreter for the source frame protocol. Unknown
/// controls fail rather than silently turning an unsupported stream into text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Terminal {
    columns: usize,
    screen: Vec<Vec<String>>,
    history: Vec<Vec<String>>,
    row: usize,
    column: usize,
    wrap_pending: bool,
    pub cursor_visible: bool,
    pub autowrap: bool,
    pub alternate_screen: bool,
    sgr: String,
}
impl Terminal {
    pub fn new(columns: usize, rows: usize) -> Self {
        assert!(columns > 0 && rows > 0);
        Self {
            columns,
            screen: vec![vec![" ".to_owned(); columns]; rows],
            history: Vec::new(),
            row: 0,
            column: 0,
            wrap_pending: false,
            cursor_visible: true,
            autowrap: true,
            alternate_screen: false,
            sgr: String::new(),
        }
    }
    pub fn resize(&mut self, columns: usize, rows: usize) {
        assert!(columns > 0 && rows > 0);
        if columns == self.columns && rows == self.screen.len() {
            return;
        }
        if rows < self.screen.len() {
            // Preserve the cursor's line when reducing the viewport; do not
            // reflow text (the renderer explicitly repaints after resizing).
            let removed = self.row.saturating_sub(rows - 1);
            self.history.extend(self.screen.drain(..removed));
            self.row -= removed;
            self.screen.truncate(rows);
        } else {
            self.screen
                .resize_with(rows, || vec![" ".to_owned(); columns]);
        }
        for line in &mut self.screen {
            line.resize(columns, " ".to_owned());
        }
        self.columns = columns;
        self.column = self.column.min(columns - 1);
        self.wrap_pending = false;
    }
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.column)
    }
    pub fn cells(&self, row: usize) -> &[String] {
        &self.screen[row]
    }
    pub fn line(&self, row: usize) -> String {
        Self::line_text(&self.screen[row])
    }
    pub fn scrollback(&self) -> Vec<String> {
        self.history
            .iter()
            .map(|line| Self::line_text(line))
            .collect()
    }
    pub fn all_lines(&self) -> Vec<String> {
        self.history
            .iter()
            .chain(&self.screen)
            .map(|line| Self::line_text(line))
            .collect()
    }
    fn line_text(line: &[String]) -> String {
        line.concat().trim_end().to_owned()
    }
    fn blank(&self) -> Vec<String> {
        vec![" ".to_owned(); self.columns]
    }
    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.row + 1 == self.screen.len() {
            self.history.push(self.screen.remove(0));
            self.screen.push(self.blank());
        } else {
            self.row += 1;
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        let value = std::str::from_utf8(bytes).expect("frame output must be UTF-8");
        let mut offset = 0;
        while offset < value.len() {
            match value.as_bytes()[offset] {
                0x1b => {
                    assert_eq!(
                        value.as_bytes().get(offset + 1),
                        Some(&b'['),
                        "unsupported escape"
                    );
                    let start = offset + 2;
                    let end = (start..value.len())
                        .find(|&i| (0x40..=0x7e).contains(&value.as_bytes()[i]))
                        .expect("unterminated CSI");
                    self.csi(&value[start..end], value.as_bytes()[end]);
                    offset = end + 1;
                }
                b'\r' => {
                    self.column = 0;
                    self.wrap_pending = false;
                    offset += 1;
                }
                b'\n' => {
                    self.line_feed();
                    offset += 1;
                }
                byte if byte < 0x20 || byte == 0x7f => panic!("unsupported control {byte:#x}"),
                _ => {
                    let end = value[offset..]
                        .find(|c: char| c.is_ascii_control())
                        .map_or(value.len(), |i| offset + i);
                    for grapheme in value[offset..end].graphemes(true) {
                        self.print(grapheme);
                    }
                    offset = end;
                }
            }
        }
    }
    fn print(&mut self, grapheme: &str) {
        let width = TerminalText::width(grapheme);
        if width == 0 {
            let previous = self.column.saturating_sub(usize::from(!self.wrap_pending));
            self.screen[self.row][previous].push_str(grapheme);
            return;
        }
        if self.autowrap && (self.wrap_pending || self.column + width > self.columns) {
            self.column = 0;
            self.line_feed();
        }
        if self.column + width > self.columns {
            return;
        }
        // Remove a prior wide glyph when overwriting either of its cells.
        let line = &mut self.screen[self.row];
        if line[self.column].is_empty() {
            let mut start = self.column;
            while start > 0 && line[start].is_empty() {
                start -= 1;
            }
            line[start] = " ".to_owned();
        }
        let old_width = TerminalText::width(&line[self.column]);
        for cell in line.iter_mut().skip(self.column).take(old_width.max(width)) {
            *cell = " ".to_owned();
        }
        line[self.column] = grapheme.to_owned();
        for cell in line.iter_mut().skip(self.column + 1).take(width - 1) {
            cell.clear();
        }
        let next = self.column + width;
        self.column = next.min(self.columns - 1);
        self.wrap_pending = next == self.columns;
    }
    fn csi(&mut self, parameters: &str, command: u8) {
        if let Some(mode) = parameters.strip_prefix('?') {
            let enabled = match command {
                b'h' => true,
                b'l' => false,
                _ => panic!("unsupported private CSI"),
            };
            match mode {
                "25" => self.cursor_visible = enabled,
                "7" => self.autowrap = enabled,
                "47" | "1047" | "1049" => {
                    self.alternate_screen = enabled;
                    panic!("frame renderer must never switch to the alternate screen");
                }
                _ => panic!("unsupported private mode {mode}"),
            }
            return;
        }
        let params: Vec<usize> = parameters
            .split(';')
            .map(|part| {
                if part.is_empty() {
                    0
                } else {
                    part.parse().expect("numeric CSI parameter")
                }
            })
            .collect();
        let count = params[0].max(1);
        match command {
            b'A' => self.row = self.row.saturating_sub(count),
            b'B' => self.row = (self.row + count).min(self.screen.len() - 1),
            b'C' => self.column = (self.column + count).min(self.columns - 1),
            b'H' => {
                assert!(params.len() <= 2);
                self.row = params[0]
                    .max(1)
                    .saturating_sub(1)
                    .min(self.screen.len() - 1);
                self.column = params
                    .get(1)
                    .copied()
                    .unwrap_or(1)
                    .max(1)
                    .saturating_sub(1)
                    .min(self.columns - 1);
            }
            b'K' => match parameters {
                "2" => self.screen[self.row] = self.blank(),
                "" | "0" => self.screen[self.row][self.column..].fill(" ".to_owned()),
                _ => panic!("unsupported erase-line CSI {parameters}"),
            },
            b'M' => {
                for _ in 0..count.min(self.screen.len() - self.row) {
                    self.screen.remove(self.row);
                    self.screen.push(self.blank());
                }
            }
            b'm' => self.sgr = parameters.to_owned(),
            _ => panic!("unsupported CSI {parameters}{}", command as char),
        }
        if command != b'm' {
            self.wrap_pending = false;
        }
    }
}

#[test]
fn interpreter_scrolls_deletes_moves_and_restores_modes() {
    let mut terminal = Terminal::new(6, 3);
    terminal.feed(b"first\r\nsecond\r\nthird\r\nfourth");
    assert_eq!(terminal.scrollback(), ["first"]);
    assert_eq!(terminal.line(0), "second");
    terminal.feed(b"\x1b[?25l\x1b[?7l\x1b[2A\r\x1b[1M\x1b[2Knew\x1b[B\r\x1b[2C\x1b[?7h\x1b[?25h");
    assert_eq!(terminal.line(0), "new");
    assert_eq!(terminal.line(1), "fourth");
    assert_eq!(terminal.cursor(), (1, 2));
    assert!(terminal.cursor_visible && terminal.autowrap && !terminal.alternate_screen);
}

#[test]
fn interpreter_uses_accepted_wide_family_and_combining_cell_widths() {
    let mut terminal = Terminal::new(8, 2);
    terminal.feed("\x1b[31m界👨‍👩‍👧‍👦e\u{301}\x1b[0m!".as_bytes());
    assert_eq!(
        &terminal.cells(0)[..6],
        ["界", "", "👨‍👩‍👧‍👦", "", "e\u{301}", "!"]
    );
    assert_eq!(terminal.cursor(), (0, 6));
}

#[test]
#[should_panic(expected = "unsupported CSI")]
fn interpreter_rejects_unrecognized_controls() {
    Terminal::new(8, 2).feed(b"\x1b[2J");
}
