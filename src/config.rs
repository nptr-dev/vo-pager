use crate::theme::Theme;
use serde::Deserialize;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub theme: Theme,
}

impl FileConfig {
    pub fn load() -> Self {
        let path = resolve_config_path(
            env::var_os("VO_CONFIG"),
            env::var_os("XDG_CONFIG_HOME"),
            env::var_os("HOME"),
        );
        Self::load_from_path(path.as_deref())
    }

    fn load_from_path(path: Option<&Path>) -> Self {
        path.and_then(|path| fs::read(path).ok())
            .and_then(|contents| serde_json::from_slice(&contents).ok())
            .unwrap_or_default()
    }
}

fn resolve_config_path(
    vo_config: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = vo_config.filter(|path| !path.is_empty()) {
        return Some(path.into());
    }

    xdg_config_home
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("vo/config.json"))
        .or_else(|| {
            home.filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join(".config/vo/config.json"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::ColorLevel;

    fn status(config: FileConfig) -> Vec<u8> {
        config
            .theme
            .resolve(ColorLevel::Ansi256)
            .status
            .paint(b"status".to_vec(), 0)
    }

    #[test]
    fn resolves_config_paths_by_precedence() {
        let cases = [
            (
                Some("/custom/config.json"),
                Some("/xdg"),
                Some("/home/user"),
                Some("/custom/config.json"),
            ),
            (
                Some("relative.json"),
                Some("/xdg"),
                Some("/home/user"),
                Some("relative.json"),
            ),
            (
                Some(""),
                Some("/xdg"),
                Some("/home/user"),
                Some("/xdg/vo/config.json"),
            ),
            (
                None,
                Some("relative"),
                Some("/home/user"),
                Some("/home/user/.config/vo/config.json"),
            ),
            (
                None,
                Some(""),
                Some("/home/user"),
                Some("/home/user/.config/vo/config.json"),
            ),
            (None, Some("relative"), None, None),
            (Some(""), Some(""), Some(""), None),
            (None, None, None, None),
        ];
        for (vo_config, xdg_config_home, home, expected) in cases {
            assert_eq!(
                resolve_config_path(
                    vo_config.map(OsString::from),
                    xdg_config_home.map(OsString::from),
                    home.map(OsString::from),
                ),
                expected.map(PathBuf::from),
            );
        }
    }

    #[test]
    fn absent_or_unreadable_config_uses_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.json");
        let default = status(FileConfig::default());
        for path in [None, Some(missing.as_path()), Some(directory.path())] {
            assert_eq!(status(FileConfig::load_from_path(path)), default);
        }
    }

    #[test]
    fn reads_valid_configs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let default = status(FileConfig::default());
        for (contents, expected) in [
            ("{}", default.as_slice()),
            (r#"{"theme": {}}"#, default.as_slice()),
            (
                r#"{"theme":{"status":{"background":17}}}"#,
                b"\x1b[0;38;5;16;48;5;17mstatus\x1b[0m",
            ),
        ] {
            fs::write(&path, contents).unwrap();
            assert_eq!(status(FileConfig::load_from_path(Some(&path))), expected);
        }
    }

    #[test]
    fn invalid_config_uses_defaults_instead_of_partial_styles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let default = status(FileConfig::default());
        for contents in [
            "",
            "{",
            r#"{"theme":{"status":{"background":17}},"wrap":true}"#,
            r#"{"theme":{"status":{"background":17},"truncation":{"foreground":"red"}}}"#,
        ] {
            fs::write(&path, contents).unwrap();
            assert_eq!(status(FileConfig::load_from_path(Some(&path))), default);
        }
    }
}
