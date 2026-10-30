use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    started: u64,
    command: String,
}
impl ForegroundProcess {
    pub fn read(pid: u32) -> Option<Self> {
        Self::from_stat(
            pid,
            &std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?,
        )
    }
    fn from_stat(pid: u32, stat: &str) -> Option<Self> {
        // comm can contain spaces and parentheses. Fields after its final ')'
        // start at field 3; starttime is field 22.
        let (prefix, fields) = stat.rsplit_once(')')?;
        let (_, command) = prefix.split_once('(')?;
        Some(Self {
            pid,
            started: fields.split_whitespace().nth(19)?.parse().ok()?,
            command: command.to_owned(),
        })
    }
}

#[derive(Default)]
pub struct PaneTitle {
    foreground: Option<ForegroundProcess>,
    explicit: Option<String>,
}
impl PaneTitle {
    pub fn foreground_pid(&self) -> Option<u32> {
        self.foreground.as_ref().map(|p| p.pid)
    }
    pub fn update(&mut self, foreground: Option<ForegroundProcess>, title: Option<String>) {
        if let Some(foreground) = foreground
            && self.foreground.as_ref() != Some(&foreground)
        {
            // An OSC title belongs to the command that sent it. A new job or
            // an exec must not inherit the previous program's title.
            self.foreground = Some(foreground);
            self.explicit = None;
        }
        if let Some(title) = title {
            self.explicit = (!title.is_empty()).then_some(title);
        }
    }
    pub fn display(&self, directory: &Path, shell_pid: Option<u32>) -> String {
        if let Some(title) = &self.explicit {
            return title.clone();
        }
        if let Some(process) = &self.foreground
            && Some(process.pid) != shell_pid
        {
            return process.command.clone();
        }
        gmux_platform::unix::paths::directory_name(directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, command: &str) -> ForegroundProcess {
        ForegroundProcess {
            pid,
            started: 123,
            command: command.into(),
        }
    }

    #[test]
    fn titles_follow_jobs_and_return_to_the_directory() {
        let directory = Path::new("/tmp/project");
        let mut title = PaneTitle::default();
        title.update(Some(process(1, "bash")), None);
        assert_eq!(title.display(directory, Some(1)), "project");
        title.update(Some(process(2, "opencode")), Some("OpenCode".into()));
        title.update(Some(process(2, "opencode")), None);
        assert_eq!(title.display(directory, Some(1)), "OpenCode");
        title.update(Some(process(3, "btop")), None);
        assert_eq!(title.display(directory, Some(1)), "btop");
        title.update(Some(process(1, "bash")), None);
        assert_eq!(title.display(directory, Some(1)), "project");
        assert_eq!(title.display(Path::new("/tmp/other"), Some(1)), "other");
    }

    #[test]
    fn exec_and_pid_reuse_discard_the_previous_title() {
        let directory = Path::new("/tmp/project");
        let mut title = PaneTitle::default();
        title.update(Some(process(2, "python")), Some("Old title".into()));
        title.update(Some(process(2, "sleep")), None);
        assert_eq!(title.display(directory, Some(1)), "sleep");
        title.update(Some(process(2, "sleep")), Some("Old sleep".into()));
        let mut reused = process(2, "sleep");
        reused.started += 1;
        title.update(Some(reused), None);
        assert_eq!(title.display(directory, Some(1)), "sleep");
    }

    #[test]
    fn empty_titles_restore_the_fallback_and_missing_processes_preserve_titles() {
        let directory = Path::new("/tmp/project");
        let mut title = PaneTitle::default();
        title.update(Some(process(2, "btop")), Some("Statistics".into()));
        title.update(None, None);
        assert_eq!(title.display(directory, Some(1)), "Statistics");
        title.update(None, Some(String::new()));
        assert_eq!(title.display(directory, Some(1)), "btop");
    }

    #[test]
    fn proc_stat_handles_parentheses_in_command_names() {
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[19] = "456";
        let process =
            ForegroundProcess::from_stat(42, &format!("42 (a (job)) {}", fields.join(" ")))
                .unwrap();
        assert_eq!(process.command, "a (job)");
        assert_eq!(process.started, 456);
    }
}
