//! Virtual path type for the CyDrive VFS.
//!
//! Contract: a [`RelPath`] always starts with `/`, uses `/` as the sole
//! separator, and never carries a trailing slash (except the root itself).
//! The Python implementation performs no validation, so pollution such as
//! `..` traversal or empty segments must be rejected at the type level here.

use std::fmt;
use std::str::FromStr;

/// Error raised when a string cannot be parsed into a valid [`RelPath`].
#[derive(Debug, thiserror::Error)]
#[error("invalid virtual path: {0}")]
pub enum PathError {
    /// The rejected input string.
    Invalid(String),
}

/// Rejects the path pollution the VFS never stores: empty segments, `.` and
/// `..` traversal markers. Shared by [`RelPath::new`] (per segment) and
/// [`RelPath::join`] (whole segment), so both apply the same rules.
fn is_valid_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".."
}

fn invalid(input: &str) -> PathError {
    PathError::Invalid(input.to_string())
}

/// A validated virtual path inside the CyDrive namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath {
    inner: String,
}

impl RelPath {
    /// Parses and validates `s` as a virtual path.
    ///
    /// Rejected: missing leading `/`, empty segments (`//`), `.` / `..`
    /// segments, backslashes, NUL bytes, and trailing slashes (except `/`).
    pub fn new(s: &str) -> Result<Self, PathError> {
        if s.is_empty() || !s.starts_with('/') {
            return Err(invalid(s));
        }
        if s == "/" {
            return Ok(Self {
                inner: s.to_string(),
            });
        }
        // A trailing slash would create an empty final segment; backslashes
        // and NUL bytes must never enter the namespace.
        if s.ends_with('/') || s.contains('\\') || s.contains('\0') {
            return Err(invalid(s));
        }
        if !s[1..].split('/').all(is_valid_segment) {
            return Err(invalid(s));
        }
        Ok(Self {
            inner: s.to_string(),
        })
    }

    /// The root path `/`.
    pub fn root() -> Self {
        Self {
            inner: "/".to_string(),
        }
    }

    /// Whether this path is the root `/`.
    pub fn is_root(&self) -> bool {
        self.inner == "/"
    }

    /// The canonical string form (starts with `/`, no trailing slash).
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    /// Final segment; empty for the root, e.g. `/a/b.txt` -> `b.txt`.
    pub fn name(&self) -> &str {
        self.inner.rsplit_once('/').map_or("", |(_, name)| name)
    }

    /// Parent directory; `None` for the root, `/a` -> `/`, `/a/b` -> `/a`.
    pub fn parent(&self) -> Option<RelPath> {
        if self.is_root() {
            return None;
        }
        let last_slash = self.inner.rfind('/')?;
        // The parent drops the final separator, except for top-level paths
        // whose parent is the root itself (`/a` -> `/`).
        Some(Self {
            inner: if last_slash == 0 {
                "/".to_string()
            } else {
                self.inner[..last_slash].to_string()
            },
        })
    }

    /// Appends a single segment; rejects empty segments and any segment
    /// containing `/`, `\`, NUL, `.` or `..`.
    pub fn join(&self, segment: &str) -> Result<RelPath, PathError> {
        // A joined segment must be a single clean component: the shared
        // segment rules plus no embedded separator.
        if segment.contains('/') || segment.contains('\\') || segment.contains('\0') {
            return Err(invalid(segment));
        }
        if !is_valid_segment(segment) {
            return Err(invalid(segment));
        }
        if self.is_root() {
            Ok(Self {
                inner: format!("/{segment}"),
            })
        } else {
            Ok(Self {
                inner: format!("{}/{segment}", self.inner),
            })
        }
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner)
    }
}

impl FromStr for RelPath {
    type Err = PathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}
