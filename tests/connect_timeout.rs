use std::io::ErrorKind;
use std::net::Ipv4Addr;

use peerlink::{Config, Event};

mod common;
use common::Message;

/// Connects a client to a nonexistent peer and waits for the timeout.
#[test]
fn client_connects_to_nonexistent() {
    let _ = env_logger::builder().is_test(true).try_init();

    let client_handle = peerlink::run::<Message>(Config::default()).unwrap();

    let server_addr = (Ipv4Addr::LOCALHOST, u16::MAX);

    let _ = client_handle.send(peerlink::Command::connect(server_addr));

    let connected: Event<_> = client_handle.recv_blocking().unwrap();
    assert!(matches!(
        connected,
        Event::ConnectedTo { result: Err(err), .. } if err.kind() == ErrorKind::ConnectionRefused
    ));
}
