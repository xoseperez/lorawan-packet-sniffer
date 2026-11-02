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

    /// A history query stopped because whoever asked for it went away.
    #[error("history query cancelled")]
    Cancelled,

    /// A history query with a row filter, asked of a database that is not in
    /// WAL mode.  Such a scan would have to run on the connection that saves
    /// records, and stall it.
    #[error("history filters need the database in WAL mode")]
    HistoryUnavailable,

    /// The database stayed locked past the history connection's wait.
    #[error("the database is busy")]
    Busy,
}
