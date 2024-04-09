use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};

/// A connect target that can be either a socket address or a resolvable domain name.
pub enum Target {
    /// The target is a socket.
    Socket(SocketAddr),
    /// The target is a fully qualified domain, along with a port.
    Domain(String, u16),
}

/// Describes a type that can be converted into a connect target (`Target`).
pub trait IntoTarget: Sync + Send + std::fmt::Debug + Clone + 'static {
    /// Returns `Some` if the conversion was successful, `None` otherwise.
    fn target(&self) -> Option<Target>;
}

impl IntoTarget for String {
    fn target(&self) -> Option<Target> {
        if let Ok(socket) = self.parse::<SocketAddrV4>() {
            return Some(Target::Socket(socket.into()));
        }

        if let Ok(socket) = self.parse::<SocketAddrV6>() {
            return Some(Target::Socket(socket.into()));
        }

        let (domain, port) = self.trim().split_once(':')?;

        let port: u16 = port.parse().ok()?;

        Some(Target::Domain(domain.to_owned(), port))
    }
}

impl IntoTarget for SocketAddr {
    fn target(&self) -> Option<Target> {
        Some(Target::Socket(*self))
    }
}

impl IntoTarget for SocketAddrV4 {
    fn target(&self) -> Option<Target> {
        Some(Target::Socket(self.to_owned().into()))
    }
}

impl IntoTarget for SocketAddrV6 {
    fn target(&self) -> Option<Target> {
        Some(Target::Socket(self.to_owned().into()))
    }
}

/// Types implementing this trait can connect to a target address in a custom manner before
/// returning a `mio::net::TcpStream`. This can be used for proxying and other custom scenarios.
/// It is the responsibility of the caller to put the stream into nonblocking mode. Failing
/// to do so will block the reactor indefinitely and render it inoperable.
pub trait Connector: Clone + Send + Sync + 'static {
    /// Sometimes it is not possible to connect in a non-blocking manner, such as when using 3rd
    /// party libraries. If anything in the connect logic blocks, this must be set to `true`. In
    /// that case connects are performed on a dedicated thread in order to not block the reactor.
    /// Setting this to `false` when it should be `true` will interfere with the operation of the
    /// reactor.
    const CONNECT_IN_BACKGROUND: bool;

    /// Connect to a target address and return a `mio` TCP stream.
    fn connect(&self, target: &impl IntoTarget) -> io::Result<mio::net::TcpStream>;
}

/// Default `Connector` implementation for `mio` that just connects to a target address.
#[derive(Clone)]
pub struct DefaultConnector;

impl Connector for DefaultConnector {
    const CONNECT_IN_BACKGROUND: bool = false;

    fn connect(&self, target: &impl IntoTarget) -> io::Result<mio::net::TcpStream> {
        let target = target.target().ok_or(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a target address",
        ))?;

        let socket_addr = match target {
            Target::Socket(socket) => socket,
            Target::Domain(domain, port) => {
                (domain, port)
                    .to_socket_addrs()?
                    .next()
                    .ok_or(io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        "the target does not resolve to a socket address",
                    ))?
            }
        };

        mio::net::TcpStream::connect(socket_addr)
    }
}

/// Connector that connects through a socks5 proxy.
#[cfg(feature = "socks")]
#[derive(Clone)]
pub struct Socks5Connector {
    /// The socket address of the proxy.
    pub proxy: std::net::SocketAddr,
    /// Optional socks username and password.
    pub credentials: Option<(String, String)>,
}

#[cfg(feature = "socks")]
impl Connector for Socks5Connector {
    const CONNECT_IN_BACKGROUND: bool = true;

    fn connect(&self, target: &impl IntoTarget) -> io::Result<mio::net::TcpStream> {
        let target = target.target().ok_or(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a target address",
        ))?;

        let stream = match self.credentials.as_ref() {
            Some((username, password)) => {
                socks::Socks5Stream::connect_with_password(self.proxy, target, username, password)?
            }
            None => socks::Socks5Stream::connect(self.proxy, target)?,
        }
        .into_inner();

        use std::time::Duration;
        let try_deadline = Duration::from_millis(5000);
        let mut elapsed = Duration::ZERO;
        let mut try_cycle_duration = Duration::from_millis(1);

        loop {
            match stream.set_nonblocking(true) {
                Ok(()) => break Ok(mio::net::TcpStream::from_std(stream)),

                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock && elapsed < try_deadline =>
                {
                    try_cycle_duration =
                        (try_cycle_duration * 2).clamp(Duration::ZERO, Duration::from_millis(1000));
                    std::thread::sleep(try_cycle_duration);
                    elapsed += try_cycle_duration;
                }

                Err(err) => break Err(err),
            }
        }
    }
}

#[cfg(feature = "socks")]
impl socks::ToTargetAddr for Target {
    fn to_target_addr(&self) -> io::Result<socks::TargetAddr> {
        match self {
            Target::Socket(socket) => Ok(socks::TargetAddr::Ip(*socket)),
            Target::Domain(domain, port) => (domain.as_str(), *port).to_target_addr(),
        }
    }
}
