//! Linux socket liveness settings and token-file loading.
use anyhow::Context;
use std::{io::Read, os::fd::AsRawFd};
/// Detect broken paths even on idle terminal streams, without userspace timers.
/// TCP_USER_TIMEOUT also bounds unacknowledged writes and zero-window stalls.
pub fn configure_liveness(stream: &impl AsRawFd) -> std::io::Result<()> {
    for (level, option, value) in [
        (libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1i32),
        (libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 15),
        (libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5),
        (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3),
        (libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 30_000),
    ] {
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                level,
                option,
                (&value as *const i32).cast(),
                std::mem::size_of_val(&value) as _,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn token(
    authentication: &gmux_protocol::network::Authentication,
) -> anyhow::Result<Option<String>> {
    match authentication {
        gmux_protocol::network::Authentication::None => Ok(None),
        gmux_protocol::network::Authentication::Token { file } => {
            use std::os::unix::fs::OpenOptionsExt;
            // Open before checking the type so replacing the path cannot
            // make a FIFO block setup or cancellation. Symlinks to regular
            // token files remain valid.
            let input = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(file)
                .with_context(|| format!("Could not read token file {}", file.display()))?;
            anyhow::ensure!(
                input.metadata()?.is_file(),
                "Token file must be a regular file"
            );
            let mut token = String::new();
            input.take(1027).read_to_string(&mut token)?;
            anyhow::ensure!(token.len() <= 1026, "Token file is too large");
            let token = token.trim_end_matches(['\r', '\n']);
            anyhow::ensure!(
                (32..=1024).contains(&token.len()) && !token.chars().any(char::is_whitespace),
                "Token file must contain 32 to 1024 bytes without whitespace"
            );
            Ok(Some(token.to_owned()))
        }
    }
}
#[cfg(test)]
mod tests {
    use gmux_protocol::network::*;
    use std::{io::Write, time::Duration};
    #[test]
    fn token_files_follow_regular_symlinks_and_reject_fifos_without_waiting() {
        use std::{os::unix::fs::OpenOptionsExt, sync::mpsc};
        let directory = std::env::temp_dir().join(format!(
            "gmux-token-file-{}",
            crate::unix::identity::daemon_id().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("token");
        let token = "0123456789abcdef0123456789abcdef";
        std::fs::write(&path, format!("{token}\n")).unwrap();
        let link = directory.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        for file in [path, link] {
            assert_eq!(
                super::token(&Authentication::Token { file })
                    .unwrap()
                    .as_deref(),
                Some(token)
            );
        }
        let fifo = directory.join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = mpsc::channel();
        let file = fifo.clone();
        let worker = std::thread::spawn(move || {
            sender
                .send(super::token(&Authentication::Token { file }))
                .unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_secs(1));
        if result.is_err() {
            // Release a regressed blocking read before failing the test.
            let mut writer = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(fifo)
                .unwrap();
            writer.write_all(token.as_bytes()).unwrap();
        }
        worker.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        let error = result
            .expect("Token loading waited for a FIFO writer")
            .unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }
}
