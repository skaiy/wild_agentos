//! Canonical workspace-bound path resolution for local file tools.
//!
//! Resolve every supplied path before use.  This rejects lexical `..` escapes
//! and symlinks that lead outside the workspace; callers must use the returned
//! path rather than the untrusted input.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Resolve `path` against the process workspace and require it to remain there.
pub(crate) fn canonicalize_workspace_path(path: impl AsRef<Path>) -> Result<PathBuf, String> {
    let workspace =
        std::env::current_dir().map_err(|error| format!("Workspace path unavailable: {error}"))?;
    canonicalize_path_within_workspace(path.as_ref(), &workspace)
}

/// Resolve `path` within an explicit workspace. Exposed for deterministic tests.
pub(crate) fn canonicalize_path_within_workspace(
    path: &Path,
    workspace: &Path,
) -> Result<PathBuf, String> {
    let workspace = workspace
        .canonicalize()
        .map_err(|error| format!("Workspace path unavailable: {error}"))?;

    let mut candidate = if path.is_absolute() {
        PathBuf::new()
    } else {
        workspace.clone()
    };
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => candidate.push(prefix.as_os_str()),
            Component::RootDir => candidate.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                candidate.pop();
            }
            Component::Normal(segment) => candidate.push(segment),
        }
    }

    let canonical = canonicalize_existing_prefix(&candidate)
        .map_err(|error| format!("Unable to resolve path {}: {error}", path.display()))?;
    if !canonical.starts_with(&workspace) {
        return Err(format!(
            "Path rejected: {} resolves outside the allowed workspace {}",
            path.display(),
            workspace.display()
        ));
    }
    Ok(canonical)
}

/// Canonicalize the existing portion of a path while preserving a missing leaf.
fn canonicalize_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    let mut suffix: Vec<OsString> = Vec::new();
    loop {
        match current.canonicalize() {
            Ok(canonical) => {
                let mut resolved = canonical;
                for segment in suffix.iter().rev() {
                    resolved.push(segment);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name() else {
                    return Err(error);
                };
                suffix.push(name.to_os_string());
                let Some(parent) = current.parent() else {
                    return Err(error);
                };
                current = parent.to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::canonicalize_path_within_workspace;

    #[test]
    #[cfg(unix)]
    fn rejects_lexical_and_symlink_workspace_escapes() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside")).unwrap();

        for path in ["../escape.txt", "outside/secret.txt"] {
            let error =
                canonicalize_path_within_workspace(path.as_ref(), workspace.path()).unwrap_err();
            assert!(
                error.contains("outside the allowed workspace"),
                "unexpected error for {path}: {error}"
            );
        }
    }

    #[test]
    fn resolves_missing_workspace_descendants() {
        let workspace = tempfile::tempdir().unwrap();
        let path =
            canonicalize_path_within_workspace(Path::new("new/nested/file.txt"), workspace.path())
                .unwrap();
        assert_eq!(path, workspace.path().join("new/nested/file.txt"));
    }

    use std::path::Path;
}
