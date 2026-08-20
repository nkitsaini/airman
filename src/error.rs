use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid configuration in {path}: {source}")]
    Config {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not initialize logging: {0}")]
    Logging(String),
}
