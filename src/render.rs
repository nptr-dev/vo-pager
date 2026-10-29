use std::io::Write;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorLevel {
    None,
    Monochrome,
    Ansi8,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Clone, Copy, Debug)]
pub struct RenderOptions {
    pub width: usize,
    pub horizontal: usize,
    pub wrap: bool,
    pub tabstop: usize,
    pub color: ColorLevel,
    pub indicator: Option<StyleState>,
}

/// SGR state can be carried explicitly between lines. `new` starts from defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StyleState {
    pub(crate) attributes: u16,
    pub(crate) foreground: Option<Color>,
    pub(crate) background: Option<Color>,
    underline: Option<Color>,
}

pub struct RenderedLine {
    pub rows: Vec<Vec<u8>>,
    pub total_rows: usize,
    pub truncated: bool,
    pub style: StyleState,
    pub display_width: usize,
}

/// Incrementally renders one logical line, retaining only the requested rows.
///
/// Storage is bounded by the viewport, a CSI buffer, and one pending display
/// cell. A display cell retains at most 16 combining scalars. Input may end in
/// LF or CRLF; bytes following the first LF are ignored.
pub struct LineRenderer {
    options: RenderOptions,
    first_row: usize,
    end_row: usize,
    rows: Vec<Vec<u8>>,
    output: Vec<u8>,
    row: usize,
    column: usize,
    logical_column: usize,
    style: StyleState,
    emitted_style: StyleState,
    cached_style: Option<(StyleState, StyleState)>,
    pending: Option<Cell>,
    backspace: bool,
    carriage_return: bool,
    mode: ParseMode,
    csi: [u8; 128],
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_expected: usize,
    ended: bool,
    truncated: bool,
    edge: Option<EdgeCell>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    Ansi(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy)]
enum ParseMode {
    Ground,
    Escape,
    EscapeIntermediate,
    Csi { len: usize, overflow: bool },
    String { bell: bool, escape: bool },
}

#[derive(Clone, Copy)]
struct Cell {
    bytes: [u8; 68],
    len: usize,
    width: usize,
    scalars: usize,
    style: StyleState,
}

#[derive(Clone, Copy)]
struct EdgeCell {
    start: usize,
    width: usize,
    preceding_style: StyleState,
}

pub(crate) const BOLD: u16 = 1 << 0;
pub(crate) const DIM: u16 = 1 << 1;
pub(crate) const ITALIC: u16 = 1 << 2;
pub(crate) const UNDERLINE: u16 = 1 << 3;
const BLINK: u16 = 1 << 4;
pub(crate) const REVERSE: u16 = 1 << 5;
const CONCEAL: u16 = 1 << 6;
const STRIKE: u16 = 1 << 7;
const OVERLINE: u16 = 1 << 8;

impl LineRenderer {
    pub fn new(options: RenderOptions, first_row: usize, row_limit: usize) -> Self {
        Self::with_style(options, first_row, row_limit, StyleState::default())
    }

    pub fn with_style(
        mut options: RenderOptions,
        first_row: usize,
        row_limit: usize,
        style: StyleState,
    ) -> Self {
        options.width = options.width.max(1);
        options.tabstop = options.tabstop.max(1);
        Self {
            options,
            first_row,
            end_row: first_row.saturating_add(row_limit),
            rows: Vec::new(),
            output: Vec::new(),
            row: 0,
            column: 0,
            logical_column: 0,
            style,
            emitted_style: StyleState::default(),
            cached_style: None,
            pending: None,
            backspace: false,
            carriage_return: false,
            mode: ParseMode::Ground,
            csi: [0; 128],
            utf8: [0; 4],
            utf8_len: 0,
            utf8_expected: 0,
            ended: false,
            truncated: false,
            edge: None,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() && !self.ended {
            if matches!(self.mode, ParseMode::Ground) && self.utf8_len == 0 && !self.carriage_return
            {
                let length = bytes[offset..]
                    .iter()
                    .position(|byte| !(0x20..0x7f).contains(byte))
                    .unwrap_or(bytes.len() - offset);
                if length != 0 {
                    self.ascii(&bytes[offset..offset + length]);
                    offset += length;
                    continue;
                }
            }
            self.byte(bytes[offset]);
            offset += 1;
        }
    }

    /// True once the chop viewport is settled. A caller may skip the suffix,
    /// losing its final SGR state and knowledge of any later visible overflow.
    pub fn is_full(&self) -> bool {
        !self.options.wrap
            && self.column >= self.options.horizontal.saturating_add(self.options.width)
    }

    /// True when no further bytes can change the requested viewport.
    /// If input processing stops here, `total_rows` and `display_width` are lower bounds.
    pub fn viewport_full(&self) -> bool {
        if self.first_row == self.end_row {
            return true;
        }
        if !self.options.wrap {
            return self.is_full();
        }
        self.row >= self.end_row
            || (self.row.saturating_add(1) == self.end_row && self.column == self.options.width)
    }

    pub fn finish(mut self) -> RenderedLine {
        if self.utf8_len != 0 {
            self.character('\u{fffd}');
        }
        self.flush_cell();
        if self.options.indicator.is_some()
            && !self.options.wrap
            && self.logical_column > self.options.horizontal.saturating_add(self.options.width)
        {
            self.indicator();
        }
        self.finish_row();
        RenderedLine {
            rows: self.rows,
            total_rows: self.row.saturating_add(1),
            truncated: self.truncated,
            style: self.style,
            display_width: self.logical_column,
        }
    }

    fn byte(&mut self, byte: u8) {
        if byte == b'\n' {
            self.carriage_return = false;
            self.ended = true;
            return;
        }
        match self.mode {
            ParseMode::Ground => self.ground(byte),
            ParseMode::Escape => {
                self.mode = match byte {
                    b'[' => ParseMode::Csi {
                        len: 0,
                        overflow: false,
                    },
                    b']' => ParseMode::String {
                        bell: true,
                        escape: false,
                    },
                    b'P' | b'X' | b'^' | b'_' => ParseMode::String {
                        bell: false,
                        escape: false,
                    },
                    0x20..=0x2f => ParseMode::EscapeIntermediate,
                    0x1b => ParseMode::Escape,
                    _ => ParseMode::Ground,
                };
            }
            ParseMode::EscapeIntermediate => {
                if byte == 0x1b {
                    self.mode = ParseMode::Escape;
                } else if !(0x20..=0x2f).contains(&byte) {
                    self.mode = ParseMode::Ground;
                }
            }
            ParseMode::Csi {
                mut len,
                mut overflow,
            } => {
                if (0x40..=0x7e).contains(&byte) {
                    if byte == b'm' && !overflow {
                        let parameters = self.csi;
                        self.sgr(&parameters[..len]);
                    }
                    self.mode = ParseMode::Ground;
                } else if byte == 0x1b {
                    self.mode = ParseMode::Escape;
                } else if byte == 0x18 || byte == 0x1a {
                    self.mode = ParseMode::Ground;
                } else {
                    if len < self.csi.len() {
                        self.csi[len] = byte;
                        len += 1;
                    } else {
                        overflow = true;
                    }
                    self.mode = ParseMode::Csi { len, overflow };
                }
            }
            ParseMode::String { bell, escape } => {
                if (escape && byte == b'\\') || byte == 0x9c || (bell && byte == 0x07) {
                    self.mode = ParseMode::Ground;
                } else {
                    self.mode = ParseMode::String {
                        bell,
                        escape: byte == 0x1b,
                    };
                }
            }
        }
    }

    fn ground(&mut self, byte: u8) {
        if self.utf8_len != 0 {
            if byte & 0xc0 == 0x80 {
                self.utf8[self.utf8_len] = byte;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_expected {
                    let character = std::str::from_utf8(&self.utf8[..self.utf8_len])
                        .ok()
                        .and_then(|text| text.chars().next())
                        .unwrap_or('\u{fffd}');
                    self.utf8_len = 0;
                    self.character(character);
                }
                return;
            }
            self.utf8_len = 0;
            self.character('\u{fffd}');
        }
        if self.carriage_return {
            self.carriage_return = false;
            if byte == b'\n' {
                self.ended = true;
                return;
            }
            self.visible_control(b'\r');
        }
        match byte {
            b'\n' => self.ended = true,
            b'\r' => self.carriage_return = true,
            b'\t' => {
                self.flush_cell();
                let spaces = self.options.tabstop - self.logical_column % self.options.tabstop;
                self.span(None, spaces, self.style);
            }
            0x08 => {
                if self.pending.is_some() {
                    self.backspace = true;
                } else {
                    self.visible_control(byte);
                }
            }
            0x1b => self.mode = ParseMode::Escape,
            0x00..=0x1f | 0x7f => self.visible_control(byte),
            0x20..=0x7e => self.character(char::from(byte)),
            0xc2..=0xdf => self.start_utf8(byte, 2),
            0xe0..=0xef => self.start_utf8(byte, 3),
            0xf0..=0xf4 => self.start_utf8(byte, 4),
            _ => self.character('\u{fffd}'),
        }
    }

    fn start_utf8(&mut self, byte: u8, expected: usize) {
        self.utf8[0] = byte;
        self.utf8_len = 1;
        self.utf8_expected = expected;
    }

    fn visible_control(&mut self, byte: u8) {
        self.flush_cell();
        let second = if byte == 0x7f { b'?' } else { byte + b'@' };
        self.ascii(&[b'^', second]);
    }

    fn ascii(&mut self, mut bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.backspace {
            self.character(char::from(bytes[0]));
            bytes = &bytes[1..];
            if bytes.is_empty() {
                return;
            }
        }
        self.flush_cell();
        let (last, body) = bytes.split_last().expect("nonempty ASCII run");
        self.span(Some(body), body.len(), self.style);
        self.pending = Some(Cell::new(char::from(*last), self.style));
    }

    fn character(&mut self, character: char) {
        if character.is_control()
            || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            self.flush_cell();
            for escaped in character.escape_default() {
                self.character(escaped);
            }
            return;
        }
        let incoming = Cell::new(character, self.style);
        if self.backspace {
            self.backspace = false;
            if let Some(mut previous) = self.pending.take() {
                if previous.text() == incoming.text() {
                    previous.style.attributes |= BOLD;
                    self.pending = Some(previous);
                } else if previous.text() == b"_" {
                    let mut replacement = incoming;
                    replacement.style.attributes |= UNDERLINE;
                    self.pending = Some(replacement);
                } else if incoming.text() == b"_" {
                    previous.style.attributes |= UNDERLINE;
                    self.pending = Some(previous);
                } else {
                    self.pending = Some(incoming);
                }
                return;
            }
        }
        if let Some(previous) = self.pending.as_mut() {
            if incoming.width == 0 {
                if !previous.append(character) {
                    self.truncated = true;
                }
                return;
            }
            if previous.len + incoming.len <= previous.bytes.len() && previous.scalars < 17 {
                let mut combined = *previous;
                combined.append(character);
                if combined.width < previous.width + incoming.width {
                    *previous = combined;
                    return;
                }
            }
        }
        self.flush_cell();
        self.pending = Some(incoming);
    }

    fn flush_cell(&mut self) {
        self.backspace = false;
        let Some(mut cell) = self.pending.take() else {
            return;
        };
        let logical_width = cell.width;
        if self.options.wrap && cell.width > self.options.width {
            cell = Cell::new('\u{fffd}', cell.style);
            self.truncated = true;
        }
        if self.options.wrap {
            if cell.width != 0
                && (self.column == self.options.width
                    || self.column.saturating_add(cell.width) > self.options.width)
            {
                self.next_row();
            }
            if self.selected() {
                self.emit(cell.text(), cell.style);
            }
        } else {
            let end = self.options.horizontal.saturating_add(self.options.width);
            let cell_end = self.column.saturating_add(cell.width);
            if self.column >= self.options.horizontal && cell_end <= end && self.column < end {
                if self.selected() {
                    if self.options.indicator.is_some() && cell.width != 0 && cell_end == end {
                        self.edge = Some(EdgeCell {
                            start: self.output.len(),
                            width: cell.width,
                            preceding_style: self.emitted_style,
                        });
                    }
                    self.emit(cell.text(), cell.style);
                }
            } else {
                self.truncated |= cell.width != 0;
                let visible = cell_end
                    .min(end)
                    .saturating_sub(self.column.max(self.options.horizontal));
                if self.selected() && visible != 0 {
                    self.chop_span(None, visible, cell.style, cell_end >= end);
                }
            }
        }
        self.column = self.column.saturating_add(cell.width);
        self.logical_column = self.logical_column.saturating_add(logical_width);
    }

    fn span(&mut self, bytes: Option<&[u8]>, count: usize, style: StyleState) {
        if count == 0 {
            return;
        }
        self.logical_column = self.logical_column.saturating_add(count);
        if !self.options.wrap {
            let end = self.options.horizontal.saturating_add(self.options.width);
            let start_offset = self
                .options
                .horizontal
                .saturating_sub(self.column)
                .min(count);
            let end_offset = end.saturating_sub(self.column).min(count);
            self.truncated |= start_offset != 0 || end_offset != count;
            if self.selected() && start_offset < end_offset {
                self.chop_span(
                    bytes.map(|bytes| &bytes[start_offset..end_offset]),
                    end_offset - start_offset,
                    style,
                    self.column.saturating_add(end_offset) == end,
                );
            }
            self.column = self.column.saturating_add(count);
            return;
        }
        let mut consumed = 0;
        while consumed < count {
            if self.column == self.options.width {
                self.next_row();
            }
            if !self.selected() {
                let remaining = count - consumed;
                let skip = if self.row < self.first_row {
                    self.first_row
                        .saturating_sub(self.row)
                        .saturating_mul(self.options.width)
                        .saturating_sub(self.column)
                        .min(remaining)
                } else {
                    remaining
                };
                let last = self.column.saturating_add(skip).saturating_sub(1);
                self.row = self.row.saturating_add(last / self.options.width);
                self.column = last % self.options.width + 1;
                consumed += skip;
                continue;
            }
            let length = (self.options.width - self.column).min(count - consumed);
            if let Some(bytes) = bytes {
                self.emit(&bytes[consumed..consumed + length], style);
            } else {
                self.emit_spaces(length, style);
            }
            self.column += length;
            consumed += length;
        }
    }

    fn selected(&self) -> bool {
        self.row >= self.first_row && self.row < self.end_row
    }

    fn chop_span(
        &mut self,
        bytes: Option<&[u8]>,
        count: usize,
        style: StyleState,
        reaches_edge: bool,
    ) {
        let prefix = if self.options.indicator.is_some() && reaches_edge {
            count - 1
        } else {
            count
        };
        if prefix != 0 {
            if let Some(bytes) = bytes {
                self.emit(&bytes[..prefix], style);
            } else {
                self.emit_spaces(prefix, style);
            }
        }
        if prefix != count {
            self.edge = Some(EdgeCell {
                start: self.output.len(),
                width: 1,
                preceding_style: self.emitted_style,
            });
            if let Some(bytes) = bytes {
                self.emit(&bytes[prefix..], style);
            } else {
                self.emit_spaces(1, style);
            }
        }
    }

    fn indicator(&mut self) {
        let (Some(edge), Some(style)) = (self.edge, self.options.indicator) else {
            return;
        };
        self.output.truncate(edge.start);
        self.emitted_style = edge.preceding_style;
        if edge.width > 1 {
            self.emit_spaces(edge.width - 1, StyleState::default());
        }
        self.emit(b">", style);
    }

    fn next_row(&mut self) {
        self.finish_row();
        self.row = self.row.saturating_add(1);
        self.column = 0;
    }

    fn finish_row(&mut self) {
        if self.selected() {
            if self.emitted_style != StyleState::default() {
                self.output.extend_from_slice(b"\x1b[0m");
            }
            self.rows.push(std::mem::take(&mut self.output));
        }
        self.emitted_style = StyleState::default();
    }

    fn emit(&mut self, bytes: &[u8], style: StyleState) {
        self.emit_style(style);
        self.output.extend_from_slice(bytes);
    }

    fn emit_spaces(&mut self, mut count: usize, style: StyleState) {
        self.emit_style(style);
        const SPACES: [u8; 128] = [b' '; 128];
        while count != 0 {
            let length = count.min(SPACES.len());
            self.output.extend_from_slice(&SPACES[..length]);
            count -= length;
        }
    }

    fn emit_style(&mut self, style: StyleState) {
        if self.options.color == ColorLevel::None {
            return;
        }
        let effective = match self.cached_style {
            Some((source, effective)) if source == style => effective,
            _ => {
                let effective = style.for_level(self.options.color);
                self.cached_style = Some((style, effective));
                effective
            }
        };
        if effective != self.emitted_style {
            effective.write(&mut self.output);
            self.emitted_style = effective;
        }
    }

    fn sgr(&mut self, bytes: &[u8]) {
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';' || *byte == b':')
        {
            return;
        }
        let mut parameters = bytes.split(|byte| *byte == b';');
        while let Some(parameter) = parameters.next() {
            if parameter.contains(&b':') {
                self.sgr_colon(parameter);
                continue;
            }
            let Some(value) = number(parameter).or_else(|| parameter.is_empty().then_some(0))
            else {
                continue;
            };
            if matches!(value, 38 | 48 | 58) {
                let mode = parameters.next().and_then(number);
                let color = match mode {
                    Some(5) => parameters
                        .next()
                        .and_then(number)
                        .and_then(|index| u8::try_from(index).ok())
                        .map(Color::Indexed),
                    Some(2) => {
                        let red = parameters
                            .next()
                            .and_then(number)
                            .and_then(|n| u8::try_from(n).ok());
                        let green = parameters
                            .next()
                            .and_then(number)
                            .and_then(|n| u8::try_from(n).ok());
                        let blue = parameters
                            .next()
                            .and_then(number)
                            .and_then(|n| u8::try_from(n).ok());
                        match (red, green, blue) {
                            (Some(red), Some(green), Some(blue)) => {
                                Some(Color::Rgb(red, green, blue))
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(color) = color {
                    self.style.set_color(value, color);
                }
            } else {
                self.style.apply(value);
            }
        }
    }

    fn sgr_colon(&mut self, bytes: &[u8]) {
        let mut values = [None; 8];
        let mut length = 0;
        for parameter in bytes.split(|byte| *byte == b':') {
            if length == values.len() {
                return;
            }
            values[length] = number(parameter);
            length += 1;
        }
        match values[0] {
            Some(4) => {
                if values[1] == Some(0) {
                    self.style.attributes &= !UNDERLINE;
                } else {
                    self.style.attributes |= UNDERLINE;
                }
            }
            Some(code @ (38 | 48 | 58)) => {
                let color = match values[1] {
                    Some(5) if length == 3 => values[2]
                        .and_then(|index| u8::try_from(index).ok())
                        .map(Color::Indexed),
                    Some(2) if length == 5 || length == 6 => {
                        let rgb = &values[length - 3..length];
                        match (rgb[0], rgb[1], rgb[2]) {
                            (Some(red), Some(green), Some(blue))
                                if red <= 255 && green <= 255 && blue <= 255 =>
                            {
                                Some(Color::Rgb(red as u8, green as u8, blue as u8))
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(color) = color {
                    self.style.set_color(code, color);
                }
            }
            Some(code) => self.style.apply(code),
            None => {}
        }
    }
}

impl Cell {
    fn new(character: char, style: StyleState) -> Self {
        let mut bytes = [0; 68];
        let len = character.encode_utf8(&mut bytes).len();
        Self {
            bytes,
            len,
            width: character.width().unwrap_or(1),
            scalars: 1,
            style,
        }
    }

    fn append(&mut self, character: char) -> bool {
        if self.scalars >= 17 || self.len + character.len_utf8() > self.bytes.len() {
            return false;
        }
        self.len += character.encode_utf8(&mut self.bytes[self.len..]).len();
        self.scalars += 1;
        self.width = std::str::from_utf8(self.text())
            .expect("display cell contains valid UTF-8")
            .width();
        true
    }

    fn text(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl StyleState {
    pub(crate) fn truncation(level: ColorLevel) -> Self {
        if level == ColorLevel::Monochrome {
            Self {
                attributes: REVERSE,
                ..Self::default()
            }
        } else {
            Self {
                foreground: Some(Color::Rgb(32, 32, 32)),
                background: Some(Color::Rgb(211, 211, 211)),
                ..Self::default()
            }
            .for_level(level)
        }
    }

    fn apply(&mut self, value: u16) {
        match value {
            0 => *self = Self::default(),
            1 => self.attributes |= BOLD,
            2 => self.attributes |= DIM,
            3 => self.attributes |= ITALIC,
            4 | 21 => self.attributes |= UNDERLINE,
            5 | 6 => self.attributes |= BLINK,
            7 => self.attributes |= REVERSE,
            8 => self.attributes |= CONCEAL,
            9 => self.attributes |= STRIKE,
            22 => self.attributes &= !(BOLD | DIM),
            23 => self.attributes &= !ITALIC,
            24 => self.attributes &= !UNDERLINE,
            25 => self.attributes &= !BLINK,
            27 => self.attributes &= !REVERSE,
            28 => self.attributes &= !CONCEAL,
            29 => self.attributes &= !STRIKE,
            30..=37 => self.foreground = Some(Color::Ansi((value - 30) as u8)),
            39 => self.foreground = None,
            40..=47 => self.background = Some(Color::Ansi((value - 40) as u8)),
            49 => self.background = None,
            53 => self.attributes |= OVERLINE,
            55 => self.attributes &= !OVERLINE,
            59 => self.underline = None,
            90..=97 => self.foreground = Some(Color::Ansi((value - 90 + 8) as u8)),
            100..=107 => self.background = Some(Color::Ansi((value - 100 + 8) as u8)),
            _ => {}
        }
    }

    fn set_color(&mut self, code: u16, color: Color) {
        match code {
            38 => self.foreground = Some(color),
            48 => self.background = Some(color),
            58 => self.underline = Some(color),
            _ => {}
        }
    }

    pub(crate) fn for_level(mut self, level: ColorLevel) -> Self {
        if level == ColorLevel::None {
            return Self::default();
        }
        if level == ColorLevel::Monochrome {
            self.foreground = None;
            self.background = None;
            self.underline = None;
            return self;
        }
        self.foreground = self.foreground.map(|color| color.for_level(level));
        self.background = self.background.map(|color| color.for_level(level));
        self.underline = if matches!(level, ColorLevel::Ansi8 | ColorLevel::Ansi16) {
            None
        } else {
            self.underline.map(|color| color.for_level(level))
        };
        self
    }

    pub(crate) fn write(self, output: &mut Vec<u8>) {
        output.extend_from_slice(b"\x1b[0");
        for (attribute, code) in [
            (BOLD, 1),
            (DIM, 2),
            (ITALIC, 3),
            (UNDERLINE, 4),
            (BLINK, 5),
            (REVERSE, 7),
            (CONCEAL, 8),
            (STRIKE, 9),
            (OVERLINE, 53),
        ] {
            if self.attributes & attribute != 0 {
                write!(output, ";{code}").expect("writing to a Vec cannot fail");
            }
        }
        if let Some(color) = self.foreground {
            color.write(output, 38);
        }
        if let Some(color) = self.background {
            color.write(output, 48);
        }
        if let Some(color) = self.underline {
            color.write(output, 58);
        }
        output.push(b'm');
    }
}

impl Color {
    fn for_level(self, level: ColorLevel) -> Self {
        match (level, self) {
            (ColorLevel::Ansi8, Color::Ansi(index) | Color::Indexed(index)) => {
                Color::Ansi(nearest_ansi(rgb(index), 8))
            }
            (ColorLevel::Ansi8, Color::Rgb(red, green, blue)) => {
                Color::Ansi(nearest_ansi((red, green, blue), 8))
            }
            (ColorLevel::Ansi16, Color::Indexed(index)) => {
                Color::Ansi(nearest_ansi(rgb(index), 16))
            }
            (ColorLevel::Ansi16, Color::Rgb(red, green, blue)) => {
                Color::Ansi(nearest_ansi((red, green, blue), 16))
            }
            (ColorLevel::Ansi256, Color::Rgb(red, green, blue)) => {
                Color::Indexed(nearest_index((red, green, blue)))
            }
            _ => self,
        }
    }

    fn write(self, output: &mut Vec<u8>, code: u16) {
        match self {
            Color::Ansi(index) if code != 58 => {
                let base = if code == 38 { 30 } else { 40 };
                let value = if index < 8 {
                    base + u16::from(index)
                } else {
                    base + 60 + u16::from(index) - 8
                };
                write!(output, ";{value}").expect("writing to a Vec cannot fail");
            }
            Color::Ansi(index) | Color::Indexed(index) => {
                write!(output, ";{code};5;{index}").expect("writing to a Vec cannot fail");
            }
            Color::Rgb(red, green, blue) => {
                write!(output, ";{code};2;{red};{green};{blue}")
                    .expect("writing to a Vec cannot fail");
            }
        }
    }
}

fn number(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0u16, |value, byte| {
        byte.is_ascii_digit().then_some(())?;
        value.checked_mul(10)?.checked_add(u16::from(*byte - b'0'))
    })
}

const ANSI_COLORS: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

fn rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => ANSI_COLORS[usize::from(index)],
        16..=231 => {
            let index = index - 16;
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            (
                LEVELS[usize::from(index / 36)],
                LEVELS[usize::from(index / 6 % 6)],
                LEVELS[usize::from(index % 6)],
            )
        }
        _ => {
            let value = 8 + (index - 232) * 10;
            (value, value, value)
        }
    }
}

fn distance(left: (u8, u8, u8), right: (u8, u8, u8)) -> u32 {
    let red = i32::from(left.0) - i32::from(right.0);
    let green = i32::from(left.1) - i32::from(right.1);
    let blue = i32::from(left.2) - i32::from(right.2);
    (red * red + green * green + blue * blue) as u32
}

fn nearest_ansi(color: (u8, u8, u8), count: usize) -> u8 {
    ANSI_COLORS[..count]
        .iter()
        .enumerate()
        .min_by_key(|(_, entry)| distance(color, **entry))
        .expect("nonempty palette")
        .0 as u8
}

fn nearest_index(color: (u8, u8, u8)) -> u8 {
    fn cube_level(component: u8) -> u8 {
        match component {
            0..=47 => 0,
            48..=115 => 1,
            116..=155 => 2,
            156..=195 => 3,
            196..=235 => 4,
            _ => 5,
        }
    }
    let cube = 16 + cube_level(color.0) * 36 + cube_level(color.1) * 6 + cube_level(color.2);
    let sum = i32::from(color.0) + i32::from(color.1) + i32::from(color.2);
    let gray = 232 + ((sum - 24 + 14) / 30).clamp(0, 23) as u8;
    if distance(color, rgb(gray)) < distance(color, rgb(cube)) {
        gray
    } else {
        cube
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(width: usize, wrap: bool, color: ColorLevel) -> RenderOptions {
        RenderOptions {
            width,
            horizontal: 0,
            wrap,
            tabstop: 8,
            color,
            indicator: None,
        }
    }

    fn render(bytes: &[u8], options: RenderOptions) -> RenderedLine {
        let mut renderer = LineRenderer::new(options, 0, 20);
        renderer.push(bytes);
        renderer.finish()
    }

    fn plain(row: &[u8]) -> String {
        let mut output = Vec::new();
        let mut escape = false;
        for byte in row {
            if *byte == 0x1b {
                escape = true;
            } else if escape {
                if *byte == b'm' {
                    escape = false;
                }
            } else {
                output.push(*byte);
            }
        }
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn wrapping_retains_only_requested_rows_and_exact_width_has_no_empty_row() {
        let mut renderer = LineRenderer::new(options(4, true, ColorLevel::None), 2, 2);
        renderer.push(b"abcd");
        renderer.push(b"efghijklmn");
        let line = renderer.finish();
        assert_eq!(line.rows, [b"ijkl".to_vec(), b"mn".to_vec()]);
        assert_eq!(line.total_rows, 4);
        let line = render(b"abcdefgh", options(4, true, ColorLevel::None));
        assert_eq!(line.rows, [b"abcd".to_vec(), b"efgh".to_vec()]);
        assert_eq!(line.total_rows, 2);
    }

    #[test]
    fn chop_and_horizontal_offsets_count_display_cells() {
        let mut settings = options(4, false, ColorLevel::None);
        settings.horizontal = 2;
        let line = render("a\u{754c}bcdef".as_bytes(), settings);
        assert_eq!(line.rows, [b" bcd".to_vec()]);
        assert!(line.truncated);
        settings.horizontal = 6;
        let line = render(b"a\tbcdef", settings);
        assert_eq!(line.rows, [b"  bc".to_vec()]);
    }

    #[test]
    fn unsafe_ansi_is_removed_across_arbitrary_chunk_boundaries() {
        let bytes = b"a\x1b[2Jb\x1b]52;c;secret\x07c\x1bPignored\x1b\\d\x1b[?25le";
        let mut renderer = LineRenderer::new(options(80, true, ColorLevel::TrueColor), 0, 20);
        for byte in bytes {
            renderer.push(&[*byte]);
        }
        assert_eq!(renderer.finish().rows, [b"abcde".to_vec()]);
        let line = render(
            b"x\x1b]unterminated\nignored",
            options(80, true, ColorLevel::TrueColor),
        );
        assert_eq!(line.rows, [b"x".to_vec()]);
    }

    #[test]
    fn sgr_is_restored_per_row_and_downgraded_to_terminal_palette() {
        let line = render(
            b"\x1b[38;2;255;0;0mabcd",
            options(2, true, ColorLevel::Ansi16),
        );
        assert_eq!(
            line.rows,
            [
                b"\x1b[0;91mab\x1b[0m".to_vec(),
                b"\x1b[0;91mcd\x1b[0m".to_vec()
            ]
        );
        let line = render(
            b"\x1b[38:2::255:0:0mx",
            options(8, true, ColorLevel::Ansi256),
        );
        assert_eq!(line.rows, [b"\x1b[0;38;5;196mx\x1b[0m".to_vec()]);
        let line = render(b"\x1b[1;31mx", options(8, true, ColorLevel::None));
        assert_eq!(line.rows, [b"x".to_vec()]);
        let line = render(b"\x1b[1;31mx", options(8, true, ColorLevel::Monochrome));
        assert_eq!(line.rows, [b"\x1b[0;1mx\x1b[0m".to_vec()]);
    }

    #[test]
    fn eight_color_terminals_receive_only_the_basic_palette() {
        for color in [
            b"\x1b[38;2;255;0;0m".as_slice(),
            b"\x1b[38;5;196m",
            b"\x1b[91m",
        ] {
            let mut renderer = LineRenderer::new(options(8, true, ColorLevel::Ansi8), 0, 1);
            renderer.push(color);
            renderer.push(b"x");
            assert_eq!(renderer.finish().rows, [b"\x1b[0;31mx\x1b[0m".to_vec()]);
        }
        let line = render(b"\x1b[1;101mx", options(8, true, ColorLevel::Ansi8));
        assert_eq!(line.rows, [b"\x1b[0;1;41mx\x1b[0m".to_vec()]);
        let line = render(b"X\x08X _\x08y", options(8, true, ColorLevel::None));
        assert_eq!(line.rows, [b"X y".to_vec()]);
    }

    #[test]
    fn utf8_combining_and_emoji_sequences_survive_split_input() {
        let text = "e\u{0301}\u{1f469}\u{200d}\u{1f4bb}x";
        let mut renderer = LineRenderer::new(options(3, true, ColorLevel::None), 0, 20);
        for byte in text.as_bytes() {
            renderer.push(&[*byte]);
        }
        let line = renderer.finish();
        assert_eq!(
            line.rows,
            [
                "e\u{0301}\u{1f469}\u{200d}\u{1f4bb}".as_bytes().to_vec(),
                b"x".to_vec()
            ]
        );
        assert_eq!(line.total_rows, 2);
    }

    #[test]
    fn backspace_overstrikes_produce_man_page_attributes() {
        let line = render(
            b"X\x08X _\x08y z\x08_",
            options(80, true, ColorLevel::Ansi16),
        );
        assert_eq!(plain(&line.rows[0]), "X y z");
        assert!(line.rows[0].windows(5).any(|bytes| bytes == b"[0;1m"));
        assert!(line.rows[0].windows(5).any(|bytes| bytes == b"[0;4m"));
    }

    #[test]
    fn invalid_utf8_and_controls_cannot_inject_terminal_commands() {
        let line = render(
            b"\xff\xe2x\0\x7f\r\nignored",
            options(80, true, ColorLevel::None),
        );
        assert_eq!(line.rows, ["\u{fffd}\u{fffd}x^@^?".as_bytes().to_vec()]);
        let line = render(b"a\rb", options(80, true, ColorLevel::None));
        assert_eq!(line.rows, [b"a^Mb".to_vec()]);
    }

    #[test]
    fn pathological_zero_width_content_and_sgr_do_not_grow_output() {
        let mut renderer = LineRenderer::new(options(4, true, ColorLevel::TrueColor), 0, 1);
        renderer.push(b"x");
        for _ in 0..100_000 {
            renderer.push(b"\x1b[31m\xcc\x81");
        }
        let line = renderer.finish();
        assert!(line.rows[0].len() < 128);
        assert!(line.truncated);
        assert_eq!(line.total_rows, 1);
    }

    #[test]
    fn retained_memory_does_not_depend_on_skipped_ascii_rows() {
        let mut renderer = LineRenderer::new(options(8, true, ColorLevel::None), 100_000, 2);
        let chunk = [b'x'; 4096];
        for _ in 0..256 {
            renderer.push(&chunk);
        }
        let line = renderer.finish();
        assert_eq!(line.total_rows, 131_072);
        assert_eq!(line.rows, [vec![b'x'; 8], vec![b'x'; 8]]);
    }

    #[test]
    fn viewport_full_allows_stopping_before_a_pathological_suffix() {
        let mut renderer = LineRenderer::new(options(4, true, ColorLevel::None), 0, 2);
        renderer.push(b"abcdefghijk");
        assert!(renderer.viewport_full());
        let line = renderer.finish();
        assert_eq!(line.rows, [b"abcd".to_vec(), b"efgh".to_vec()]);
        assert_eq!(line.total_rows, 3);
        let mut renderer = LineRenderer::new(options(4, false, ColorLevel::None), 0, 1);
        renderer.push(b"abcde");
        assert!(renderer.is_full());
    }

    #[test]
    fn rightmost_pending_cell_is_resolved_before_viewport_is_full() {
        let mut renderer = LineRenderer::new(options(4, true, ColorLevel::None), 0, 1);
        renderer.push(b"abcd");
        assert!(!renderer.viewport_full());
        renderer.push(b"\x08D");
        assert!(!renderer.viewport_full());
        renderer.push(b"e");
        assert!(renderer.viewport_full());
        let line = renderer.finish();
        assert_eq!(line.rows, [b"abcD".to_vec()]);
        assert_eq!(line.total_rows, 2);

        let line = render(b"abcd\n", options(4, true, ColorLevel::None));
        assert_eq!(line.rows, [b"abcd".to_vec()]);
        assert_eq!(line.total_rows, 1);
        for bytes in [b"".as_slice(), b"\n", b"\r\n", b"\x1b[31m\n"] {
            let line = render(bytes, options(4, true, ColorLevel::Ansi16));
            assert_eq!(line.rows, [Vec::<u8>::new()]);
            assert_eq!(line.total_rows, 1);
        }
        let mut renderer = LineRenderer::new(options(4, true, ColorLevel::None), 0, 0);
        renderer.push(b"abcd");
        let line = renderer.finish();
        assert!(line.rows.is_empty());
        assert_eq!(line.total_rows, 1);
    }

    #[test]
    fn truncation_indicator_requires_content_beyond_the_right_edge() {
        let mut settings = options(12, false, ColorLevel::None);
        settings.indicator = Some(StyleState::truncation(settings.color));
        let line = render(b"0123456789abcdefghij", settings);
        assert_eq!(line.rows, [b"0123456789a>".to_vec()]);
        assert_eq!(line.display_width, 20);
        let line = render(b"0123456789ab", settings);
        assert_eq!(line.rows, [b"0123456789ab".to_vec()]);
        assert!(!line.truncated);
        settings.horizontal = 8;
        let line = render(b"0123456789abcdefghij", settings);
        assert_eq!(line.rows, [b"89abcdefghij".to_vec()]);
        settings.horizontal = 9;
        let line = render(b"0123456789abcdefghij", settings);
        assert_eq!(line.rows, [b"9abcdefghij".to_vec()]);
        settings.horizontal = 2;
        let line = render(b"0123456789abcdefghij", settings);
        assert_eq!(line.rows, [b"23456789abc>".to_vec()]);
    }

    #[test]
    fn indicators_replace_whole_unicode_cells_and_fit_one_column() {
        let mut settings = options(4, false, ColorLevel::None);
        settings.indicator = Some(StyleState::truncation(settings.color));
        let line = render("ab\u{754c}\u{0301}x".as_bytes(), settings);
        assert_eq!(line.rows, [b"ab >".to_vec()]);
        assert_eq!(line.display_width, 5);
        let line = render("abc\u{754c}".as_bytes(), settings);
        assert_eq!(line.rows, [b"abc>".to_vec()]);
        let line = render("abcd\u{0301}x".as_bytes(), settings);
        assert_eq!(line.rows, [b"abc>".to_vec()]);
        settings.width = 1;
        for text in ["xy", "\u{754c}", "\u{1f469}\u{200d}\u{1f4bb}"] {
            let line = render(text.as_bytes(), settings);
            assert_eq!(line.rows, [b">".to_vec()]);
        }
        assert_eq!(render(b"x", settings).rows, [b"x".to_vec()]);
        settings.horizontal = 1;
        let line = render("\u{754c}".as_bytes(), settings);
        assert_eq!(line.rows, [b" ".to_vec()]);
        settings.wrap = true;
        settings.horizontal = 0;
        let line = render("\u{754c}\u{754c}".as_bytes(), settings);
        assert_eq!(line.display_width, 4);
        assert_eq!(
            line.rows,
            [
                "\u{fffd}".as_bytes().to_vec(),
                "\u{fffd}".as_bytes().to_vec()
            ]
        );
    }

    #[test]
    fn tab_width_and_indicator_are_independent_of_chunk_boundaries() {
        let mut settings = options(8, false, ColorLevel::None);
        settings.indicator = Some(StyleState::truncation(settings.color));
        let line = render(b"a\tb", settings);
        assert_eq!(line.rows, [b"a      >".to_vec()]);
        assert_eq!(line.display_width, 9);
        assert_eq!(render(b"a\t", settings).rows, [b"a       ".to_vec()]);
        settings.horizontal = 1;
        assert_eq!(render(b"a\tb", settings).rows, [b"       b".to_vec()]);
        settings.horizontal = 0;
        settings.width = 4;
        settings.color = ColorLevel::TrueColor;
        settings.indicator = Some(StyleState::truncation(settings.color));
        let bytes = "\u{1b}[31mab\u{754c}\u{0301}x\u{1b}[0m\t".as_bytes();
        let expected = render(bytes, settings);
        assert_eq!(plain(&expected.rows[0]), "ab >");
        assert_eq!(expected.display_width, 8);
        for size in 1..=bytes.len() {
            let mut renderer = LineRenderer::new(settings, 0, 1);
            for chunk in bytes.chunks(size) {
                renderer.push(chunk);
            }
            let line = renderer.finish();
            assert_eq!(line.rows, expected.rows);
            assert_eq!(line.display_width, expected.display_width);
        }
        assert_eq!(render(b"\x1b[31mx\x08x\x1b[0m", settings).display_width, 1);
    }

    #[test]
    fn indicators_reset_source_style_and_do_not_retain_the_line_suffix() {
        let mut settings = options(4, false, ColorLevel::TrueColor);
        settings.indicator = Some(StyleState::truncation(settings.color));
        let line = render(b"\x1b[1;31mabcdex", settings);
        assert_eq!(
            line.rows,
            [b"\x1b[0;1;31mabc\x1b[0;38;2;32;32;32;48;2;211;211;211m>\x1b[0m".to_vec()]
        );
        let mut renderer = LineRenderer::new(settings, 0, 1);
        let chunk = [b'x'; 4096];
        for _ in 0..256 {
            renderer.push(&chunk);
        }
        let line = renderer.finish();
        assert_eq!(
            line.rows,
            [b"xxx\x1b[0;38;2;32;32;32;48;2;211;211;211m>\x1b[0m".to_vec()]
        );
        assert_eq!(line.display_width, 1_048_576);
    }

    #[test]
    fn indicators_use_contrasting_neutral_colors_for_each_terminal_palette() {
        for (level, marker) in [
            (ColorLevel::None, b">".as_slice()),
            (ColorLevel::Monochrome, b"\x1b[0;7m>\x1b[0m"),
            (ColorLevel::Ansi8, b"\x1b[0;30;47m>\x1b[0m"),
            (ColorLevel::Ansi16, b"\x1b[0;30;47m>\x1b[0m"),
            (ColorLevel::Ansi256, b"\x1b[0;38;5;234;48;5;252m>\x1b[0m"),
            (
                ColorLevel::TrueColor,
                b"\x1b[0;38;2;32;32;32;48;2;211;211;211m>\x1b[0m",
            ),
        ] {
            let mut settings = options(1, false, level);
            settings.indicator = Some(StyleState::truncation(settings.color));
            let line = render(b"\x1b[1;7;31;44mxy", settings);
            assert_eq!(line.rows, [marker.to_vec()]);
            assert_eq!(line.display_width, 2);
            assert_eq!(render(b"x", settings).rows, [b"x".to_vec()]);
        }
        let mut settings = options(4, false, ColorLevel::TrueColor);
        settings.indicator = Some(StyleState::truncation(settings.color));
        let line = render(b"\x1b[1;7;31;44mabcde", settings);
        assert_eq!(
            line.rows,
            [b"\x1b[0;1;7;31;44mabc\x1b[0;38;2;32;32;32;48;2;211;211;211m>\x1b[0m".to_vec()]
        );
        assert_eq!(render(b"next", settings).rows, [b"next".to_vec()]);
    }

    #[test]
    fn explicit_style_carry_preserves_attributes_without_replaying_controls() {
        let first = render(b"\x1b[31mx", options(4, true, ColorLevel::Ansi16));
        let mut renderer =
            LineRenderer::with_style(options(4, true, ColorLevel::Ansi16), 0, 1, first.style);
        renderer.push(b"y");
        assert_eq!(renderer.finish().rows, [b"\x1b[0;31my\x1b[0m".to_vec()]);
    }

    #[test]
    fn color_quantization_matches_nearest_fixed_xterm_color() {
        for red in (0..=255).step_by(17) {
            for green in (0..=255).step_by(17) {
                for blue in (0..=255).step_by(17) {
                    let color = (red, green, blue);
                    let expected = (16..=255)
                        .min_by_key(|index| distance(color, rgb(*index)))
                        .unwrap();
                    assert_eq!(nearest_index(color), expected);
                }
            }
        }
    }
}
