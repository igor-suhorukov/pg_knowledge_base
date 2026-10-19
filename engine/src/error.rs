// SPDX-License-Identifier: Apache-2.0
//! The engine's errors, as the C ABI's status codes (include/kb_engine.h).

use std::fmt;

pub const KB_OK: i32 = 0;
pub const KB_ERR_INTERNAL: i32 = 1;
pub const KB_ERR_SYNTAX: i32 = 2;
pub const KB_ERR_INVALID: i32 = 3;
pub const KB_ERR_EVALUATION: i32 = 4;
pub const KB_ERR_HOST: i32 = 5;
pub const KB_ERR_UNSUPPORTED: i32 = 6;
pub const KB_ERR_NOT_FOUND: i32 = 7;
pub const KB_ERR_EXISTS: i32 = 8;
pub const KB_ERR_MAPPING: i32 = 9;
pub const KB_ERR_PANIC: i32 = 10;
pub const KB_ERR_MEMORY: i32 = 11;

#[derive(Debug, Clone)]
pub struct EngineError {
    pub code: i32,
    /// The status an HTTP response gives the error; 0 lets the code decide.
    pub http_status: i32,
    pub message: String,
}

pub type EResult<T> = Result<T, EngineError>;

impl EngineError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            http_status: 0,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(KB_ERR_INTERNAL, message)
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(KB_ERR_SYNTAX, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(KB_ERR_INVALID, message)
    }

    pub fn evaluation(message: impl Into<String>) -> Self {
        Self::new(KB_ERR_EVALUATION, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(KB_ERR_UNSUPPORTED, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(KB_ERR_NOT_FOUND, message)
    }

    pub fn exists(message: impl Into<String>) -> Self {
        Self::new(KB_ERR_EXISTS, message)
    }

    pub fn mapping(message: impl Into<String>) -> Self {
        Self::new(KB_ERR_MAPPING, message)
    }

    /// A callback failed: PostgreSQL's error is the host's, kept for the
    /// caller to raise once the entry point has returned.
    pub fn host() -> Self {
        Self::new(KB_ERR_HOST, "the host raised an error")
    }

    pub fn with_http(mut self, status: i32) -> Self {
        self.http_status = status;
        self
    }

    pub fn is_host(&self) -> bool {
        self.code == KB_ERR_HOST
    }

    /// The status of an HTTP response that reports the error.
    pub fn http(&self) -> i32 {
        if self.http_status != 0 {
            return self.http_status;
        }
        match self.code {
            KB_ERR_SYNTAX | KB_ERR_INVALID | KB_ERR_MAPPING => 400,
            KB_ERR_NOT_FOUND => 404,
            KB_ERR_EXISTS => 409,
            KB_ERR_UNSUPPORTED => 501,
            KB_ERR_MEMORY => 503,
            _ => 500,
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EngineError {}

impl From<std::str::Utf8Error> for EngineError {
    fn from(e: std::str::Utf8Error) -> Self {
        Self::invalid(format!("a string is not UTF-8: {e}"))
    }
}

impl From<spargebra::SparqlSyntaxError> for EngineError {
    fn from(e: spargebra::SparqlSyntaxError) -> Self {
        Self::syntax(e.to_string())
    }
}

impl From<oxiri::IriParseError> for EngineError {
    fn from(e: oxiri::IriParseError) -> Self {
        Self::invalid(format!("invalid IRI: {e}"))
    }
}

impl From<oxrdfio::RdfParseError> for EngineError {
    fn from(e: oxrdfio::RdfParseError) -> Self {
        match e {
            oxrdfio::RdfParseError::Syntax(e) => Self::syntax(e.to_string()),
            oxrdfio::RdfParseError::Io(e) => Self::from(e),
        }
    }
}

impl From<spareval::QueryEvaluationError> for EngineError {
    fn from(e: spareval::QueryEvaluationError) -> Self {
        // the dataset's own errors come back through the evaluator: a host
        // error stays one, so that the caller raises PostgreSQL's
        if let spareval::QueryEvaluationError::Dataset(inner) = &e {
            if let Some(inner) = inner.downcast_ref::<EngineError>() {
                return inner.clone();
            }
        }
        if let spareval::QueryEvaluationError::Service(inner) = &e {
            if let Some(inner) = inner.downcast_ref::<EngineError>() {
                return inner.clone();
            }
        }
        Self::evaluation(e.to_string())
    }
}

impl From<std::io::Error> for EngineError {
    fn from(e: std::io::Error) -> Self {
        // a writer's error may carry the host's or the engine's own
        if let Some(inner) = e.get_ref() {
            if let Some(inner) = inner.downcast_ref::<EngineError>() {
                return inner.clone();
            }
        }
        Self::internal(format!("I/O: {e}"))
    }
}

impl From<EngineError> for std::io::Error {
    fn from(e: EngineError) -> Self {
        std::io::Error::other(e)
    }
}
