"""A small VT terminal and a real controlling PTY for pager measurements.

Only the pager is a child process. Stream producers use a thread, so wait4()
reports the pager's peak RSS instead of including a producer process.
"""

from __future__ import annotations

import codecs
import errno
import fcntl
import os
from pathlib import Path
import select
import signal
import struct
import subprocess
import sys
import termios
import tempfile
import time
import unicodedata
from dataclasses import dataclass
from typing import Callable


Color = tuple[str, int] | tuple[str, int, int, int] | None
_supervisor: str | None = None
_supervisor_directory: tempfile.TemporaryDirectory | None = None


def supervisor_binary() -> str:
    global _supervisor, _supervisor_directory
    if _supervisor:
        return _supervisor
    if not sys.platform.startswith("linux"):
        raise RuntimeError("PTY resource measurement requires Linux wait4 and posix_spawn setsid")
    root = Path(__file__).resolve().parent.parent
    source = root / "benchmarks/pty_spawn.rs"
    configured_directory = os.environ.get("VO_TEST_WORKDIR")
    if configured_directory:
        directory = Path(configured_directory)
    else:
        _supervisor_directory = tempfile.TemporaryDirectory(prefix="vo-pty-supervisor-")
        directory = Path(_supervisor_directory.name)
    output = directory.resolve() / "pty-spawn"
    output.parent.mkdir(parents=True, exist_ok=True)
    if not output.exists() or output.stat().st_mtime_ns < source.stat().st_mtime_ns:
        subprocess.run(
            ["rustc", "--edition=2024", "-C", "opt-level=2", "-C", "debuginfo=0", str(source), "-o", str(output)],
            check=True,
        )
    _supervisor = str(output)
    return _supervisor


@dataclass
class Cell:
    text: str = " "
    foreground: Color = None
    background: Color = None


class Screen:
    """Model cursor movement, erasure, SGR colors, and terminal cell widths."""

    def __init__(self, columns: int, rows: int):
        self.columns = columns
        self.rows = rows
        self.cells = self._blank()
        self.x = self.y = 0
        self.foreground: Color = None
        self.background: Color = None
        self.wrap = True
        self.wrap_pending = False
        self.scroll_top = 0
        self.scroll_bottom = rows - 1
        self.saved_cursor = (0, 0)
        self.primary = None
        self.state = "text"
        self.sequence = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def _blank(self):
        return [[Cell() for _ in range(self.columns)] for _ in range(self.rows)]

    def resize(self, columns: int, rows: int):
        self.cells = [
            (self.cells[y][:columns] if y < self.rows else [])
            + [Cell() for _ in range(max(0, columns - (len(self.cells[y]) if y < self.rows else 0)))]
            for y in range(rows)
        ]
        self.columns, self.rows = columns, rows
        self.x, self.y = min(self.x, columns - 1), min(self.y, rows - 1)
        self.scroll_top, self.scroll_bottom = 0, rows - 1
        self.wrap_pending = False

    def row(self, index: int) -> str:
        return "".join(cell.text for cell in self.cells[index]).rstrip()

    def text(self) -> str:
        return "\n".join(self.row(index) for index in range(self.rows))

    def contains(self, text: str) -> bool:
        return text in self.text()

    def colors_for(self, text: str) -> list[Color]:
        for row in self.cells:
            rendered = "".join(cell.text for cell in row)
            offset = rendered.find(text)
            if offset >= 0:
                result = []
                position = 0
                for cell in row:
                    if offset <= position < offset + len(text) and cell.text:
                        result.append(cell.foreground)
                    position += len(cell.text)
                return result
        raise AssertionError(f"text absent from terminal: {text!r}\n{self.text()}")

    def feed(self, data: bytes):
        for char in self.decoder.decode(data):
            if self.state == "osc":
                if char == "\x07":
                    self.state = "text"
                elif char == "\x1b":
                    self.state = "osc_escape"
                continue
            if self.state == "osc_escape":
                self.state = "text" if char == "\\" else "osc"
                continue
            if self.state == "charset":
                self.state = "text"
                continue
            if self.state == "escape":
                self.state = "text"
                if char == "[":
                    self.state, self.sequence = "csi", ""
                elif char in "]P^_":
                    self.state = "osc"
                elif char in "()*+":
                    self.state = "charset"
                elif char == "7":
                    self.saved_cursor = (self.x, self.y)
                elif char == "8":
                    self.x, self.y = self.saved_cursor
                elif char == "D":
                    self._linefeed()
                elif char == "M":
                    if self.y == self.scroll_top:
                        self._scroll(-1)
                    else:
                        self.y = max(0, self.y - 1)
                elif char == "E":
                    self.x = 0
                    self._linefeed()
                continue
            if self.state == "csi":
                if "@" <= char <= "~":
                    self._csi(self.sequence, char)
                    self.state = "text"
                else:
                    self.sequence += char
                continue
            if char == "\x1b":
                self.state = "escape"
            elif char == "\r":
                self.x, self.wrap_pending = 0, False
            elif char in "\n\v\f":
                self._linefeed()
            elif char == "\b":
                self.x, self.wrap_pending = max(0, self.x - 1), False
            elif char == "\t":
                self.x = min(self.columns - 1, ((self.x // 8) + 1) * 8)
            elif ord(char) >= 32 and char != "\x7f":
                self._print(char)

    def _print(self, char: str):
        if unicodedata.combining(char) or char in "\u200d\ufe0e\ufe0f":
            previous = self.x if self.wrap_pending else self.x - 1
            while previous > 0 and not self.cells[self.y][previous].text:
                previous -= 1
            if previous >= 0:
                self.cells[self.y][previous].text += char
            return
        width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
        if self.wrap_pending or self.x + width > self.columns:
            if self.wrap:
                self.x = 0
                self._linefeed()
            self.wrap_pending = False
        if self.x + width > self.columns:
            return
        self.cells[self.y][self.x] = Cell(char, self.foreground, self.background)
        if width == 2:
            self.cells[self.y][self.x + 1] = Cell("", self.foreground, self.background)
        self.x += width
        if self.x == self.columns:
            self.x -= 1
            self.wrap_pending = self.wrap

    def _linefeed(self):
        self.wrap_pending = False
        if self.y == self.scroll_bottom:
            self._scroll(1)
        else:
            self.y = min(self.rows - 1, self.y + 1)

    def _scroll(self, count: int):
        region = self.cells[self.scroll_top : self.scroll_bottom + 1]
        amount = min(abs(count), len(region))
        blank = [[Cell() for _ in range(self.columns)] for _ in range(amount)]
        region = region[amount:] + blank if count > 0 else blank + region[: len(region) - amount]
        self.cells[self.scroll_top : self.scroll_bottom + 1] = region

    def _erase(self, row: int, start: int, end: int):
        for column in range(start, end):
            self.cells[row][column] = Cell()

    def _csi(self, parameters: str, operation: str):
        private = parameters.startswith("?")
        parameters = parameters.lstrip("?<>=")
        try:
            values = [int(value) if value else 0 for value in parameters.split(";")]
        except ValueError:
            return
        first = values[0] if values else 0
        count = first or 1
        if operation in "ABCD":
            if operation == "A":
                self.y = max(0, self.y - count)
            elif operation == "B":
                self.y = min(self.rows - 1, self.y + count)
            elif operation == "C":
                self.x = min(self.columns - 1, self.x + count)
            else:
                self.x = max(0, self.x - count)
            self.wrap_pending = False
        elif operation in "Hf":
            self.y = min(self.rows - 1, max(0, (values[0] or 1) - 1))
            self.x = min(self.columns - 1, max(0, ((values[1] if len(values) > 1 else 1) or 1) - 1))
            self.wrap_pending = False
        elif operation in "G`":
            self.x, self.wrap_pending = min(self.columns - 1, count - 1), False
        elif operation == "d":
            self.y, self.wrap_pending = min(self.rows - 1, count - 1), False
        elif operation == "J":
            if first in (2, 3):
                self.cells = self._blank()
            elif first == 0:
                self._erase(self.y, self.x, self.columns)
                for row in range(self.y + 1, self.rows):
                    self._erase(row, 0, self.columns)
            elif first == 1:
                for row in range(self.y):
                    self._erase(row, 0, self.columns)
                self._erase(self.y, 0, self.x + 1)
        elif operation == "K":
            if first == 0:
                self._erase(self.y, self.x, self.columns)
            elif first == 1:
                self._erase(self.y, 0, self.x + 1)
            elif first == 2:
                self._erase(self.y, 0, self.columns)
        elif operation in "ST":
            self._scroll(count if operation == "S" else -count)
        elif operation == "r":
            self.scroll_top = max(0, count - 1)
            self.scroll_bottom = min(self.rows - 1, (values[1] if len(values) > 1 and values[1] else self.rows) - 1)
            self.x = self.y = 0
            self.wrap_pending = False
        elif operation == "s":
            self.saved_cursor = (self.x, self.y)
        elif operation == "u":
            self.x, self.y = self.saved_cursor
        elif operation in "hl" and private:
            enabled = operation == "h"
            if 7 in values:
                self.wrap = enabled
                self.wrap_pending = False
            if any(value in (47, 1047, 1049) for value in values):
                if enabled and self.primary is None:
                    self.primary = (self.cells, self.x, self.y)
                    self.cells, self.x, self.y = self._blank(), 0, 0
                elif not enabled and self.primary is not None:
                    self.cells, self.x, self.y = self.primary
                    self.primary = None
                self.wrap_pending = False
        elif operation == "m":
            index = 0
            while index < len(values):
                value = values[index]
                if value == 0:
                    self.foreground = None
                    self.background = None
                elif value == 39:
                    self.foreground = None
                elif value == 49:
                    self.background = None
                elif 30 <= value <= 37:
                    self.foreground = ("ansi", value - 30)
                elif 90 <= value <= 97:
                    self.foreground = ("ansi", value - 90 + 8)
                elif 40 <= value <= 47:
                    self.background = ("ansi", value - 40)
                elif 100 <= value <= 107:
                    self.background = ("ansi", value - 100 + 8)
                elif value in (38, 48) and index + 2 < len(values):
                    if values[index + 1] == 5:
                        color = ("indexed", values[index + 2])
                        index += 2
                    elif values[index + 1] == 2 and index + 4 < len(values):
                        color = ("rgb", *values[index + 2 : index + 5])
                        index += 4
                    else:
                        index += 1
                        continue
                    if value == 38:
                        self.foreground = color
                    else:
                        self.background = color
                index += 1


def terminal_environment(term: str = "xterm-256color", extra: dict[str, str] | None = None):
    # A whitelist prevents LESS, LESSOPEN, NO_COLOR, and user shell hooks from
    # changing benchmark behavior. No shell is involved in launching the pager.
    environment = {
        "PATH": os.environ.get("PATH", os.defpath),
        "TERM": term,
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "LESSHISTFILE": "/dev/null",
    }
    if extra:
        environment.update(extra)
    return environment


class PtyProcess:
    def __init__(
        self,
        command: list[str],
        *,
        columns: int = 80,
        rows: int = 24,
        stdin_fd: int | None = None,
        environment: dict[str, str] | None = None,
        cpu: int | None = None,
    ):
        supervisor = supervisor_binary()
        self.master, self.slave = os.openpty()
        self.report_reader, report_writer = os.pipe()
        self.original_termios = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        self.screen = Screen(columns, rows)
        self.raw = bytearray()
        self.output_bytes = 0
        self.status = None
        self.rss_bytes = None
        self.helper_status = None
        self.launch_error = None
        self.pid = None
        self.report = bytearray()
        self.closed = False
        # Opening the slave after setsid acquires the controlling TTY. The
        # native supervisor reports wait4() for its pager child, because Linux
        # also retains a Python parent's peak RSS across posix_spawn/vfork exec.
        report_fd = max(250, self.master + 1, self.slave + 1, self.report_reader + 1, report_writer + 1, (stdin_fd or 0) + 1)
        actions = [
            (os.POSIX_SPAWN_OPEN, 2, os.ttyname(self.slave), os.O_RDWR, 0),
            (os.POSIX_SPAWN_DUP2, 2, 1),
            (os.POSIX_SPAWN_DUP2, 2 if stdin_fd is None else stdin_fd, 0),
            (os.POSIX_SPAWN_CLOSE, self.master),
            (os.POSIX_SPAWN_CLOSE, self.slave),
            (os.POSIX_SPAWN_DUP2, report_writer, report_fd),
            (os.POSIX_SPAWN_CLOSE, report_writer),
            (os.POSIX_SPAWN_CLOSE, self.report_reader),
        ]
        if stdin_fd is not None:
            actions.append((os.POSIX_SPAWN_CLOSE, stdin_fd))
        self.start_ns = time.monotonic_ns()
        try:
            self.helper_pid = os.posix_spawnp(
                supervisor, [supervisor, str(report_fd), *command], environment or terminal_environment(),
                file_actions=actions, setsid=True,
            )
        except BaseException:
            os.close(self.master)
            os.close(self.slave)
            os.close(self.report_reader)
            os.close(report_writer)
            raise
        os.close(report_writer)
        if stdin_fd is not None:
            os.close(stdin_fd)
        os.set_blocking(self.master, False)
        os.set_blocking(self.report_reader, False)
        deadline = time.monotonic() + 10
        while self.pid is None:
            self._read_report()
            if self.pid is not None:
                break
            if self.poll_exit() or time.monotonic() > deadline:
                error = self.launch_error or "measurement supervisor failed to launch the pager"
                self.close()
                raise RuntimeError(error)
            select.select([self.report_reader], [], [], 0.05)
        if cpu is not None:
            os.sched_setaffinity(self.pid, {cpu})

    def _read_report(self):
        while True:
            try:
                data = os.read(self.report_reader, 65536)
            except BlockingIOError:
                break
            if not data:
                break
            self.report.extend(data)
        while b"\n" in self.report:
            line, _, rest = self.report.partition(b"\n")
            self.report = bytearray(rest)
            fields = line.decode().split()
            if fields[0] == "PID":
                self.pid = int(fields[1])
            elif fields[0] == "EXIT":
                self.status, self.rss_bytes = int(fields[1]), int(fields[2])
            elif fields[0] == "ERROR":
                self.launch_error = line.decode()

    def read_available(self):
        while True:
            try:
                data = os.read(self.master, 65536)
            except BlockingIOError:
                return
            except OSError as error:
                if error.errno == errno.EIO:
                    return
                raise
            if not data:
                return
            self.output_bytes += len(data)
            self.raw.extend(data)
            if len(self.raw) > 16 * 1024 * 1024:
                del self.raw[: len(self.raw) - 16 * 1024 * 1024]
            self.screen.feed(data)

    def poll_exit(self) -> bool:
        self._read_report()
        if self.helper_status is None:
            pid, status, _ = os.wait4(self.helper_pid, os.WNOHANG)
            if pid:
                self.helper_status = status
                self._read_report()
                if self.status is None:
                    self.launch_error = self.launch_error or f"measurement supervisor exited without pager usage: {status}"
        return self.helper_status is not None

    def wait_for(self, predicate: Callable[[Screen], bool], timeout: float = 10, *, after_bytes: int | None = None):
        deadline = time.monotonic() + timeout
        while True:
            self.read_available()
            if (after_bytes is None or self.output_bytes > after_bytes) and predicate(self.screen):
                return time.monotonic_ns()
            if self.poll_exit():
                error = self.launch_error or f"pager exited before expected display (status {self.exit_code})"
                raise RuntimeError(f"{error}:\n{self.screen.text()}")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"pager display timed out:\n{self.screen.text()}")
            select.select([self.master, self.report_reader], [], [], min(remaining, 0.05))

    def send(self, data: bytes) -> int:
        started = time.monotonic_ns()
        offset = 0
        while offset < len(data):
            try:
                offset += os.write(self.master, data[offset:])
            except BlockingIOError:
                select.select([], [self.master], [], 1)
        return started

    def resize(self, columns: int, rows: int):
        self.screen.resize(columns, rows)
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def wait_exit(self, timeout: float = 10):
        deadline = time.monotonic() + timeout
        while not self.poll_exit():
            self.read_available()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"pager failed to exit:\n{self.screen.text()}")
            select.select([self.master, self.report_reader], [], [], min(remaining, 0.05))
        self.read_available()
        if self.launch_error:
            raise RuntimeError(self.launch_error)
        return self.exit_code

    @property
    def exit_code(self):
        return None if self.status is None else os.waitstatus_to_exitcode(self.status)

    @property
    def peak_rss_bytes(self):
        if self.rss_bytes is None:
            raise RuntimeError("wait_exit must collect resource usage first")
        return self.rss_bytes

    def termios_restored(self):
        return termios.tcgetattr(self.slave) == self.original_termios

    def close(self):
        if self.closed:
            return
        try:
            if not self.poll_exit():
                if self.status is None:
                    try:
                        os.kill(self.pid or self.helper_pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                try:
                    self.wait_exit(2)
                except TimeoutError:
                    try:
                        os.kill(self.pid or self.helper_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    self.wait_exit(2)
        finally:
            os.close(self.master)
            os.close(self.slave)
            os.close(self.report_reader)
            self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
