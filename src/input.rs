use memchr::{memchr, memchr_iter};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::time::SystemTime;

const BLOCK_SIZE: usize = 64 * 1024;
const INDEX_STRIDE: u64 = 1024;

enum Input {
    File,
    Stream(File),
}

/// One offset per 1024 lines bounds index memory independently of line lengths.
struct LineIndex {
    anchors: Vec<u64>,
    scanned: u64,
    newlines: u64,
    last_start: u64,
}

struct Cache {
    bytes: Vec<u8>,
    start: u64,
    length: usize,
}

pub struct Source {
    file: File,
    input: Input,
    pub name: String,
    length: u64,
    eof: bool,
    modified: Option<SystemTime>,
    index: LineIndex,
    scan_buffer: Vec<u8>,
    cache: Cache,
}

impl Source {
    pub fn open(path: &Path) -> io::Result<Self> {
        // Nonblocking open also permits quitting a FIFO before its first writer arrives.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        let name = path.to_string_lossy().into_owned();
        let ordinary_file = metadata.is_file()
            && (metadata.len() != 0 || matches!(file.read_at(&mut [0], 0), Ok(0)));
        if ordinary_file {
            Ok(Self::new(file, Input::File, name, metadata.len(), true))
        } else {
            // Virtual files such as /proc entries may have size zero and still yield data.
            Self::stream(file, name)
        }
    }

    pub fn stdin() -> io::Result<Self> {
        // A duplicate lets Source own its descriptor without closing the caller's stdin.
        let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let input = unsafe { File::from_raw_fd(fd) };
        // Redirected regular files also start at the inherited stream offset.
        Self::stream(input, "stdin".into())
    }

    fn stream(input: File, name: String) -> io::Result<Self> {
        Ok(Self::new(
            tempfile::tempfile()?,
            Input::Stream(input),
            name,
            0,
            false,
        ))
    }

    fn new(file: File, input: Input, name: String, length: u64, eof: bool) -> Self {
        let modified = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        Self {
            file,
            input,
            name,
            length,
            eof,
            modified,
            index: LineIndex {
                anchors: vec![0],
                scanned: 0,
                newlines: 0,
                last_start: 0,
            },
            scan_buffer: vec![0; BLOCK_SIZE],
            cache: Cache {
                bytes: vec![0; BLOCK_SIZE],
                start: 0,
                length: 0,
            },
        }
    }

    pub fn stream_fd(&self) -> Option<RawFd> {
        match &self.input {
            Input::Stream(input) if !self.eof => Some(input.as_raw_fd()),
            _ => None,
        }
    }

    pub fn is_stream(&self) -> bool {
        matches!(self.input, Input::Stream(_))
    }

    pub fn release_buffers(&mut self) {
        self.scan_buffer = Vec::new();
        self.cache.bytes = Vec::new();
        self.cache.length = 0;
    }

    pub fn len(&self) -> u64 {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn eof(&self) -> bool {
        self.eof
    }
    pub fn indexed(&self) -> bool {
        self.index.scanned == self.length
    }

    pub fn line_count(&self) -> u64 {
        self.index.newlines + u64::from(self.index.scanned > self.index.last_start)
    }

    /// Read only after poll reports readiness. Each call caps work to one block.
    pub fn ingest(&mut self) -> io::Result<bool> {
        self.scan_buffer.resize(BLOCK_SIZE, 0);
        let Input::Stream(input) = &mut self.input else {
            return Ok(false);
        };
        if self.eof {
            return Ok(false);
        }
        let size = match input.read(&mut self.scan_buffer) {
            Ok(size) => size,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if size == 0 {
            self.eof = true;
            return Ok(true);
        }
        self.file
            .write_all_at(&self.scan_buffer[..size], self.length)?;
        self.index.append(&self.scan_buffer[..size]);
        self.length += size as u64;
        // A partial cached block must be refreshed when data is appended to it.
        if self.cache.start + self.cache.length as u64 == self.length - size as u64 {
            self.cache.length = 0;
        }
        Ok(true)
    }

    /// Incremental file indexing keeps long jumps and searches interruptible.
    pub fn scan_step(&mut self) -> io::Result<bool> {
        if self.indexed() {
            return Ok(false);
        }
        self.scan_buffer.resize(BLOCK_SIZE, 0);
        let remaining = (self.length - self.index.scanned).min(BLOCK_SIZE as u64) as usize;
        let size = self
            .file
            .read_at(&mut self.scan_buffer[..remaining], self.index.scanned)?;
        if size == 0 {
            self.length = self.index.scanned;
            self.cache.length = 0;
            return Ok(false);
        }
        self.index.append(&self.scan_buffer[..size]);
        Ok(true)
    }

    pub fn ensure_line(&mut self, line: u64) -> io::Result<bool> {
        while self.line_count() <= line && !self.indexed() {
            self.scan_step()?;
        }
        Ok(line < self.line_count())
    }

    pub fn line_offset(&mut self, line: u64) -> io::Result<Option<u64>> {
        if !self.ensure_line(line)? {
            return Ok(None);
        }
        let anchor = (line / INDEX_STRIDE) as usize;
        let mut offset = self.index.anchors[anchor];
        let mut remaining = line % INDEX_STRIDE;
        while remaining != 0 {
            let bytes = self.block(offset)?;
            let mut consumed = bytes.len();
            for newline in memchr_iter(b'\n', bytes) {
                remaining -= 1;
                if remaining == 0 {
                    consumed = newline + 1;
                    break;
                }
            }
            offset += consumed as u64;
        }
        Ok(Some(offset))
    }

    pub fn line_at_offset(&mut self, offset: u64) -> io::Result<u64> {
        let offset = offset.min(self.length);
        while self.index.scanned < offset {
            self.scan_step()?;
        }
        let anchor = self
            .index
            .anchors
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let mut position = self.index.anchors[anchor];
        let mut line = anchor as u64 * INDEX_STRIDE;
        while position < offset {
            let bytes = self.block(position)?;
            let count = bytes.len().min((offset - position) as usize);
            line += memchr_iter(b'\n', &bytes[..count]).count() as u64;
            position += count as u64;
        }
        Ok(line.min(self.line_count().saturating_sub(1)))
    }

    /// Feed a logical line in bounded slices; the callback may stop decoding early.
    /// The returned offset is the next logical line, even after early termination.
    pub fn visit_line(
        &mut self,
        start: u64,
        mut consume: impl FnMut(&[u8]) -> bool,
    ) -> io::Result<u64> {
        let mut offset = start;
        let mut decoding = true;
        while offset < self.length {
            let bytes = self.block(offset)?;
            let newline = memchr(b'\n', bytes);
            let end = newline.unwrap_or(bytes.len());
            if decoding {
                decoding = consume(&bytes[..end]);
            }
            offset += (end + usize::from(newline.is_some())) as u64;
            if newline.is_some() {
                break;
            }
        }
        Ok(offset)
    }

    /// Fetch the available bytes in the fixed-size aligned read cache.
    pub fn block(&mut self, offset: u64) -> io::Result<&[u8]> {
        if offset >= self.length {
            return Ok(&[]);
        }
        if self.cache.length == 0
            || offset < self.cache.start
            || offset >= self.cache.start + self.cache.length as u64
        {
            self.cache.bytes.resize(BLOCK_SIZE, 0);
            self.cache.start = offset / BLOCK_SIZE as u64 * BLOCK_SIZE as u64;
            let remaining = (self.length - self.cache.start).min(BLOCK_SIZE as u64) as usize;
            self.cache.length = self
                .file
                .read_at(&mut self.cache.bytes[..remaining], self.cache.start)?;
            if self.cache.length == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "input file changed while being viewed",
                ));
            }
        }
        Ok(&self.cache.bytes[(offset - self.cache.start) as usize..self.cache.length])
    }

    /// Follow a growing regular file. Re-index from the start if it was truncated.
    pub fn refresh(&mut self) -> io::Result<bool> {
        if !matches!(self.input, Input::File) {
            return Ok(false);
        }
        let metadata = self.file.metadata()?;
        let length = metadata.len();
        let modified = metadata.modified().ok();
        if length == self.length && modified == self.modified {
            return Ok(false);
        }
        self.cache.length = 0;
        if length <= self.length {
            self.index = LineIndex {
                anchors: vec![0],
                scanned: 0,
                newlines: 0,
                last_start: 0,
            };
        }
        self.length = length;
        self.modified = modified;
        Ok(true)
    }
}

impl LineIndex {
    fn append(&mut self, bytes: &[u8]) {
        for newline in memchr_iter(b'\n', bytes) {
            self.newlines += 1;
            self.last_start = self.scanned + newline as u64 + 1;
            if self.newlines.is_multiple_of(INDEX_STRIDE) {
                self.anchors.push(self.last_start);
            }
        }
        self.scanned += bytes.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn source(bytes: &[u8]) -> Source {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        Source::new(file, Input::File, "test".into(), bytes.len() as u64, true)
    }

    #[test]
    fn sparse_index_handles_block_boundaries_and_unterminated_lines() {
        let mut bytes = Vec::new();
        for line in 0..4099 {
            writeln!(bytes, "{line:05} {}", "x".repeat(31)).unwrap();
        }
        bytes.extend_from_slice(b"tail");
        let mut source = source(&bytes);
        for line in [4099, 0, 1024, 2047, 4098] {
            let start = source.line_offset(line).unwrap().unwrap();
            assert_eq!(source.line_at_offset(start).unwrap(), line);
            let mut actual = Vec::new();
            source
                .visit_line(start, |chunk| {
                    actual.extend_from_slice(chunk);
                    true
                })
                .unwrap();
            assert!(
                actual.starts_with(
                    if line == 4099 {
                        b"tail".to_vec()
                    } else {
                        format!("{line:05}").into_bytes()
                    }
                    .as_slice()
                )
            );
        }
        assert_eq!(source.line_count(), 4100);
        assert_eq!(source.index.anchors.len(), 5);
    }

    #[test]
    fn clipping_a_huge_line_does_not_retain_it() {
        let bytes = vec![b'x'; BLOCK_SIZE * 9];
        let mut source = source(&bytes);
        let mut calls = 0;
        assert_eq!(
            source
                .visit_line(0, |_| {
                    calls += 1;
                    false
                })
                .unwrap(),
            bytes.len() as u64
        );
        assert_eq!(calls, 1);
        assert_eq!(source.cache.bytes.len(), BLOCK_SIZE);
        assert_eq!(source.index.anchors.len(), 1);
    }

    #[test]
    fn empty_and_trailing_newline_do_not_create_phantom_lines() {
        for (bytes, lines) in [
            (b"".as_slice(), 0),
            (b"\n", 1),
            (b"one\n", 1),
            (b"one\ntwo", 2),
        ] {
            let mut source = source(bytes);
            while !source.indexed() {
                source.scan_step().unwrap();
            }
            assert_eq!(source.line_count(), lines);
            assert_eq!(source.line_offset(lines).unwrap(), None);
        }
    }

    #[test]
    fn growth_and_truncation_refresh_index_and_cache() {
        let mut source = source(b"one\n");
        source.ensure_line(0).unwrap();
        assert_eq!(source.block(0).unwrap(), b"one\n");
        source.file.write_all_at(b"two\n", 4).unwrap();
        assert!(source.refresh().unwrap());
        assert_eq!(source.line_offset(1).unwrap(), Some(4));
        source.file.set_len(2).unwrap();
        assert!(source.refresh().unwrap());
        assert_eq!(source.line_offset(0).unwrap(), Some(0));
        assert_eq!(source.line_count(), 1);
        assert_eq!(source.block(0).unwrap(), b"on");
    }
}
