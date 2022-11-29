use bitcoin::network::constants::ServiceFlags;
use bitcoin::network::message::{NetworkMessage, RawNetworkMessage};
use bitcoin::network::message_network::VersionMessage;
use bitcoin::Network;
use peerlink::reactor::Event;
use peerlink::{Config, Reactor};

/// This example connects to a peer, performs a handshake and does a ping-pong. The user must
/// provide a peer socket address as the first parameter.
fn main() -> std::io::Result<()> {
    env_logger::init();

    // Choose the peer to connect to.
    let peer_addr = match std::env::args().skip(1).next() {
        Some(addr) => addr.parse().unwrap(),
        None => panic!("must provide a peer ip address and port"),
    };

    // Define a version message that we'll use when handshaking.
    let version = VersionMessage {
        nonce: 0,
        receiver: bitcoin::network::Address::new(&peer_addr, ServiceFlags::NONE),
        relay: false,
        sender: bitcoin::network::Address {
            address: [0; 8],
            port: 8333,
            services: ServiceFlags::NONE,
        },
        services: ServiceFlags::NONE,
        start_height: 0,
        user_agent: "test client".to_string(),
        version: 60002,
        timestamp: unix_time() as i64,
    };

    // Create the reactor and get its handle.
    let (reactor, handle) = Reactor::new(Config::default())?;
    let _join_handle = reactor.run();

    // Connect to our peer.
    handle.send(peerlink::Command::Connect(peer_addr))?;

    match handle.receive()? {
        // We want a successful connection here, everything else is an error.
        Event::ConnectedTo(Ok((peer, peer_addr_2))) => {
            assert_eq!(peer_addr_2, peer_addr);

            // We are connected, initiate a handshake.
            handle.send(peerlink::Command::Message(
                peer,
                RawNetworkMessage {
                    magic: Network::Bitcoin.magic(),
                    payload: NetworkMessage::Version(version),
                },
            ))?;

            match handle.receive()? {
                // We want a `version` message back, everything else is an error.
                Event::Message {
                    message:
                        RawNetworkMessage {
                            payload: NetworkMessage::Version(version),
                            ..
                        },
                    ..
                } => {
                    println!("Peer responds with version: {:?}", version);

                    // Send a `verack` to confirm.
                    handle.send(peerlink::Command::Message(
                        peer,
                        RawNetworkMessage {
                            magic: Network::Bitcoin.magic(),
                            payload: NetworkMessage::Verack,
                        },
                    ))?;

                    match handle.receive()? {
                        // We want a `verack` message back, everything else is an error.
                        Event::Message {
                            message:
                                RawNetworkMessage {
                                    payload: NetworkMessage::Verack,
                                    ..
                                },
                            ..
                        } => {
                            println!("Sending a ping...");
                            handle.send(peerlink::Command::Message(
                                peer,
                                RawNetworkMessage {
                                    magic: Network::Bitcoin.magic(),
                                    payload: NetworkMessage::Ping(333),
                                },
                            ))?;

                            loop {
                                println!("wait for message...");

                                match handle.receive()? {
                                    Event::Message {
                                        message: RawNetworkMessage { payload, .. },
                                        ..
                                    } => match payload {
                                        NetworkMessage::Ping(nonce) => {
                                            // Send a pong back in case we got a ping.
                                            println!("Got ping: {nonce}");
                                            handle.send(peerlink::Command::Message(
                                                peer,
                                                RawNetworkMessage {
                                                    magic: Network::Bitcoin.magic(),
                                                    payload: NetworkMessage::Pong(nonce),
                                                },
                                            ))?;
                                            println!("Sent pong: {nonce}");
                                        }
                                        NetworkMessage::Pong(pong) => {
                                            // This is our pong.
                                            println!("Got pong: {pong}");
                                            assert_eq!(333, pong);
                                            break;
                                        }
                                        m => {
                                            println!("other message received: {}", m.cmd());
                                        }
                                    },
                                    Event::Disconnected { reason, .. } => {
                                        println!("the peer left because: {:?}", reason);
                                        break;
                                    }
                                    event => println!("event: {:?}", event),
                                };
                            }
                        }

                        event => {
                            println!("got event {:?}", event);
                            panic!()
                        }
                    }
                }

                event => {
                    println!("got event {:?}", event);
                    panic!()
                }
            }
        }

        event => {
            println!("got event {:?}", event);
            panic!()
        }
    }

    println!("done pinging, initiate shutdown");
    handle.send(peerlink::Command::Shutdown)?;

    let result = _join_handle.join();
    println!("join result: {:?}", result);

    Ok(())
}

fn unix_time() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let start = SystemTime::now();
    start.duration_since(UNIX_EPOCH).unwrap().as_secs()
}
