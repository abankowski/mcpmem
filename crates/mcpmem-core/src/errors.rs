use thiserror::Error;

#[derive(Error, Debug)]
pub enum MCSError {
    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Method not found: {0}")]
    MethodNotFound(String),

    #[error("Invalid params: {0}")]
    InvalidParams(String),

    #[error("Insufficient scope: {tool} needs {scope}")]
    InsufficientScope { tool: String, scope: &'static str },

    #[error("Memory error: {0}")]
    MemoryError(String),

    /// A SQLite `UNIQUE` constraint refused a write the caller could have
    /// avoided — the duplicate-key case of the admin API. Named as its own
    /// variant so a handler can answer an avoidable conflict (409) instead
    /// of a store fault (500).
    #[error("Constraint violation: {0}")]
    ConstraintViolation(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Serialization error: {0}")]
    SerializationError(String),
}

impl MCSError {
    pub const fn error_code(&self) -> i64 {
        match self {
            MCSError::ParseError(_) => -32700,
            MCSError::MethodNotFound(_) => -32601,
            MCSError::InvalidParams(_) => -32602,
            MCSError::InsufficientScope { .. } => -32002,
            MCSError::MemoryError(_) => -32000,
            MCSError::ConstraintViolation(_) => -32005,
            MCSError::IoError(_) => -32003,
            MCSError::JsonError(_) => -32700,
            MCSError::SerializationError(_) => -32004,
        }
    }
}

pub type Result<T> = std::result::Result<T, MCSError>;
