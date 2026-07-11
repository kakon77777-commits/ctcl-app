use thiserror::Error;

/// Mirrors the error codes used by the CTCL Worker's REST API (commoninstant.org),
/// so the desktop app's local API can return the same codes an agent already knows
/// from calling the hosted service.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CtclError {
    #[error("invalid time value: {0}")]
    InvalidTimeValue(String),
    #[error("unknown encoding: {0}")]
    UnknownEncoding(String),
    #[error("invalid timezone: {0}")]
    InvalidTimezone(String),
    #[error("unsupported policy: {0}")]
    UnsupportedPolicy(String),
}

impl CtclError {
    pub fn code(&self) -> &'static str {
        match self {
            CtclError::InvalidTimeValue(_) => "INVALID_TIME_VALUE",
            CtclError::UnknownEncoding(_) => "UNKNOWN_ENCODING",
            CtclError::InvalidTimezone(_) => "INVALID_TIMEZONE",
            CtclError::UnsupportedPolicy(_) => "UNSUPPORTED_POLICY",
        }
    }
}
