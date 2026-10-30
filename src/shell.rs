//! Locate packaged hooks and reproduce Ghostty's supported injection startup.
use portable_pty::CommandBuilder;
use std::path::{Path, PathBuf};

fn resources() -> Option<PathBuf> {
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("share/gmux")));
    [
        std::env::var_os("GMUX_RESOURCES_DIR").map(PathBuf::from),
        installed,
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data")),
    ]
    .into_iter()
    .flatten()
    .find(|dir| {
        dir.join("shell-integration/bash/ghostty.bash").is_file()
            && dir.join("shell-integration/zsh/.zshenv").is_file()
    })
    // Shells start in the pane's directory, which may differ from the daemon's.
    .and_then(|dir| dir.canonicalize().ok())
}

pub(crate) fn configure(command: &mut CommandBuilder, shell: &Path) {
    let Some(resources) = resources() else { return };
    command.env("GMUX_RESOURCES_DIR", &resources);
    if std::env::var_os("GMUX_SHELL_INTEGRATION").is_some_and(|v| v == "0") {
        return;
    }
    match shell.file_name().and_then(|name| name.to_str()) {
        Some("bash") => {
            // POSIX ENV runs instead of Bash's normal startup. The pinned
            // script disables POSIX mode and sources login startup files.
            command.arg("--posix");
            if let Some(value) = std::env::var_os("ENV") {
                command.env("GHOSTTY_BASH_ENV", value);
            } else {
                command.env_remove("GHOSTTY_BASH_ENV");
            }
            command.env("ENV", resources.join("shell-integration/bash/ghostty.bash"));
            command.env("GHOSTTY_BASH_INJECT", "1");
            command.env_remove("GHOSTTY_BASH_RCFILE");
            command.env_remove("GHOSTTY_BASH_UNEXPORT_HISTFILE");
            if std::env::var_os("HISTFILE").is_none() {
                command.env(
                    "HISTFILE",
                    gmux_platform::unix::paths::home().join(".bash_history"),
                );
                command.env("GHOSTTY_BASH_UNEXPORT_HISTFILE", "1");
            }
        }
        Some("zsh") => {
            if let Some(value) = std::env::var_os("ZDOTDIR") {
                command.env("GHOSTTY_ZSH_ZDOTDIR", value);
            } else {
                command.env_remove("GHOSTTY_ZSH_ZDOTDIR");
            }
            command.env("ZDOTDIR", resources.join("shell-integration/zsh"));
        }
        _ => return,
    }
    // Optional Ghostty features invoke its executable or change user styling.
    // Gmux uses only semantic marking and working-directory reports.
    command.env("GHOSTTY_SHELL_FEATURES", "");
}
