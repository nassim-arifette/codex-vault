use crate::paths::normalized_path;
use std::path::Path;

/// Normalize component boundaries, resolving existing paths but also deleted project paths.
pub fn project_key(path: &Path) -> String {
    let normalized = normalized_path(path);
    let raw = normalized.to_string_lossy().replace('\\', "/");
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "." => {}
            ".." if parts.len() > 1 => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let key = parts.join("/");
    let key = if key.len() > 1 {
        key.trim_end_matches('/').to_string()
    } else {
        key
    };
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

pub fn in_project(candidate: &str, root: &str) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
        || root == "/" && candidate.starts_with('/')
}
