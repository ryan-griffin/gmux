//! Path formatting with an explicit home directory, without local-machine queries.
use std::path::Path;

pub fn display_path(path: &Path, home: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(home) {
        if relative.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", relative.display())
        }
    } else {
        path.display().to_string()
    }
}

pub fn directory_name(path: &Path, home: &Path) -> String {
    if path == home {
        return "~".into();
    }
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}
