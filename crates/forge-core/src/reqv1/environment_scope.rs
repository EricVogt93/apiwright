//! Git-friendly environment defaults for folders and individual requests.

use std::path::{Component, Path, PathBuf};

pub const FOLDER_ENVIRONMENT_FILE: &str = ".forge-environment";
pub const FILE_ENVIRONMENT_SUFFIX: &str = ".forge-environment";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentSelection {
    pub value: String,
    pub source: PathBuf,
}

fn environment_file(node: &Path) -> PathBuf {
    if node.is_dir() {
        node.join(FOLDER_ENVIRONMENT_FILE)
    } else {
        let name = node
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        node.with_file_name(format!(".{name}{FILE_ENVIRONMENT_SUFFIX}"))
    }
}

pub fn own_environment(node: &Path) -> Result<Option<String>, String> {
    let path = environment_file(node);
    if path.is_symlink() {
        return Err(format!(
            "refusing to read environment selection through symbolic link {}",
            path.display()
        ));
    }
    match std::fs::read_to_string(path) {
        Ok(value) => Ok((!value.trim().is_empty()).then(|| value.trim().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot read environment selection for {}: {error}",
            node.display()
        )),
    }
}

pub fn effective_environment(
    root: &Path,
    node: &Path,
) -> Result<Option<EnvironmentSelection>, String> {
    if !node.starts_with(root) {
        return Err(format!("{} is outside the project", node.display()));
    }
    let mut current = Some(node);
    while let Some(candidate) = current {
        if let Some(value) = own_environment(candidate)? {
            return Ok(Some(EnvironmentSelection {
                value,
                source: candidate.to_path_buf(),
            }));
        }
        if candidate == root {
            break;
        }
        current = candidate.parent();
    }
    Ok(None)
}

pub fn set_environment(node: &Path, value: &str) -> Result<(), String> {
    let value = validate_environment_name(value)?;
    let path = environment_file(node);
    if path.is_symlink() {
        return Err(format!(
            "refusing to write environment selection through symbolic link {}",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    std::fs::write(&path, format!("{value}\n"))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

pub(super) fn validate_environment_name(value: &str) -> Result<&str, String> {
    let value = value.trim();
    let mut components = Path::new(value).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("environment must be a single name".to_string());
    }
    Ok(value)
}

pub fn remove_environment(node: &Path) -> Result<(), String> {
    let path = environment_file(node);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_override_can_fall_back_to_nearest_folder() {
        let root = tempfile::tempdir().unwrap();
        let story = root.path().join("requests/checkout");
        std::fs::create_dir_all(&story).unwrap();
        let request = story.join("submit.request.json");
        std::fs::write(&request, "{}").unwrap();

        set_environment(root.path(), "local").unwrap();
        set_environment(&story, "staging").unwrap();
        assert_eq!(
            effective_environment(root.path(), &request)
                .unwrap()
                .unwrap(),
            EnvironmentSelection {
                value: "staging".to_string(),
                source: story.clone(),
            }
        );

        set_environment(&request, "production").unwrap();
        assert_eq!(
            effective_environment(root.path(), &request)
                .unwrap()
                .unwrap()
                .value,
            "production"
        );

        remove_environment(&request).unwrap();
        assert_eq!(
            effective_environment(root.path(), &request)
                .unwrap()
                .unwrap()
                .value,
            "staging"
        );
    }

    #[test]
    fn environment_names_cannot_escape_the_environment_directory() {
        let root = tempfile::tempdir().unwrap();
        assert!(set_environment(root.path(), "../secret").is_err());
        assert!(set_environment(root.path(), "story/dev").is_err());
        assert!(set_environment(root.path(), "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn environment_selection_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), root.path().join(FOLDER_ENVIRONMENT_FILE)).unwrap();

        assert!(own_environment(root.path())
            .unwrap_err()
            .contains("symbolic link"));
        assert!(set_environment(root.path(), "local")
            .unwrap_err()
            .contains("symbolic link"));
    }

    #[cfg(unix)]
    #[test]
    fn environment_directory_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("external.json"), "{}").unwrap();
        symlink(outside.path(), root.path().join("environments")).unwrap();

        assert!(
            crate::reqv1::load_environment(root.path(), Some("external"))
                .unwrap_err()
                .message
                .contains("symbolic link")
        );
    }
}
