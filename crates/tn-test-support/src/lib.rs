//! Shared fixture and platform-test support.

use std::path::{Path, PathBuf};

/// Returns all files below `root` with the requested extension in lexical path order.
///
/// # Errors
///
/// Returns an I/O error when a directory entry cannot be read.
pub fn sorted_files(root: &Path, extension: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut files = std::fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|value| value == extension))
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}
