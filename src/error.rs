//! Unified error type for LocalPDF-TUI.
//!
//! Every engine and CLI failure surfaces as `AppError` so both execution
//! modes can format a deterministic message: the TUI renders it in the
//! status banner, agent mode serializes it into the JSON error document.

use std::fmt;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    /// The workspace root itself could not be resolved.
    Workspace(String, std::io::Error),

    /// A path failed to canonicalize or escaped the sandbox.
    PathEscape { given: String },

    /// A file-system operation failed.
    Io(String, std::io::Error),

    /// The PDF could not be parsed or written.
    Pdf(String),

    /// A page-range specification was malformed or out of bounds.
    PageRange(String),

    /// A pre-condition of the operation was not met.
    Operation(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Workspace(path, e) => write!(f, "workspace '{path}' is not usable: {e}"),
            AppError::PathEscape { given } => {
                write!(f, "path '{given}' was rejected: it escapes the sandboxed workspace")
            }
            AppError::Io(what, e) => write!(f, "{what}: {e}"),
            AppError::Pdf(msg) => write!(f, "pdf error: {msg}"),
            AppError::PageRange(msg) => write!(f, "page range error: {msg}"),
            AppError::Operation(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AppError::Workspace(_, e) | AppError::Io(_, e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io("i/o failure".to_string(), e)
    }
}
