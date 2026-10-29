use crate::theme::Theme;
use serde::Deserialize;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub theme: Theme,
}

#[derive(Debug, PartialEq, Eq)]
struct ConfigPath {
    path: PathBuf,
    explicit: bool,
}

impl FileConfig {
    pub fn load() -> io::Result<Self> {
        Self::load_from_path(resolve_config_path(
            env::var_os("VO_CONFIG"),
            env::var_os("XDG_CONFIG_HOME"),
            env::var_os("HOME"),
        ))
    }

    fn load_from_path(config_path: Option<ConfigPath>) -> io::Result<Self> {
        let Some(config_path) = config_path else {
            return Ok(Self::default());
        };
        let contents = match fs::read(&config_path.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !config_path.explicit => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "cannot read config file '{}': {error}",
                        config_path.path.display()
                    ),
                ));
            }
        };
        serde_json::from_slice(&contents).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cannot parse config file '{}': {error}",
                    config_path.path.display()
                ),
            )
        })
    }
}

fn resolve_config_path(
    vo_config: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<ConfigPath> {
    if let Some(path) = vo_config.filter(|path| !path.is_empty()) {
        return Some(ConfigPath {
            path: path.into(),
            explicit: true,
        });
    }

    let path = xdg_config_home
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("vo/config.json"))
        .or_else(|| {
            home.filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join(".config/vo/config.json"))
        })?;
    Some(ConfigPath {
        path,
        explicit: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_config_paths_by_precedence() {
        let cases = [
            (
                Some("/custom/config.json"),
                Some("/xdg"),
                Some("/home/user"),
                Some(("/custom/config.json", true)),
            ),
            (
                Some("relative.json"),
                Some("/xdg"),
                Some("/home/user"),
                Some(("relative.json", true)),
            ),
            (
                Some(""),
                Some("/xdg"),
                Some("/home/user"),
                Some(("/xdg/vo/config.json", false)),
            ),
            (
                None,
                Some("relative"),
                Some("/home/user"),
                Some(("/home/user/.config/vo/config.json", false)),
            ),
            (
                None,
                Some(""),
                Some("/home/user"),
                Some(("/home/user/.config/vo/config.json", false)),
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
                expected.map(|(path, explicit)| ConfigPath {
                    path: path.into(),
                    explicit,
                }),
            );
        }
    }

    #[test]
    fn absent_default_config_uses_defaults() {
        let directory = tempfile::tempdir().unwrap();
        assert!(FileConfig::load_from_path(None).is_ok());
        assert!(
            FileConfig::load_from_path(Some(ConfigPath {
                path: directory.path().join("missing.json"),
                explicit: false,
            }))
            .is_ok()
        );
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.json");
        let error = FileConfig::load_from_path(Some(ConfigPath {
            path: path.clone(),
            explicit: true,
        }))
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }

    #[test]
    fn reads_default_and_empty_theme_objects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        for contents in ["{}", r#"{"theme": {}}"#] {
            fs::write(&path, contents).unwrap();
            assert!(
                FileConfig::load_from_path(Some(ConfigPath {
                    path: path.clone(),
                    explicit: true,
                }))
                .is_ok()
            );
        }
    }

    #[test]
    fn invalid_json_and_unknown_fields_report_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        for contents in ["{", r#"{"wrap": true}"#] {
            fs::write(&path, contents).unwrap();
            let error = FileConfig::load_from_path(Some(ConfigPath {
                path: path.clone(),
                explicit: false,
            }))
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(path.to_str().unwrap()));
        }
    }

    #[test]
    fn other_io_errors_report_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let error = FileConfig::load_from_path(Some(ConfigPath {
            path: path.clone(),
            explicit: false,
        }))
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::IsADirectory);
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }
}
