use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::mpsc,
    time::Duration,
};

pub struct Request {
    pub body: Value,
    pub reply: mpsc::SyncSender<Value>,
}
pub struct Server {
    pub receiver: mpsc::Receiver<Request>,
    path: PathBuf,
}
pub fn socket_path() -> PathBuf {
    std::env::var_os("GMUX_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::temp_dir().join(format!("gmux-{}", unsafe { libc::getuid() }))
                })
                .join("gmux.sock")
        })
}
impl Server {
    pub fn start() -> anyhow::Result<Self> {
        let path = socket_path();
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!("gmux is already listening on {}", path.display());
        }
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        if path.exists() {
            use std::os::unix::fs::FileTypeExt;
            anyhow::ensure!(
                std::fs::symlink_metadata(&path)?.file_type().is_socket(),
                "Refusing to replace a non-socket file at {}",
                path.display()
            );
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let (sender, receiver) = mpsc::sync_channel(32);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                // One newline-delimited request per connection, capped at 1 MiB.
                let mut line = String::new();
                let reader = BufReader::new(&stream);
                use std::io::Read;
                if reader.take(1_048_577).read_line(&mut line).is_err() {
                    continue;
                }
                let response = if line.len() > 1_048_576 {
                    serde_json::json!({"error": "Request is too large"})
                } else if let Ok(body) = serde_json::from_str(&line) {
                    let (reply, response) = mpsc::sync_channel(1);
                    if sender.send(Request { body, reply }).is_err() {
                        break;
                    }
                    response
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap_or_else(
                            |_| serde_json::json!({"error": "Application did not respond"}),
                        )
                } else {
                    serde_json::json!({"error": "Invalid JSON request"})
                };
                let _ = writeln!(stream, "{response}");
            }
        });
        Ok(Self { receiver, path })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
pub fn call(request: Value) -> anyhow::Result<Value> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        anyhow::anyhow!(
            "Cannot connect to gmux at {}: {e}. Start the app first.",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(6)))?;
    writeln!(stream, "{request}")?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let value: Value = serde_json::from_str(&response)?;
    if let Some(error) = value.get("error") {
        anyhow::bail!("{}", error.as_str().unwrap_or("Request failed"));
    }
    Ok(value)
}
