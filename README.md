# vo - View Optimally

`vo` is a Rust terminal pager for files, pipes, and FIFOs. Long lines are
truncated to the terminal width by default. `Ctrl-W` toggles wrapping; both
layouts are calculated by the pager and update when the terminal is resized.
By default, truncated lines end with `>` on a light-grey background with a dark
foreground, downgraded to the terminal's palette. Horizontal scrolling stops at
the widest line encountered in the current input, including lines no longer on
screen. The limit adjusts with terminal width and incoming stream data. The bottom bar
shows the file path, or `stdin`. In truncation mode, `C-w Wrap` appears when an
encountered line exceeds the available content width. Commands and messages
temporarily replace the bar.

Build or install with Rust 1.88 or newer:

```sh
cargo build --release --locked
cargo install --path . --locked
vo application.log
command | vo
vo --wrap -N application.log
vo +G application.log
```

It currently supports Unix terminals. Interactive input comes from `/dev/tty`,
so keyboard commands remain available while stdin is a pipe. Output redirected
to another process or file is copied byte for byte. `TERM=dumb`, unknown
terminal entries, and entries without cursor positioning use a plain-text
fallback with document escape sequences removed.

The viewing commands use less and vi bindings:

| Keys | Action |
| --- | --- |
| `q`, `:q` | Quit |
| `j`, `e`, Enter, Down / `k`, `y`, Up | Next / previous display row |
| Space, `f`, PageDown, Ctrl-F / `b`, PageUp, Ctrl-B | Next / previous page |
| `d`, Ctrl-D / `u`, Ctrl-U | Down / up half a page |
| `z` / `w` | Down / up a window; a numeric prefix sets its size |
| `g`, `<`, Home / `G`, `>`, End | First / last screen; a numeric prefix selects a line |
| `N%`, `Np` | Jump to N percent of the input bytes |
| `/regex` / Ctrl-R, then `regex` | Search forward / backward |
| `n` / `N` | Repeat search / reverse search direction |
| `F`, Ctrl-C | Follow new input / stop following or cancel work |
| `h`, Left / `l`, Right | Scroll left / right; a prefix sets the distance in cells |
| `mX`, `'X`, `''` | Set mark X, jump to it, return before a jump |
| `:n`, `:p`, `:e` | Next file, previous file, examine another file |
| `=`, Ctrl-G, `:f` | File information |
| `r`, `R`, Ctrl-L / Ctrl-Z | Redraw / suspend |
| `v`, `!command` | Invoke the editor / a shell command |
| F1, `?` | Help |
| Ctrl-W, `-S` | Toggle truncation and wrapping |
| `-N` | Toggle line numbers |

Numeric prefixes apply to movement and searches. Search prompts support arrows,
Home/End, Ctrl-A/Ctrl-E, Delete, Backspace, Ctrl-K, Ctrl-U, and Ctrl-W word
deletion. Search regexes are byte-oriented with UTF-8 literals and ASCII
character classes and case folding. ANSI sequences are excluded from search.
Backreferences and lookaround are unsupported. Regex compilation has explicit
size limits; numeric search counts are capped at 10,000.

`vo --help` lists options, including less-compatible `-S`, `-N`, `-i`, `-I`,
`-F`, `-X`, `-R`, and `-x`. SGR (Select Graphic Rendition) styles are handled
by default. Terminal capabilities come from the terminfo database, `TERM`, and
`COLORTERM` (`truecolor` or `24bit`). RGB and indexed colors are downgraded to the
terminal's 256-, 16-, or 8-color palette. Monochrome terminals retain attributes
while dropping colors. `NO_COLOR` and `--color=never` suppress document and theme
SGR; `--color=always` overrides `NO_COLOR` while retaining the detected palette.
Cursor movement, erasure, OSC, DCS, and other document control sequences are
discarded. Each logical line starts with default attributes, as with less `-R`.

Display width accounts for tabs, wide characters, combining marks, emoji
sequences, invalid UTF-8, and common manual-page backspace overstrikes. A cell
retains at most 16 combining scalars so pathological zero-width input cannot
grow memory without a limit. Unrecognized controls are displayed as text.

Regular files use positional reads into a 64 KiB cache and a separate 64 KiB
indexing buffer. The line index stores one 8-byte offset per 1,024 newlines;
allocation capacity can be up to twice its used size. Pages retain only visible
rows and one row of lookahead. Long lines are consumed in chunks. Streams are
spooled to an automatically removed temporary file under `TMPDIR`, keeping
history available without retaining the input bytes in RAM. Spool disk use
grows with the stream. Inactive stream buffers are released when changing
files, and returning to stdin restores its retained history.

End jumps, percentage jumps, and regex searches perform work incrementally so
Ctrl-C and quitting stay available. `F` follows the open regular file, including
growth and truncation. Replacing its pathname with a new inode requires
reopening it using `:e`. This is a viewing alternative, with the commands above;
lesskey customizations, input preprocessors, filtering, and the full GNU less
option set are outside the current implementation.

## Themes

`vo` selects one JSON config path for interactive viewing, in this order:

1. The path in `VO_CONFIG`, when nonempty. Relative paths are allowed.
2. `$XDG_CONFIG_HOME/vo/config.json`, when `XDG_CONFIG_HOME` is an absolute path.
3. `$HOME/.config/vo/config.json`, including on macOS.

An absent default file preserves the built-in theme. An explicit `VO_CONFIG`
path must exist. Invalid JSON, unknown fields, and unreadable files produce an
error naming the config file before entering terminal mode. Redirected output
continues to copy the input bytes without loading a theme.

For example:

```json
{
  "theme": {
    "status": { "foreground": "#cdd6f4", "background": "#313244" },
    "hint": { "foreground": "#89b4fa", "bold": true },
    "prompt": { "foreground": "#a6e3a1" },
    "message": { "foreground": "#f9e2af" },
    "line_numbers": { "foreground": 245, "dim": true },
    "truncation": { "foreground": "#1e1e2e", "background": "#89b4fa" }
  }
}
```

Every style accepts `foreground`, `background`, and the boolean attributes
`bold`, `dim`, `italic`, `underline`, and `reverse`. Colors can be `"#RRGGBB"`,
an integer palette index from 0 to 255, or `"default"` for the terminal's default
color. Omitted fields retain their defaults; `hint`, `prompt`, and `message`
inherit `status`. Styles are reset between UI elements and document content,
so the document keeps its own ANSI colors.

Capability detection and theme color conversion run once at startup using the
environment and terminfo. Redraws reuse the resolved styles and cached UI SGR
sequences.

## Development

Verify with:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
python3 -B tests/pty.py --vo target/release/vo
```

[Deterministic benchmarks](benchmarks/README.md) compare real terminal file and
stream viewing against GNU less, reporting startup, complete-input throughput,
and pager-only peak RSS. Inputs, terminal size, locale, environment, run order,
binary hashes, and machine metadata are recorded. Measurements use warm caches;
results do not assume that either pager wins.
