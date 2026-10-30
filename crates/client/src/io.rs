//! Runtime sockets and bounded framing, shared by Unix and TCP clients.
use crate::ipc::Stream;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::either::Either;

pub(crate) enum Socket {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
}
type Reader<'a> = Either<tokio::net::unix::ReadHalf<'a>, tokio::net::tcp::ReadHalf<'a>>;
type Writer<'a> = Either<tokio::net::unix::WriteHalf<'a>, tokio::net::tcp::WriteHalf<'a>>;
impl Socket {
    pub fn split(&mut self) -> (Reader<'_>, Writer<'_>) {
        match self {
            Self::Unix(socket) => {
                let (read, write) = socket.split();
                (Either::Left(read), Either::Left(write))
            }
            Self::Tcp(socket) => {
                let (read, write) = socket.split();
                (Either::Right(read), Either::Right(write))
            }
        }
    }
    pub fn shutdown(&self) {
        use std::os::fd::AsRawFd;
        // Both variants own a live socket for the duration of this call.
        unsafe {
            libc::shutdown(self.as_raw_fd(), libc::SHUT_RDWR);
        }
    }
    pub fn from_std(stream: Stream) -> std::io::Result<Self> {
        Ok(match stream {
            Stream::Unix(s) => {
                s.set_nonblocking(true)?;
                Self::Unix(tokio::net::UnixStream::from_std(s)?)
            }
            Stream::Tcp(s) => {
                s.set_nonblocking(true)?;
                Self::Tcp(tokio::net::TcpStream::from_std(s)?)
            }
        })
    }
    pub fn into_std(self) -> std::io::Result<Stream> {
        let stream = match self {
            Self::Unix(s) => Stream::Unix(s.into_std()?),
            Self::Tcp(s) => Stream::Tcp(s.into_std()?),
        };
        match &stream {
            Stream::Unix(s) => s.set_nonblocking(false)?,
            Stream::Tcp(s) => s.set_nonblocking(false)?,
        };
        Ok(stream)
    }
}
impl std::os::fd::AsRawFd for Socket {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Self::Unix(s) => s.as_raw_fd(),
            Self::Tcp(s) => s.as_raw_fd(),
        }
    }
}
pub(crate) async fn read_json_bytes(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    anyhow::ensure!(
        bytes.len() <= limit && bytes.last() == Some(&b'\n'),
        "Invalid or oversized network message"
    );
    Ok(bytes)
}
pub(crate) async fn read_json(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: usize,
) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::from_slice(
        &read_json_bytes(reader, limit).await?,
    )?)
}
pub(crate) async fn write_json(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &impl serde::Serialize,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}
