use thiserror::Error;

#[derive(Debug, Error)]
pub enum RogerError {
    #[error("config error: {0}")]
    Config(String),

    #[error("matrix error: {0}")]
    Matrix(#[from] matrix_sdk::Error),

    #[error("matrix http error: {0}")]
    MatrixHttp(#[from] matrix_sdk::HttpError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("env error: {0}")]
    Env(#[from] dotenvy::Error),
}
