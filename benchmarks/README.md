# Pager measurements

Build `vo` with `cargo build --release`, then run:

```sh
python3 -B benchmarks/bench.py --size-mib 32 --repeat 5 --output results.json --corpus-dir corpora
```

The script requires Linux, Python 3 with its standard library, `rustc`, and GNU
`less`. Python supplies PTYs, terminal control, thread-based stream producers,
and structured results without an additional dependency. A small Rust
supervisor measures the pager process with `wait4`.

By default the script writes JSON to standard output and creates corpora in a
temporary directory. `--output` saves JSON to the selected path, including
incomplete checkpoints until the final `complete` field becomes true.
`--corpus-dir` retains generated input files. The supervisor executable and PTY
test files use temporary directories unless `VO_TEST_WORKDIR` selects a working
directory. Use `--seed`, `--size-mib`, `--repeat`, `--timeout`, `--vo`, or `--less`
to change a run. `--cpu` pins each pager to one allowed Linux logical CPU after
its process is created. Progress and the results table go to standard error.

Each seeded corpus has exactly the requested byte count and a recorded SHA256:

| Corpus | Content |
| --- | --- |
| `plain` | Varied-width ASCII log records |
| `ansi` | Log records with basic SGR and truecolor SGR sequences |
| `long-utf8` | Long lines with double-width characters, combining marks, and emoji |

The generator uses a fixed 64-bit linear congruential generator. It does not
depend on Python's random generator, the clock, or system entropy. The final
partial record uses ASCII padding so every file remains valid UTF-8. Unique
first and last lines identify rendered startup and EOF.

Every run has an actual 80 by 24 controlling terminal. `TERM=xterm-256color`
and `LC_ALL=C.UTF-8` are fixed. The child environment is a whitelist, so
`LESS`, `LESSOPEN`, `LESSKEY`, `NO_COLOR`, and shell configuration cannot alter
the comparison. `LESSHISTFILE=/dev/null` avoids persistent search history.

Commands are `less -S -N -R -- FILE` and `vo -S -N -- FILE`. Stream cases use
the same options, omit `FILE`, and pipe the complete corpus to standard input.
The `-N` option makes GNU less count lines on a full-file `G` traversal. The
script never redirects pager output to a file, which would exercise GNU less's
copying behavior instead of its interactive pager.

Each corpus and input mode receives an unrecorded warmup for each pager.
The file is read before every timed run to warm the filesystem cache. Measured
pager order alternates across repeats and cases. JSON contains every sample,
median, range, and median absolute deviation, along with commands, corpus
hashes, binary hashes, build metadata, versions, and machine information.

Three times are reported:

| Metric | Start | End |
| --- | --- | --- |
| Startup | Starting the native supervisor | First-line marker rendered and nonempty source location (vo) or prompt (less) on the last row |
| Scan | Sending `G` to the controlling terminal | Final-line marker rendered and nonempty source location (vo) or `(END)` prompt (less) on the last row |
| End to end | Starting the native supervisor | The same final-line display condition |

The vo bottom bar shows the input's file path or `stdin`. GNU less signals EOF
with `(END)`. Each pager's scan wait requires its final corpus marker and the
last-row condition above after output arrives in response to `G`.

The waits parse terminal output into a cell-based screen. They do not infer
readiness from a fixed delay or match bytes from an earlier terminal frame.
The JSON includes throughput for both scan and end-to-end time. Startup work
can reduce later scan work, so end-to-end throughput is the primary full-input
comparison. Supervisor startup, process launch, scheduling, pipe delivery, and
the final terminal redraw remain part of the measured operation.

Peak RSS is `wait4`'s `ru_maxrss` for the pager child, converted from Linux KiB
to bytes. The Rust supervisor excludes the Python interpreter's peak RSS:
Linux retains a process's pre-exec peak RSS even when launched with
`posix_spawn`, so measuring a direct Python child would give misleading
results. Stream delivery runs in a Python parent thread and does not enter the
measured child RSS. The supervisor's small pre-exec RSS floor is disclosed in
JSON as `measurement_supervisor.true_child_peak_rss_bytes`; differences near
that floor are not resolved by this measurement. RSS measures resident pages,
not the total size of temporary spool files or the operating system's file
cache.

These are warm-cache measurements on the recorded machine. They do not prove
cold-storage throughput, physical terminal behavior, or performance on another
machine. Small corpora amplify launch and terminal redraw costs. Results can
show regressions as well as improvements; the benchmark does not require or
assume that `vo` wins.

The actual terminal behavior checks run separately:

```sh
python3 -B tests/pty.py --vo target/release/vo
```

They cover navigation, anchored search across long colored lines, Unicode
prompt editing, Ctrl-W layout changes, resize, and the last screen of wrapped
input. Input checks include following appended or truncated files, quitting an
unconnected FIFO, partial-line stream updates, and byte-exact redirected output.
They also check document ANSI control filtering, color downgrade and suppression,
UTF-8 cell widths, content remaining visible after `-F`, and original termios
restoration after normal exit and SIGTERM.

Recorded baseline: Linux x86_64, Rust 1.98.1, GNU less 704, source revision
f6b63f7206fb396c180fac2b211c437511451be1. Seven repeats per case used
32 MiB corpora, warm caches, and CPU 7 affinity. Values below are medians; the
[snapshot](results/linux-x86_64-32mib.json) retains all 84 samples, ranges,
median absolute deviations, corpus and binary hashes, and machine conditions.
Executable and corpus paths in the snapshot are normalized for portability.

| Corpus | Input | vo MiB/s | less MiB/s | vo RSS MiB | less RSS MiB |
| --- | --- | ---: | ---: | ---: | ---: |
| plain | file | 1114 | 199 | 2.95 | 2.62 |
| plain | stream | 818 | 119 | 2.81 | 34.81 |
| ansi | file | 2075 | 331 | 2.98 | 2.64 |
| ansi | stream | 749 | 106 | 2.90 | 34.73 |
| long-utf8 | file | 1132 | 261 | 2.96 | 2.75 |
| long-utf8 | stream | 989 | 172 | 2.96 | 34.87 |

These measurements use end-to-end throughput, including launch and the final
display. File throughput improved by 4.3 to 6.3 times; stream throughput
improved by 5.7 to 7.0 times. Stream peak RSS fell by about 92 percent. File
peak RSS remains 0.2 to 0.3 MiB higher, and startup is not consistently faster.
The file-memory target remains unmet on this machine. Scheduling variation
is visible in the individual samples; these results describe this workload
and environment.
