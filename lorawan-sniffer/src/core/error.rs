use thiserror::Error;

#[derive(Error, Debug)]
pub enum SnifferError {
    #[error("decode error: {0}")]
    Decode(String),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("storage lock poisoned")]
    LockPoisoned,
}
