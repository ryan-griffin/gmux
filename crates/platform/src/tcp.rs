//! Deadline-aware TCP I/O using the standard library.
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

pub fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Remote request timed out",
        ));
    }
    Ok(remaining)
}

// Socket timeouts apply to individual system calls. Recompute the remaining
// time so a partial write or a trickling reply cannot extend the operation.
pub struct TimedStream {
    pub socket: TcpStream,
    pub deadline: Option<Instant>,
}
impl Read for TimedStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if let Some(deadline) = self.deadline {
            self.socket.set_read_timeout(Some(remaining(deadline)?))?;
        }
        self.socket.read(bytes)
    }
}
impl Write for TimedStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(deadline) = self.deadline {
            self.socket.set_write_timeout(Some(remaining(deadline)?))?;
        }
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}
