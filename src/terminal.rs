use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use term::terminfo::parm::{Param::Number, Variables, expand};
use terminfo::{Database, capability::Value};

use crate::render::ColorLevel;

const RESIZED: usize = 1;
const TERMINATED: usize = 2;
const STOPPED: usize = 4;
const ESCAPE_DELAY: Duration = Duration::from_millis(40);
const SIGNAL_POLL_MS: i32 = 100;
static SIGNALS: AtomicUsize = AtomicUsize::new(0);
static TERMINAL_IN_USE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
pub struct Capabilities {
    pub color: ColorLevel,
    pub style: bool,
    controls: Controls,
    usable: bool,
}

#[derive(Clone, Debug, Default)]
struct Controls {
    cursor: Vec<u8>,
    clear_line: Vec<u8>,
    clear_screen: Vec<u8>,
    reset: Vec<u8>,
    enter_screen: Vec<u8>,
    exit_screen: Vec<u8>,
    hide_cursor: Vec<u8>,
    show_cursor: Vec<u8>,
    disable_wrap: Vec<u8>,
    enable_wrap: Vec<u8>,
    enter_keypad: Vec<u8>,
    exit_keypad: Vec<u8>,
    bell: Vec<u8>,
    keys: Vec<(Vec<u8>, Key)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Tab,
    F1,
}

#[derive(Debug, Default)]
pub struct Events {
    pub keys: Vec<Key>,
    pub input_ready: bool,
    pub resized: bool,
    pub terminated: bool,
}

pub struct Terminal {
    tty: File,
    output: io::Stdout,
    original: libc::termios,
    raw: libc::termios,
    capabilities: Capabilities,
    no_init: bool,
    active: bool,
    _signals: SignalHandlers,
    decoder: Decoder,
    frame: Vec<u8>,
    expansion: Variables,
    last_size: (usize, usize),
    prompt_cursor: Option<usize>,
}

struct SignalHandlers {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

struct Decoder {
    pending: Vec<u8>,
    pending_since: Option<Instant>,
    keys: Vec<(Vec<u8>, Key)>,
    pasting: bool,
}

enum Decoded {
    Key(Key, usize),
    Skip(usize),
    Paste(bool, usize),
    Pending,
}

impl Capabilities {
    pub fn detect() -> Self {
        let term = env::var("TERM").unwrap_or_default();
        let database = if term.is_empty() || term == "dumb" {
            None
        } else {
            Database::from_name(&term).ok()
        };
        let colorterm = env::var("COLORTERM").ok();
        Self::from_database(database.as_ref(), colorterm.as_deref())
    }

    pub fn interactive(&self) -> bool {
        self.usable
            && io::stdout().is_terminal()
            && OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open("/dev/tty")
                .is_ok()
    }

    fn from_database(database: Option<&Database>, colorterm: Option<&str>) -> Self {
        let Some(database) = database else {
            return Self {
                color: ColorLevel::None,
                style: false,
                controls: Controls::default(),
                usable: false,
            };
        };
        let controls = Controls::from_database(database);
        let cursor_works = expand(
            &controls.cursor,
            &[Number(0), Number(0)],
            &mut Variables::new(),
        )
        .is_ok_and(|bytes| !bytes.is_empty());
        let style = ansi_sgr(&controls.reset);
        let foreground = raw_string(database, "set_a_foreground");
        let ansi_color = expand(&foreground, &[Number(1)], &mut Variables::new())
            .is_ok_and(|bytes| ansi_sgr(&bytes));
        let colors = match database.raw("max_colors") {
            Some(Value::Number(count)) => *count,
            _ => 0,
        };
        let true_color = colorterm
            .is_some_and(|value| value.eq_ignore_ascii_case("truecolor") || value == "24bit")
            || matches!(database.raw("Tc"), Some(Value::True))
            || matches!(database.raw("RGB"), Some(Value::True))
            || colors >= 1 << 24;
        let color = if !style {
            ColorLevel::None
        } else if !ansi_color {
            ColorLevel::Monochrome
        } else if true_color {
            ColorLevel::TrueColor
        } else if colors >= 256 {
            ColorLevel::Ansi256
        } else if colors >= 16 {
            ColorLevel::Ansi16
        } else if colors >= 8 {
            ColorLevel::Ansi8
        } else {
            ColorLevel::Monochrome
        };
        let usable = cursor_works && !controls.clear_line.is_empty();
        Self {
            color,
            style,
            controls,
            usable,
        }
    }
}

impl Terminal {
    pub fn open(capabilities: Capabilities, no_init: bool) -> io::Result<Self> {
        if !capabilities.interactive() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "terminal lacks cursor positioning or line clearing",
            ));
        }
        let tty = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/tty")?;
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(tty.as_raw_fd(), &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        let signals = SignalHandlers::install()?;
        let decoder = Decoder::new(capabilities.controls.keys.clone());
        let mut terminal = Self {
            tty,
            output: io::stdout(),
            original,
            raw,
            capabilities,
            no_init,
            active: false,
            _signals: signals,
            decoder,
            frame: Vec::new(),
            expansion: Variables::new(),
            last_size: (0, 0),
            prompt_cursor: None,
        };
        terminal.resume()?;
        terminal.last_size = terminal.size();
        Ok(terminal)
    }

    pub fn size(&self) -> (usize, usize) {
        for fd in [self.output.as_raw_fd(), self.tty.as_raw_fd()] {
            let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
            if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0
                && size.ws_col > 0
                && size.ws_row > 0
            {
                return (usize::from(size.ws_col), usize::from(size.ws_row));
            }
        }
        (dimension("COLUMNS", 80), dimension("LINES", 24))
    }

    pub fn read_events(&mut self, stream_fd: Option<RawFd>, timeout_ms: i32) -> io::Result<Events> {
        let mut events = Events::default();
        self.take_signals(&mut events)?;
        if events.terminated {
            return Ok(events);
        }
        events.keys = self.decoder.decode(false);
        let mut timeout = if timeout_ms < 0 {
            SIGNAL_POLL_MS
        } else {
            timeout_ms.min(SIGNAL_POLL_MS)
        };
        if !events.keys.is_empty() || events.resized {
            timeout = 0;
        } else if let Some(since) = self.decoder.pending_since {
            timeout = timeout.min(
                ESCAPE_DELAY
                    .saturating_sub(since.elapsed())
                    .as_millis()
                    .min(i32::MAX as u128) as i32,
            );
        }
        let mut descriptors = [
            libc::pollfd {
                fd: self.tty.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stream_fd.unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // A finite poll also handles a signal delivered between reading SIGNALS and poll.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        self.take_signals(&mut events)?;
        if events.terminated {
            return Ok(events);
        }
        let tty_events = descriptors[0].revents;
        if tty_events & libc::POLLIN != 0 {
            let mut bytes = [0u8; 4096];
            match self.tty.read(&mut bytes) {
                Ok(0) => events.terminated = true,
                Ok(count) => {
                    self.decoder.feed(&bytes[..count]);
                    events.keys.extend(self.decoder.decode(false));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        if tty_events & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            events.terminated = true;
        }
        events.input_ready =
            descriptors[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        let expired = self
            .decoder
            .pending_since
            .is_some_and(|since| since.elapsed() >= ESCAPE_DELAY);
        events.keys.extend(self.decoder.decode(expired));
        let size = self.size();
        if size != self.last_size {
            self.last_size = size;
            events.resized = true;
        }
        Ok(events)
    }

    pub fn set_prompt_cursor(&mut self, column: Option<usize>) {
        self.prompt_cursor = column;
    }

    pub fn draw(&mut self, rows: &[Vec<u8>], status: &[u8]) -> io::Result<()> {
        let (width, height) = self.size();
        self.frame.clear();
        for row in 0..height.saturating_sub(1) {
            self.position(row, 0)?;
            self.frame
                .extend_from_slice(&self.capabilities.controls.reset);
            self.frame
                .extend_from_slice(&self.capabilities.controls.clear_line);
            if let Some(content) = rows.get(row) {
                self.frame.extend_from_slice(content);
            }
            self.frame
                .extend_from_slice(&self.capabilities.controls.reset);
        }
        self.position(height.saturating_sub(1), 0)?;
        self.frame
            .extend_from_slice(&self.capabilities.controls.reset);
        self.frame
            .extend_from_slice(&self.capabilities.controls.clear_line);
        self.frame.extend_from_slice(status);
        self.frame
            .extend_from_slice(&self.capabilities.controls.reset);
        if let Some(column) = self.prompt_cursor {
            self.frame
                .extend_from_slice(&self.capabilities.controls.show_cursor);
            self.position(
                height.saturating_sub(1),
                column.min(width.saturating_sub(1)),
            )?;
        } else {
            self.frame
                .extend_from_slice(&self.capabilities.controls.hide_cursor);
        }
        let mut output = self.output.lock();
        output.write_all(&self.frame)?;
        output.flush()
    }

    pub fn bell(&mut self) -> io::Result<()> {
        self.output.write_all(&self.capabilities.controls.bell)?;
        self.output.flush()
    }

    pub fn suspend(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        self.frame.clear();
        let controls = &self.capabilities.controls;
        self.frame.extend_from_slice(&controls.reset);
        self.frame.extend_from_slice(&controls.enable_wrap);
        self.frame.extend_from_slice(&controls.exit_keypad);
        self.frame.extend_from_slice(&controls.show_cursor);
        let position_result = if !self.no_init && !controls.enter_screen.is_empty() {
            self.frame.extend_from_slice(&controls.exit_screen);
            Ok(())
        } else {
            let result = self.position(self.size().1.saturating_sub(1), 0);
            self.frame
                .extend_from_slice(&self.capabilities.controls.clear_line);
            self.frame.extend_from_slice(b"\r\n");
            result
        };
        let output_result = position_result.and_then(|()| {
            self.output
                .write_all(&self.frame)
                .and_then(|()| self.output.flush())
        });
        let termios_result = set_attributes(self.tty.as_raw_fd(), &self.original);
        output_result.and(termios_result)
    }

    pub fn resume(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        if let Err(error) = set_attributes(self.tty.as_raw_fd(), &self.raw) {
            let _ = set_attributes(self.tty.as_raw_fd(), &self.original);
            return Err(error);
        }
        self.active = true;
        self.frame.clear();
        let controls = &self.capabilities.controls;
        if !self.no_init {
            self.frame.extend_from_slice(&controls.enter_screen);
        }
        self.frame.extend_from_slice(&controls.reset);
        self.frame.extend_from_slice(&controls.hide_cursor);
        self.frame.extend_from_slice(&controls.disable_wrap);
        self.frame.extend_from_slice(&controls.enter_keypad);
        self.frame.extend_from_slice(&controls.clear_screen);
        if let Err(error) = self
            .output
            .write_all(&self.frame)
            .and_then(|()| self.output.flush())
        {
            let _ = self.suspend();
            return Err(error);
        }
        Ok(())
    }

    pub fn stop(&mut self) -> io::Result<()> {
        self.suspend()?;
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = libc::SIG_DFL;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
        if unsafe { libc::sigaction(libc::SIGTSTP, &action, &mut previous) } != 0 {
            let error = io::Error::last_os_error();
            let _ = self.resume();
            return Err(error);
        }
        let result = unsafe { libc::raise(libc::SIGTSTP) };
        let error = if result != 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        };
        unsafe { libc::sigaction(libc::SIGTSTP, &previous, std::ptr::null_mut()) };
        self.resume()?;
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn position(&mut self, row: usize, column: usize) -> io::Result<()> {
        let cursor = &self.capabilities.controls.cursor;
        let start = self.frame.len();
        let bytes = expand(
            cursor,
            &[Number(row as i32), Number(column as i32)],
            &mut self.expansion,
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        self.frame.extend_from_slice(&bytes);
        strip_padding(&mut self.frame, start);
        Ok(())
    }

    fn take_signals(&mut self, events: &mut Events) -> io::Result<()> {
        let signals = SIGNALS.swap(0, Ordering::Relaxed);
        events.terminated |= signals & TERMINATED != 0;
        events.resized |= signals & RESIZED != 0;
        if signals & STOPPED != 0 && !events.terminated {
            self.stop()?;
            events.resized = true;
        }
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

impl Controls {
    fn from_database(database: &Database) -> Self {
        let mut controls = Self {
            cursor: raw_string(database, "cursor_address"),
            clear_line: expanded_string(database, "clr_eol"),
            clear_screen: expanded_string(database, "clear_screen"),
            reset: expanded_string(database, "exit_attribute_mode"),
            enter_screen: expanded_string(database, "enter_ca_mode"),
            exit_screen: expanded_string(database, "exit_ca_mode"),
            hide_cursor: expanded_string(database, "cursor_invisible"),
            show_cursor: expanded_string(database, "cursor_normal"),
            disable_wrap: expanded_string(database, "exit_am_mode"),
            enable_wrap: expanded_string(database, "enter_am_mode"),
            enter_keypad: expanded_string(database, "keypad_xmit"),
            exit_keypad: expanded_string(database, "keypad_local"),
            bell: expanded_string(database, "bell"),
            keys: Vec::new(),
        };
        for (name, key) in [
            ("key_up", Key::Up),
            ("key_down", Key::Down),
            ("key_left", Key::Left),
            ("key_right", Key::Right),
            ("key_ppage", Key::PageUp),
            ("key_npage", Key::PageDown),
            ("key_home", Key::Home),
            ("key_end", Key::End),
            ("key_dc", Key::Delete),
            ("key_backspace", Key::Backspace),
            ("key_f1", Key::F1),
        ] {
            let bytes = raw_string(database, name);
            if !bytes.is_empty() {
                controls.keys.push((bytes, key));
            }
        }
        // Every change to a terminal mode must have a matching restoration sequence.
        pair(&mut controls.enter_screen, &mut controls.exit_screen);
        pair(&mut controls.hide_cursor, &mut controls.show_cursor);
        pair(&mut controls.disable_wrap, &mut controls.enable_wrap);
        pair(&mut controls.enter_keypad, &mut controls.exit_keypad);
        controls
    }
}

impl SignalHandlers {
    fn install() -> io::Result<Self> {
        if TERMINAL_IN_USE.swap(true, Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an interactive terminal is already open",
            ));
        }
        SIGNALS.store(0, Ordering::Relaxed);
        let mut handlers = Self {
            previous: Vec::new(),
        };
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = handle_signal as *const () as usize;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        for signal in [
            libc::SIGWINCH,
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGHUP,
            libc::SIGTSTP,
            libc::SIGCONT,
        ] {
            let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
            if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
                return Err(io::Error::last_os_error());
            }
            handlers.previous.push((signal, previous));
        }
        Ok(handlers)
    }
}

impl Drop for SignalHandlers {
    fn drop(&mut self) {
        for (signal, previous) in self.previous.iter().rev() {
            unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
        }
        TERMINAL_IN_USE.store(false, Ordering::Release);
    }
}

extern "C" fn handle_signal(signal: libc::c_int) {
    let flag = match signal {
        libc::SIGWINCH | libc::SIGCONT => RESIZED,
        libc::SIGTSTP => STOPPED,
        _ => TERMINATED,
    };
    SIGNALS.fetch_or(flag, Ordering::Relaxed);
}

impl Decoder {
    fn new(keys: Vec<(Vec<u8>, Key)>) -> Self {
        Self {
            pending: Vec::new(),
            pending_since: None,
            keys,
            pasting: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        if self.pending_since.is_none() && !bytes.is_empty() {
            self.pending_since = Some(Instant::now());
        }
    }

    fn decode(&mut self, expired: bool) -> Vec<Key> {
        let mut result = Vec::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let bytes = &self.pending[consumed..];
            let decoded = self.known_key(bytes, expired).unwrap_or_else(|| {
                if bytes[0] == 0x1b {
                    self.escape(bytes, expired)
                } else {
                    self.character(bytes, expired)
                }
            });
            match decoded {
                Decoded::Key(key, count) => {
                    result.push(key);
                    consumed += count;
                }
                Decoded::Skip(count) => consumed += count,
                Decoded::Paste(active, count) => {
                    self.pasting = active;
                    consumed += count;
                }
                Decoded::Pending => break,
            }
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
            self.pending_since = if self.pending.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
        }
        result
    }

    fn known_key(&self, bytes: &[u8], expired: bool) -> Option<Decoded> {
        for (sequence, key) in &self.keys {
            if bytes.starts_with(sequence) {
                return Some(if self.pasting {
                    Decoded::Skip(sequence.len())
                } else {
                    Decoded::Key(*key, sequence.len())
                });
            }
            if sequence.starts_with(bytes) && !expired {
                return Some(Decoded::Pending);
            }
        }
        None
    }

    fn escape(&self, bytes: &[u8], expired: bool) -> Decoded {
        if bytes.len() == 1 {
            return if expired {
                if self.pasting {
                    Decoded::Skip(1)
                } else {
                    Decoded::Key(Key::Escape, 1)
                }
            } else {
                Decoded::Pending
            };
        }
        if bytes.starts_with(b"\x1b[[") {
            if bytes.len() < 4 {
                return if expired {
                    Decoded::Skip(bytes.len())
                } else {
                    Decoded::Pending
                };
            }
            return if bytes[3] == b'A' && !self.pasting {
                Decoded::Key(Key::F1, 4)
            } else {
                Decoded::Skip(4)
            };
        }
        if bytes[1] == b'[' || bytes[1] == b'O' {
            let Some(end) = bytes[2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
                .map(|index| index + 2)
            else {
                return if expired || bytes.len() > 64 {
                    Decoded::Skip(bytes.len())
                } else {
                    Decoded::Pending
                };
            };
            let count = end + 1;
            let parameter = bytes[2..end]
                .split(|byte| *byte == b';')
                .next()
                .unwrap_or_default();
            if bytes[end] == b'~' && parameter == b"200" {
                return Decoded::Paste(true, count);
            }
            if bytes[end] == b'~' && parameter == b"201" {
                return Decoded::Paste(false, count);
            }
            if self.pasting {
                return Decoded::Skip(count);
            }
            let key = match bytes[end] {
                b'A' => Some(Key::Up),
                b'B' => Some(Key::Down),
                b'C' => Some(Key::Right),
                b'D' => Some(Key::Left),
                b'H' => Some(Key::Home),
                b'F' => Some(Key::End),
                b'P' if (bytes[1] == b'O' && parameter.is_empty()) || parameter == b"1" => {
                    Some(Key::F1)
                }
                b'~' => match parameter {
                    b"1" | b"7" => Some(Key::Home),
                    b"3" => Some(Key::Delete),
                    b"4" | b"8" => Some(Key::End),
                    b"5" => Some(Key::PageUp),
                    b"6" => Some(Key::PageDown),
                    b"11" => Some(Key::F1),
                    _ => None,
                },
                _ => None,
            };
            return match key {
                Some(key) => Decoded::Key(key, count),
                None => Decoded::Skip(count),
            };
        }
        if self.pasting {
            Decoded::Skip(1)
        } else {
            Decoded::Key(Key::Escape, 1)
        }
    }

    fn character(&self, bytes: &[u8], expired: bool) -> Decoded {
        let first = bytes[0];
        if self.pasting && (first < 0x20 || first == 0x7f) {
            return match first {
                b'\n' | b'\r' | b'\t' => Decoded::Key(Key::Char(' '), 1),
                _ => Decoded::Skip(1),
            };
        }
        let key = match first {
            b'\r' | b'\n' => Some(Key::Enter),
            0x08 | 0x7f => Some(Key::Backspace),
            b'\t' => Some(Key::Tab),
            0 => Some(Key::Ctrl(' ')),
            1..=26 => Some(Key::Ctrl(char::from(first + b'a' - 1))),
            28 => Some(Key::Ctrl('\\')),
            29 => Some(Key::Ctrl(']')),
            30 => Some(Key::Ctrl('^')),
            31 => Some(Key::Ctrl('_')),
            0x20..=0x7e => Some(Key::Char(char::from(first))),
            _ => None,
        };
        if let Some(key) = key {
            return Decoded::Key(key, 1);
        }
        let count = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Decoded::Key(Key::Char('\u{fffd}'), 1),
        };
        if bytes.len() < count {
            return if expired {
                Decoded::Key(Key::Char('\u{fffd}'), 1)
            } else {
                Decoded::Pending
            };
        }
        let character = std::str::from_utf8(&bytes[..count])
            .ok()
            .and_then(|value| value.chars().next());
        match character {
            Some(character) => Decoded::Key(Key::Char(character), count),
            None => Decoded::Key(Key::Char('\u{fffd}'), 1),
        }
    }
}

fn raw_string(database: &Database, name: &str) -> Vec<u8> {
    match database.raw(name) {
        Some(Value::String(bytes)) => bytes.clone(),
        _ => Vec::new(),
    }
}

fn expanded_string(database: &Database, name: &str) -> Vec<u8> {
    let raw = raw_string(database, name);
    let mut bytes = expand(&raw, &[], &mut Variables::new()).unwrap_or_default();
    strip_padding(&mut bytes, 0);
    bytes
}

fn strip_padding(bytes: &mut Vec<u8>, start: usize) {
    let mut read = start;
    let mut write = start;
    while read < bytes.len() {
        if bytes[read..].starts_with(b"$<")
            && let Some(end) = bytes[read + 2..].iter().position(|byte| *byte == b'>')
        {
            read += end + 3;
            continue;
        }
        bytes[write] = bytes[read];
        write += 1;
        read += 1;
    }
    bytes.truncate(write);
}

fn pair(enter: &mut Vec<u8>, exit: &mut Vec<u8>) {
    if enter.is_empty() || exit.is_empty() {
        enter.clear();
        exit.clear();
    }
}

fn ansi_sgr(bytes: &[u8]) -> bool {
    bytes.windows(2).enumerate().any(|(index, prefix)| {
        prefix == b"\x1b["
            && bytes[index + 2..]
                .iter()
                .find(|byte| !(0x30u8..=0x3f).contains(*byte))
                .is_some_and(|byte| *byte == b'm')
    })
}

fn dimension(name: &str, fallback: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0 && *value <= usize::from(u16::MAX))
        .unwrap_or(fallback)
}

fn set_attributes(fd: RawFd, attributes: &libc::termios) -> io::Result<()> {
    loop {
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, attributes) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder() -> Decoder {
        Decoder::new(vec![(b"\x1bOA".to_vec(), Key::Up)])
    }

    #[test]
    fn decodes_navigation_and_control_keys() {
        let mut decoder = decoder();
        decoder.feed(b"j\x17\x03\x1b[A\x1b[6~\x1bOH\x7f\n");
        assert_eq!(
            decoder.decode(false),
            [
                Key::Char('j'),
                Key::Ctrl('w'),
                Key::Ctrl('c'),
                Key::Up,
                Key::PageDown,
                Key::Home,
                Key::Backspace,
                Key::Enter,
            ]
        );
    }

    #[test]
    fn waits_for_split_escape_sequences_and_unicode() {
        let mut decoder = decoder();
        decoder.feed(b"\x1b");
        assert!(decoder.decode(false).is_empty());
        decoder.feed(b"O");
        assert!(decoder.decode(false).is_empty());
        decoder.feed(b"A\xc3");
        assert_eq!(decoder.decode(false), [Key::Up]);
        decoder.feed(b"\xa9");
        assert_eq!(decoder.decode(false), [Key::Char('\u{e9}')]);
        decoder.feed(b"\x1b");
        assert_eq!(decoder.decode(true), [Key::Escape]);
    }

    #[test]
    fn decodes_split_f1_fallbacks() {
        for sequence in [
            &b"\x1bOP"[..],
            &b"\x1b[11~"[..],
            &b"\x1b[[A"[..],
            &b"\x1b[1;2P"[..],
        ] {
            let mut decoder = Decoder::new(Vec::new());
            for (index, byte) in sequence.iter().enumerate() {
                decoder.feed(std::slice::from_ref(byte));
                let keys = decoder.decode(false);
                if index + 1 == sequence.len() {
                    assert_eq!(keys, [Key::F1], "sequence {sequence:?}");
                } else {
                    assert!(keys.is_empty(), "sequence {sequence:?}, byte {index}");
                }
            }
            assert!(decoder.decode(false).is_empty());
        }
        let mut decoder = Decoder::new(Vec::new());
        decoder.feed(b"\x1b[[B");
        assert!(decoder.decode(false).is_empty());
    }

    #[test]
    fn decodes_custom_terminfo_f1_sequences() {
        for sequence in [&b"\x1b[97~"[..], &b"\x01F"[..]] {
            let mut builder = Database::new();
            builder.name("test");
            builder.raw("key_f1", sequence.to_vec());
            let database = builder.build().unwrap();
            let controls = Controls::from_database(&database);
            let mut decoder = Decoder::new(controls.keys);
            decoder.feed(&sequence[..1]);
            assert!(decoder.decode(false).is_empty());
            decoder.feed(&sequence[1..]);
            assert_eq!(decoder.decode(false), [Key::F1]);
        }
    }

    #[test]
    fn drops_unknown_control_sequences_and_paste_controls() {
        let mut decoder = decoder();
        decoder.feed(
            b"\x1b[99~\x1b[200~hi\x17\x1b[A\x1bOP\x1b[11~\x1b[[A\x1b[1;2P\nthere\x1b[201~\x17",
        );
        assert_eq!(
            decoder.decode(false),
            [
                Key::Char('h'),
                Key::Char('i'),
                Key::Char(' '),
                Key::Char('t'),
                Key::Char('h'),
                Key::Char('e'),
                Key::Char('r'),
                Key::Char('e'),
                Key::Ctrl('w'),
            ]
        );
    }

    #[test]
    fn capabilities_require_positioning_and_clearing() {
        let mut builder = Database::new();
        builder.name("test");
        builder.raw("cursor_address", b"\x1b[%i%p1%d;%p2%dH".to_vec());
        let database = builder.build().unwrap();
        assert!(!Capabilities::from_database(Some(&database), None).usable);
        let mut builder = Database::new();
        builder.name("test");
        builder.raw("cursor_address", b"\x1b[%i%p1%d;%p2%dH".to_vec());
        builder.raw("clr_eol", b"\x1b[K".to_vec());
        let database = builder.build().unwrap();
        assert!(Capabilities::from_database(Some(&database), None).usable);
        assert!(!Capabilities::from_database(None, None).usable);
    }

    #[test]
    fn detects_declared_palettes_and_truecolor_environment() {
        for (colors, expected) in [
            (0, ColorLevel::Monochrome),
            (7, ColorLevel::Monochrome),
            (8, ColorLevel::Ansi8),
            (16, ColorLevel::Ansi16),
            (256, ColorLevel::Ansi256),
            (1 << 24, ColorLevel::TrueColor),
        ] {
            let mut builder = Database::new();
            builder.name("test");
            builder.raw("exit_attribute_mode", b"\x1b(B\x1b[m".to_vec());
            builder.raw("set_a_foreground", b"\x1b[3%p1%dm".to_vec());
            builder.raw("max_colors", colors);
            let database = builder.build().unwrap();
            let capabilities = Capabilities::from_database(Some(&database), None);
            assert_eq!(capabilities.color, expected);
            assert!(capabilities.style);
            assert_eq!(
                Capabilities::from_database(Some(&database), Some("truecolor")).color,
                ColorLevel::TrueColor
            );
        }
    }

    #[test]
    fn strips_terminfo_padding_without_removing_regular_bytes() {
        let mut bytes = b"before\x1b[A$<5/>after$<1>!".to_vec();
        strip_padding(&mut bytes, 6);
        assert_eq!(bytes, b"before\x1b[Aafter!");
    }

    #[test]
    fn detects_sgr_with_charset_resets() {
        assert!(ansi_sgr(b"\x1b(B\x1b[m"));
        assert!(ansi_sgr(b"\x1b[m\x0f"));
        assert!(ansi_sgr(b"\x1b[38;5;1m"));
        assert!(!ansi_sgr(b"\x1b[H"));
    }
}
