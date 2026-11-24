//! Unified error types shared across custody, host/sandbox, core, engine, and MCP layers.
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Io,
    InvalidInput,
    NotFound,
    Conflict,
    Unauthorized,
    Policy,
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub kind: ErrorKind,
    pub status: u16,
    pub message: String,
    pub code: Option<String>,
    pub os_code: Option<i32>,
}

pub type HarnessError = RuntimeError;
pub type HostError = RuntimeError;
pub type Result<T> = std::result::Result<T, RuntimeError>;

impl RuntimeError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        let status = match kind {
            ErrorKind::InvalidInput => 400,
            ErrorKind::NotFound => 404,
            ErrorKind::Conflict => 409,
            ErrorKind::Unauthorized | ErrorKind::Policy => 403,
            ErrorKind::Io | ErrorKind::Unavailable => 503,
        };
        Self {
            kind,
            status,
            message: message.into(),
            code: None,
            os_code: None,
        }
    }

    pub fn coded(kind: ErrorKind, code: impl Into<String>, message: impl Into<String>) -> Self {
        let mut err = Self::new(kind, message);
        err.code = Some(code.into());
        err
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::coded(ErrorKind::InvalidInput, "invalid-request", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::coded(ErrorKind::Conflict, "conflict", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::coded(ErrorKind::Unauthorized, "forbidden", message)
    }

    pub fn policy(message: impl Into<String>) -> Self {
        Self::coded(ErrorKind::Policy, "policy", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::coded(ErrorKind::Unavailable, "unavailable", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::coded(ErrorKind::NotFound, "not-found", message)
    }

    pub fn missing(message: impl Into<String>) -> Self {
        Self::not_found(message)
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn http(status: u16, code: &str, message: impl Into<String>) -> Self {
        let kind = match status {
            400 => ErrorKind::InvalidInput,
            404 => ErrorKind::NotFound,
            409 => ErrorKind::Conflict,
            403 => ErrorKind::Unauthorized,
            _ => ErrorKind::Unavailable,
        };
        Self {
            kind,
            status,
            message: message.into(),
            code: Some(code.into()),
            os_code: None,
        }
    }

    pub fn code_str(&self) -> &str {
        self.code.as_deref().unwrap_or("runtime-refused")
    }

    pub fn status_code(&self) -> u16 {
        self.status
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

impl From<std::io::Error> for RuntimeError {
    fn from(error: std::io::Error) -> Self {
        let kind = if error.kind() == std::io::ErrorKind::NotFound {
            ErrorKind::NotFound
        } else {
            ErrorKind::Io
        };
        let mut value = Self::new(kind, format!("Filesystem operation failed: {error}"));
        value.os_code = error.raw_os_error();
        value
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(error: serde_json::Error) -> Self {
        Self::invalid(format!("Invalid JSON: {error}"))
    }
}

impl From<rusqlite::Error> for RuntimeError {
    fn from(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::QueryReturnedNoRows => Self::not_found("Record not found."),
            rusqlite::Error::SqliteFailure(failure, _) => {
                let code = match failure.extended_code & 255 {
                    5 | 6 => ErrorKind::Conflict,
                    19 => ErrorKind::Conflict,
                    _ => ErrorKind::Unavailable,
                };
                Self::new(code, format!("Local database operation failed: {error}"))
            }
            _ => Self::unavailable(format!("Local database operation failed: {error}")),
        }
    }
}

impl From<std::string::FromUtf8Error> for RuntimeError {
    fn from(_: std::string::FromUtf8Error) -> Self {
        Self::invalid("Input is not valid UTF-8")
    }
}
