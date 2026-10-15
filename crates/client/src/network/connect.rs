//! Bounded resolution and interruptible Linux TCP connection setup.
use crate::ipc::Cancellation;
use std::{
    io,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    os::fd::FromRawFd,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};

const CHECK_INTERVAL: Duration = Duration::from_millis(25);
type Addresses = io::Result<Vec<SocketAddr>>;
type Lookup = (String, mpsc::SyncSender<Addresses>);

pub(super) fn resolve(
    address: &str,
    deadline: Instant,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<Vec<SocketAddr>> {
    if let Ok(address) = address.parse() {
        return Ok(vec![address]);
    }
    // libc's resolver cannot be interrupted. Bound its workers and queue so
    // cancellation never joins a resolver or creates unlimited blocked threads.
    static LOOKUPS: OnceLock<mpsc::SyncSender<Lookup>> = OnceLock::new();
    let lookups = LOOKUPS.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<Lookup>(32);
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..2 {
            let receiver = receiver.clone();
            std::thread::spawn(move || {
                loop {
                    let Ok((address, reply)) = receiver.lock().unwrap().recv() else {
                        break;
                    };
                    let result = address
                        .to_socket_addrs()
                        .map(|addresses| addresses.take(8).collect());
                    let _ = reply.send(result);
                }
            });
        }
        sender
    });
    let (sender, receiver) = mpsc::sync_channel(1);
    lookups
        .try_send((address.to_owned(), sender))
        .map_err(|_| anyhow::anyhow!("Name resolution queue is full"))?;
    wait_for_lookup(receiver, deadline, cancellation)
}

fn wait_for_lookup(
    receiver: mpsc::Receiver<Addresses>,
    deadline: Instant,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<Vec<SocketAddr>> {
    loop {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        let wait = super::remaining(deadline)?.min(CHECK_INTERVAL);
        match receiver.recv_timeout(wait) {
            Ok(addresses) => return Ok(addresses?),
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(error) => return Err(error.into()),
        }
    }
}

pub(super) fn connect(
    address: SocketAddr,
    deadline: Instant,
    cancellation: Option<&Cancellation>,
) -> anyhow::Result<TcpStream> {
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    let family = if address.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let fd = unsafe {
        libc::socket(
            family,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    let _active = cancellation.map(|c| c.register(&stream)).transpose()?;
    let result = match address {
        SocketAddr::V4(address) => {
            let address = libc::sockaddr_in {
                sin_family: libc::AF_INET as _,
                sin_port: address.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(address.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                libc::connect(
                    fd,
                    (&address as *const libc::sockaddr_in).cast(),
                    std::mem::size_of_val(&address) as _,
                )
            }
        }
        SocketAddr::V6(address) => {
            let address = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as _,
                sin6_port: address.port().to_be(),
                sin6_flowinfo: address.flowinfo(),
                sin6_scope_id: address.scope_id(),
                sin6_addr: libc::in6_addr {
                    s6_addr: address.ip().octets(),
                },
            };
            unsafe {
                libc::connect(
                    fd,
                    (&address as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of_val(&address) as _,
                )
            }
        }
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error.into());
        }
        loop {
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            let timeout = super::remaining(deadline)?
                .min(CHECK_INTERVAL)
                .as_millis()
                .max(1) as i32;
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            match unsafe { libc::poll(&mut poll, 1, timeout) } {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error().into()),
                0 => continue,
                _ => {
                    if let Some(cancellation) = cancellation {
                        cancellation.check()?;
                    }
                    if let Some(error) = stream.take_error()? {
                        return Err(error.into());
                    }
                    break;
                }
            }
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, os::fd::AsRawFd};

    #[test]
    fn resolution_and_connection_support_ipv4_and_ipv6() {
        use std::io::{Read, Write};
        for address in ["127.0.0.1:0", "[::1]:0"] {
            let listener = match TcpListener::bind(address) {
                Ok(listener) => listener,
                Err(error)
                    if address.starts_with('[')
                        && matches!(
                            error.raw_os_error(),
                            Some(libc::EAFNOSUPPORT | libc::EADDRNOTAVAIL | libc::EPROTONOSUPPORT)
                        ) =>
                {
                    continue;
                }
                Err(error) => panic!("{address}: {error}"),
            };
            let address = listener.local_addr().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let addresses =
                resolve(&format!("localhost:{}", address.port()), deadline, None).unwrap();
            assert!(!addresses.is_empty());
            assert!(
                addresses
                    .iter()
                    .all(|resolved| resolved.port() == address.port())
            );
            let mut client = connect(address, deadline, None).unwrap();
            let (mut peer, _) = listener.accept().unwrap();
            peer.write_all(b"connected").unwrap();
            let mut bytes = [0u8; 9];
            client.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"connected");
        }
    }

    #[test]
    fn cancellation_interrupts_a_pending_connection_and_resolution() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let cancellation = Cancellation::default();
        let calls = cancellation.clone();
        let worker = std::thread::spawn(move || {
            connect(
                address,
                Instant::now() + Duration::from_secs(5),
                Some(&calls),
            )
        });
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        cancellation.cancel();
        assert!(worker.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(1));

        let (_lookup, receiver) = mpsc::sync_channel(1);
        let cancellation = Cancellation::default();
        let calls = cancellation.clone();
        let worker = std::thread::spawn(move || {
            wait_for_lookup(
                receiver,
                Instant::now() + Duration::from_secs(5),
                Some(&calls),
            )
        });
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        cancellation.cancel();
        assert!(worker.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
