use std::fmt;
use std::io;

/// Failures that prevent a native store observation from being complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryError {
    Io {
        coordinate: String,
        operation: &'static str,
        kind: io::ErrorKind,
    },
    InvalidMetadata {
        coordinate: String,
        reason: &'static str,
    },
    BoundExhausted {
        coordinate: String,
        bound: &'static str,
    },
    TimedOut {
        coordinate: String,
    },
    CommandFailed {
        coordinate: String,
        status: String,
    },
}

impl DiscoveryError {
    pub(crate) fn io(
        coordinate: impl Into<String>,
        operation: &'static str,
        error: &io::Error,
    ) -> Self {
        Self::Io {
            coordinate: coordinate.into(),
            operation,
            kind: error.kind(),
        }
    }

    pub fn coordinate(&self) -> &str {
        match self {
            Self::Io { coordinate, .. }
            | Self::InvalidMetadata { coordinate, .. }
            | Self::BoundExhausted { coordinate, .. }
            | Self::TimedOut { coordinate }
            | Self::CommandFailed { coordinate, .. } => coordinate,
        }
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                coordinate,
                operation,
                kind,
            } => write!(f, "{coordinate}: {operation} failed ({kind:?})"),
            Self::InvalidMetadata { coordinate, reason } => write!(
                f,
                "{coordinate}: invalid native identity metadata ({reason})"
            ),
            Self::BoundExhausted { coordinate, bound } => {
                write!(f, "{coordinate}: discovery bound exhausted ({bound})")
            }
            Self::TimedOut { coordinate } => write!(f, "{coordinate}: metadata request timed out"),
            Self::CommandFailed { coordinate, status } => {
                write!(f, "{coordinate}: metadata command failed ({status})")
            }
        }
    }
}

impl std::error::Error for DiscoveryError {}
