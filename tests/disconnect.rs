use std::net::{Ipv4Addr, SocketAddrV4};

use peerlink::reactor::DisconnectReason;
use peerlink::{Command, Config, Event, PeerId, Reactor};

mod common;
use common::Message;

// These tests are designed to perform client and server disconnects in a variety of manners:
// orderly, disorderly, and abrupt. In each instance, the remaining party must notice the
// disconnect immediately and without fail.

#[test]
fn client_orderly_disconnect() {
    shutdown_test(8000, Command::Disconnect(PeerId(0)), true);
}

#[test]
fn client_shutdown_leave() {
    shutdown_test(8001, Command::Shutdown, true);
}

#[test]
fn client_abrupt_leave() {
    shutdown_test(8002, Command::Panic, true);
}

#[test]
fn server_orderly_disconnect() {
    shutdown_test(8003, Command::Disconnect(PeerId(0)), false);
}

#[test]
fn server_shutdown_leave() {
    shutdown_test(8004, Command::Shutdown, false);
}

#[test]
fn server_abrupt_leave() {
    shutdown_test(8005, Command::Panic, false);
}

fn shutdown_test(port: u16, shutdown_command: Command<Message, String>, client_is_leaving: bool) {
    let _ = env_logger::builder().is_test(true).try_init();

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();

    let config = Config {
        bind_addr: vec![server_addr],
        ..Default::default()
    };

    let (server_reactor, server_handle) = Reactor::new(config).unwrap();
    let (client_reactor, client_handle) = Reactor::new(Config::default()).unwrap();

    let _ = server_reactor.run();
    let _ = client_reactor.run();

    // We are using this because there is a split-second moment between the server binding to a local
    // socket and registering it for polling where it can miss notifications. This will never
    // happen in practice but it is essentially a race condition that can deadlock a test if the
    // running machine is fast enough.
    std::thread::sleep(std::time::Duration::from_millis(10));

    client_handle
        .send(Command::Connect(server_addr.to_string()))
        .unwrap();

    assert!(matches!(
        client_handle.receive_blocking().unwrap(),
        Event::ConnectedTo { .. }
    ));

    assert!(matches!(
        server_handle.receive_blocking().unwrap(),
        Event::ConnectedFrom { .. }
    ));

    let (leaving, remaining) = if client_is_leaving {
        (client_handle, server_handle)
    } else {
        (server_handle, client_handle)
    };

    leaving.send(shutdown_command).unwrap();

    assert!(matches!(
        remaining.receive_blocking().unwrap(),
        Event::Disconnected {
            reason: DisconnectReason::Left,
            ..
        }
    ));
}
