use std::{env, fs, path::PathBuf};

use serde::Deserialize;

use crate::{
    cli::{BackendChoice, Cli},
    error::AppError,
};

const APP_DIRECTORY: &str = "airman";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    backend: BackendChoice,
    wifi_interface: Option<String>,
    bluetooth_adapter: Option<String>,
    auto_scan: Option<bool>,
    auto_discover: Option<bool>,
    log_level: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub backend: BackendChoice,
    pub wifi_interface: Option<String>,
    pub bluetooth_adapter: Option<String>,
    pub auto_scan: bool,
    pub auto_discover: bool,
    pub log_level: String,
    pub log_file: Option<PathBuf>,
}

impl Settings {
    pub fn load(cli: &Cli) -> Result<Self, AppError> {
        let path = config_path();
        let file = match fs::read_to_string(&path) {
            Ok(contents) => {
                toml::from_str::<FileConfig>(&contents).map_err(|source| AppError::Config {
                    path: path.clone(),
                    source,
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
            Err(source) => return Err(AppError::Io { path, source }),
        };

        Ok(Self {
            backend: cli.backend.unwrap_or(file.backend),
            wifi_interface: cli.wifi_interface.clone().or(file.wifi_interface),
            bluetooth_adapter: cli.bluetooth_adapter.clone().or(file.bluetooth_adapter),
            auto_scan: if cli.no_auto_scan {
                false
            } else {
                file.auto_scan.unwrap_or(true)
            },
            auto_discover: if cli.no_auto_discover {
                false
            } else {
                file.auto_discover.unwrap_or(true)
            },
            log_level: cli
                .log_level
                .clone()
                .or(file.log_level)
                .unwrap_or_else(|| "airman=info".to_owned()),
            log_file: cli.log_file.clone(),
        })
    }
}

pub fn config_path() -> PathBuf {
    xdg_path("XDG_CONFIG_HOME", ".config")
        .join(APP_DIRECTORY)
        .join("config.toml")
}

pub fn state_directory() -> PathBuf {
    xdg_path("XDG_STATE_HOME", ".local/state").join(APP_DIRECTORY)
}

fn xdg_path(variable: &str, home_suffix: &str) -> PathBuf {
    env::var_os(variable).map(PathBuf::from).unwrap_or_else(|| {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(home_suffix)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn command_line_overrides_defaults() {
        let cli = Cli::try_parse_from([
            "airman",
            "--backend",
            "iwd",
            "--wifi-interface",
            "wlan9",
            "--no-auto-scan",
            "--no-auto-discover",
        ])
        .unwrap();

        assert_eq!(cli.backend, Some(BackendChoice::Iwd));
        assert_eq!(cli.wifi_interface.as_deref(), Some("wlan9"));
        assert!(cli.no_auto_scan);
        assert!(cli.no_auto_discover);
    }

    #[test]
    fn file_config_rejects_unknown_keys() {
        assert!(toml::from_str::<FileConfig>("mystery = true").is_err());
    }
}
