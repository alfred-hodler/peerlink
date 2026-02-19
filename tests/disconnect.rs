use std::net::Ipv4Addr;

use peerlink::{Command, Config, DisconnectReason, Event};

mod common;
use common::Message;

// These tests are designed to perform client and server disconnects in a variety of manners:
// orderly, disorderly, and abrupt. In each instance, the remaining party must notice the
// disconnect immediately and without fail.

enum LeaveType {
    Disconnect,
    Shutdown,
    Abrupt,
}

#[test]
fn client_orderly_disconnect() {
    shutdown_test(8000, LeaveType::Disconnect, true);
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
    shutdown_test(8003, LeaveType::Disconnect, false);
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

    let connected_to_server = client_handle.recv_blocking().unwrap();
    let server_id = match connected_to_server {
        Event::ConnectedTo {
            result: Ok(peer), ..
        } => peer,
        _ => panic!("unexpected event"),
    };

    let connected_from_client = server_handle.recv_blocking().unwrap();
    let client_id = match connected_from_client {
        Event::ConnectedFrom { peer, .. } => peer,
        _ => panic!("unexpected event"),
    };

    let (leaving, peer_id_to_disconnect, remaining) = if client_is_leaving {
        (client_handle, server_id, server_handle)
    } else {
        (server_handle, client_id, client_handle)
    };

    match shutdown_command {
        LeaveType::Disconnect => {
            leaving
                .send(Command::Disconnect(peer_id_to_disconnect))
                .unwrap();
        }
        LeaveType::Shutdown => {
            leaving.shutdown().1.unwrap();
        }
        LeaveType::Abrupt => drop(leaving),
    }

    assert!(matches!(
        remaining.recv_blocking().unwrap(),
        Event::Disconnected {
            reason: DisconnectReason::Left,
            ..
        }
    ));
}
