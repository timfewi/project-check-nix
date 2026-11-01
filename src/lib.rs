#![forbid(unsafe_code)]

pub mod cli;
pub mod diagnostics;
pub mod docs;
pub mod git;
pub mod manifest;
pub mod process;
pub mod runner;
pub mod util;
pub mod watch;

use std::fmt;

pub const MAX_MANIFEST_BYTES: usize = 1_048_576;
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SCANNER_REPORT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_REPORT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub interrupted: Option<i32>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            interrupted: None,
        }
    }
    pub fn interrupted(signal: i32) -> Self {
        Self {
            message: format!("interrupted by signal {signal}"),
            interrupted: Some(signal),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::new(error.to_string())
    }
}
pub type Result<T, E = Error> = std::result::Result<T, E>;
