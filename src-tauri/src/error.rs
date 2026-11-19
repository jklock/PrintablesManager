//! Structured, code-bearing errors.
//!
//! Every command returns `Result<T, AppError>`. The value serialises to
//! `{ "code": "...", "message": "...", "detail": "..." }`, which the frontend
//! already understands (`err.message || err`). Before this module existed the
//! API layer reported every failure as a bare string, so an authentication
//! failure surfaced as "model not found".

use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Serialize)]
pub struct AppError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AppError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("INTERNAL", message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new("IO", message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("INVALID_INPUT", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("NOT_FOUND", message)
    }

    pub fn not_authenticated(message: impl Into<String>) -> Self {
        Self::new("NOT_AUTHENTICATED", message)
    }

    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new("RATE_LIMITED", message)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new("NETWORK", message)
    }

    pub fn graphql(message: impl Into<String>) -> Self {
        Self::new("GRAPHQL", message)
    }

    pub fn git(message: impl Into<String>) -> Self {
        Self::new("GIT", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("CONFLICT", message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("UNSUPPORTED", message)
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

impl From<String> for AppError {
    fn from(value: String) -> Self {
        AppError::internal(value)
    }
}

impl From<&str> for AppError {
    fn from(value: &str) -> Self {
        AppError::internal(value)
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        AppError::io(value.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        AppError::internal(format!("Invalid JSON: {value}"))
    }
}

impl From<git2::Error> for AppError {
    fn from(value: git2::Error) -> Self {
        AppError::git(value.message().to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
