use core::panic;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::thread::JoinHandle;

use bitcoin::network::message::{NetworkMessage, RawNetworkMessage};
use peerlink::reactor::Handle;
use peerlink::{Command, PeerId, Reactor};

/// This test is designed to start one client reactor and one server reactor, send 10 pings from
/// the client to the server and expect 10 pongs back (interleaved). The client must shut down
/// in an orderly manner.
#[test]
fn interleaved() {
    let Scaffold {
        server,
        client,
        server_addr,
        client_join_handle,
        ..
    } = start_server_client(8100);

    client.send(Command::Connect(server_addr)).unwrap();

    // this is how the server sees the client
    let client_peer = match server.receive().unwrap() {
        peerlink::Event::ConnectedFrom { peer, .. } => {
            println!("server: client has connected");
            peer
        }
        _ => panic!(),
    };

    // this is how the client sees the server
    let server_peer = match client.receive().unwrap() {
        peerlink::Event::ConnectedTo(Ok((peer, _))) => {
            println!("client: connected to server");
            peer
        }
        _ => panic!(),
    };

    for nonce in 0..10 {
        message(&client, server_peer, NetworkMessage::Ping(nonce));

        match server.receive().unwrap() {
            peerlink::Event::Message {
                peer,
                message:
                    RawNetworkMessage {
                        magic: 0,
                        payload: NetworkMessage::Ping(got_nonce),
                    },
            } if nonce == got_nonce && peer == client_peer => {
                println!("server: got ping: {nonce}");

                message(&server, client_peer, NetworkMessage::Pong(nonce));

                match client.receive().unwrap() {
                    peerlink::Event::Message {
                        peer,
                        message:
                            RawNetworkMessage {
                                magic: 0,
                                payload: NetworkMessage::Pong(got_nonce),
                            },
                    } if nonce == got_nonce && peer == server_peer => {
                        println!("server: got pong: {nonce}");
                    }

                    event => {
                        println!("{:?}", event);
                        panic!("")
                    }
                }
            }

            _ => panic!(),
        };
    }

    client.send(Command::Shutdown).unwrap();
    let _ = client_join_handle.join();
}

/// This test is designed to start one client reactor and one server reactor, rapidly send 100
/// pings from the client to the server, disconnect and only then let the server start reading
/// events.
#[test]
fn stuff_messages() {
    let Scaffold {
        server,
        client,
        server_addr,
        ..
    } = start_server_client(8101);

    client.send(Command::Connect(server_addr)).unwrap();

    // this is how the client sees the server
    let server_peer = match client.receive().unwrap() {
        peerlink::Event::ConnectedTo(Ok((peer, _))) => {
            println!("client: connected to server");
            peer
        }
        _ => panic!(),
    };

    for i in 0..100 {
        message(&client, server_peer, NetworkMessage::Ping(i));
    }

    client.send(Command::Disconnect(server_peer)).unwrap();

    // this is how the server sees the client
    match server.receive().unwrap() {
        peerlink::Event::ConnectedFrom { .. } => {
            println!("server: client has connected");
        }
        _ => panic!(),
    }

    for i in 0..100 {
        match server.receive().unwrap() {
            peerlink::Event::Message {
                message:
                    RawNetworkMessage {
                        payload: NetworkMessage::Ping(p),
                        ..
                    },
                ..
            } if i == p => {}
            event => {
                println!("{:?}", event);
                panic!()
            }
        }
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

    let (server_reactor, server_handle) = Reactor::new(vec![server_addr]).unwrap();
    let (client_reactor, client_handle) = Reactor::new(vec![]).unwrap();

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
