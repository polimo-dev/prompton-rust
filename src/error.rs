//! The one error type every fallible SDK call returns.

use crate::snapshot::DecodeError;
use crate::template::TemplateError;

/// Anything that can go wrong in the SDK.
///
/// The three errors an app is expected to branch on are [`Error::UnknownUseCase`],
/// [`Error::Unresolved`] and [`Error::UnknownPrompt`]: each of them is a bug in the deployment or
/// in the call, never a reason to fall back to a hard-coded prompt. [`Error::NotReady`] is the
/// only one an app should retry, and it can only happen before the first use-case document has
/// been obtained from any tier.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No use-case document in memory, on disk or in the bundle, and PromptOn could not be reached.
    #[error(
        "PromptOn is unreachable and no use-case document is cached (memory, disk or bundle): {0}"
    )]
    NotReady(String),

    /// The use-case document has no use case with that key.
    #[error("unknown use case: {0}")]
    UnknownUseCase(String),

    /// The use case exists but has no live deployment in this environment.
    #[error("use case {0} has no live deployment in this environment")]
    Unresolved(String),

    /// The live deployment pins no prompt version under that name. There is no fallback to
    /// `default`.
    #[error("use case {use_case} pins no prompt named {prompt:?} (available: {prompt_names:?})")]
    UnknownPrompt {
        /// The use case that was read.
        use_case: String,
        /// The prompt name that was asked for.
        prompt: String,
        /// The prompt names the live revision does pin.
        prompt_names: Vec<String>,
    },

    /// Rendering failed: a missing variable, a rejected construct, or a bad template.
    #[error(transparent)]
    Template(#[from] TemplateError),

    /// A use-case document could not be decoded.
    #[error(transparent)]
    Decode(#[from] DecodeError),

    /// PromptOn answered with an error status.
    #[error("PromptOn returned {status}{}: {message}", .code.as_deref().map(|c| format!(" {c}")).unwrap_or_default())]
    Http {
        /// The HTTP status.
        status: u16,
        /// The `error.code` from the response body, when there was one.
        code: Option<String>,
        /// The `error.message` from the response body, or the raw body.
        message: String,
        /// The `error.details` object, when there was one.
        details: serde_json::Value,
    },

    /// The request never reached PromptOn (DNS, TCP, TLS, timeout).
    #[error("could not reach PromptOn: {0}")]
    Transport(String),

    /// The SDK is configured in a way that makes this call impossible.
    #[error("PromptOn SDK configuration error: {0}")]
    Config(String),

    /// A remote call was attempted while the SDK is in offline or test mode, or without an API key.
    #[error("PromptOn remote calls are disabled: {0}")]
    RemoteDisabled(String),

    /// A local file could not be read or written.
    #[error("PromptOn SDK file error: {0}")]
    Io(#[from] std::io::Error),

    /// A payload could not be encoded or decoded.
    #[error("PromptOn SDK JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A monitoring-log record is missing a required field.
    #[error("invalid monitoring log record: {0}")]
    InvalidRecord(String),
}

impl Error {
    /// The missing variable's name, when this error is a missing-variable one.
    pub fn missing_variable(&self) -> Option<&str> {
        match self {
            Error::Template(TemplateError::MissingVariable(name)) => Some(name),
            _ => None,
        }
    }

    /// Whether the error means "PromptOn itself is unavailable" rather than "your call is wrong".
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            Error::NotReady(_)
                | Error::Transport(_)
                | Error::Http {
                    status: 500..=599,
                    ..
                }
        )
    }
}

/// The SDK's result alias.
pub type Result<T> = std::result::Result<T, Error>;
