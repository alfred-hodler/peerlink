use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use peerlink::{Config, Event, Reactor, StreamConfig};

mod common;
use common::Message;

/// Connects a client to a nonexistent peer and waits for the timeout.
#[test]
fn client_connects_to_nonexistent() {
    let _ = env_logger::builder().is_test(true).try_init();

    let (client_reactor, client_handle) = Reactor::<_, Message, String>::new(Config {
        stream_config: StreamConfig {
            stream_connect_timeout: Duration::from_secs(1),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();

    let _ = client_reactor.run();

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, u16::MAX);

    let _ = client_handle.send(peerlink::Command::Connect(server_addr.to_string()));

    let connected: Event<_, _> = client_handle.receive_blocking().unwrap();
    assert!(matches!(
        connected,
        Event::ConnectedTo { result: Err(err), .. } if err.kind() == ErrorKind::TimedOut
    ));
}
