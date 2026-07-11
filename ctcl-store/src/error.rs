use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown instant: {0}")]
    UnknownInstant(String),
    #[error("unknown system: {0}")]
    UnknownSystem(String),
    #[error("unknown group: {0}")]
    UnknownGroup(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    Core(#[from] ctcl_core::CtclError),
}

impl StoreError {
    /// Mirrors the CTCL Worker's error codes so a local error looks the same
    /// shape as one from commoninstant.org.
    pub fn code(&self) -> &'static str {
        match self {
            StoreError::UnknownInstant(_) => "UNKNOWN_INSTANT",
            StoreError::UnknownSystem(_) => "UNKNOWN_SYSTEM",
            StoreError::UnknownGroup(_) => "UNKNOWN_GROUP",
            StoreError::InvalidInput(_) => "INVALID_TIME_VALUE",
            StoreError::Core(e) => e.code(),
            StoreError::Sqlite(_) => "STORE_UNAVAILABLE",
            StoreError::Json(_) => "STORE_UNAVAILABLE",
        }
    }
}
