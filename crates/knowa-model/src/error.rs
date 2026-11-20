//! The single error type shared by every Knowa crate. Its `code` is part of the
//! public contract (DESIGN §6.3): agents branch on it, so codes are never renamed.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Internal,
    Usage,
    NotFound,
    Invalid,
    Conflict,
    Unsupported,
    Integrity,
    CursorExpired,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Internal => "internal",
            ErrorCode::Usage => "usage",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Invalid => "invalid",
            ErrorCode::Conflict => "conflict",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Integrity => "integrity",
            ErrorCode::CursorExpired => "cursor_expired",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Internal => 1,
            ErrorCode::Usage => 2,
            ErrorCode::NotFound => 3,
            ErrorCode::Invalid => 4,
            ErrorCode::Conflict => 5,
            ErrorCode::Unsupported => 6,
            ErrorCode::Integrity => 7,
            ErrorCode::CursorExpired => 8,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Error { code, message: message.into(), detail: None }
    }
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
    pub fn internal(m: impl Into<String>) -> Self { Self::new(ErrorCode::Internal, m) }
    pub fn usage(m: impl Into<String>) -> Self { Self::new(ErrorCode::Usage, m) }
    pub fn not_found(m: impl Into<String>) -> Self { Self::new(ErrorCode::NotFound, m) }
    pub fn invalid(m: impl Into<String>) -> Self { Self::new(ErrorCode::Invalid, m) }
    pub fn conflict(m: impl Into<String>) -> Self { Self::new(ErrorCode::Conflict, m) }
    pub fn unsupported(m: impl Into<String>) -> Self { Self::new(ErrorCode::Unsupported, m) }
    pub fn integrity(m: impl Into<String>) -> Self { Self::new(ErrorCode::Integrity, m) }
    pub fn cursor_expired(m: impl Into<String>) -> Self { Self::new(ErrorCode::CursorExpired, m) }

    /// The `--json` error envelope: `{"error":{"code","message","detail"?}}`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut e = serde_json::json!({ "code": self.code.as_str(), "message": self.message });
        if let Some(d) = &self.detail {
            e["detail"] = d.clone();
        }
        serde_json::json!({ "error": e })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for Error {}

/// I/O errors are internal unless the caller maps them to something more specific
/// (e.g. a missing file the user named is `not_found` — map that at the call site).
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::internal(format!("io: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::invalid(format!("json: {e}"))
    }
}
