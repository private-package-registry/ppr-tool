//! Error taxonomy. Every failure carries a kind that maps to a documented exit code.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Wrong invocation: missing flags, bad values. Exit 2.
    Usage,
    /// Local validation of archives, names or versions failed. Exit 3.
    Validation,
    /// Credentials missing or rejected. Exit 4.
    Auth,
    /// Registry or network failure. Exit 5.
    Registry,
    /// The verification command failed. Exit 6.
    VerifyFailed,
    /// Local publication state does not match the request. Exit 7.
    State,
    /// Unexpected internal failure. Exit 1.
    Internal,
}

impl Kind {
    pub fn exit_code(self) -> i32 {
        match self {
            Kind::Internal => 1,
            Kind::Usage => 2,
            Kind::Validation => 3,
            Kind::Auth => 4,
            Kind::Registry => 5,
            Kind::VerifyFailed => 6,
            Kind::State => 7,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Kind::Internal => "internal",
            Kind::Usage => "usage",
            Kind::Validation => "validation",
            Kind::Auth => "auth",
            Kind::Registry => "registry",
            Kind::VerifyFailed => "verify-failed",
            Kind::State => "state",
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
    pub hint: Option<String>,
    /// Extra machine-readable fields for the `--json` error object.
    pub detail: Option<serde_json::Value>,
}

impl Error {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Error { kind, message: message.into(), hint: None, detail: None }
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
    /// Keeps the message and hint but reclassifies the failure.
    pub fn kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Error::new(Kind::Usage, message)
    }
    pub fn validation(message: impl Into<String>) -> Self {
        Error::new(Kind::Validation, message)
    }
    pub fn auth(message: impl Into<String>) -> Self {
        Error::new(Kind::Auth, message)
    }
    pub fn registry(message: impl Into<String>) -> Self {
        Error::new(Kind::Registry, message)
    }
    pub fn state(message: impl Into<String>) -> Self {
        Error::new(Kind::State, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Error::new(Kind::Internal, message)
    }
    /// Prefix the message with a location such as a file name.
    pub fn at(mut self, location: impl fmt::Display) -> Self {
        self.message = format!("{location}: {}", self.message);
        self
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
        Error::internal(format!("I/O error: {error}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::internal(format!("JSON error: {error}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
