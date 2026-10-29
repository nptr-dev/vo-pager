use lexopt::{Arg, ValueExt};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use vo_pager::config::FileConfig;
use vo_pager::pager::{Case, Config};
use vo_pager::render::ColorLevel;
use vo_pager::terminal::Capabilities;

#[derive(Default)]
struct Args {
    wrap: bool,
    line_numbers: bool,
    case: Case,
    quit_if_one_screen: bool,
    no_init: bool,
    color: Color,
    tabstop: u16,
    initial: Option<String>,
    files: Vec<PathBuf>,
}

#[derive(Default)]
enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

enum Invocation {
    Run(Args),
    Help,
    Version,
}

impl Args {
    fn parse(mut parser: lexopt::Parser) -> Result<Invocation, lexopt::Error> {
        let mut args = Self {
            tabstop: 8,
            ..Self::default()
        };
        while let Some(argument) = parser.next()? {
            match argument {
                Arg::Short('h') | Arg::Long("help") => return Ok(Invocation::Help),
                Arg::Short('V') | Arg::Long("version") => return Ok(Invocation::Version),
                Arg::Short('S') | Arg::Long("chop-long-lines") => args.wrap = false,
                Arg::Long("wrap") => args.wrap = true,
                Arg::Short('N') | Arg::Long("line-numbers") => args.line_numbers = true,
                Arg::Short('i') | Arg::Long("ignore-case") => args.case = Case::Smart,
                Arg::Short('I') | Arg::Long("ignore-case-always") => args.case = Case::Insensitive,
                Arg::Short('F') | Arg::Long("quit-if-one-screen") => args.quit_if_one_screen = true,
                Arg::Short('X') | Arg::Long("no-init") => args.no_init = true,
                Arg::Short('R') | Arg::Long("raw-control-chars") => {}
                Arg::Long("color") => {
                    args.color = match parser.value()?.string()?.as_str() {
                        "auto" => Color::Auto,
                        "always" => Color::Always,
                        "never" => Color::Never,
                        _ => return Err("--color expects auto, always, or never".into()),
                    }
                }
                Arg::Short('x') | Arg::Long("tabstop") => {
                    args.tabstop = parser.value()?.parse()?;
                    if !(1..=256).contains(&args.tabstop) {
                        return Err("--tabstop must be between 1 and 256".into());
                    }
                }
                Arg::Long("initial-command") => args.initial = Some(parser.value()?.string()?),
                Arg::Value(value) => args.files.push(value.into()),
                argument => return Err(argument.unexpected()),
            }
        }
        if args.files.first().is_some_and(|path| {
            path.to_str().is_some_and(|text| {
                matches!(text, "+G" | "+F")
                    || text
                        .strip_prefix('+')
                        .is_some_and(|line| line.parse::<u64>().is_ok())
            })
        }) {
            args.initial = Some(args.files.remove(0).to_string_lossy().into_owned());
        }
        Ok(Invocation::Run(args))
    }
}

fn main() {
    if let Err(error) = execute()
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        eprintln!("vo: {error}");
        std::process::exit(1);
    }
}

fn execute() -> io::Result<()> {
    let mut args = match Args::parse(lexopt::Parser::from_env())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
    {
        Invocation::Help => {
            return io::stdout().write_all(HELP.as_bytes());
        }
        Invocation::Version => {
            return writeln!(io::stdout(), "vo {}", env!("CARGO_PKG_VERSION"));
        }
        Invocation::Run(args) => args,
    };
    if args.files.is_empty() {
        if io::stdin().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provide a file or pipe input to vo",
            ));
        }
        args.files.push(PathBuf::from("-"));
    }
    if !io::stdout().is_terminal() {
        return copy(&args.files, false);
    }
    let capabilities = Capabilities::detect();
    if !capabilities.interactive() {
        return copy(&args.files, true);
    }
    if args.files.iter().any(|path| path == Path::new("-")) && io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stdin is also the command terminal; provide a file or pipe",
        ));
    }
    let color = match args.color {
        Color::Never => ColorLevel::None,
        Color::Auto if std::env::var_os("NO_COLOR").is_some() => ColorLevel::None,
        _ => capabilities.color,
    };
    let theme = FileConfig::load()?.theme.resolve(color);
    let config = Config {
        wrap: args.wrap,
        line_numbers: args.line_numbers,
        tabstop: usize::from(args.tabstop),
        color,
        theme,
        no_init: args.no_init,
        quit_if_one_screen: args.quit_if_one_screen,
        case: args.case,
        initial: args.initial,
    };
    vo_pager::pager::run(args.files, config, capabilities)
}

fn copy(paths: &[PathBuf], plain: bool) -> io::Result<()> {
    let mut output = io::stdout().lock();
    for path in paths {
        if path == Path::new("-") {
            if plain {
                vo_pager::ansi::copy_plain(io::stdin().lock(), &mut output)?;
            } else {
                io::copy(&mut io::stdin().lock(), &mut output)?;
            }
        } else {
            let mut file = std::fs::File::open(path)?;
            if plain {
                vo_pager::ansi::copy_plain(&mut file, &mut output)?;
            } else {
                io::copy(&mut file, &mut output)?;
            }
        }
    }
    output.flush()
}

const HELP: &str = "vo - View Optimally\n\n\
Usage: vo [OPTIONS] [+G|+F|+LINE] [FILE ...]\n\n\
Read files or a stream from stdin; '-' explicitly selects stdin.\n\
Ctrl-W toggles viewport truncation and wrapping; F1 or ? shows pager keys.\n\n\
Options:\n\
  -S, --chop-long-lines     Truncate long lines (default)\n\
      --wrap                Wrap at terminal display-cell boundaries\n\
  -N, --line-numbers        Show logical line numbers\n\
  -i, --ignore-case         Ignore case unless pattern has uppercase letters\n\
  -I, --ignore-case-always  Always ignore case in searches\n\
  -F, --quit-if-one-screen  Exit when the input fits in one screen\n\
  -X, --no-init             Do not use the alternate screen\n\
  -R, --raw-control-chars   less compatibility; ANSI styles work by default\n\
      --color POLICY        auto, always, or never (default: auto)\n\
  -x, --tabstop CELLS       Tab stop width, 1..256 (default: 8)\n\
      --initial-command C   Start at G (end), F (follow), or line number\n\
  -h, --help                Show this help\n\
  -V, --version             Show the version\n\n\
Theme config: $VO_CONFIG, $XDG_CONFIG_HOME/vo/config.json,\n\
              or ~/.config/vo/config.json (including macOS).\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_parser_preserves_less_flags_values_and_literal_paths() {
        let Invocation::Run(args) = Args::parse(lexopt::Parser::from_args([
            "-SNx4",
            "--color=never",
            "+42",
            "--",
            "-log",
        ]))
        .unwrap() else {
            panic!()
        };
        assert!(args.line_numbers);
        assert_eq!(args.tabstop, 4);
        assert_eq!(args.initial.as_deref(), Some("+42"));
        assert_eq!(args.files, [PathBuf::from("-log")]);
        assert!(matches!(args.color, Color::Never));
        assert!(Args::parse(lexopt::Parser::from_args(["--tabstop", "0"])).is_err());
        assert!(Args::parse(lexopt::Parser::from_args(["--color=invalid"])).is_err());
    }
}
