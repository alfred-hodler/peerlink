use core::panic;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::thread::JoinHandle;

use bitcoin::consensus::Decodable;
use bitcoin::network::message::{NetworkMessage, RawNetworkMessage};
use peerlink::reactor::{DisconnectReason, Handle};
use peerlink::{Command, Config, Event, PeerId, Reactor};

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
        message(&client, server_peer, NetworkMessage::Ping(nonce));
        expect_ping(server.receive().unwrap(), nonce);
        message(&server, client_peer, NetworkMessage::Pong(nonce));
        expect_pong(client.receive().unwrap(), nonce);
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
        message(&client, server_peer, NetworkMessage::Ping(i));
    }

    client.send(Command::Disconnect(server_peer)).unwrap();

    for i in 0..1000 {
        expect_ping(server.receive().unwrap(), i);
    }

    match server.receive().unwrap() {
        peerlink::Event::Disconnected {
            reason: DisconnectReason::Left,
            ..
        } => {}
        event => {
            println!("event: {:?}", event);
            panic!("did not get disconnect");
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
        message(&client, server_peer, NetworkMessage::Ping(i));
        message(&server, client_peer, NetworkMessage::Ping(i));
    }

    for i in 0..1000 {
        expect_ping(server.receive().unwrap(), i);
        expect_ping(client.receive().unwrap(), i);
    }
}

/// Starts one server and several clients, makes the clients simultaneously send pings to the
/// server and lets the server respond with pongs immediately as they come in. Clients must process
/// those pongs immediately, before the loop moves on to the next nonce.
#[test]
fn many_to_one_interleaved() {
    let _ = env_logger::builder().is_test(true).try_init();

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8103).into();

    let server_config = Config {
        bind_addr: vec![server_addr],
        ..Default::default()
    };

    let (server_reactor, server) = Reactor::new(server_config).unwrap();
    server_reactor.run();

    std::thread::sleep(std::time::Duration::from_millis(10));

    let n_clients = 5;

    let clients: Vec<_> = (0..n_clients)
        .into_iter()
        .enumerate()
        .map(|(i, _)| {
            let (client_reactor, client) = Reactor::new(Config::default()).unwrap();
            client_reactor.run();

            let (client_peer, _) = connect(&client, &server, server_addr);
            assert_eq!(i, client_peer.0);

            client
        })
        .collect();

    for nonce in 0..100 {
        for (i, c) in clients.iter().enumerate() {
            message(c, PeerId(0), NetworkMessage::Ping(nonce));
            let ping_from_peer = expect_ping(server.receive().unwrap(), nonce);
            assert_eq!(ping_from_peer, PeerId(i));
            message(&server, ping_from_peer, NetworkMessage::Pong(nonce));
            expect_pong(c.receive().unwrap(), nonce);
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

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8104).into();

    let server_config = Config {
        bind_addr: vec![server_addr],
        ..Default::default()
    };

    let (server_reactor, server) = Reactor::new(server_config).unwrap();
    server_reactor.run();

    std::thread::sleep(std::time::Duration::from_millis(10));

    let n_clients = 5;
    let n_pings = 100;

    let clients: Vec<_> = (0..n_clients)
        .into_iter()
        .enumerate()
        .map(|(i, _)| {
            let (client_reactor, client) = Reactor::new(Config::default()).unwrap();
            client_reactor.run();

            let (client_peer, _) = connect(&client, &server, server_addr);
            assert_eq!(i, client_peer.0);

            client
        })
        .collect();

    for nonce in 0..n_pings {
        for c in &clients {
            message(c, PeerId(0), NetworkMessage::Ping(nonce));
        }
    }

    // no guarantee that the server is processing every peer sequentially
    for _ in 0..(clients.len() * n_pings as usize) {
        let (peer, nonce) = read_ping(server.receive().unwrap());
        message(&server, peer, NetworkMessage::Pong(nonce));
    }

    assert!(server.try_receive().is_none());

    for nonce in 0..100 {
        for c in &clients {
            expect_pong(c.receive().unwrap(), nonce);
        }
    }

    for c in &clients {
        assert!(c.try_receive().is_none());
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

    let raw_block = include_bytes!("block_540107").to_vec();
    let block = bitcoin::Block::consensus_decode(&mut raw_block.as_slice()).unwrap();

    client
        .send(Command::Message(
            client_peer,
            RawNetworkMessage {
                magic: 0,
                payload: NetworkMessage::Block(block.clone()),
            },
        ))
        .unwrap();

    match server.receive().unwrap() {
        Event::Message {
            message:
                RawNetworkMessage {
                    payload: NetworkMessage::Block(rx_block),
                    ..
                },
            ..
        } if block == rx_block => {}
        _ => panic!(),
    }
}

#[allow(dead_code)]
struct Scaffold {
    server: Handle,
    client: Handle,
    server_join_handle: JoinHandle<Result<(), std::io::Error>>,
    client_join_handle: JoinHandle<Result<(), std::io::Error>>,
    server_addr: SocketAddr,
}

fn start_server_client(server_port: u16) -> Scaffold {
    let _ = env_logger::builder().is_test(true).try_init();

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port).into();

    let server_config = Config {
        bind_addr: vec![server_addr],
        ..Default::default()
    };

    let (server_reactor, server_handle) = Reactor::new(server_config).unwrap();
    let (client_reactor, client_handle) = Reactor::new(Config::default()).unwrap();

    let server_join_handle = server_reactor.run();
    let client_join_handle = client_reactor.run();

    // We are using this because there is a split-second moment between the server binding to a local
    // socket and registering it for polling where it can miss notifications. This will never
    // happen in practice but it is essentially a race condition that can deadlock a test if the
    // running machine is fast enough.
    std::thread::sleep(std::time::Duration::from_millis(10));

    Scaffold {
        server: server_handle,
        client: client_handle,
        server_join_handle,
        client_join_handle,
        server_addr,
    }
}

fn connect(client: &Handle, server: &Handle, server_addr: SocketAddr) -> (PeerId, PeerId) {
    client.send(Command::Connect(server_addr)).unwrap();

    let client_peer = match server.receive().unwrap() {
        peerlink::Event::ConnectedFrom { peer, .. } => {
            println!("server: client has connected");
            peer
        }
        _ => panic!(),
    };

    let server_peer = match client.receive().unwrap() {
        peerlink::Event::ConnectedTo(Ok((peer, _))) => {
            println!("client: connected to server");
            peer
        }
        _ => panic!(),
    };

    (client_peer, server_peer)
}

fn message(handle: &Handle, peer: PeerId, message: NetworkMessage) {
    handle
        .send(Command::Message(
            peer,
            RawNetworkMessage {
                magic: 0,
                payload: message,
            },
        ))
        .unwrap();
}

fn expect_ping(event: peerlink::Event, nonce: u64) -> PeerId {
    match event {
        peerlink::Event::Message {
            message:
                RawNetworkMessage {
                    payload: NetworkMessage::Ping(p),
                    ..
                },
            peer,
        } if nonce == p => peer,
        event => {
            println!("expected Ping({nonce}) but got {:?}", event);
            panic!()
        }
    }
}

fn expect_pong(event: peerlink::Event, nonce: u64) -> PeerId {
    match event {
        peerlink::Event::Message {
            message:
                RawNetworkMessage {
                    payload: NetworkMessage::Pong(p),
                    ..
                },
            peer,
        } if nonce == p => peer,
        event => {
            println!("expected Pong({nonce}) but got {:?}", event);
            panic!()
        }
    }
}

fn read_ping(event: peerlink::Event) -> (PeerId, u64) {
    match event {
        peerlink::Event::Message {
            message:
                RawNetworkMessage {
                    payload: NetworkMessage::Ping(p),
                    ..
                },
            peer,
        } => (peer, p),
        event => {
            println!("expected Ping(_) but got {:?}", event);
            panic!()
        }
    }
}
