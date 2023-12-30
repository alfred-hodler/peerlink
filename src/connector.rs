use std::io;
use std::net::ToSocketAddrs;

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
    fn connect(&self, target: &str) -> io::Result<mio::net::TcpStream>;
}

/// Default `Connector` implementation for `mio` that just connects to a target address.
#[derive(Clone)]
pub struct DefaultConnector;

impl Connector for DefaultConnector {
    const CONNECT_IN_BACKGROUND: bool = false;

    fn connect(&self, target: &str) -> io::Result<mio::net::TcpStream> {
        mio::net::TcpStream::connect(target.to_socket_addrs()?.next().unwrap())
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

    fn connect(&self, target: &str) -> io::Result<mio::net::TcpStream> {
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
