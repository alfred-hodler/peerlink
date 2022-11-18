use bitcoin::network::message::{NetworkMessage, RawNetworkMessage};
use peerlink::reactor::Handle;
use peerlink::{Command, PeerId, Reactor};

/// This test is designed to start one client reactor and one server reactor, send 10 pings from
/// the client to the server and expect 10 pongs back (interleaved). The client must shut down
/// in an orderly manner.
#[test]
fn pingpong() {
    env_logger::builder().is_test(true).init();

    let server_addr = "127.0.0.1:8333".parse().unwrap();

    let (server_reactor, server_handle) = Reactor::new(vec![server_addr]).unwrap();

    let (client_reactor, client_handle) = Reactor::new(vec![]).unwrap();

    let _server_join_handle = server_reactor.run();
    let client_join_handle = client_reactor.run();

    // We are using this because there is a split-second moment between the server binding to a local
    // socket and registering it for polling where it can miss notifications. This will never
    // happen in practice but it is essentially a race condition that can deadlock a test if the
    // running machine is fast enough.
    std::thread::sleep(std::time::Duration::from_millis(10));

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

    client_handle.send(Command::Shutdown).unwrap();
    let _ = client_join_handle.join();
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
