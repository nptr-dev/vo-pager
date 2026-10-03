use crate::ansi::Text;
use crate::input::Source;
use regex_automata::Input;
use regex_automata::dfa::{Automaton, dense};
use regex_automata::util::{primitives::StateID, syntax};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

pub struct Pattern {
    dfa: dense::DFA<Vec<u32>>,
    start: StateID,
    pub text: String,
    pub forward: bool,
}

/// A DFA (deterministic finite automaton) carries regex state across cache blocks.
/// Search therefore has no line-size limit and does not copy long lines.
pub struct Search {
    pattern: Arc<Pattern>,
    offset: u64,
    line: u64,
    stop: Option<u64>,
    state: StateID,
    matched: bool,
    started: bool,
    visible: Text,
    remaining: usize,
    backward_matches: VecDeque<u64>,
    backward: bool,
    skip_partial: bool,
}

pub enum Progress {
    More,
    Waiting,
    Found(u64),
    Absent,
}

impl Pattern {
    pub fn compile(text: String, forward: bool, ignore_case: bool) -> Result<Arc<Self>, String> {
        let dfa = dense::Builder::new()
            .configure(
                dense::Config::new()
                    .dfa_size_limit(Some(4 * 1024 * 1024))
                    .determinize_size_limit(Some(16 * 1024 * 1024)),
            )
            .syntax(
                syntax::Config::new()
                    .unicode(false)
                    .utf8(false)
                    .case_insensitive(ignore_case),
            )
            .build(&text)
            .map_err(|error| error.to_string())?;
        let start = dfa
            .start_state_forward(&Input::new(b""))
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(Self {
            dfa,
            start,
            text,
            forward,
        }))
    }
}

impl Search {
    pub fn ready(&self, source: &Source) -> bool {
        self.offset < self.stop.unwrap_or(source.len()).min(source.len())
            || source.eof()
            || self.backward
    }

    pub fn new(
        source: &mut Source,
        pattern: Arc<Pattern>,
        line: u64,
        forward: bool,
        count: usize,
    ) -> io::Result<Self> {
        let next_line = if source.is_empty() {
            0
        } else {
            line.saturating_add(1)
        };
        let next_offset = if forward {
            source.line_offset(next_line)?
        } else {
            None
        };
        let skip_partial = forward
            && next_offset.is_none()
            && !source.is_empty()
            && !source.eof()
            && source.block(source.len() - 1)?[0] != b'\n';
        let (offset, first_line, stop) = if forward {
            (next_offset.unwrap_or(source.len()), next_line, None)
        } else {
            (
                0,
                0,
                Some(source.line_offset(line)?.unwrap_or(source.len())),
            )
        };
        Ok(Self {
            state: pattern.start,
            pattern,
            offset,
            line: first_line,
            stop,
            matched: false,
            started: false,
            visible: Text::default(),
            remaining: count.max(1),
            backward_matches: VecDeque::new(),
            backward: !forward,
            skip_partial,
        })
    }

    pub fn step(&mut self, source: &mut Source) -> io::Result<Progress> {
        let end = self.stop.unwrap_or(source.len()).min(source.len());
        if self.offset >= end {
            if !self.backward && !source.eof() {
                return Ok(Progress::Waiting);
            }
            if self.started
                && let Some(line) = self.finish_line()
            {
                return Ok(Progress::Found(line));
            }
            return Ok(
                if self.backward && self.backward_matches.len() == self.remaining {
                    Progress::Found(*self.backward_matches.front().unwrap())
                } else {
                    Progress::Absent
                },
            );
        }
        let bytes = source.block(self.offset)?;
        let count = bytes.len().min((end - self.offset) as usize);
        for byte in &bytes[..count] {
            self.offset += 1;
            // A stream may still be extending the current line. Start after its LF.
            if self.skip_partial {
                if *byte == b'\n' {
                    self.skip_partial = false;
                }
                continue;
            }
            if *byte == b'\n' {
                if let Some(line) = self.finish_line() {
                    return Ok(Progress::Found(line));
                }
                self.line += 1;
            } else {
                self.started = true;
                if self.visible.push(*byte) && *byte != b'\r' {
                    self.state = self.pattern.dfa.next_state(self.state, *byte);
                    self.matched |= self.pattern.dfa.is_match_state(self.state);
                }
            }
        }
        Ok(Progress::More)
    }

    fn finish_line(&mut self) -> Option<u64> {
        let state = self.pattern.dfa.next_eoi_state(self.state);
        let matched = self.matched || self.pattern.dfa.is_match_state(state);
        self.state = self.pattern.start;
        self.visible = Text::default();
        self.matched = false;
        self.started = false;
        if matched {
            if self.backward {
                self.backward_matches.push_back(self.line);
                if self.backward_matches.len() > self.remaining {
                    self.backward_matches.pop_front();
                }
            } else {
                self.remaining -= 1;
                if self.remaining == 0 {
                    return Some(self.line);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    fn find(bytes: &[u8], expression: &str, line: u64, forward: bool) -> Option<u64> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        let mut source = Source::open(file.path()).unwrap();
        let pattern = Pattern::compile(expression.into(), forward, false).unwrap();
        let mut search = Search::new(&mut source, pattern, line, forward, 1).unwrap();
        loop {
            match search.step(&mut source).unwrap() {
                Progress::Found(line) => return Some(line),
                Progress::Absent => return None,
                Progress::More => {}
                Progress::Waiting => panic!("regular file search must finish"),
            }
        }
    }

    #[test]
    fn regex_crosses_cache_boundaries_without_buffering_lines() {
        let mut bytes = b"first\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', 65530));
        bytes.extend_from_slice(b"needle-tail\nlast\n");
        assert_eq!(find(&bytes, "a+needle-tail$", 0, true), Some(1));
        assert_eq!(find(&bytes, "needle", 2, false), Some(1));
    }

    #[test]
    fn anchors_are_logical_line_boundaries_and_ansi_is_ignored() {
        let bytes = b"first\npre\x1b[31mfix\x1b[0m\nnext\n";
        assert_eq!(find(bytes, "^prefix$", 0, true), Some(1));
        assert_eq!(find(bytes, "^fix$", 0, true), None);
        assert_eq!(find(bytes, "first", 2, false), Some(0));
    }

    #[test]
    fn stream_search_skips_the_unfinished_current_line_and_handles_empty_input() {
        for initial in [b"first-partial".as_slice(), b""] {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            let mut source =
                Source::stream(File::from(OwnedFd::from(reader)), "stream".into()).unwrap();
            if !initial.is_empty() {
                writer.write_all(initial).unwrap();
                source.ingest().unwrap();
            }
            let pattern = Pattern::compile("^needle$".into(), true, false).unwrap();
            let mut search = Search::new(&mut source, pattern, 0, true, 1).unwrap();
            assert!(matches!(
                search.step(&mut source).unwrap(),
                Progress::Waiting
            ));
            writer.write_all(b"needle\nOTHER\nneedle\n").unwrap();
            source.ingest().unwrap();
            assert!(
                matches!(search.step(&mut source).unwrap(), Progress::Found(line) if line == if initial.is_empty() { 0 } else { 2 })
            );
        }
    }
}
