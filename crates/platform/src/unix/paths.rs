//! Local home directory discovery, expansion, and display.
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    directories::BaseDirs::new().unwrap().home_dir().to_owned()
}
/// Expand home paths on the machine that will run the terminal.
pub fn expand_home(path: &Path) -> PathBuf {
    if path == Path::new("~") {
        home()
    } else if let Ok(relative) = path.strip_prefix("~") {
        home().join(relative)
    } else {
        path.to_owned()
    }
}
/// Display a local path relative to this machine's home directory.
pub fn display_path(path: &Path) -> String {
    crate::paths::display_path(path, &home())
}

pub fn directory_name(path: &Path) -> String {
    crate::paths::directory_name(path, &home())
}
