use core::panic;
use std::net::{Ipv4Addr, SocketAddr};

use peerlink::Handle;
use peerlink::{Command, Config, DisconnectReason, Event, PeerId};

mod common;
use common::Message;

/// Starts one client and one server and performs ping-pongs between them in an interleaved manner.
#[test]
fn interleaved() {
    let Scaffold {
        server,
        client,
        server_addr,
        ..
    } = start_server_client(8100);

    let (client_peer, server_peer) = connect(&client, &server, server_addr);

    for nonce in 0..1000 {
        message(&client, server_peer, Message::Ping(nonce));
        expect_ping(server.recv_blocking().unwrap(), nonce);
        message(&server, client_peer, Message::Pong(nonce));
        expect_pong(client.recv_blocking().unwrap(), nonce);
    }
}

/// Starts one client and one server, rapidly sends a batch of pings from the client to the server,
/// disconnects and only then let the server start reading events.
#[test]
fn stuff_messages() {
    let Scaffold {
        server,
        client,
        server_addr,
        ..
    } = start_server_client(8101);

    let (_client_peer, server_peer) = connect(&client, &server, server_addr);

    for i in 0..1000 {
        message(&client, server_peer, Message::Ping(i));
    }

    client.send(Command::Disconnect(server_peer)).unwrap();

    for i in 0..1000 {
        expect_ping(server.recv_blocking().unwrap(), i);
    }

    match server.recv_blocking().unwrap() {
        peerlink::Event::Disconnected {
            reason: DisconnectReason::Left,
            ..
        } => {}
        event => {
            panic!("did not get disconnect; event: {:?}", event);
        }
    }
}

/// Starts one client and one server, makes them send each other messages in large batches
/// simultaneously and then do bulk reads on both ends.
#[test]
fn batches() {
    let Scaffold {
        server,
        client,
        server_addr,
        ..
    } = start_server_client(8102);

    let (client_peer, server_peer) = connect(&client, &server, server_addr);

    for i in 0..1000 {
        message(&client, server_peer, Message::Ping(i));
        message(&server, client_peer, Message::Ping(i));
    }

    for i in 0..1000 {
        expect_ping(server.recv_blocking().unwrap(), i);
        expect_ping(client.recv_blocking().unwrap(), i);
    }
}

/// Starts one server and several clients, makes the clients simultaneously send pings to the
/// server and lets the server respond with pongs immediately as they come in. Clients must process
/// those pongs immediately, before the loop moves on to the next nonce.
#[test]
fn many_to_one_interleaved() {
    let _ = env_logger::builder().is_test(true).try_init();

    let server_addr = (Ipv4Addr::LOCALHOST, 8103);

    let server_config = Config {
        bind_addr: vec![server_addr.into()],
        ..Default::default()
    };

    let server = peerlink::run(server_config).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(10));

    let n_clients = 5;

    let clients: Vec<_> = (0..n_clients)
        .enumerate()
        .map(|(i, _)| {
            let client = peerlink::run(Config::default()).unwrap();

            let (client_peer, _) = connect(&client, &server, server_addr.into());
            assert_eq!(i as u64, client_peer.inner());

            client
        })
        .collect();

    for nonce in 0..100 {
        for (i, c) in clients.iter().enumerate() {
            message(c, PeerId::set_raw(0), Message::Ping(nonce));
            let ping_from_peer = expect_ping(server.recv_blocking().unwrap(), nonce);
            assert_eq!(ping_from_peer, PeerId::set_raw(i as u64));
            message(&server, ping_from_peer, Message::Pong(nonce));
            expect_pong(c.recv_blocking().unwrap(), nonce);
        }
    }
}

/// Starts one server and several clients, makes the clients simultaneously send pings to the
/// server and makes the server wait until the sending is done. The server then reads all the
/// pings in bulk and responds to each of them with a pong. Clients must verify they are receiving
/// all the pongs.
#[test]
fn many_to_one_bulk() {
    let _ = env_logger::builder().is_test(true).try_init();

    let server_addr = (Ipv4Addr::LOCALHOST, 8104);

    let server_config = Config {
        bind_addr: vec![server_addr.into()],
        ..Default::default()
    };

    let server = peerlink::run(server_config).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(10));

    let n_clients = 5;
    let n_pings = 100;

    let clients: Vec<_> = (0..n_clients)
        .enumerate()
        .map(|(i, _)| {
            let client = peerlink::run(Config::default()).unwrap();

            let (client_peer, _) = connect(&client, &server, server_addr.into());
            assert_eq!(i as u64, client_peer.inner());

            client
        })
        .collect();

    for nonce in 0..n_pings {
        for c in &clients {
            message(c, PeerId::set_raw(0), Message::Ping(nonce));
        }
    }

    // no guarantee that the server is processing every peer sequentially
    for _ in 0..(clients.len() * n_pings as usize) {
        let (peer, nonce) = read_ping(server.recv_blocking().unwrap());
        message(&server, peer, Message::Pong(nonce));
    }

    assert_eq!(server.event_count(), 0);

    for nonce in 0..100 {
        for c in &clients {
            expect_pong(c.recv_blocking().unwrap(), nonce);
        }
    }

    for c in &clients {
        assert_eq!(c.event_count(), 0);
    }
}

#[test]
fn very_large() {
    let Scaffold {
        server,
        client,
        server_addr,
        ..
    } = start_server_client(8105);

    let (client_peer, _) = connect(&client, &server, server_addr);
    let ten_mb = vec![1; 1024 * 1024 * 10];
    let message = Message::Data(ten_mb);

    client.send(Command::Message(client_peer, message)).unwrap();

    match server.recv_blocking().unwrap() {
        Event::Message {
            message: Message::Data(data),
            size,
            ..
        } if data.len() == 1024 * 1024 * 10
            && data.iter().all(|x| *x == 1)
            && size == (1024 * 1024 * 10 + 4 + 4) => {}
        _ => panic!("bad data received"),
    }
}

/// Starts one server and sequentally connects and disconnects clients to it. Verifies that each
/// peer id from the perspective of the server is unique, i.e. that there is no peer id reuse.
#[test]
fn peer_id_increments() {
    let _ = env_logger::builder().is_test(true).try_init();

    let server_addr = (Ipv4Addr::LOCALHOST, 8106);

    let server_config = Config {
        bind_addr: vec![server_addr.into()],
        ..Default::default()
    };

    let server = peerlink::run(server_config).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(10));

    let n_clients = 5;

    for i in 0..n_clients {
        let client = peerlink::run(Config::default()).unwrap();

        let (client_peer, _) = connect(&client, &server, server_addr.into());
        assert_eq!(i as u64, client_peer.inner());

        let client_that_left = disconnect(&client, &server, PeerId::set_raw(0));
        assert_eq!(client_that_left, client_peer);
    }
}

#[allow(dead_code)]
struct Scaffold {
    server: Handle<Message>,
    client: Handle<Message>,
    server_addr: SocketAddr,
}

fn start_server_client(server_port: u16) -> Scaffold {
    let _ = env_logger::builder().is_test(true).try_init();

    let server_addr = (Ipv4Addr::LOCALHOST, server_port);

    let server_config = Config {
        bind_addr: vec![server_addr.into()],
        ..Default::default()
    };

    let server_handle = peerlink::run(server_config).unwrap();
    let client_handle = peerlink::run(Config::default()).unwrap();

    Scaffold {
        server: server_handle,
        client: client_handle,
        server_addr: server_addr.into(),
    }
}

fn connect(
    client: &Handle<Message>,
    server: &Handle<Message>,
    server_addr: SocketAddr,
) -> (PeerId, PeerId) {
    client.send(Command::connect(server_addr)).unwrap();

    let client_peer = match server.recv_blocking().unwrap() {
        peerlink::Event::ConnectedFrom { peer, .. } => {
            println!("server: client has connected");
            peer
        }
        _ => panic!(),
    };

    let server_peer = match client.recv_blocking().unwrap() {
        peerlink::Event::ConnectedTo {
            result: Ok(peer), ..
        } => {
            println!("client: connected to server");
            peer
        }
        _ => panic!(),
    };

    (client_peer, server_peer)
}

fn disconnect(client: &Handle<Message>, server: &Handle<Message>, server_peer: PeerId) -> PeerId {
    client.send(Command::Disconnect(server_peer)).unwrap();

    let client_that_left = match server.recv_blocking().unwrap() {
        peerlink::Event::Disconnected {
            peer,
            reason: DisconnectReason::Left,
        } => {
            println!("server: client has disconnected");
            peer
        }
        _ => panic!(),
    };

    match client.recv_blocking().unwrap() {
        peerlink::Event::Disconnected {
            peer,
            reason: DisconnectReason::Requested,
        } if peer == server_peer => {
            println!("client: disconnected from server");
        }
        _ => panic!(),
    };

    client_that_left
}

fn message(handle: &Handle<Message>, peer: PeerId, message: Message) {
    handle.send(Command::Message(peer, message)).unwrap();
}

fn expect_ping(event: Event<Message>, nonce: u64) -> PeerId {
    match event {
        peerlink::Event::Message {
            message: Message::Ping(p),
            peer,
            size,
        } if nonce == p && size == 12 => peer,
        event => {
            panic!("expected Ping({nonce}) but got {:?}", event);
        }
    }
}

fn expect_pong(event: Event<Message>, nonce: u64) -> PeerId {
    match event {
        peerlink::Event::Message {
            message: Message::Pong(p),
            peer,
            size,
        } if nonce == p && size == 12 => peer,
        event => {
            panic!("expected Pong({nonce}) but got {:?}", event);
        }
    }
}

fn read_ping(event: Event<Message>) -> (PeerId, u64) {
    match event {
        peerlink::Event::Message {
            message: Message::Ping(p),
            peer,
            size,
        } if size == 12 => (peer, p),
        event => {
            panic!("expected Ping(_) but got {:?}", event);
        }
    }
}
