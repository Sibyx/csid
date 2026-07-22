//! Error type for the CSIQ codec and the raw-stream parser.

use std::io;

/// Errors produced while reading, writing, or parsing CSI data.
#[derive(Debug, thiserror::Error)]
pub enum CsiqError {
    /// Underlying byte source/sink failure.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// The file did not start with the `CSIQ` magic.
    #[error("bad magic: expected {expected:?}, found {found:?}")]
    BadMagic { expected: [u8; 4], found: [u8; 4] },

    /// The container declares a format version this build cannot read.
    #[error("unsupported CSIQ version {0} (this build reads v{max})", max = crate::FORMAT_VERSION)]
    UnsupportedVersion(u16),

    /// A length prefix pointed past the end of the buffer, or a field was
    /// truncated. Carries a short human context.
    #[error("truncated or malformed record: {0}")]
    Malformed(&'static str),

    /// The embedded session block was not valid UTF-8 JSON.
    #[error("invalid session metadata: {0}")]
    Session(#[from] serde_json::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, CsiqError>;
