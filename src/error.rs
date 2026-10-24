use std::path::PathBuf;
use std::{fmt, io};

/// Everything that can go wrong in SpiroDB.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An operating-system I/O failure.
    Io(io::Error),
    /// On-disk data failed a checksum or could not be decoded.
    Corruption(String),
    /// Another process (or another `Db` in this one) already has the database open.
    Locked(PathBuf),
    /// An earlier unrecoverable failure (a background flush or compaction, or a
    /// log write). The database still serves reads but refuses writes.
    Background(String),
    /// The caller passed something SpiroDB cannot accept.
    InvalidArgument(String),
    /// SQL that could not be parsed. The message gives the line and column.
    Syntax(String),
    /// A SQL statement that parsed but cannot run: an unknown table or
    /// column, a type mismatch, a locked database, and so on.
    Sql(String),
    /// A SQL statement that would break a constraint (UNIQUE, NOT NULL).
    Constraint(String),
    /// A write sent to a read-only replica. Nothing was changed, so it is safe
    /// to send again to the primary.
    ReadOnly(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn corruption(msg: impl Into<String>) -> Error {
    Error::Corruption(msg.into())
}

// `io::Error` is not `Clone`, but iterators and the background-error slot need
// to hand out copies of a stored error. The kind and message are what matter.
impl Clone for Error {
    fn clone(&self) -> Self {
        match self {
            Error::Io(e) => Error::Io(io::Error::new(e.kind(), e.to_string())),
            Error::Corruption(msg) => Error::Corruption(msg.clone()),
            Error::Locked(path) => Error::Locked(path.clone()),
            Error::Background(msg) => Error::Background(msg.clone()),
            Error::InvalidArgument(msg) => Error::InvalidArgument(msg.clone()),
            Error::Syntax(msg) => Error::Syntax(msg.clone()),
            Error::Sql(msg) => Error::Sql(msg.clone()),
            Error::Constraint(msg) => Error::Constraint(msg.clone()),
            Error::ReadOnly(msg) => Error::ReadOnly(msg.clone()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Corruption(msg) => write!(f, "corruption: {msg}"),
            Error::Locked(path) => write!(f, "database at {} is already open", path.display()),
            Error::Background(msg) => write!(f, "database is read-only after an earlier failure: {msg}"),
            Error::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            Error::Syntax(msg) | Error::Sql(msg) | Error::Constraint(msg) | Error::ReadOnly(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
