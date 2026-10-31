use serde::Deserialize;
use unicode_width::UnicodeWidthStr;

use crate::render::{BOLD, Color, ColorLevel, DIM, ITALIC, REVERSE, StyleState, UNDERLINE};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    status: Style,
    filename: Style,
    line_count: Style,
    hint: Style,
    prompt: Style,
    message: Style,
    line_numbers: Style,
    truncation: Style,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Style {
    foreground: Option<ThemeColor>,
    background: Option<ThemeColor>,
    bold: Option<bool>,
    dim: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    reverse: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "ColorValue")]
struct ThemeColor(Option<Color>);

#[derive(Deserialize)]
#[serde(untagged)]
enum ColorValue {
    Index(u8),
    Name(String),
}

pub struct ResolvedTheme {
    pub status: UiStyle,
    pub filename: UiStyle,
    pub line_count: UiStyle,
    pub hint: UiStyle,
    pub prompt: UiStyle,
    pub message: UiStyle,
    pub line_numbers: UiStyle,
    pub truncation: StyleState,
}

pub struct UiStyle {
    prefix: Vec<u8>,
}

impl Theme {
    /// Resolve colors and cache SGR sequences once for the selected terminal palette.
    pub fn resolve(&self, level: ColorLevel) -> ResolvedTheme {
        let mut status = StyleState::default();
        status.foreground = Some(Color::Rgb(255, 255, 255));
        status.background = Some(Color::Rgb(32, 32, 32));
        let status = self.status.resolve(status);
        let mut filename = status;
        filename.foreground = Some(Color::Rgb(0, 0, 0));
        filename.background = Some(Color::Rgb(211, 211, 211));
        ResolvedTheme {
            status: UiStyle::new(status, level),
            filename: UiStyle::new(self.filename.resolve(filename), level),
            line_count: UiStyle::new(self.line_count.resolve(status), level),
            hint: UiStyle::new(self.hint.resolve(status), level),
            prompt: UiStyle::new(self.prompt.resolve(status), level),
            message: UiStyle::new(self.message.resolve(status), level),
            line_numbers: UiStyle::new(self.line_numbers.resolve(StyleState::default()), level),
            truncation: self
                .truncation
                .resolve(StyleState::truncation(level))
                .for_level(level),
        }
    }
}

impl UiStyle {
    /// Style already-clipped plain text, padding styled bars to their display width.
    pub fn paint(&self, text: Vec<u8>, width: usize) -> Vec<u8> {
        if self.prefix.is_empty() {
            return text;
        }
        let padding = if width == 0 {
            0
        } else {
            width.saturating_sub(String::from_utf8_lossy(&text).width())
        };
        let mut output = Vec::with_capacity(self.prefix.len() + text.len() + padding + 4);
        output.extend_from_slice(&self.prefix);
        output.extend_from_slice(&text);
        output.extend(std::iter::repeat_n(b' ', padding));
        output.extend_from_slice(b"\x1b[0m");
        output
    }

    fn new(style: StyleState, level: ColorLevel) -> Self {
        let style = style.for_level(level);
        let mut prefix = Vec::new();
        if style != StyleState::default() {
            style.write(&mut prefix);
        }
        Self { prefix }
    }
}

impl Style {
    fn resolve(&self, mut base: StyleState) -> StyleState {
        if let Some(color) = self.foreground {
            base.foreground = color.0;
        }
        if let Some(color) = self.background {
            base.background = color.0;
        }
        for (enabled, attribute) in [
            (self.bold, BOLD),
            (self.dim, DIM),
            (self.italic, ITALIC),
            (self.underline, UNDERLINE),
            (self.reverse, REVERSE),
        ] {
            if let Some(enabled) = enabled {
                if enabled {
                    base.attributes |= attribute;
                } else {
                    base.attributes &= !attribute;
                }
            }
        }
        base
    }
}

impl TryFrom<ColorValue> for ThemeColor {
    type Error = String;

    fn try_from(value: ColorValue) -> Result<Self, Self::Error> {
        match value {
            ColorValue::Index(index) => Ok(Self(Some(Color::Indexed(index)))),
            ColorValue::Name(name) if name == "default" => Ok(Self(None)),
            ColorValue::Name(name) => {
                if let Some(hex) = name.strip_prefix('#')
                    && hex.len() == 6
                    && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && let Ok(rgb) = u32::from_str_radix(hex, 16)
                {
                    return Ok(Self(Some(Color::Rgb(
                        (rgb >> 16) as u8,
                        (rgb >> 8) as u8,
                        rgb as u8,
                    ))));
                }
                Err(format!(
                    "invalid color {name:?}: expected #RRGGBB, an index from 0 to 255, or default"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(json: &str, level: ColorLevel) -> ResolvedTheme {
        serde_json::from_str::<Theme>(json).unwrap().resolve(level)
    }

    #[test]
    fn partial_styles_inherit_defaults_and_bar_styles() {
        let theme = theme(
            r##"{
                "status": {"foreground": "#abcdef", "background": 17, "bold": true, "dim": true},
                "prompt": {"foreground": "default", "bold": false},
                "hint": {"dim": false, "underline": true},
                "truncation": {"bold": true}
            }"##,
            ColorLevel::TrueColor,
        );
        assert_eq!(
            theme.status.paint(b"status".to_vec(), 8),
            b"\x1b[0;1;2;38;2;171;205;239;48;5;17mstatus  \x1b[0m"
        );
        assert_eq!(
            theme.filename.paint(b"file ".to_vec(), 0),
            b"\x1b[0;1;2;38;2;0;0;0;48;2;211;211;211mfile \x1b[0m"
        );
        assert_eq!(
            theme.line_count.paint(b" :12".to_vec(), 0),
            theme.status.paint(b" :12".to_vec(), 0)
        );
        assert_eq!(
            theme.prompt.paint(b"/".to_vec(), 0),
            b"\x1b[0;2;48;5;17m/\x1b[0m"
        );
        assert_eq!(
            theme.hint.paint(b"Wrap".to_vec(), 0),
            b"\x1b[0;1;4;38;2;171;205;239;48;5;17mWrap\x1b[0m"
        );
        assert_eq!(
            theme.message.paint(b"message".to_vec(), 0),
            theme.status.paint(b"message".to_vec(), 0)
        );
        let mut indicator = Vec::new();
        theme.truncation.write(&mut indicator);
        assert_eq!(indicator, b"\x1b[0;1;38;2;32;32;32;48;2;211;211;211m");
        assert_eq!(theme.line_numbers.paint(b"1 ".to_vec(), 0), b"1 ");
    }

    #[test]
    fn theme_colors_follow_terminal_palette_and_color_policy() {
        for (level, expected) in [
            (ColorLevel::None, b"x".as_slice()),
            (ColorLevel::Monochrome, b"\x1b[0;1mx\x1b[0m"),
            (ColorLevel::Ansi8, b"\x1b[0;1;31;44mx\x1b[0m"),
            (ColorLevel::Ansi16, b"\x1b[0;1;91;104mx\x1b[0m"),
            (ColorLevel::Ansi256, b"\x1b[0;1;38;5;196;48;5;21mx\x1b[0m"),
            (
                ColorLevel::TrueColor,
                b"\x1b[0;1;38;2;255;0;0;48;5;21mx\x1b[0m",
            ),
        ] {
            let theme = theme(
                r##"{"status":{"foreground":"#ff0000","background":21,"bold":true}}"##,
                level,
            );
            assert_eq!(theme.status.paint(b"x".to_vec(), 0), expected, "{level:?}");
        }
    }

    #[test]
    fn bar_padding_counts_display_cells_before_adding_styles() {
        let theme = theme(r#"{"status":{"background":17}}"#, ColorLevel::Ansi256);
        assert_eq!(
            theme
                .status
                .paint("\u{754c}e\u{301}".as_bytes().to_vec(), 5),
            "\u{1b}[0;38;5;231;48;5;17m\u{754c}e\u{301}  \u{1b}[0m".as_bytes()
        );
    }

    #[test]
    fn invalid_styles_are_rejected_at_the_config_boundary() {
        for json in [
            r##"{"status":{"foreground":"#123"}}"##,
            r##"{"status":{"foreground":"#gggggg"}}"##,
            r##"{"status":{"foreground":"#\u00e90000"}}"##,
            r#"{"status":{"foreground":256}}"#,
            r#"{"status":{"foreground":-1}}"#,
            r#"{"status":{"foreground":[1,2,3]}}"#,
            r#"{"status":{"foreground":"\u001b[31m"}}"#,
            r#"{"status":{"bold":"yes"}}"#,
            r#"{"status":{"forground":17}}"#,
            r#"{"statuz":{}}"#,
        ] {
            assert!(serde_json::from_str::<Theme>(json).is_err(), "{json}");
        }
    }
}
