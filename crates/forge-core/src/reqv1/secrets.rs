//! Shared loading for the project-local, gitignored `.env.local` provider.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

pub fn load_file_secrets(root: &Path) -> BTreeMap<String, String> {
    let path = root.join(".env.local");
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return BTreeMap::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            (!key.is_empty()).then(|| {
                let value = value.trim();
                let value = serde_json::from_str::<String>(value).unwrap_or_else(|_| {
                    value
                        .strip_prefix('\'')
                        .and_then(|value| value.strip_suffix('\''))
                        .unwrap_or(value)
                        .to_string()
                });
                (key.to_string(), value)
            })
        })
        .collect()
}

/// Merge imported or edited values into the gitignored project secret file.
/// Values are JSON-quoted, written atomically, and restricted to owner access
/// on Unix. Existing secret keys are preserved.
pub fn save_file_secrets(root: &Path, updates: &BTreeMap<String, String>) -> Result<(), String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve project root: {error}"))?;
    let marker = root.join("project.json");
    let marker_metadata = std::fs::symlink_metadata(&marker)
        .map_err(|error| format!("cannot inspect project.json: {error}"))?;
    if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
        return Err("project.json must be a regular file".to_string());
    }
    let _lock = super::project_lock(&root)?;
    let path = root.join(".env.local");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err("refusing symbolic link .env.local".to_string());
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(".env.local must be a regular file".to_string());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot inspect .env.local: {error}")),
    }
    let mut secrets = load_file_secrets(&root);
    for (name, value) in updates {
        if name.trim().is_empty()
            || name.trim() != name
            || name
                .chars()
                .any(|character| matches!(character, '\n' | '\r' | '='))
        {
            return Err(format!(
                "secret name {name:?} is not a valid .env.local key"
            ));
        }
        secrets.insert(name.clone(), value.clone());
    }
    crate::store::ensure_gitignore(&root).map_err(|error| error.to_string())?;
    let mut content = String::new();
    for (name, value) in secrets {
        let encoded = serde_json::to_string(&value).map_err(|error| error.to_string())?;
        content.push_str(&name);
        content.push('=');
        content.push_str(&encoded);
        content.push('\n');
    }
    let mut staged = tempfile::NamedTempFile::new_in(&root)
        .map_err(|error| format!("cannot stage .env.local: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot secure staged secrets: {error}"))?;
    }
    staged
        .write_all(content.as_bytes())
        .map_err(|error| format!("cannot write staged secrets: {error}"))?;
    staged
        .as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync staged secrets: {error}"))?;
    staged
        .persist(&path)
        .map_err(|error| format!("cannot replace .env.local: {}", error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_names_and_values_without_comments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env.local"),
            "# local\nTOKEN=\"secret\"\n EMPTY = ''\nBROKEN\n",
        )
        .unwrap();

        let secrets = load_file_secrets(dir.path());

        assert_eq!(secrets.get("TOKEN").map(String::as_str), Some("secret"));
        assert_eq!(secrets.get("EMPTY").map(String::as_str), Some(""));
        assert!(!secrets.contains_key("BROKEN"));
    }

    #[test]
    fn loads_json_quoted_secret_values_losslessly() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env.local"),
            "TOKEN=\" leading \\\"quoted\\\" value \\\\ path \"\n",
        )
        .unwrap();

        let secrets = load_file_secrets(dir.path());

        assert_eq!(
            secrets.get("TOKEN").map(String::as_str),
            Some(" leading \"quoted\" value \\ path ")
        );
    }

    #[test]
    fn saves_secrets_atomically_without_losing_existing_values() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
        std::fs::write(dir.path().join(".env.local"), "OLD=\"old value\"\n").unwrap();
        let updates = BTreeMap::from([("TOKEN".to_string(), " leading = value #1 ".to_string())]);

        save_file_secrets(dir.path(), &updates).unwrap();

        let secrets = load_file_secrets(dir.path());
        assert_eq!(secrets.get("OLD").map(String::as_str), Some("old value"));
        assert_eq!(
            secrets.get("TOKEN").map(String::as_str),
            Some(" leading = value #1 ")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join(".env.local"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn secret_loader_and_writer_refuse_symlinked_secret_store() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.json"), r#"{"formatVersion":1}"#).unwrap();
        std::fs::write(outside.path().join("secrets"), "TOKEN=outside\n").unwrap();
        symlink(
            outside.path().join("secrets"),
            dir.path().join(".env.local"),
        )
        .unwrap();

        assert!(load_file_secrets(dir.path()).is_empty());
        assert!(save_file_secrets(
            dir.path(),
            &BTreeMap::from([("TOKEN".to_string(), "replacement".to_string())])
        )
        .is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secrets")).unwrap(),
            "TOKEN=outside\n"
        );
    }
}
