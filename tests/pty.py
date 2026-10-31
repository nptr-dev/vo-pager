#!/usr/bin/env python3
"""Behavior checks against an actual terminal, including open input streams."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
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

    def config(self, theme: dict, relative="config.json") -> Path:
        path = Path(self.temporary.name) / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"theme": theme}), encoding="utf-8")
        return path

    def assert_cell_colors(self, cells, foreground, background):
        self.assertEqual(
            [(cell.foreground, cell.background) for cell in cells],
            [(foreground, background)] * len(cells),
        )

    def has_position(self, screen, line: int) -> bool:
        return re.search(rf" :{line}(?: +C-w Wrap)?$", screen.row(screen.rows - 1)) is not None

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
        child.wait_for(lambda screen: "line-0000" in screen.row(0) and self.has_position(screen, 1))
        self.send_wait(child, b"j", lambda screen: "line-0001" in screen.row(0) and self.has_position(screen, 2))
        self.send_wait(child, b"k", lambda screen: "line-0000" in screen.row(0) and self.has_position(screen, 1))
        self.send_wait(child, b"5j", lambda screen: "line-0005" in screen.row(0) and self.has_position(screen, 6))
        self.send_wait(child, b"g", lambda screen: "line-0000" in screen.row(0) and self.has_position(screen, 1))
        self.send_wait(child, b" ", lambda screen: "line-0023" in screen.row(0) and self.has_position(screen, 24))
        self.send_wait(child, b"b", lambda screen: "line-0000" in screen.row(0) and self.has_position(screen, 1))
        self.send_wait(child, b"d", lambda screen: "line-0011" in screen.row(0) and self.has_position(screen, 12))
        self.send_wait(child, b"u", lambda screen: "line-0000" in screen.row(0) and self.has_position(screen, 1))
        self.send_wait(child, b"/needle-unique\n", lambda screen: "line-0070" in screen.row(0) and self.has_position(screen, 71))
        self.send_wait(child, b"n", lambda screen: "line-0140" in screen.row(0) and self.has_position(screen, 141))
        self.send_wait(child, b"N", lambda screen: "line-0070" in screen.row(0) and self.has_position(screen, 71))
        self.send_wait(child, b"''", lambda screen: "line-0140" in screen.row(0) and self.has_position(screen, 141))
        self.send_wait(child, b"''", lambda screen: "line-0070" in screen.row(0) and self.has_position(screen, 71))
        self.send_wait(child, b"G", lambda screen: "line-0177" in screen.row(0) and screen.contains("line-0199") and self.has_position(screen, 178))
        self.send_wait(child, b"g", lambda screen: "line-0000" in screen.row(0) and self.has_position(screen, 1))
        self.quit(child)

    def test_ctrl_w_and_resize_change_actual_line_layout(self):
        path = self.file("long-start-" + "A" * 49 + "tail-marker\nNEXT-LINE\n")
        child = self.pager(path, columns=40, rows=10)
        child.wait_for(lambda screen: screen.contains("NEXT-LINE"))
        self.assertFalse(child.screen.contains("tail-marker"))
        self.assertIn("NEXT-LINE", child.screen.row(1))
        self.assertTrue(child.screen.row(0).endswith(">"))
        self.assertEqual(child.screen.row(9), f"{str(path)[:28]} :1".ljust(32) + "C-w Wrap")
        self.send_wait(child, b"\x17", lambda screen: screen.contains("tail-marker") and "NEXT-LINE" in screen.row(2))
        self.assertEqual(child.screen.row(9), f"{str(path)[:37]} :1")
        self.send_wait(child, b"j", lambda screen: screen.row(0) == "A" * 20 + "tail-marker" and self.has_position(screen, 1))
        self.send_wait(child, b"j", lambda screen: screen.row(0) == "NEXT-LINE" and self.has_position(screen, 2))
        self.send_wait(child, b"k", lambda screen: screen.row(0) == "A" * 20 + "tail-marker" and self.has_position(screen, 1))
        self.send_wait(child, b"k", lambda screen: screen.row(0).startswith("long-start-") and self.has_position(screen, 1))
        self.send_wait(child, b"\x17", lambda screen: not screen.contains("tail-marker") and "NEXT-LINE" in screen.row(1))
        output = child.output_bytes
        child.resize(100, 10)
        child.wait_for(lambda screen: "tail-marker" in screen.row(0), after_bytes=output)
        self.assertFalse(child.screen.row(0).endswith(">"))
        self.assertEqual(child.screen.row(9), f"{str(path)[:97]} :1")
        output = child.output_bytes
        child.resize(24, 8)
        child.wait_for(lambda screen: not screen.contains("tail-marker") and "NEXT-LINE" in screen.row(1), after_bytes=output)
        self.assertTrue(child.screen.row(0).endswith(">"))
        self.assertEqual(child.screen.row(7), f"{str(path)[:12]} :1 C-w Wrap")
        self.quit(child)

    def test_config_sets_initial_layout_and_cli_flags_override_it(self):
        path = self.file("0123456789TAIL\nNEXT\n")
        config = Path(self.temporary.name) / "config.json"
        cases = [
            ({}, (), False),
            ({"wrap": False}, (), False),
            ({"wrap": True}, (), True),
            ({"wrap": True}, ("-S",), False),
            ({"wrap": True}, ("--chop-long-lines",), False),
            ({"wrap": False}, ("--wrap",), True),
            ({"wrap": True}, ("--wrap", "-S"), False),
            ({"wrap": False}, ("-S", "--wrap"), True),
        ]
        for configuration, options, wrapped in cases:
            with self.subTest(configuration=configuration, options=options):
                config.write_text(json.dumps(configuration), encoding="utf-8")
                child = self.pager(path, *options, columns=12, rows=5, extra={"VO_CONFIG": str(config)})
                expected = ["0123456789TA", "IL", "NEXT"] if wrapped else ["0123456789T>", "NEXT"]
                child.wait_for(lambda screen: [screen.row(row) for row in range(len(expected))] == expected)
                if not wrapped:
                    self.send_wait(child, b"l", lambda screen: screen.row(0) == "23456789TAIL")
                self.quit(child)

    def test_location_bar_preserves_position_when_clipping_unicode_filename(self):
        path = Path(self.temporary.name) / "\u754ce\u0301\u754c.txt"
        path.write_text("FIRST\nLAST", encoding="utf-8")
        prefix = str(path.parent) + "/"
        child = self.pager(path, "--wrap", columns=len(prefix) + 12, rows=5)
        child.wait_for(lambda screen: screen.row(4) == f"{path} :1")
        output = child.output_bytes
        child.resize(len(prefix) + 7, 5)
        child.wait_for(lambda screen: screen.row(4) == prefix + "\u754ce\u0301  :1", after_bytes=output)
        self.assert_cell_colors(child.screen.cells[4][-3:-2], ("indexed", 16), ("indexed", 252))
        self.assert_cell_colors(child.screen.cells[4][-2:], ("indexed", 231), ("indexed", 234))
        output = child.output_bytes
        child.resize(2, 5)
        child.wait_for(lambda screen: screen.row(4) == " :", after_bytes=output)
        self.assert_cell_colors(child.screen.cells[4][:1], ("indexed", 16), ("indexed", 252))
        self.assert_cell_colors(child.screen.cells[4][1:], ("indexed", 231), ("indexed", 234))
        self.quit(child)

    def test_hjkl_scroll_help_keys_and_backward_search(self):
        path = self.file("START-" + "0123456789" * 9 + "-END\nDOWN-LINE\nSEARCH-NEEDLE\nLAST\n")
        child = self.pager(path, columns=40, rows=10)
        child.wait_for(lambda screen: screen.row(0).startswith("START-"))
        self.send_wait(child, b"6l", lambda screen: screen.row(0).startswith("0123456789"))
        self.send_wait(child, b"6h", lambda screen: screen.row(0).startswith("START-"))
        self.send_wait(child, b"j", lambda screen: screen.row(0) == "DOWN-LINE")
        self.send_wait(child, b"k", lambda screen: screen.row(0).startswith("START-"))
        self.send_wait(child, b"?", lambda screen: screen.row(0) == "vo - View Optimally")
        self.send_wait(child, b"q", lambda screen: screen.row(0).startswith("START-"))
        for sequence in (b"\x1bOP", b"\x1b[11~", b"\x1b[[A"):
            self.send_wait(child, sequence, lambda screen: screen.row(0) == "vo - View Optimally")
            self.send_wait(child, sequence, lambda screen: screen.row(0).startswith("START-"))
        self.send_wait(child, b"3g", lambda screen: screen.row(0) == "SEARCH-NEEDLE")
        self.send_wait(child, b"\x12START-\n", lambda screen: screen.row(0).startswith("START-"))
        self.quit(child)

    def test_default_ui_has_contrasting_foreground_and_background(self):
        path = self.file("\x1b[1;7;31;44m" + "A" * 60 + "\x1b[0m\nSECOND\n")
        cases = [
            ("xterm-256color", (), None, ("indexed", 234), ("indexed", 16), ("indexed", 252), ("indexed", 231), ("indexed", 234)),
            ("xterm-256color", (), {"COLORTERM": "truecolor"}, ("rgb", 32, 32, 32), ("rgb", 0, 0, 0), ("rgb", 211, 211, 211), ("rgb", 255, 255, 255), ("rgb", 32, 32, 32)),
            ("linux", (), None, ("ansi", 0), ("ansi", 0), ("ansi", 7), ("ansi", 7), ("ansi", 0)),
            ("xterm-256color", ("--color=never",), None, None, None, None, None, None),
            ("xterm-256color", (), {"NO_COLOR": "1"}, None, None, None, None, None),
        ]
        for term, options, extra, foreground, filename_foreground, filename_background, status_foreground, status_background in cases:
            with self.subTest(term=term, options=options, environment=extra):
                child = self.pager(path, *options, columns=12, rows=5, term=term, extra=extra)
                child.wait_for(lambda screen: screen.row(0) == "A" * 11 + ">" and screen.row(1) == "SECOND")
                marker = child.screen.cells[0][11]
                self.assertEqual(marker.foreground, foreground)
                self.assertEqual(marker.background, filename_background)
                self.assertIsNone(child.screen.cells[1][0].background)
                self.assertEqual(child.screen.row(4), " :1 C-w Wrap")
                self.assert_cell_colors(child.screen.cells[4][:1], filename_foreground, filename_background)
                self.assert_cell_colors(child.screen.cells[4][1:], status_foreground, status_background)

                output = child.output_bytes
                child.resize(max(80, len(str(path)) + 8), 5)
                child.wait_for(lambda screen: screen.row(0) == "A" * 60 and screen.row(4) == f"{path} :1", after_bytes=output)
                position_start = len(str(path)) + 1
                self.assert_cell_colors(child.screen.cells[4][:position_start], filename_foreground, filename_background)
                self.assert_cell_colors(child.screen.cells[4][position_start:position_start + 2], status_foreground, status_background)
                padding = child.screen.cells[4][position_start + 2:]
                self.assertTrue(padding)
                self.assert_cell_colors(padding, status_foreground, status_background)
                self.quit(child)

    def test_custom_theme_styles_ui_without_changing_document_sgr(self):
        config = self.config({
            "status": {"foreground": "#ff0000", "background": 17},
            "filename": {"foreground": 111, "background": 24},
            "hint": {"foreground": 214},
            "line_count": {"foreground": 118, "background": 23},
            "prompt": {"background": 22},
            "message": {"foreground": "default", "background": 52},
            "line_numbers": {"foreground": 33, "background": 236},
            "truncation": {"foreground": 226},
        })
        reader, writer = os.pipe()
        os.write(writer, b"\x1b[31;44m" + b"A" * 60 + b"\x1b[0m\nPLAIN\n")
        os.close(writer)
        child = self.pager(None, "-N", columns=40, rows=6, stdin_fd=reader, extra={"VO_CONFIG": str(config)})
        child.wait_for(lambda screen: screen.row(0) == "1 " + "A" * 37 + ">" and screen.row(5) == "stdin :1".ljust(32) + "C-w Wrap")
        self.assert_cell_colors(child.screen.cells[0][:2], ("indexed", 33), ("indexed", 236))
        self.assert_cell_colors(child.screen.cells[0][2:39], ("ansi", 1), ("ansi", 4))
        self.assert_cell_colors(child.screen.cells[0][39:], ("indexed", 226), ("indexed", 252))
        self.assert_cell_colors(child.screen.cells[1][2:7], None, None)
        self.assert_cell_colors(child.screen.cells[5][:6], ("indexed", 111), ("indexed", 24))
        self.assert_cell_colors(child.screen.cells[5][6:8], ("indexed", 118), ("indexed", 23))
        self.assert_cell_colors(child.screen.cells[5][8:32], ("indexed", 196), ("indexed", 17))
        self.assert_cell_colors(child.screen.cells[5][32:], ("indexed", 214), ("indexed", 17))

        # Existing sessions retain the resolved config when the file changes.
        config.write_text("{", encoding="utf-8")
        self.send_wait(child, b"/absent", lambda screen: screen.row(5) == "/absent")
        self.assert_cell_colors(child.screen.cells[5], ("indexed", 196), ("indexed", 22))
        self.assert_cell_colors(child.screen.cells[0][2:39], ("ansi", 1), ("ansi", 4))
        self.send_wait(child, b"\n", lambda screen: screen.row(5) == "Pattern not found")
        self.assert_cell_colors(child.screen.cells[5], None, ("indexed", 52))
        self.assert_cell_colors(child.screen.cells[1][2:7], None, None)
        self.send_wait(child, b"\x17", lambda screen: screen.row(5) == "stdin :1")
        self.assert_cell_colors(child.screen.cells[5][:6], ("indexed", 111), ("indexed", 24))
        self.assert_cell_colors(child.screen.cells[5][6:8], ("indexed", 118), ("indexed", 23))
        self.assert_cell_colors(child.screen.cells[5][8:], ("indexed", 196), ("indexed", 17))
        self.assert_cell_colors(child.screen.cells[2][2:7], None, None)
        self.quit(child)

    def test_theme_colors_follow_terminal_capabilities_and_overrides(self):
        path = self.file("A" * 60 + "\nPLAIN\n")
        config = self.config({
            "status": {"foreground": "#ff0000", "background": 21, "bold": True},
            "filename": {"foreground": "#00ffff", "background": 17},
            "line_count": {"foreground": "#00ff00", "background": 22},
            "truncation": {"foreground": "#ff0000", "background": 21},
        })
        cases = [
            ("xterm-256color", (), {}, ("indexed", 196), ("indexed", 21), ("indexed", 46), ("indexed", 22), ("indexed", 51), ("indexed", 17)),
            ("xterm-256color", (), {"COLORTERM": "truecolor"}, ("rgb", 255, 0, 0), ("indexed", 21), ("rgb", 0, 255, 0), ("indexed", 22), ("rgb", 0, 255, 255), ("indexed", 17)),
            ("linux", (), {}, ("ansi", 1), ("ansi", 4), ("ansi", 2), ("ansi", 2), ("ansi", 6), ("ansi", 4)),
            ("xterm-256color", ("--color=never",), {}, None, None, None, None, None, None),
            ("xterm-256color", (), {"NO_COLOR": "1"}, None, None, None, None, None, None),
            ("xterm-256color", ("--color=always",), {"NO_COLOR": "1", "COLORTERM": "truecolor"}, ("rgb", 255, 0, 0), ("indexed", 21), ("rgb", 0, 255, 0), ("indexed", 22), ("rgb", 0, 255, 255), ("indexed", 17)),
        ]
        for term, options, environment, foreground, background, count_foreground, count_background, filename_foreground, filename_background in cases:
            with self.subTest(term=term, options=options, environment=environment):
                child = self.pager(
                    path, "-N", *options, columns=20, rows=5, term=term,
                    extra={"VO_CONFIG": str(config), **environment},
                )
                child.wait_for(lambda screen: screen.row(0) == "1 " + "A" * 17 + ">" and screen.row(4).endswith("C-w Wrap"))
                self.assert_cell_colors(child.screen.cells[0][:2], None, None)
                self.assert_cell_colors(child.screen.cells[0][19:], foreground, background)
                self.assert_cell_colors(child.screen.cells[1][2:7], None, None)
                self.assertEqual(child.screen.row(4), f"{str(path)[:8]} :1 C-w Wrap")
                self.assert_cell_colors(child.screen.cells[4][:9], filename_foreground, filename_background)
                self.assert_cell_colors(child.screen.cells[4][9:11], count_foreground, count_background)
                self.assert_cell_colors(child.screen.cells[4][11:], foreground, background)
                if foreground is None:
                    for sgr in re.findall(rb"\x1b\[([0-9;]*)m", child.raw):
                        self.assertTrue(all(code in (b"", b"0") for code in sgr.split(b";")), sgr)
                self.quit(child)

    def test_config_path_precedence_and_home_fallback(self):
        path = self.file("CONTENT\n")
        explicit = self.config({"status": {"background": 17}}, "explicit.json")
        xdg = self.config({"status": {"background": 22}}, "xdg/vo/config.json").parent.parent
        home = self.config({"status": {"background": 52}}, "home/.config/vo/config.json").parents[2]
        missing = Path(self.temporary.name) / "missing-xdg"
        cases = [
            ({"VO_CONFIG": str(explicit), "XDG_CONFIG_HOME": str(xdg), "HOME": str(home)}, ("indexed", 17)),
            ({"VO_CONFIG": "", "XDG_CONFIG_HOME": str(xdg), "HOME": str(home)}, ("indexed", 22)),
            ({"XDG_CONFIG_HOME": "relative", "HOME": str(home)}, ("indexed", 52)),
            ({"VO_CONFIG": "", "XDG_CONFIG_HOME": "", "HOME": str(home)}, ("indexed", 52)),
            ({"XDG_CONFIG_HOME": str(missing), "HOME": str(home)}, ("indexed", 234)),
        ]
        for environment, background in cases:
            with self.subTest(environment=environment):
                child = self.pager(path, columns=40, rows=5, extra=environment)
                child.wait_for(lambda screen: screen.row(0) == "CONTENT" and screen.row(4) == f"{str(path)[:37]} :1")
                position_start = child.screen.row(4).index(":1")
                self.assert_cell_colors(child.screen.cells[4][:position_start], ("indexed", 16), ("indexed", 252))
                self.assert_cell_colors(child.screen.cells[4][position_start:], ("indexed", 231), background)
                self.quit(child)

    def test_config_load_failures_silently_use_defaults(self):
        path = self.file("A" * 60 + "\nPLAIN\n")
        home = Path(self.temporary.name) / "home"
        cases = [
            (Path(self.temporary.name) / "malformed.json", "{", None),
            (home / ".config/vo/config.json", '{"theme":{"status":{"foreground":"red"}}}', {"HOME": str(home)}),
            (Path(self.temporary.name) / "missing.json", None, None),
            (Path(self.temporary.name), None, None),
        ]
        for config, contents, environment in cases:
            with self.subTest(config=config):
                if contents is not None:
                    config.parent.mkdir(parents=True, exist_ok=True)
                    config.write_text(contents, encoding="utf-8")
                child = self.pager(path, columns=40, rows=5, extra=environment or {"VO_CONFIG": str(config)})
                child.wait_for(lambda screen: screen.row(0) == "A" * 39 + ">" and screen.row(4).endswith("C-w Wrap"))
                self.assertEqual(child.screen.row(1), "PLAIN")
                self.assert_cell_colors(child.screen.cells[0][39:], ("indexed", 234), ("indexed", 252))
                self.assertEqual(child.screen.row(4), f"{str(path)[:28]} :1".ljust(32) + "C-w Wrap")
                position_start = child.screen.row(4).index(":1")
                self.assert_cell_colors(child.screen.cells[4][:position_start], ("indexed", 16), ("indexed", 252))
                self.assert_cell_colors(child.screen.cells[4][position_start:], ("indexed", 231), ("indexed", 234))
                self.assertNotIn(b"vo: ", child.raw)
                self.quit(child)

    def test_fallback_and_information_commands_ignore_invalid_config(self):
        path = self.file("\x1b[31mDUMB-CONTENT\x1b[0m\n")
        config = Path(self.temporary.name) / "invalid.json"
        config.write_text("{", encoding="utf-8")
        cases = [
            (path, (), "dumb", b"DUMB-CONTENT"),
            (None, ("--help",), "xterm-256color", b"vo - View Optimally"),
            (None, ("--version",), "xterm-256color", b"vo "),
        ]
        for source, options, term, expected in cases:
            with self.subTest(options=options, term=term):
                child = self.pager(source, *options, term=term, rows=40, extra={"VO_CONFIG": str(config)})
                self.assertEqual(child.wait_exit(), 0)
                self.assertIn(expected, child.raw)
                self.assertNotIn(b"\x1b", child.raw)
                self.assertTrue(child.termios_restored())

    def test_horizontal_scroll_is_bounded_by_lines_seen_offscreen_and_resize(self):
        path = self.file(
            "0123456789abcdefghij\nABCDEFGHIJKL\nabcdefghijkl\n"
            "mnopqrstuvwx\nXYZabcdefghij\nSHORT\n"
            "0123456789ABCDEFGHIJKLMNOPQRST\n"
        )
        child = self.pager(path, columns=12, rows=4)
        child.wait_for(lambda screen: screen.row(0) == "0123456789a>" and screen.row(1) == "ABCDEFGHIJKL")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "89abcdefghij")
        self.assertTrue(child.screen.row(3).endswith("C-w Wrap"))
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "89abcdefghij")
        self.send_wait(child, b"999\x1b[D", lambda screen: screen.row(0) == "0123456789a>")
        self.send_wait(child, b"3j", lambda screen: screen.row(0) == "mnopqrstuvwx")
        self.assertTrue(child.screen.row(3).endswith("C-w Wrap"))
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "uvwx")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "uvwx")
        output = child.output_bytes
        child.resize(18, 4)
        child.wait_for(lambda screen: screen.row(0) == "opqrstuvwx", after_bytes=output)
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "opqrstuvwx")
        output = child.output_bytes
        child.resize(8, 4)
        child.wait_for(lambda screen: screen.row(0) == "opqrstu>", after_bytes=output)
        self.send_wait(child, b"999\x1b[C", lambda screen: not screen.row(0))
        self.send_wait(child, b"g", lambda screen: screen.row(0) == "cdefghij")
        self.send_wait(child, b"G", lambda screen: screen.contains("CDEFGHI>"))
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.contains("MNOPQRST"))
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.contains("MNOPQRST"))
        self.quit(child)

    def test_horizontal_limits_count_ansi_and_unicode_cells_per_file(self):
        path = self.file("\x1b[31m" + "\u754c" * 8 + "abcd\x1b[0m\n")
        short = Path(self.temporary.name) / "short.txt"
        short.write_text("FIRST\nLAST\n", encoding="ascii")
        child = self.pager(path, str(short), columns=12, rows=5)
        child.wait_for(lambda screen: screen.row(0) == "FIRST")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "FIRST")
        self.send_wait(child, b":n", lambda screen: screen.row(0) == "\u754c" * 5 + " >")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "\u754c" * 4 + "abcd")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "\u754c" * 4 + "abcd")
        self.send_wait(child, b":p", lambda screen: screen.row(0) == "FIRST")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "FIRST")
        self.send_wait(child, b":n", lambda screen: screen.row(0) == "\u754c" * 4 + "abcd")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "\u754c" * 4 + "abcd")
        self.quit(child)

    def test_horizontal_limit_grows_with_partial_stream_input(self):
        reader, writer = os.pipe()
        producer = os.fdopen(writer, "wb", buffering=0)
        self.addCleanup(producer.close)
        child = self.pager(None, stdin_fd=reader, columns=12, rows=5)
        child.wait_for(lambda screen: screen.row(4) == "stdin :0")
        self.send_wait(child, b"999\x1b[C", lambda screen: not screen.row(0) and screen.row(4) == "stdin :0")
        output = child.output_bytes
        producer.write(b"0123456789abcdefghij")
        child.wait_for(lambda screen: screen.row(0) == "0123456789a>" and self.has_position(screen, 1), after_bytes=output)
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "89abcdefghij" and self.has_position(screen, 1))
        output = child.output_bytes
        producer.write(b"klmnopqrst")
        child.wait_for(lambda screen: screen.row(0) == "89abcdefghi>" and self.has_position(screen, 1), after_bytes=output)
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "ijklmnopqrst")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "ijklmnopqrst")
        output = child.output_bytes
        child.resize(32, 5)
        child.wait_for(lambda screen: screen.row(0) == "0123456789abcdefghijklmnopqrst", after_bytes=output)
        self.assertEqual(child.screen.row(4), "stdin :1")
        self.send_wait(child, b"999\x1b[C", lambda screen: screen.row(0) == "0123456789abcdefghijklmnopqrst")
        output = child.output_bytes
        producer.close()
        child.wait_for(lambda screen: screen.row(4) == "stdin :1", after_bytes=output)
        self.quit(child)

    def test_switching_files_restores_stream_history_and_file_position(self):
        path = self.file("FILE-FIRST\nFILE-LAST\n")
        reader, writer = os.pipe()
        os.write(writer, b"STREAM-FIRST\nSTREAM-LAST\n")
        os.close(writer)
        child = self.pager(path, "-", stdin_fd=reader)
        child.wait_for(lambda screen: "STREAM-FIRST" in screen.row(0) and self.has_position(screen, 1))
        self.send_wait(child, b":n", lambda screen: "FILE-FIRST" in screen.row(0) and self.has_position(screen, 1))
        self.send_wait(child, b"2g", lambda screen: "FILE-LAST" in screen.row(0) and self.has_position(screen, 2))
        self.send_wait(child, b":p", lambda screen: "STREAM-FIRST" in screen.row(0) and self.has_position(screen, 1))
        self.send_wait(child, b"j", lambda screen: "STREAM-LAST" in screen.row(0) and self.has_position(screen, 2))
        self.send_wait(child, b":n", lambda screen: "FILE-LAST" in screen.row(0) and self.has_position(screen, 2))
        self.send_wait(child, b":p", lambda screen: "STREAM-LAST" in screen.row(0) and self.has_position(screen, 2))
        self.quit(child)

    def test_open_pipe_is_interactive_and_quits_before_eof(self):
        reader, writer = os.pipe()
        self.addCleanup(os.close, writer)
        child = self.pager(None, stdin_fd=reader, rows=8)
        child.wait_for(lambda screen: screen.row(7) == "stdin :0")
        os.write(writer, b"LIVE-FIRST\nlive-second")
        child.wait_for(lambda screen: screen.contains("LIVE-FIRST") and screen.row(7) == "stdin :1")
        child.wait_for(lambda screen: "live-second" in screen.row(1))
        output = child.output_bytes
        os.write(writer, b"-continued\nlive-third")
        child.wait_for(lambda screen: "live-second-continued" in screen.row(1) and "live-third" in screen.row(2) and screen.row(7) == "stdin :1", after_bytes=output)
        self.send_wait(child, b"j", lambda screen: "live-second-continued" in screen.row(0) and screen.row(7) == "stdin :2")
        output = child.output_bytes
        os.write(writer, b"-continued\nlive-fourth")
        child.wait_for(lambda screen: "live-third-continued" in screen.row(1) and "live-fourth" in screen.row(2) and screen.row(7) == "stdin :2", after_bytes=output)
        self.send_wait(child, b"=", lambda screen: "line 2 | 4+ lines" in screen.row(7))
        # The writer stays open until cleanup. Exit therefore proves q does not
        # wait for the stream to close or for a blocked reader to return.
        self.quit(child)

    def test_wrapped_end_jump_shows_the_last_screen(self):
        path = self.file("FIRST\n" + "wrapped-content " * 100 + "WRAPPED-EOF\n")
        child = self.pager(path, "--wrap", columns=32, rows=8)
        child.wait_for(lambda screen: "FIRST" in screen.row(0) and self.has_position(screen, 1))
        self.assertFalse(child.screen.contains("WRAPPED-EOF"))
        self.send_wait(child, b"G", lambda screen: screen.contains("WRAPPED-EOF") and screen.row(7) == f"{str(path)[:screen.columns - 3]} :2")
        self.assertNotIn("FIRST", child.screen.row(0))
        self.assertTrue(any("wrapped-content" in child.screen.row(row) for row in range(6)))
        self.send_wait(child, b"g", lambda screen: "FIRST" in screen.row(0) and self.has_position(screen, 1))
        self.quit(child)

    def test_follow_empty_file_append_and_truncate(self):
        path = self.file("")
        child = self.pager(path)
        child.wait_for(lambda screen: screen.row(23) == f"{str(path)[:screen.columns - 3]} :0")
        child.send(b"F")
        output = child.output_bytes
        with path.open("ab") as producer:
            producer.write(b"APPENDED-FIRST\nAPPENDED-SECOND\n")
        child.wait_for(lambda screen: screen.contains("APPENDED-FIRST") and screen.contains("APPENDED-SECOND") and screen.row(23) == f"{str(path)[:screen.columns - 3]} :1", after_bytes=output)
        output = child.output_bytes
        path.write_text("AFTER-TRUNCATE\n", encoding="utf-8")
        child.wait_for(lambda screen: screen.contains("AFTER-TRUNCATE") and not screen.contains("APPENDED-FIRST") and screen.row(23) == f"{str(path)[:screen.columns - 3]} :1", after_bytes=output)
        self.quit(child)

    def test_named_fifo_can_quit_before_a_producer_connects(self):
        path = Path(self.temporary.name) / "input.fifo"
        os.mkfifo(path)
        child = self.pager(path)
        child.wait_for(lambda screen: screen.row(23) == f"{str(path)[:screen.columns - 3]} :0")
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
        config = Path(self.temporary.name) / "invalid.json"
        config.write_text("{", encoding="utf-8")
        for source in ("file", "stream"):
            with self.subTest(source=source):
                command = [str(VO), "--", str(path)] if source == "file" else [str(VO)]
                result = subprocess.run(
                    command, input=content if source == "stream" else None,
                    capture_output=True, timeout=5, check=True,
                    env=terminal_environment(extra={"VO_CONFIG": str(config)}),
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
