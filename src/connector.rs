use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::{io, net};

/// A connect target that can be either a socket address or a resolvable domain name.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Target {
    /// The target is a socket.
    Socket(SocketAddr),
    /// The target is a fully qualified domain, along with a port.
    Domain(String, u16),
}

impl From<SocketAddr> for Target {
    fn from(value: SocketAddr) -> Self {
        Self::Socket(value)
    }
}

impl From<SocketAddrV4> for Target {
    fn from(value: SocketAddrV4) -> Self {
        Self::Socket(value.into())
    }
}

impl From<SocketAddrV6> for Target {
    fn from(value: SocketAddrV6) -> Self {
        Self::Socket(value.into())
    }
}

impl From<(net::Ipv4Addr, u16)> for Target {
    fn from(value: (net::Ipv4Addr, u16)) -> Self {
        Self::Socket(value.into())
    }
}

impl From<(net::Ipv6Addr, u16)> for Target {
    fn from(value: (net::Ipv6Addr, u16)) -> Self {
        Self::Socket(value.into())
    }
}

impl From<(&str, u16)> for Target {
    fn from((domain, port): (&str, u16)) -> Self {
        Self::Domain(domain.to_owned(), port)
    }
}

impl From<(String, u16)> for Target {
    fn from((domain, port): (String, u16)) -> Self {
        Self::Domain(domain, port)
    }
}

impl std::str::FromStr for Target {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Ok(addr) = value.parse::<SocketAddr>() {
            Ok(Self::Socket(addr))
        } else {
            let (domain, port) = value.split_once(':').ok_or(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a target address",
            ))?;

            let port: u16 = port
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not a valid port"))?;
            Ok(Self::Domain(domain.to_owned(), port))
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Socket(socket_addr) => socket_addr.fmt(f),
            Target::Domain(domain, port) => write!(f, "{domain}:{port}"),
        }
    }
}

/// Types implementing this trait can connect to a target address in a custom manner before
/// returning a [`mio::net::TcpStream`]. This can be used for proxying and other custom scenarios.
/// It is the responsibility of the caller to put the stream into nonblocking mode. Failing
/// to do so will block the reactor indefinitely and render it inoperable.
pub trait Connector: Clone + Send + Sync + 'static {
    /// Sometimes it is not possible to connect in a non-blocking manner, such as when using 3rd
    /// party libraries. If anything in the connect logic blocks, this must be set to `true`. In
    /// that case connects are performed on a dedicated thread in order to not block the reactor.
    /// Setting this to `false` when it should be `true` will interfere with the operation of the
    /// reactor.
    const CONNECT_IN_BACKGROUND: bool;

    /// Connect to a target address and return a [`mio`] TCP stream.
    fn connect(&self, target: &Target) -> io::Result<mio::net::TcpStream>;
}

/// Default [`Connector`] implementation for [`mio`] that just connects to a target address.
#[derive(Clone)]
pub struct DefaultConnector;

impl Connector for DefaultConnector {
    const CONNECT_IN_BACKGROUND: bool = false;

    fn connect(&self, target: &Target) -> io::Result<mio::net::TcpStream> {
        let socket_addr = match target {
            Target::Socket(socket) => *socket,
            Target::Domain(domain, port) => (domain.as_str(), *port)
                .to_socket_addrs()?
                .next()
                .ok_or(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "target -> socket address: DNS resolution failure",
                ))?,
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

    fn connect(&self, target: &Target) -> io::Result<mio::net::TcpStream> {
        use socks::ToTargetAddr;
        let target = target
            .to_target_addr()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not a target address"))?;

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
