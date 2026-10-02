#!/usr/bin/env python3
"""Measure interactive pager startup, EOF traversal, and child peak RSS."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading

from pty_driver import PtyProcess, supervisor_binary, terminal_environment


ROOT = Path(__file__).resolve().parent.parent
SCHEMA = 1
CORPUS_SCHEMA = 1
MIB = 1024 * 1024


def positive_integer(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("must be finite and positive")
    return parsed


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vo", type=Path, default=ROOT / "target/release/vo", help="release vo binary")
    parser.add_argument("--less", default="less", help="GNU less executable")
    parser.add_argument("--size-mib", type=positive_integer, default=32, help="bytes per corpus in MiB (default: 32)")
    parser.add_argument("--repeat", type=positive_integer, default=5, help="measured runs per pager and case (default: 5)")
    parser.add_argument("--seed", type=int, default=1729, help="deterministic corpus seed (default: 1729)")
    parser.add_argument("--timeout", type=positive_float, default=60, help="seconds allowed per display transition")
    parser.add_argument("--cpu", type=int, help="pin each pager to this allowed logical CPU on Linux")
    parser.add_argument("--output", type=Path, help="JSON destination (default: standard output)")
    parser.add_argument("--corpus-dir", type=Path, help="retain corpora here (default: a temporary directory)")
    return parser.parse_args()


def corpus_line(kind: str, index: int, state: int) -> bytes:
    prefix = f"2026-01-01T00:00:{index % 60:02d}Z {index:09d} "
    payload = f"request={state:016x} worker={state % 17:02d} status=200 duration={state % 10000:04d}us "
    if kind == "plain":
        return (prefix + "INFO " + payload + "abcdef0123456789 " * (state % 7) + "\n").encode()
    if kind == "ansi":
        color = (31, 32, 33, 34, 35, 36)[state % 6]
        return (
            prefix
            + f"\x1b[{color}mINFO\x1b[0m "
            + payload
            + f"\x1b[38;2;{state % 256};{(state >> 8) % 256};{(state >> 16) % 256}m"
            + "colored-payload " * (1 + state % 5)
            + "\x1b[0m\n"
        ).encode()
    if kind == "long-utf8":
        return (prefix + payload + "wide=\u754c\u9762 combine=e\u0301 emoji=\U0001f600 " * (128 + state % 128) + "\n").encode()
    raise ValueError(f"unknown corpus kind: {kind}")


def generate_corpus(directory: Path, kind: str, size: int, seed: int) -> dict:
    """Exact byte count, fixed LCG, UTF-8 records, and unique boundary markers."""
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"{kind}-{size}-{seed}.log"
    temporary = path.with_suffix(".tmp")
    first_marker = f"VO_BENCH_BEGIN_{kind}"
    end_marker = f"VO_BENCH_EOF_{kind}"
    first = (first_marker + "\n").encode()
    last = (end_marker + "\n").encode()
    remaining = size - len(first) - len(last)
    digest = hashlib.sha256()
    state = seed & ((1 << 64) - 1)
    index = 0
    lines = 2
    with temporary.open("wb", buffering=MIB) as output:
        output.write(first)
        digest.update(first)
        while remaining:
            state = (state * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
            record = corpus_line(kind, index, state)
            if len(record) > remaining:
                record = b"x" * (remaining - 1) + b"\n"
            output.write(record)
            digest.update(record)
            remaining -= len(record)
            index += 1
            lines += 1
        output.write(last)
        digest.update(last)
    temporary.replace(path)
    return {
        "kind": kind,
        "path": str(path.resolve()),
        "bytes": size,
        "lines": lines,
        "sha256": digest.hexdigest(),
        "seed": seed,
        "generator_schema": CORPUS_SCHEMA,
        "first_marker": first_marker,
        "end_marker": end_marker,
    }


def warm_file(path: Path):
    with path.open("rb", buffering=0) as source:
        buffer = bytearray(MIB)
        while source.readinto(buffer):
            pass


class StreamProducer:
    def __init__(self, path: Path, writer: int):
        self.path = path
        self.writer = writer
        self.error: BaseException | None = None
        self.finished = threading.Event()
        self.thread = threading.Thread(target=self._run, name="pager-input", daemon=True)

    def _run(self):
        try:
            with self.path.open("rb") as source:
                while block := source.read(MIB):
                    view = memoryview(block)
                    while view:
                        written = os.write(self.writer, view)
                        view = view[written:]
        except (BrokenPipeError, OSError) as error:
            self.error = error
        finally:
            os.close(self.writer)
            self.finished.set()

    def start(self):
        self.thread.start()

    def join(self):
        self.thread.join(3)
        if self.thread.is_alive():
            raise RuntimeError("stream producer remained blocked after pager exit")
        if self.error:
            raise RuntimeError(f"stream producer failed: {self.error}")


def measure(executable: str, name: str, corpus: dict, source: str, timeout: float, cpu: int | None) -> dict:
    path = Path(corpus["path"])
    warm_file(path)
    command = [executable, "-S", "-N"]
    if name == "less":
        command.append("-R")
    producer = None
    reader = None
    if source == "file":
        command.extend(["--", str(path)])
    else:
        reader, writer = os.pipe()
        producer = StreamProducer(path, writer)
    process = None
    try:
        process = PtyProcess(command, stdin_fd=reader, environment=terminal_environment(), cpu=cpu)
        if producer:
            producer.start()
        startup_ns = process.wait_for(
            lambda screen: screen.contains(corpus["first_marker"]) and bool(screen.row(screen.rows - 1).strip()),
            timeout,
        )
        output_bytes = process.output_bytes
        scan_start_ns = process.send(b"G")
        eof_ns = process.wait_for(
            lambda screen: screen.contains(corpus["end_marker"]) and "(END)" in screen.row(screen.rows - 1),
            timeout,
            after_bytes=output_bytes,
        )
        process.send(b"q")
        exit_code = process.wait_exit(timeout)
        if exit_code:
            raise RuntimeError(f"{name} exited with {exit_code}")
        if not process.termios_restored():
            raise RuntimeError(f"{name} did not restore terminal attributes")
        if producer:
            producer.join()
        startup_seconds = (startup_ns - process.start_ns) / 1e9
        scan_seconds = (eof_ns - scan_start_ns) / 1e9
        end_to_end_seconds = (eof_ns - process.start_ns) / 1e9
        return {
            "pager": name,
            "corpus": corpus["kind"],
            "source": source,
            "command": command,
            "startup_seconds": startup_seconds,
            "scan_seconds": scan_seconds,
            "end_to_end_seconds": end_to_end_seconds,
            "scan_mib_per_second": corpus["bytes"] / MIB / scan_seconds,
            "end_to_end_mib_per_second": corpus["bytes"] / MIB / end_to_end_seconds,
            "peak_rss_bytes": process.peak_rss_bytes,
            "output_bytes": process.output_bytes,
            "exit_code": exit_code,
        }
    finally:
        if process:
            process.close()
        elif reader is not None:
            os.close(reader)
        if producer and producer.thread.ident is not None:
            producer.thread.join(3)
        elif producer:
            os.close(producer.writer)


def command_output(command: list[str]) -> str | None:
    try:
        result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=10, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return result.stdout.strip()[:4096] if result.returncode == 0 else None


def executable_info(executable: str) -> dict:
    path = Path(executable)
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(MIB):
            digest.update(block)
    return {
        "path": str(path.resolve()),
        "version": command_output([executable, "--version"]),
        "sha256": digest.hexdigest(),
        "mtime_utc": dt.datetime.fromtimestamp(path.stat().st_mtime, dt.timezone.utc).isoformat(),
    }


def summarize(samples: list[dict]) -> list[dict]:
    result = []
    for corpus in ("plain", "ansi", "long-utf8"):
        for source in ("file", "stream"):
            for pager in ("vo", "less"):
                matching = [sample for sample in samples if (sample["corpus"], sample["source"], sample["pager"]) == (corpus, source, pager)]
                metrics = {}
                for metric in (
                    "startup_seconds", "scan_seconds", "end_to_end_seconds",
                    "scan_mib_per_second", "end_to_end_mib_per_second", "peak_rss_bytes",
                ):
                    values = [sample[metric] for sample in matching]
                    median = statistics.median(values)
                    metrics[metric] = {
                        "median": median,
                        "min": min(values),
                        "max": max(values),
                        "median_absolute_deviation": statistics.median(abs(value - median) for value in values),
                    }
                result.append({"corpus": corpus, "source": source, "pager": pager, "runs": len(matching), "metrics": metrics})
    return result


def resolve_executable(value: str | Path) -> str:
    found = shutil.which(str(value))
    if found is None:
        raise FileNotFoundError(f"executable not found: {value}")
    return str(Path(found).resolve())


def write_checkpoint(path: Path | None, metadata: dict):
    if path is not None:
        path.write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n")


def run(args):
    vo = resolve_executable(args.vo)
    less = resolve_executable(args.less)
    if args.cpu is not None:
        if not hasattr(os, "sched_setaffinity"):
            raise ValueError("--cpu requires Linux sched_setaffinity")
        if args.cpu not in os.sched_getaffinity(0):
            raise ValueError(f"CPU {args.cpu} is outside this process's allowed affinity")
    size = args.size_mib * MIB
    corpora = [generate_corpus(args.corpus_dir, kind, size, args.seed) for kind in ("plain", "ansi", "long-utf8")]
    with PtyProcess([resolve_executable("true")]) as baseline:
        baseline.wait_exit()
        spawn_floor = baseline.peak_rss_bytes
    metadata = {
        "schema": SCHEMA,
        "complete": False,
        "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "machine": {
            "platform": platform.platform(),
            "uname": list(platform.uname()),
            "python": platform.python_version(),
            "logical_cpus": os.cpu_count(),
            "cpu_affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
            "ram_bytes": os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE"),
        },
        "build": {
            "suggested_command": ["cargo", "build", "--release"],
            "profile": "release" if "/release/" in vo else "unknown",
            "rustc": command_output(["rustc", "--version"]),
            "git_revision": command_output(["git", "rev-parse", "HEAD"]),
            "git_status": command_output(["git", "status", "--porcelain"]),
        },
        "executables": {"vo": executable_info(vo), "less": executable_info(less)},
        "measurement_supervisor": {
            "path": supervisor_binary(),
            "source_sha256": hashlib.sha256((ROOT / "benchmarks/pty_spawn.rs").read_bytes()).hexdigest(),
            "binary_sha256": hashlib.sha256(Path(supervisor_binary()).read_bytes()).hexdigest(),
            "build_command": ["rustc", "--edition=2024", "-C", "opt-level=2", "-C", "debuginfo=0", "benchmarks/pty_spawn.rs"],
            "true_child_peak_rss_bytes": spawn_floor,
        },
        "conditions": {
            "terminal": {"columns": 80, "rows": 24},
            "environment": {key: value for key, value in terminal_environment().items() if key != "PATH"},
            "cpu": args.cpu,
            "warm_cache": True,
            "repeats": args.repeat,
            "timeout_seconds": args.timeout,
            "rss": "native supervisor wait4 pager child ru_maxrss; bytes; stream producer is a Python parent thread",
            "startup_end": "first boundary marker rendered and last-row prompt nonempty",
            "scan_start": "immediately before sending G to the controlling PTY",
            "scan_end": "EOF boundary marker rendered and last-row prompt contains (END)",
            "end_to_end": "posix_spawn start to scan end; excludes corpus generation and cache warming",
        },
        "corpora": corpora,
        "samples": [],
    }
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
    write_checkpoint(args.output, metadata)
    for corpus_index, corpus in enumerate(corpora):
        for source_index, source in enumerate(("file", "stream")):
            print(f"warming {corpus['kind']} {source}", file=sys.stderr, flush=True)
            for name, executable in (("vo", vo), ("less", less)):
                measure(executable, name, corpus, source, args.timeout, args.cpu)
            for repetition in range(args.repeat):
                order = (("vo", vo), ("less", less))
                if (repetition + corpus_index + source_index) % 2:
                    order = tuple(reversed(order))
                for name, executable in order:
                    sample = measure(executable, name, corpus, source, args.timeout, args.cpu)
                    sample["repetition"] = repetition + 1
                    metadata["samples"].append(sample)
                    print(
                        f"{corpus['kind']:9s} {source:6s} {name:4s} run={repetition + 1} "
                        f"startup={sample['startup_seconds'] * 1000:.2f}ms "
                        f"total={sample['end_to_end_mib_per_second']:.1f}MiB/s "
                        f"rss={sample['peak_rss_bytes'] / MIB:.2f}MiB",
                        file=sys.stderr,
                        flush=True,
                    )
                    write_checkpoint(args.output, metadata)
    metadata["summary"] = summarize(metadata["samples"])
    metadata["complete"] = True
    write_checkpoint(args.output, metadata)
    print("corpus     source pager startup_ms total_MiB/s scan_MiB/s peak_RSS_MiB", file=sys.stderr)
    for summary in metadata["summary"]:
        metrics = summary["metrics"]
        print(
            f"{summary['corpus']:10s} {summary['source']:6s} {summary['pager']:5s} "
            f"{metrics['startup_seconds']['median'] * 1000:10.2f} "
            f"{metrics['end_to_end_mib_per_second']['median']:11.1f} "
            f"{metrics['scan_mib_per_second']['median']:10.1f} "
            f"{metrics['peak_rss_bytes']['median'] / MIB:12.2f}",
            file=sys.stderr,
        )
    if args.output is not None:
        print(f"JSON: {args.output.resolve()}", file=sys.stderr)
    else:
        print(json.dumps(metadata, indent=2, sort_keys=True))


def main():
    args = arguments()
    if args.corpus_dir is not None:
        run(args)
    else:
        with tempfile.TemporaryDirectory(prefix="vo-bench-corpora-") as directory:
            args.corpus_dir = Path(directory)
            run(args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, TimeoutError, ValueError) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        sys.exit(1)
