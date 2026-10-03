#!/usr/bin/env python3
"""Behavior checks against an actual terminal, including open input streams."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "benchmarks"))
from pty_driver import PtyProcess, terminal_environment


VO = ROOT / "target/release/vo"


class PagerTerminalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not VO.is_file():
            raise RuntimeError(f"build vo first: binary absent at {VO}")

    def setUp(self):
        directory = os.environ.get("VO_TEST_WORKDIR")
        if directory:
            Path(directory).mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix="vo-pty-", dir=directory)
        self.addCleanup(self.temporary.cleanup)

    def file(self, text: str) -> Path:
        path = Path(self.temporary.name) / "input.txt"
        path.write_text(text, encoding="utf-8")
        return path

    def pager(self, path: Path | None, *options: str, columns=80, rows=24, term="xterm-256color", extra=None, stdin_fd=None):
        command = [str(VO), *options]
        if path is not None:
            command.extend(["--", str(path)])
        child = PtyProcess(
            command, columns=columns, rows=rows,
            environment=terminal_environment(term, extra), stdin_fd=stdin_fd,
        )
        self.addCleanup(child.close)
        return child

    def send_wait(self, child, keys: bytes, predicate):
        output = child.output_bytes
        child.send(keys)
        child.wait_for(predicate, after_bytes=output)

    def quit(self, child):
        child.send(b"q")
        self.assertEqual(child.wait_exit(), 0)
        self.assertTrue(child.termios_restored(), "q must restore the original termios")

    def test_navigation_and_search(self):
        path = self.file("".join(f"line-{index:04d} {'needle-unique' if index in (70, 140) else 'ordinary'}\n" for index in range(200)))
        child = self.pager(path)
        child.wait_for(lambda screen: "line-0000" in screen.row(0))
        self.send_wait(child, b"j", lambda screen: "line-0001" in screen.row(0))
        self.send_wait(child, b"k", lambda screen: "line-0000" in screen.row(0))
        self.send_wait(child, b"5j", lambda screen: "line-0005" in screen.row(0))
        self.send_wait(child, b"g", lambda screen: "line-0000" in screen.row(0))
        self.send_wait(child, b" ", lambda screen: "line-0000" not in screen.row(0))
        self.send_wait(child, b"b", lambda screen: "line-0000" in screen.row(0))
        self.send_wait(child, b"d", lambda screen: "line-0000" not in screen.row(0))
        self.send_wait(child, b"u", lambda screen: "line-0000" in screen.row(0))
        self.send_wait(child, b"/needle-unique\n", lambda screen: screen.contains("line-0070"))
        self.send_wait(child, b"n", lambda screen: screen.contains("line-0140"))
        self.send_wait(child, b"N", lambda screen: screen.contains("line-0070"))
        self.send_wait(child, b"G", lambda screen: screen.contains("line-0199") and "(END)" in screen.row(screen.rows - 1))
        self.send_wait(child, b"g", lambda screen: "line-0000" in screen.row(0))
        self.quit(child)

    def test_ctrl_w_and_resize_change_actual_line_layout(self):
        path = self.file("long-start-" + "A" * 49 + "tail-marker\nNEXT-LINE\n")
        child = self.pager(path, columns=40, rows=10)
        child.wait_for(lambda screen: screen.contains("NEXT-LINE"))
        self.assertFalse(child.screen.contains("tail-marker"))
        self.assertIn("NEXT-LINE", child.screen.row(1))
        self.send_wait(child, b"\x17", lambda screen: screen.contains("tail-marker") and "NEXT-LINE" in screen.row(2))
        self.send_wait(child, b"\x17", lambda screen: not screen.contains("tail-marker") and "NEXT-LINE" in screen.row(1))
        output = child.output_bytes
        child.resize(100, 10)
        child.wait_for(lambda screen: "tail-marker" in screen.row(0), after_bytes=output)
        output = child.output_bytes
        child.resize(24, 8)
        child.wait_for(lambda screen: not screen.contains("tail-marker") and "NEXT-LINE" in screen.row(1), after_bytes=output)
        self.quit(child)

    def test_switching_files_restores_stream_history_and_file_position(self):
        path = self.file("FILE-FIRST\nFILE-LAST\n")
        reader, writer = os.pipe()
        os.write(writer, b"STREAM-FIRST\nSTREAM-LAST\n")
        os.close(writer)
        child = self.pager(path, "-", stdin_fd=reader)
        child.wait_for(lambda screen: "STREAM-FIRST" in screen.row(0))
        self.send_wait(child, b":n", lambda screen: "FILE-FIRST" in screen.row(0))
        self.send_wait(child, b"2g", lambda screen: "FILE-LAST" in screen.row(0))
        self.send_wait(child, b":p", lambda screen: "STREAM-FIRST" in screen.row(0))
        self.send_wait(child, b":n", lambda screen: "FILE-LAST" in screen.row(0))
        self.quit(child)

    def test_open_pipe_is_interactive_and_quits_before_eof(self):
        reader, writer = os.pipe()
        self.addCleanup(os.close, writer)
        child = self.pager(None, stdin_fd=reader, rows=8)
        child.wait_for(lambda screen: bool(screen.row(7).strip()))
        self.assertNotIn("(END)", child.screen.row(7))
        os.write(writer, b"LIVE-FIRST\nlive-second")
        child.wait_for(lambda screen: screen.contains("LIVE-FIRST") and bool(screen.row(screen.rows - 1).strip()))
        child.wait_for(lambda screen: "live-second" in screen.row(1))
        output = child.output_bytes
        os.write(writer, b"-continued\nlive-third")
        child.wait_for(lambda screen: "live-second-continued" in screen.row(1) and "live-third" in screen.row(2), after_bytes=output)
        self.send_wait(child, b"j", lambda screen: "live-second-continued" in screen.row(0))
        # The writer stays open until cleanup. Exit therefore proves q does not
        # wait for the stream to close or for a blocked reader to return.
        self.quit(child)

    def test_wrapped_end_jump_shows_the_last_screen(self):
        path = self.file("FIRST\n" + "wrapped-content " * 100 + "WRAPPED-EOF\n")
        child = self.pager(path, "--wrap", columns=32, rows=8)
        child.wait_for(lambda screen: "FIRST" in screen.row(0))
        self.assertFalse(child.screen.contains("WRAPPED-EOF"))
        self.send_wait(child, b"G", lambda screen: screen.contains("WRAPPED-EOF") and "(END)" in screen.row(7))
        self.assertNotIn("FIRST", child.screen.row(0))
        self.assertTrue(any("wrapped-content" in child.screen.row(row) for row in range(6)))
        self.quit(child)

    def test_follow_empty_file_append_and_truncate(self):
        path = self.file("")
        child = self.pager(path)
        child.wait_for(lambda screen: "(END)" in screen.row(23))
        child.send(b"F")
        output = child.output_bytes
        with path.open("ab") as producer:
            producer.write(b"APPENDED-FIRST\nAPPENDED-SECOND\n")
        child.wait_for(lambda screen: screen.contains("APPENDED-FIRST") and screen.contains("APPENDED-SECOND"), after_bytes=output)
        output = child.output_bytes
        path.write_text("AFTER-TRUNCATE\n", encoding="utf-8")
        child.wait_for(lambda screen: screen.contains("AFTER-TRUNCATE") and not screen.contains("APPENDED-FIRST"), after_bytes=output)
        self.quit(child)

    def test_named_fifo_can_quit_before_a_producer_connects(self):
        path = Path(self.temporary.name) / "input.fifo"
        os.mkfifo(path)
        child = self.pager(path)
        child.wait_for(lambda screen: bool(screen.row(23).strip()))
        self.assertNotIn("(END)", child.screen.row(23))
        self.quit(child)

    def test_search_handles_long_colored_lines_and_exact_anchors(self):
        # NEEDLE-END crosses byte 65536, while SGR surrounds its logical line.
        path = self.file("TOP\nCROSS-\x1b[31m" + "a" * 65517 + "NEEDLE-END\x1b[0m\nAFTER\n")
        child = self.pager(path)
        child.wait_for(lambda screen: "TOP" in screen.row(0))
        self.send_wait(child, b"/^CROSS-a+NEEDLE-END$\n", lambda screen: "CROSS-" in screen.row(0))
        self.send_wait(child, b"g", lambda screen: "TOP" in screen.row(0))
        self.send_wait(child, b"/^NEEDLE-END$\n", lambda screen: "Pattern not found" in screen.row(23))
        self.assertIn("TOP", child.screen.row(0))
        self.quit(child)

    def test_search_prompt_edits_a_unicode_character_at_the_cursor(self):
        path = self.file("TOP\ncaf\u00e9-target\nLAST\n")
        child = self.pager(path)
        child.wait_for(lambda screen: "TOP" in screen.row(0))
        keys = (
            "/caf\u754c-target".encode("utf-8")
            + b"\x1b[D" * 7
            + b"\x7f"
            + "\u00e9".encode("utf-8")
            + b"\x05"
        )
        self.send_wait(child, keys, lambda screen: "/caf\u00e9-target" in screen.row(23))
        self.send_wait(child, b"\n", lambda screen: "caf\u00e9-target" in screen.row(0))
        self.quit(child)

    def test_redirected_file_and_stream_preserve_binary_bytes(self):
        content = b"\x00\xff\xfe\x1b[31mcolored\x1b[0m\r\n\t\x08unterminated"
        path = self.file("")
        path.write_bytes(content)
        for source in ("file", "stream"):
            with self.subTest(source=source):
                command = [str(VO), "--", str(path)] if source == "file" else [str(VO)]
                result = subprocess.run(
                    command, input=content if source == "stream" else None,
                    capture_output=True, timeout=5, check=True,
                    env=terminal_environment(),
                )
                self.assertEqual(result.stdout, content)
                self.assertEqual(result.stderr, b"")

    def test_safe_ansi_does_not_execute_document_controls(self):
        path = self.file(
            "FIRST-KEPT\n"
            "\x1b[31mRED-CONTENT\x1b[0m\n"
            "\x1b[2JCONTROL-KEPT\x1b]52;c;c2VjcmV0\x07\n"
            "\x1b]0;malicious-title\x07TITLE-KEPT\n"
            "\x1b[?1049lBUFFER-KEPT\n"
        )
        child = self.pager(path, "--color=always")
        child.wait_for(lambda screen: screen.contains("BUFFER-KEPT"))
        self.assertTrue(child.screen.contains("FIRST-KEPT"))
        self.assertTrue(child.screen.contains("RED-CONTENT"))
        self.assertTrue(child.screen.contains("CONTROL-KEPT"))
        self.assertTrue(child.screen.contains("TITLE-KEPT"))
        self.assertNotIn(b"\x1b]52;", child.raw)
        self.assertNotIn(b"\x1b]0;malicious-title", child.raw)
        self.assertTrue(any(color is not None for color in child.screen.colors_for("RED-CONTENT")))
        self.quit(child)

    def test_color_capabilities_and_overrides(self):
        path = self.file("\x1b[38;2;255;0;0mCOLOR-MARKER\x1b[0m\n")
        cases = [
            ("xterm-256color", (), None, "indexed"),
            ("xterm-256color", (), {"COLORTERM": "truecolor"}, "rgb"),
            ("linux", (), None, "ansi"),
            ("dumb", (), None, None),
            ("xterm-256color", ("--color=never",), None, None),
            ("xterm-256color", (), {"NO_COLOR": "1"}, None),
        ]
        for term, options, extra, expected in cases:
            with self.subTest(term=term, options=options, environment=extra):
                child = self.pager(path, *options, term=term, extra=extra)
                child.wait_for(lambda screen: screen.contains("COLOR-MARKER"))
                colors = child.screen.colors_for("COLOR-MARKER")
                self.assertTrue(colors)
                if expected is None:
                    self.assertTrue(all(color is None for color in colors), colors)
                else:
                    self.assertTrue(all(color is not None and color[0] == expected for color in colors), colors)
                self.quit(child)

    def test_utf8_widths_are_counted_in_cells(self):
        # Five wide characters occupy ten cells. AB fills twelve; TAIL must
        # appear only after switching to explicit wrapping.
        path = self.file("\u754c" * 5 + "ABTAIL\nNEXT\n" + "e\u0301" * 12 + "Z\n")
        child = self.pager(path, columns=12, rows=8)
        child.wait_for(lambda screen: screen.contains("NEXT"))
        self.assertFalse(child.screen.contains("TAIL"))
        self.assertIn("NEXT", child.screen.row(1))
        self.assertFalse("Z" in child.screen.row(2))
        self.send_wait(child, b"\x17", lambda screen: "TAIL" in screen.row(1) and "NEXT" in screen.row(2))
        self.assertTrue(child.screen.contains("Z"))
        self.quit(child)

    def test_terminal_is_restored_after_signal(self):
        path = self.file("signal-content\n" * 100)
        child = self.pager(path)
        child.wait_for(lambda screen: screen.contains("signal-content"))
        os.kill(child.pid, signal.SIGTERM)
        child.wait_exit()
        self.assertTrue(child.termios_restored(), "SIGTERM must restore the original termios")

    def test_quit_if_one_screen_leaves_content_visible(self):
        path = self.file("ONE-SCREEN-FIRST\nONE-SCREEN-LAST\n")
        child = self.pager(path, "-F", rows=8)
        self.assertEqual(child.wait_exit(), 0)
        self.assertTrue(child.screen.contains("ONE-SCREEN-FIRST"), child.screen.text())
        self.assertTrue(child.screen.contains("ONE-SCREEN-LAST"), child.screen.text())
        self.assertTrue(child.termios_restored(), "-F must restore the original termios")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vo", type=Path, default=VO)
    args, test_arguments = parser.parse_known_args()
    VO = args.vo.resolve()
    unittest.main(argv=[sys.argv[0], *test_arguments], verbosity=2)
