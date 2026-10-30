use crate::input::Source;
use crate::render::{ColorLevel, LineRenderer, RenderOptions};
use crate::search::{Pattern, Progress, Search};
use crate::terminal::{Capabilities, Key, Terminal};
use crate::theme::ResolvedTheme;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const WORK_INTERVAL: Duration = Duration::from_millis(8);
const MAX_SEARCH_COUNT: usize = 10_000;

#[derive(Clone, Copy, Default)]
pub enum Case {
    #[default]
    Sensitive,
    Smart,
    Insensitive,
}

pub struct Config {
    pub wrap: bool,
    pub line_numbers: bool,
    pub tabstop: usize,
    pub color: ColorLevel,
    pub theme: ResolvedTheme,
    pub no_init: bool,
    pub quit_if_one_screen: bool,
    pub case: Case,
    pub initial: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Position {
    line: u64,
    row: usize,
}

#[derive(Clone, Copy, Default)]
struct HorizontalScroll {
    offset: usize,
    max_width: usize,
}

impl HorizontalScroll {
    fn observe(&mut self, width: usize) {
        self.max_width = self.max_width.max(width);
    }

    fn clamp(&mut self, width: usize) {
        self.offset = self.offset.min(self.max_width.saturating_sub(width));
    }

    fn right(&mut self, count: usize, width: usize) {
        self.offset = self
            .offset
            .saturating_add(count)
            .min(self.max_width.saturating_sub(width));
    }

    fn left(&mut self, count: usize) {
        self.offset = self.offset.saturating_sub(count);
    }
}

#[derive(Default)]
struct Page {
    rows: Vec<Vec<u8>>,
    positions: Vec<Position>,
    next: Option<Position>,
    end: bool,
}

enum Job {
    End,
    Line(u64),
    Percent(u64),
    Search(Search),
}

#[derive(Clone, Copy)]
enum PromptKind {
    Forward,
    Backward,
    Examine,
    Shell,
}

struct Prompt {
    kind: PromptKind,
    text: String,
    count: usize,
    cursor: usize,
}

struct History {
    source: Option<Source>,
    top: Position,
    previous: Position,
    marks: [Option<Position>; 128],
    horizontal: HorizontalScroll,
}

struct Pager {
    source: Source,
    paths: Vec<PathBuf>,
    history: HashMap<usize, History>,
    current: usize,
    config: Config,
    top: Position,
    previous: Position,
    page: Page,
    marks: [Option<Position>; 128],
    horizontal: HorizontalScroll,
    prefix: String,
    command: Option<char>,
    prompt: Option<Prompt>,
    pattern: Option<Arc<Pattern>>,
    job: Option<Job>,
    following: bool,
    help: bool,
    message: String,
    window: Option<usize>,
    half_window: Option<usize>,
    dirty: bool,
    moved: bool,
    content_drawn: bool,
}

pub fn run(paths: Vec<PathBuf>, config: Config, capabilities: Capabilities) -> io::Result<()> {
    let source = open_source(&paths[0])?;
    let mut terminal = Terminal::open(capabilities, config.no_init)?;
    let initial = config.initial.clone();
    let mut pager = Pager {
        source,
        paths,
        history: HashMap::new(),
        current: 0,
        config,
        top: Position::default(),
        previous: Position::default(),
        page: Page::default(),
        marks: [None; 128],
        horizontal: HorizontalScroll::default(),
        prefix: String::new(),
        command: None,
        prompt: None,
        pattern: None,
        job: None,
        following: false,
        help: false,
        message: String::new(),
        window: None,
        half_window: None,
        dirty: true,
        moved: false,
        content_drawn: false,
    };
    pager.source.scan_step()?;
    if let Some(command) = initial {
        pager.initial(&command);
    }
    let mut last_frame = Instant::now() - FRAME_INTERVAL;
    let mut last_follow = Instant::now();
    loop {
        let work_ready = pager.job.is_some() && !pager.source.indexed()
            || matches!(&pager.job, Some(Job::Search(search)) if search.ready(&pager.source));
        let timeout = if work_ready
            || pager.dirty
                && (!pager.content_drawn
                    || last_frame.elapsed() >= FRAME_INTERVAL
                    || pager.job.is_none() && pager.source.eof())
        {
            0
        } else if pager.dirty {
            FRAME_INTERVAL
                .saturating_sub(last_frame.elapsed())
                .as_millis()
                .max(1) as i32
        } else {
            100
        };
        let events = terminal.read_events(pager.source.stream_fd(), timeout)?;
        if events.terminated {
            break;
        }
        if events.input_ready {
            pager.dirty |= pager.source.ingest()?;
        }
        if events.resized {
            pager.top.row = 0;
            pager.dirty = true;
        }
        for key in events.keys {
            if pager.handle(key, &mut terminal)? {
                return Ok(());
            }
        }
        if pager.following && last_follow.elapsed() >= Duration::from_millis(100) {
            pager.dirty |= pager.source.refresh()?;
            if !pager.source.indexed() {
                pager.job = Some(Job::End);
            }
            last_follow = Instant::now();
        }
        let (width, height) = terminal.size();
        let height = height.saturating_sub(1).max(1);
        pager.work(width, height)?;
        if pager.dirty
            && (!pager.content_drawn
                || last_frame.elapsed() >= FRAME_INTERVAL
                || pager.job.is_none() && pager.source.eof())
        {
            pager.draw(&mut terminal)?;
            last_frame = Instant::now();
            pager.dirty = false;
            if pager.config.quit_if_one_screen
                && !pager.moved
                && pager.page.end
                && pager.job.is_none()
            {
                terminal.suspend()?;
                let mut output = io::stdout().lock();
                for row in &pager.page.rows {
                    output.write_all(row)?;
                    output.write_all(b"\n")?;
                }
                output.flush()?;
                return Ok(());
            }
        }
    }
    Ok(())
}

impl Pager {
    fn initial(&mut self, command: &str) {
        let command = command.trim_start_matches('+');
        match command {
            "G" => self.job = Some(Job::End),
            "F" => {
                self.following = true;
                self.job = Some(Job::End);
            }
            value if value.parse::<u64>().is_ok() => {
                self.job = Some(Job::Line(value.parse::<u64>().unwrap().saturating_sub(1)))
            }
            _ => self.message = "Initial command must be +G, +F, or +LINE".into(),
        }
    }

    fn options(&self, width: usize) -> RenderOptions {
        RenderOptions {
            width: width.max(1),
            horizontal: self.horizontal.offset,
            wrap: self.config.wrap,
            tabstop: self.config.tabstop,
            color: self.config.color,
            indicator: Some(self.config.theme.truncation),
        }
    }

    fn content_width(&self, width: usize, height: usize) -> usize {
        width
            .saturating_sub(self.number_width(width, height))
            .max(1)
    }

    fn number_width(&self, width: usize, height: usize) -> usize {
        let line = self
            .source
            .line_count()
            .max(self.top.line + height as u64)
            .max(1);
        let columns = line.ilog10() as usize + 2;
        if self.config.line_numbers && columns < width {
            columns
        } else {
            0
        }
    }

    fn draw(&mut self, terminal: &mut Terminal) -> io::Result<()> {
        let (width, height) = terminal.size();
        let height = height.saturating_sub(1).max(1);
        terminal.set_prompt_cursor(None);
        if self.help {
            let rows: Vec<_> = HELP
                .lines()
                .take(height)
                .map(|line| clip(line, width))
                .collect();
            return terminal.draw(
                &rows,
                &self.config.theme.status.paint(
                    clip("vo help | q or Escape returns to the file", width),
                    width,
                ),
            );
        }
        let number_width = self.number_width(width, height);
        self.horizontal
            .clamp(width.saturating_sub(number_width).max(1));
        let options = self.options(width.saturating_sub(number_width));
        let mut rows = Vec::with_capacity(height);
        let mut positions = Vec::with_capacity(height);
        let mut position = self.top;
        let mut offset = self.source.line_offset(position.line)?;
        let mut next = None;
        while rows.len() < height {
            let Some(start) = offset else {
                break;
            };
            if start >= self.source.len() {
                break;
            }
            let remaining = height - rows.len();
            // One extra row distinguishes a wrapped continuation from the next line.
            let mut renderer = LineRenderer::new(options, position.row, remaining + 1);
            let after = self.source.visit_line(start, |bytes| {
                renderer.push(bytes);
                !options.wrap || !renderer.viewport_full()
            })?;
            let rendered = renderer.finish();
            self.horizontal.observe(rendered.display_width);
            let continuing = rendered.rows.len() > remaining;
            let visible = rendered.rows.len().min(remaining);
            for (index, mut row) in rendered.rows.into_iter().take(visible).enumerate() {
                let row_position = Position {
                    line: position.line,
                    row: position.row + index,
                };
                if number_width != 0 {
                    let number = if row_position.row == 0 {
                        format!("{:>w$} ", position.line + 1, w = number_width - 1)
                    } else {
                        " ".repeat(number_width)
                    };
                    let mut numbered = self.config.theme.line_numbers.paint(number.into_bytes(), 0);
                    numbered.append(&mut row);
                    row = numbered;
                }
                rows.push(row);
                positions.push(row_position);
            }
            if continuing {
                next = Some(Position {
                    line: position.line,
                    row: position.row + visible,
                });
                break;
            }
            position = Position {
                line: position.line + 1,
                row: 0,
            };
            offset = if after < self.source.len() {
                Some(after)
            } else {
                None
            };
            if rows.len() == height && offset.is_some() {
                next = Some(position);
            }
        }
        self.page = Page {
            rows,
            positions,
            next,
            end: offset.is_none() && next.is_none() && self.source.eof(),
        };
        if self.config.quit_if_one_screen && !self.moved && self.page.end && self.job.is_none() {
            return Ok(());
        }
        let status = if let Some(prompt) = &self.prompt {
            let status = format!("{}{}", prompt.label(), prompt.text);
            let prefix = clip(
                &format!("{}{}", prompt.label(), &prompt.text[..prompt.cursor]),
                usize::MAX,
            );
            let caret = String::from_utf8_lossy(&prefix).width();
            let horizontal = caret.saturating_sub(width.saturating_sub(1));
            terminal.set_prompt_cursor(Some(caret - horizontal));
            self.config
                .theme
                .prompt
                .paint(clip_at(&status, width, horizontal), width)
        } else if let Some(command) = self.command {
            self.config
                .theme
                .prompt
                .paint(clip(&command.to_string(), width), width)
        } else if !self.prefix.is_empty() {
            self.config
                .theme
                .prompt
                .paint(clip(&self.prefix, width), width)
        } else if !self.message.is_empty() {
            self.config
                .theme
                .message
                .paint(clip(&self.message, width), width)
        } else {
            self.location_bar(width, options.width)
        };
        self.content_drawn |= !self.source.is_empty();
        terminal.draw(&self.page.rows, &status)
    }

    fn location_bar(&self, width: usize, content_width: usize) -> Vec<u8> {
        let hint: &[u8] = if !self.config.wrap && self.horizontal.max_width > content_width {
            b"C-w Wrap"
        } else {
            b""
        };
        let location_width = width.saturating_sub(hint.len() + usize::from(!hint.is_empty()));
        let count = clip(
            &format!(":{}", self.line_count_text()),
            location_width.saturating_sub(1),
        );
        let mut name = clip(
            &self.source.name,
            location_width.saturating_sub(count.len() + 1),
        );
        if location_width != 0 {
            name.push(b' ');
        }
        let occupied = String::from_utf8_lossy(&name).width() + count.len();
        let mut status = self.config.theme.status.paint(name, 0);
        status.extend(self.config.theme.line_count.paint(count, 0));
        status.extend(self.config.theme.status.paint(
            vec![b' '; width.saturating_sub(hint.len()).saturating_sub(occupied)],
            0,
        ));
        if !hint.is_empty() {
            status.extend(
                self.config
                    .theme
                    .hint
                    .paint(hint[..hint.len().min(width)].to_vec(), 0),
            );
        }
        status
    }

    fn row_count(&mut self, line: u64, width: usize) -> io::Result<usize> {
        if !self.config.wrap {
            return Ok(1);
        }
        let Some(start) = self.source.line_offset(line)? else {
            return Ok(0);
        };
        let mut renderer = LineRenderer::new(self.options(width), 0, 0);
        self.source.visit_line(start, |bytes| {
            renderer.push(bytes);
            true
        })?;
        let rendered = renderer.finish();
        self.horizontal.observe(rendered.display_width);
        Ok(rendered.total_rows.max(1))
    }

    fn move_up(&mut self, count: usize, width: usize) -> io::Result<()> {
        if !self.config.wrap {
            self.top = Position {
                line: self.top.line.saturating_sub(count as u64),
                row: 0,
            };
            return Ok(());
        }
        let mut count = count;
        while count > 0 {
            let within = self.top.row.min(count);
            self.top.row -= within;
            count -= within;
            if count == 0 || self.top.line == 0 {
                break;
            }
            self.top.line -= 1;
            self.top.row = self.row_count(self.top.line, width)?.saturating_sub(1);
            count -= 1;
        }
        Ok(())
    }

    fn move_down(&mut self, mut count: usize, width: usize) -> io::Result<()> {
        if !self.config.wrap {
            let target = self.top.line.saturating_add(count as u64);
            if target < self.source.line_count() {
                self.top = Position {
                    line: target,
                    row: 0,
                };
            } else {
                self.job = Some(Job::Line(target));
            }
            return Ok(());
        }
        if count < self.page.positions.len() {
            self.top = self.page.positions[count];
            return Ok(());
        }
        if let Some(next) = self.page.next {
            let displayed = self.page.positions.len();
            if count >= displayed && displayed > 0 {
                self.top = next;
                count -= displayed;
            }
        }
        while count > 0 {
            let total = self.row_count(self.top.line, width)?;
            let remaining = total.saturating_sub(self.top.row + 1);
            if count <= remaining {
                self.top.row += count;
                break;
            }
            if !self.source.ensure_line(self.top.line + 1)? {
                self.top.row += remaining;
                break;
            }
            self.top.line += 1;
            self.top.row = 0;
            count -= remaining + 1;
        }
        Ok(())
    }

    fn work(&mut self, width: usize, height: usize) -> io::Result<()> {
        let Some(mut job) = self.job.take() else {
            return Ok(());
        };
        let deadline = Instant::now() + WORK_INTERVAL;
        loop {
            match &mut job {
                Job::Search(search) => match search.step(&mut self.source)? {
                    Progress::Found(line) => {
                        self.remember();
                        self.top = Position { line, row: 0 };
                        self.message.clear();
                        self.dirty = true;
                        return Ok(());
                    }
                    Progress::Absent => {
                        self.message = "Pattern not found".into();
                        self.dirty = true;
                        return Ok(());
                    }
                    Progress::Waiting => break,
                    Progress::More => {}
                },
                Job::Line(target) if *target < self.source.line_count() => {
                    self.top = Position {
                        line: *target,
                        row: 0,
                    };
                    self.dirty = true;
                    return Ok(());
                }
                Job::End | Job::Line(_) | Job::Percent(_) if !self.source.indexed() => {
                    self.source.scan_step()?;
                }
                Job::Line(target) if self.source.eof() => {
                    self.top = Position {
                        line: (*target).min(self.source.line_count().saturating_sub(1)),
                        row: 0,
                    };
                    self.dirty = true;
                    return Ok(());
                }
                Job::Percent(percent) if self.source.eof() => {
                    let offset = self.source.len() / 100 * *percent
                        + self.source.len() % 100 * *percent / 100;
                    self.top = Position {
                        line: self.source.line_at_offset(offset)?,
                        row: 0,
                    };
                    self.dirty = true;
                    return Ok(());
                }
                Job::End => {
                    let width = self.content_width(width, height);
                    let previous = self.top;
                    let line = self.source.line_count().saturating_sub(1);
                    self.top = Position {
                        line,
                        row: self.row_count(line, width)?.saturating_sub(1),
                    };
                    self.move_up(height.saturating_sub(1), width)?;
                    self.dirty |= self.top != previous;
                    if self.source.eof() {
                        self.dirty = true;
                        return Ok(());
                    }
                    break;
                }
                _ => break,
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        self.job = Some(job);
        Ok(())
    }

    fn remember(&mut self) {
        self.previous = self.top;
    }

    fn handle(&mut self, key: Key, terminal: &mut Terminal) -> io::Result<bool> {
        self.dirty = true;
        if self.help {
            if matches!(key, Key::Char('q' | 'Q' | '?') | Key::F1 | Key::Escape) {
                self.help = false;
            }
            return Ok(false);
        }
        if self.prompt.is_some() {
            return self.edit_prompt(key, terminal);
        }
        if let Some(command) = self.command.take() {
            match (command, key) {
                (':', Key::Char('q' | 'Q')) => return Ok(true),
                (':', Key::Char('n')) => self.switch(self.current.saturating_add(1))?,
                (':', Key::Char('p')) => self.switch(self.current.saturating_sub(1))?,
                (':', Key::Char('e')) => self.prompt = Some(Prompt::new(PromptKind::Examine, 1)),
                (':', Key::Char('f')) => self.message = self.info(),
                ('m', Key::Char(mark)) if (mark as usize) < self.marks.len() => {
                    self.marks[mark as usize] = Some(self.top)
                }
                ('\'', Key::Char('\'')) => std::mem::swap(&mut self.top, &mut self.previous),
                ('\'', Key::Char(mark)) if (mark as usize) < self.marks.len() => {
                    if let Some(position) = self.marks[mark as usize] {
                        self.remember();
                        self.top = position;
                    } else {
                        self.message = "Mark not set".into();
                    }
                }
                ('-', Key::Char('S')) => {
                    self.config.wrap = !self.config.wrap;
                    self.top.row = 0;
                }
                ('-', Key::Char('N')) => {
                    self.config.line_numbers = !self.config.line_numbers;
                    self.top.row = 0;
                }
                ('-', Key::Char('i')) => self.config.case = Case::Smart,
                ('-', Key::Char('I')) => self.config.case = Case::Insensitive,
                _ => {}
            }
            return Ok(false);
        }
        if let Key::Char(digit @ '0'..='9') = key {
            if self.prefix.len() < 18 {
                self.prefix.push(digit);
            }
            return Ok(false);
        }
        let number = self.prefix.parse::<usize>().ok();
        let explicit = number.is_some();
        let count = number.unwrap_or(1).max(1);
        self.prefix.clear();
        self.message.clear();
        if !matches!(key, Key::Ctrl('l') | Key::Char('r' | 'R')) {
            self.job = None;
            self.following = false;
        }
        let (width, height) = terminal.size();
        let height = height.saturating_sub(1).max(1);
        let width = self.content_width(width, height);
        self.moved = true;
        match key {
            Key::Char('q' | 'Q') => return Ok(true),
            Key::Char('j' | 'e') | Key::Down | Key::Enter | Key::Ctrl('e' | 'n' | 'j' | 'm') => {
                self.move_down(count, width)?
            }
            Key::Char('k' | 'y') | Key::Up | Key::Ctrl('y' | 'p' | 'k') => {
                self.move_up(count, width)?
            }
            Key::Char(' ' | 'f') | Key::PageDown | Key::Ctrl('f' | 'v') => {
                self.move_down(if explicit { count } else { height }, width)?
            }
            Key::Char('b') | Key::PageUp | Key::Ctrl('b') => {
                self.move_up(if explicit { count } else { height }, width)?
            }
            Key::Char('z') => {
                if explicit {
                    self.window = Some(count);
                }
                self.move_down(self.window.unwrap_or(height), width)?;
            }
            Key::Char('w') => {
                if explicit {
                    self.window = Some(count);
                }
                self.move_up(self.window.unwrap_or(height), width)?;
            }
            Key::Char('d') | Key::Ctrl('d') => {
                if explicit {
                    self.half_window = Some(count);
                }
                self.move_down(self.half_window.unwrap_or(height / 2).max(1), width)?;
            }
            Key::Char('u') | Key::Ctrl('u') => {
                if explicit {
                    self.half_window = Some(count);
                }
                self.move_up(self.half_window.unwrap_or(height / 2).max(1), width)?;
            }
            Key::Char('g' | '<') | Key::Home => {
                self.remember();
                self.job = Some(Job::Line(if explicit {
                    count.saturating_sub(1) as u64
                } else {
                    0
                }));
            }
            Key::Char('G' | '>') | Key::End => {
                self.remember();
                self.job = Some(if explicit {
                    Job::Line(count.saturating_sub(1) as u64)
                } else {
                    Job::End
                });
            }
            Key::Char('%' | 'p') => {
                self.remember();
                self.job = Some(Job::Percent(number.unwrap_or(0).min(100) as u64));
            }
            Key::Char('F') => {
                self.following = true;
                self.job = Some(Job::End);
            }
            Key::Char('h') | Key::Left if !self.config.wrap => {
                self.horizontal
                    .left(if explicit { count } else { width / 2 }.max(1));
            }
            Key::Char('l') | Key::Right if !self.config.wrap => {
                self.horizontal
                    .right(if explicit { count } else { width / 2 }.max(1), width);
            }
            Key::Escape | Key::Ctrl('c') => self.message = "Interrupted".into(),
            Key::Ctrl('w') => {
                self.config.wrap = !self.config.wrap;
                self.top.row = 0;
            }
            Key::Char('/') => {
                self.prompt = Some(Prompt::new(
                    PromptKind::Forward,
                    count.min(MAX_SEARCH_COUNT),
                ))
            }
            Key::Ctrl('r') => {
                self.prompt = Some(Prompt::new(
                    PromptKind::Backward,
                    count.min(MAX_SEARCH_COUNT),
                ))
            }
            Key::Char('n' | 'N') => {
                if let Some(pattern) = self.pattern.clone() {
                    let forward = if key == Key::Char('N') {
                        !pattern.forward
                    } else {
                        pattern.forward
                    };
                    self.job = Some(Job::Search(Search::new(
                        &mut self.source,
                        pattern,
                        self.top.line,
                        forward,
                        count.min(MAX_SEARCH_COUNT),
                    )?));
                } else {
                    self.message = "No previous search".into();
                }
            }
            Key::Char('m' | '\'' | ':' | '-') => {
                if let Key::Char(command) = key {
                    self.command = Some(command);
                }
            }
            Key::F1 | Key::Char('?') => self.help = true,
            Key::Char('=') | Key::Ctrl('g') => self.message = self.info(),
            Key::Char('r' | 'R') | Key::Ctrl('l') => {
                self.dirty = true;
                self.source.refresh()?;
            }
            Key::Ctrl('z') => terminal.stop()?,
            Key::Char('!') => self.prompt = Some(Prompt::new(PromptKind::Shell, 1)),
            Key::Char('v') => self.editor(terminal)?,
            _ => {}
        }
        Ok(false)
    }

    fn line_count_text(&self) -> String {
        format!(
            "{}{}",
            self.source.line_count(),
            if self.source.indexed() && self.source.eof() {
                ""
            } else {
                "+"
            }
        )
    }

    fn info(&self) -> String {
        format!(
            "{} | line {} | {} lines | {} bytes | file {}/{}",
            self.source.name,
            self.top.line + 1,
            self.line_count_text(),
            self.source.len(),
            self.current + 1,
            self.paths.len()
        )
    }

    fn switch(&mut self, index: usize) -> io::Result<()> {
        if index >= self.paths.len() {
            self.message = "No next file".into();
            return Ok(());
        }
        if index == self.current {
            return Ok(());
        }
        let mut history = self.history.remove(&index);
        let source = match history.as_mut().and_then(|entry| entry.source.take()) {
            Some(source) => Ok(source),
            None => open_source(&self.paths[index]),
        };
        match source {
            Ok(mut source) => {
                source.scan_step()?;
                let mut previous_source = std::mem::replace(&mut self.source, source);
                previous_source.release_buffers();
                self.history.insert(
                    self.current,
                    History {
                        source: if previous_source.is_stream() {
                            Some(previous_source)
                        } else {
                            None
                        },
                        top: self.top,
                        previous: self.previous,
                        marks: self.marks,
                        horizontal: self.horizontal,
                    },
                );
                self.current = index;
                self.content_drawn = false;
                self.top = history
                    .as_ref()
                    .map_or(Position::default(), |entry| entry.top);
                self.previous = history.as_ref().map_or(self.top, |entry| entry.previous);
                self.marks = history.as_ref().map_or([None; 128], |entry| entry.marks);
                self.horizontal = history
                    .as_ref()
                    .map_or(HorizontalScroll::default(), |entry| entry.horizontal);
                self.page = Page::default();
            }
            Err(error) => {
                self.message = error.to_string();
                if let Some(history) = history {
                    self.history.insert(index, history);
                }
            }
        }
        Ok(())
    }

    fn edit_prompt(&mut self, key: Key, terminal: &mut Terminal) -> io::Result<bool> {
        let prompt = self.prompt.as_mut().unwrap();
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.prompt = None;
            }
            Key::Backspace => {
                if prompt.text.is_empty() {
                    self.prompt = None;
                } else if prompt.cursor > 0 {
                    let start = prompt.previous();
                    prompt.text.drain(start..prompt.cursor);
                    prompt.cursor = start;
                }
            }
            Key::Delete => {
                let end = prompt.next();
                prompt.text.drain(prompt.cursor..end);
            }
            Key::Left | Key::Ctrl('b') => prompt.cursor = prompt.previous(),
            Key::Right | Key::Ctrl('f') => prompt.cursor = prompt.next(),
            Key::Home | Key::Ctrl('a') => prompt.cursor = 0,
            Key::End | Key::Ctrl('e') => prompt.cursor = prompt.text.len(),
            Key::Ctrl('k') => prompt.text.truncate(prompt.cursor),
            Key::Ctrl('u') => {
                prompt.text.clear();
                prompt.cursor = 0;
            }
            Key::Ctrl('w') => {
                let mut start = prompt.cursor;
                while prompt.text[..start].ends_with(char::is_whitespace) {
                    start = prompt.text[..start]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                }
                while start > 0 && !prompt.text[..start].ends_with(char::is_whitespace) {
                    start = prompt.text[..start]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                }
                prompt.text.drain(start..prompt.cursor);
                prompt.cursor = start;
            }
            Key::Char(character) if !character.is_control() => {
                if prompt.text.len() < 4096 {
                    prompt.text.insert(prompt.cursor, character);
                    prompt.cursor += character.len_utf8();
                }
            }
            Key::Enter | Key::Ctrl('m' | 'j') => {
                let prompt = self.prompt.take().unwrap();
                match prompt.kind {
                    PromptKind::Forward | PromptKind::Backward => {
                        let forward = matches!(prompt.kind, PromptKind::Forward);
                        let text = if prompt.text.is_empty() {
                            self.pattern
                                .as_ref()
                                .map_or(String::new(), |pattern| pattern.text.clone())
                        } else {
                            prompt.text
                        };
                        let ignore = match self.config.case {
                            Case::Sensitive => false,
                            Case::Insensitive => true,
                            Case::Smart => !text.chars().any(char::is_uppercase),
                        };
                        match Pattern::compile(text, forward, ignore) {
                            Ok(pattern) => {
                                self.pattern = Some(pattern.clone());
                                self.job = Some(Job::Search(Search::new(
                                    &mut self.source,
                                    pattern,
                                    self.top.line,
                                    forward,
                                    prompt.count,
                                )?));
                            }
                            Err(error) => self.message = format!("Invalid regex: {error}"),
                        }
                    }
                    PromptKind::Examine => {
                        let path = PathBuf::from(prompt.text);
                        let index = self.paths.len();
                        self.paths.push(path);
                        self.switch(index)?;
                        if self.current != index {
                            self.paths.pop();
                        }
                    }
                    PromptKind::Shell => {
                        terminal.suspend()?;
                        let result = std::process::Command::new(
                            std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()),
                        )
                        .arg("-c")
                        .arg(prompt.text)
                        .stdin(std::fs::File::open("/dev/tty")?)
                        .status();
                        terminal.resume()?;
                        self.message = result.map_or_else(
                            |error| error.to_string(),
                            |status| format!("Shell exited with {status}"),
                        );
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn editor(&mut self, terminal: &mut Terminal) -> io::Result<()> {
        if self.source.is_stream() {
            self.message = "Cannot edit a stream".into();
            return Ok(());
        }
        terminal.suspend()?;
        let editor = std::env::var_os("VISUAL")
            .or_else(|| std::env::var_os("EDITOR"))
            .unwrap_or_else(|| "vi".into());
        let result = std::process::Command::new(
            std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()),
        )
        .arg("-c")
        .arg(format!("{} \"$@\"", editor.to_string_lossy()))
        .arg("vo-editor")
        .arg(format!("+{}", self.top.line + 1))
        .arg(&self.paths[self.current])
        .stdin(std::fs::File::open("/dev/tty")?)
        .status();
        terminal.resume()?;
        self.message = result.map_or_else(
            |error| error.to_string(),
            |status| format!("Editor exited with {status}"),
        );
        self.source.refresh()?;
        Ok(())
    }
}

impl Prompt {
    fn new(kind: PromptKind, count: usize) -> Self {
        Self {
            kind,
            text: String::new(),
            count,
            cursor: 0,
        }
    }
    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }
    fn next(&self) -> usize {
        self.cursor
            + self.text[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8)
    }
    fn label(&self) -> &str {
        match self.kind {
            PromptKind::Forward => "/",
            PromptKind::Backward => "?",
            PromptKind::Examine => "Examine: ",
            PromptKind::Shell => "!",
        }
    }
}

fn open_source(path: &Path) -> io::Result<Source> {
    if path == Path::new("-") {
        Source::stdin()
    } else {
        Source::open(path)
    }
}

fn clip(text: &str, width: usize) -> Vec<u8> {
    clip_at(text, width, 0)
}

fn clip_at(text: &str, width: usize, horizontal: usize) -> Vec<u8> {
    if width == 0 {
        return Vec::new();
    }
    let mut renderer = LineRenderer::new(
        RenderOptions {
            width: width.max(1),
            horizontal,
            wrap: false,
            tabstop: 8,
            color: ColorLevel::None,
            indicator: None,
        },
        0,
        1,
    );
    renderer.push(text.as_bytes());
    renderer
        .finish()
        .rows
        .into_iter()
        .next()
        .unwrap_or_default()
}

const HELP: &str = "vo - View Optimally\n\n\
  q                      quit\n\
  j/e/Enter/Down          next line; k/y/Up previous line\n\
  Space/f/PageDown        next page; b/PageUp previous page\n\
  d/Ctrl-D, u/Ctrl-U      down/up half a page\n\
  z, w                   down/up a window (N sets window size)\n\
  g/Home, G/End           first/last line (N selects line N)\n\
  N%, Np                 jump to N percent of the input\n\
  /, Ctrl-R              search forward/backward\n\
  n, N                   repeat search/reverse direction\n\
  F                      follow input; Ctrl-C stops\n\
  h/Left, l/Right         scroll left/right (N cells)\n\
  Ctrl-W, -S             toggle truncation/wrapping\n\
  -N                     toggle line numbers\n\
  mX, 'X, ''             set/jump mark X, return before jump\n\
  :n, :p, :e             next/previous/examine file\n\
  =, Ctrl-G, :f          show file information\n\
  r/R/Ctrl-L             redraw; Ctrl-Z suspends\n\
  v, !command            editor, shell command\n\
  F1, ?                  help\n\
Numeric prefixes apply to navigation and searches.";
