//! File-based, git-friendly persistence.
//!
//! A workspace is a plain directory tree (see the repository README for the
//! layout). Identity is the file/directory name; `order` arrays in
//! collection/folder metadata control tree ordering, with entries missing
//! from the list appended alphabetically so that git merges stay trivial.

mod ops;
mod variables;
mod workspace;

pub use ops::*;
pub use variables::*;
pub use workspace::*;

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: invalid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0} is not a ApiWright workspace (missing forge.json)")]
    NotAWorkspace(PathBuf),
    #[error("{path}: unsupported format version {found} (this build supports up to {supported})")]
    UnsupportedFormat {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("{0} already exists")]
    AlreadyExists(PathBuf),
    #[error(
        "invalid name {0:?}: names must not be empty, contain path separators, or start with a dot"
    )]
    InvalidName(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

pub(crate) fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Read + parse a JSON file.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> StoreResult<T> {
    let text = std::fs::read_to_string(path).map_err(io_err(path))?;
    serde_json::from_str(&text).map_err(|source| StoreError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Serialize as pretty-printed 2-space JSON with a trailing newline —
/// the canonical on-disk representation (stable diffs).
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> StoreResult<()> {
    let mut text = serde_json::to_string_pretty(value).map_err(|source| StoreError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    text.push('\n');
    atomic_write(path, |file| file.write_all(text.as_bytes()))
}

fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> StoreResult<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io_err(parent))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(io_err(path))?;
    match std::fs::metadata(path) {
        Ok(metadata) => staged
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(io_err(path))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_err(path)(error)),
    }
    write(staged.as_file_mut()).map_err(io_err(path))?;
    staged.as_file().sync_all().map_err(io_err(path))?;
    staged
        .persist(path)
        .map_err(|error| io_err(path)(error.error))?;
    Ok(())
}

/// Validate a user-supplied file/folder name.
pub fn validate_name(name: &str) -> StoreResult<()> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) || name.contains("..")
    {
        return Err(StoreError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// Turn a display name into a filesystem-safe slug (`Create Charge` → `create-charge`).
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true;
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-').to_string();
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod atomic_tests {
    use super::*;

    #[test]
    fn write_failure_preserves_previous_bytes_and_removes_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("request.json");
        std::fs::write(&path, b"previous bytes\n").unwrap();
        let result = atomic_write(&path, |file| {
            file.write_all(b"partial new bytes")?;
            Err(std::io::Error::other("injected disk write failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"previous bytes\n");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        save_json(&path, &serde_json::json!({"saved": true})).unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "{\n  \"saved\": true\n}\n"
        );
    }
}
