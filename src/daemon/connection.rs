//! Nonblocking frontend transports. Engines and output accounting stay with the owner.
use super::network::Connection;
use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    sync::Arc,
};

pub(super) enum Socket {
    Local(UnixStream),
    Remote(Arc<Connection>),
}
impl From<UnixStream> for Socket {
    fn from(socket: UnixStream) -> Self {
        Self::Local(socket)
    }
}
impl Socket {
    pub(super) fn cancelled(&self) -> bool {
        matches!(self, Self::Remote(connection) if connection.cancelled())
    }
}
impl AsRawFd for Socket {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Local(socket) => socket.as_raw_fd(),
            Self::Remote(connection) => connection.socket.as_raw_fd(),
        }
    }
}
impl Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Local(socket) => socket.read(bytes),
            Self::Remote(connection) => (&connection.socket).read(bytes),
        }
    }
}
impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Local(socket) => socket.write(bytes),
            Self::Remote(connection) => (&connection.socket).write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
