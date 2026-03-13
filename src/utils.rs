use std::borrow::Cow;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct SnipTextFmtCtx {
    pub(crate) bytes: usize,
    pub(crate) max_bytes: usize,
}

/// Snips `text` to fit within `max_bytes` by appending a message from `snip_text_fmt`.
///
/// Note: if the formatted snip message produced by `snip_text_fmt` is longer than
/// `max_bytes`, the returned string may exceed `max_bytes` in length.
pub(crate) fn snip_long_text(
    text: Cow<'_, str>,
    max_bytes: usize,
    snip_text_fmt: impl Fn(SnipTextFmtCtx) -> String,
) -> Cow<'_, str> {
    if text.len() <= max_bytes {
        return text;
    }

    let snip_msg = snip_text_fmt(SnipTextFmtCtx {
        bytes: text.len(),
        max_bytes,
    });
    let mut byte_limit = max_bytes.saturating_sub(snip_msg.len());

    while byte_limit > 0 && !text.is_char_boundary(byte_limit) {
        byte_limit -= 1;
    }
    let mut result = String::with_capacity(byte_limit + snip_msg.len());
    result.push_str(&text[..byte_limit]);
    result.push_str(&snip_msg);
    Cow::from(result)
}

#[derive(Debug, Clone)]
pub(crate) struct FilePermissions {
    pub(crate) canonical_root: PathBuf,
}

impl FilePermissions {
    pub(crate) fn new() -> Result<Self, io::Error> {
        Ok(Self {
            canonical_root: std::env::current_dir()?.canonicalize()?,
        })
    }

    /// Validates that a file path is allowed to read.
    ///
    /// - Ensures the path is within the current working directory.
    /// - Rejects symbolic links to prevent symlink-based escapes.
    ///
    /// Returns the canonicalized path if valid, or an error if not.
    pub(crate) fn validate_read(
        &self,
        file_path: impl Into<PathBuf>,
    ) -> Result<PathBuf, io::Error> {
        use std::io::{Error, ErrorKind};

        // Build the requested path relative to the root if necessary.
        let requested = file_path.into();
        let candidate: PathBuf = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.canonical_root.join(requested)
        };

        let canonical_candidate = candidate.canonicalize()?;

        // Ensure the target path is inside the allowed root.
        if !canonical_candidate.starts_with(&self.canonical_root) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "Access to paths outside the workspace is not allowed",
            ));
        }

        // Reject symlinks to avoid symlink-based escapes.
        let metadata = std::fs::symlink_metadata(&canonical_candidate)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "Access to symbolic links is not allowed",
            ));
        }

        Ok(canonical_candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    #[test]
    fn snip_long_text_does_not_snip_short_text() {
        let short_text = "Hello";
        let max_bytes = 5;
        let result = snip_long_text(short_text.into(), max_bytes, |_| String::from("..."));
        assert_snapshot!(result, @"Hello");
    }

    #[test]
    fn snip_long_text_snips_ascii_text() {
        let long_text = "Hello, World!".repeat(3);
        let max_bytes = 35;
        let result = snip_long_text(long_text.into(), max_bytes, |ctx| {
            format!("... bytes: {}, max_bytes: {}", ctx.bytes, ctx.max_bytes)
        });
        assert_snapshot!(result, @"Hello, ... bytes: 39, max_bytes: 35");
    }

    #[test]
    fn snip_long_text_snips_unicode_text() {
        let long_text = "こんにちは、世界！".repeat(3);
        let max_bytes = 32;
        let result = snip_long_text(long_text.into(), max_bytes, |ctx| {
            format!("... bytes: {}, max_bytes: {}", ctx.bytes, ctx.max_bytes)
        });
        assert_snapshot!(result, @"こ... bytes: 81, max_bytes: 32");
    }
}
