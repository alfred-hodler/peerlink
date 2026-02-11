use std::net::Ipv4Addr;

use peerlink::{Command, Config, DisconnectReason, Event, PeerId};

mod common;
use common::Message;

// These tests are designed to perform client and server disconnects in a variety of manners:
// orderly, disorderly, and abrupt. In each instance, the remaining party must notice the
// disconnect immediately and without fail.

enum LeaveType {
    Disconnect(PeerId),
    Shutdown,
    Abrupt,
}

#[test]
fn client_orderly_disconnect() {
    shutdown_test(8000, LeaveType::Disconnect(PeerId::set_raw(0)), true);
}

#[test]
fn client_shutdown_leave() {
    shutdown_test(8001, LeaveType::Shutdown, true);
}

#[test]
fn client_abrupt_leave() {
    shutdown_test(8002, LeaveType::Abrupt, true);
}

#[test]
fn server_orderly_disconnect() {
    shutdown_test(8003, LeaveType::Disconnect(PeerId::set_raw(0)), false);
}

#[test]
fn server_shutdown_leave() {
    shutdown_test(8004, LeaveType::Shutdown, false);
}

#[test]
fn server_abrupt_leave() {
    shutdown_test(8005, LeaveType::Abrupt, false);
}

fn shutdown_test(port: u16, shutdown_command: LeaveType, client_is_leaving: bool) {
    let _ = env_logger::builder().is_test(true).try_init();

    let server_addr = (Ipv4Addr::LOCALHOST, port);

    let config = Config {
        bind_addr: vec![server_addr.into()],
        ..Default::default()
    };

    let server_handle = peerlink::run::<Message>(config).unwrap();
    let client_handle = peerlink::run(Config::default()).unwrap();

    // We are using this because there is a split-second moment between the server binding to a local
    // socket and registering it for polling where it can miss notifications. This will never
    // happen in practice but it is essentially a race condition that can deadlock a test if the
    // running machine is fast enough.
    std::thread::sleep(std::time::Duration::from_millis(10));

    client_handle.send(Command::connect(server_addr)).unwrap();

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

    match shutdown_command {
        LeaveType::Disconnect(peer_id) => {
            leaving.send(Command::Disconnect(peer_id)).unwrap();
        }
        LeaveType::Shutdown => {
            leaving.shutdown().unwrap();
        }
        LeaveType::Abrupt => drop(leaving),
    }

    assert!(matches!(
        remaining.receive_blocking().unwrap(),
        Event::Disconnected {
            reason: DisconnectReason::Left,
            ..
        }
    ));
}
