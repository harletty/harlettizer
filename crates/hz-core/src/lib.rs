//! Shared types for the harlettizer encoding engine.
//!
//! What every other crate needs and no one crate owns: the error type an
//! encoding job reports, the bit reader and writer the codecs are built on,
//! the digest a delivery bitstream protects its metadata with, and the
//! coordinate and speaker vocabulary the renderer and the metadata
//! share. Nothing here knows about a format.
//!
//! It stays this way on purpose. A type that only one crate uses belongs in
//! that crate, where its invariants are next to the code that keeps them; this
//! is for the things that would otherwise be defined twice and drift.

pub mod bits;
pub mod bits_read;
pub mod coords;
pub mod hmac;
pub mod speakers;

use std::fmt;
use std::path::PathBuf;

/// The result type used across the workspace.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in the engine.
///
/// One flat enum rather than a per-crate hierarchy: an encoding job is a
/// pipeline, and the only thing a caller ever does with a failure is report it
/// against the file that caused it. The variants therefore carry the path,
/// which is what a job log needs to be actionable.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An I/O failure, with the file it happened on.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    /// A file exists but is not what it claims to be.
    Malformed { path: PathBuf, detail: String },

    /// A file is valid, but uses a feature this engine does not implement.
    ///
    /// Kept distinct from [`Error::Malformed`] on purpose: these are real
    /// streams, and the honest report is "we cannot encode this yet", not
    /// "your input is broken".
    Unsupported { path: PathBuf, detail: String },

    /// The work was done and the result is not fit to ship.
    ///
    /// Kept distinct from the other three because it is not a complaint about
    /// the input: the file was read, the stream was written, and a bound the
    /// caller can move says it should not go out as it is. The output is left
    /// where it was written — a refusal is a statement about what is in it,
    /// and the fastest way to check a bound is to hear what it stopped.
    Refused { path: PathBuf, detail: String },

    /// A master set references a file that could not be found.
    MissingComponent {
        referenced_by: PathBuf,
        reference: String,
    },
}

impl Error {
    /// Attach a path to an [`std::io::Error`].
    ///
    /// ```
    /// # use hz_core::Error;
    /// let e = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
    /// let e = Error::io("/tmp/x.atmos", e);
    /// assert!(e.to_string().contains("x.atmos"));
    /// ```
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Report a file that is not what it claims to be.
    pub fn malformed(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self::Malformed {
            path: path.into(),
            detail: detail.into(),
        }
    }

    /// The result of a run that finished and is not fit to ship.
    ///
    /// ```
    /// # use hz_core::Error;
    /// let e = Error::refused("/tmp/vf.thd", "a source went 41° astray");
    /// assert!(e.to_string().contains("refused"));
    /// ```
    pub fn refused(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self::Refused {
            path: path.into(),
            detail: detail.into(),
        }
    }

    /// Report a valid input the engine does not implement yet.
    pub fn unsupported(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self::Unsupported {
            path: path.into(),
            detail: detail.into(),
        }
    }

    /// The file the failure is about, for a job log.
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Io { path, .. }
            | Self::Malformed { path, .. }
            | Self::Unsupported { path, .. }
            | Self::Refused { path, .. } => path,
            Self::MissingComponent { referenced_by, .. } => referenced_by,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Malformed { path, detail } => {
                write!(f, "{}: malformed: {detail}", path.display())
            }
            Self::Unsupported { path, detail } => {
                write!(f, "{}: not supported yet: {detail}", path.display())
            }
            Self::Refused { path, detail } => {
                write!(f, "{}: refused: {detail}", path.display())
            }
            Self::MissingComponent {
                referenced_by,
                reference,
            } => write!(
                f,
                "{}: references `{reference}`, which does not exist",
                referenced_by.display()
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_the_file() {
        let e = Error::malformed("/a/b.atmos", "no presentations");
        assert_eq!(e.to_string(), "/a/b.atmos: malformed: no presentations");
        assert_eq!(e.path(), std::path::Path::new("/a/b.atmos"));
    }

    #[test]
    fn missing_component_quotes_the_reference() {
        let e = Error::MissingComponent {
            referenced_by: "/a/b.atmos".into(),
            reference: "b.atmos.audio".into(),
        };
        assert!(e.to_string().contains("`b.atmos.audio`"));
    }
}
