use bitcoin::network::message::{NetworkMessage, RawNetworkMessage};
use peerlink::reactor::Handle;
use peerlink::{Command, PeerId, Reactor};

/// This test is designed to start one client reactor and one server reactor, send 10 pings from
/// the client to the server and expect 10 pongs back (interleaved).
#[test]
fn interact() {
    env_logger::builder().is_test(true).init();

    let server_addr = "127.0.0.1:8333".parse().unwrap();

    let (server_reactor, server_handle) = Reactor::new(vec![server_addr]).unwrap();

    let (client_reactor, client_handle) = Reactor::new(vec![]).unwrap();

    let _server_join_handle = server_reactor.run();
    let _client_join_handle = client_reactor.run();

    client_handle.send(Command::Connect(server_addr)).unwrap();

    // this is how the server sees the client
    let client_peer = match server_handle.receive().unwrap() {
        peerlink::Event::ConnectedFrom { peer, .. } => {
            println!("server: client has connected");
            peer
        }
        _ => panic!(),
    };

    // this is how the client sees the server
    let server_peer = match client_handle.receive().unwrap() {
        peerlink::Event::ConnectedTo(Ok((peer, _))) => {
            println!("client: connected to server");
            peer
        }
        _ => panic!(),
    };

    for nonce in 0..10 {
        message(&client_handle, server_peer, NetworkMessage::Ping(nonce));

        match server_handle.receive().unwrap() {
            peerlink::Event::Message {
                peer,
                message:
                    RawNetworkMessage {
                        magic: 0,
                        payload: NetworkMessage::Ping(got_nonce),
                    },
            } if nonce == got_nonce && peer == client_peer => {
                println!("server: got ping: {nonce}");

                message(&server_handle, client_peer, NetworkMessage::Pong(nonce));

                match client_handle.receive().unwrap() {
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

                    _ => panic!(),
                }
            }

            _ => panic!(),
        };
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
