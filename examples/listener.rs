use std::collections::HashSet;

use peerlink::reactor::Event;
use peerlink::{Command, Config, Reactor};

/// This example opens a localhost listener on several ports and waits for inbound connections.
/// It mirrors any received messages to the sender.
fn main() -> std::io::Result<()> {
    env_logger::init();

    let config = Config {
        bind_addr: vec![
            "127.0.0.1:8333".parse().unwrap(),
            "127.0.0.1:8334".parse().unwrap(),
            "127.0.0.1:8335".parse().unwrap(),
        ],
        ..Default::default()
    };

    // Create the reactor and get its handle.
    let (reactor, handle) = Reactor::new(config)?;
    let _join_handle = reactor.run();

    let mut peers = HashSet::new();

    loop {
        match handle.receive()? {
            Event::ConnectedFrom { peer, addr, .. } => {
                println!("Peer {peer} connected from {addr}");
                peers.insert(peer);
            }
            Event::Disconnected { peer, reason } => {
                println!("Peer {peer} disconnected with reason: {:?}", reason);
                peers.remove(&peer);
            }
            Event::Message { peer, message } => {
                println!("Peer {peer} sent us: {:?}", message);
                handle.send(Command::Message(peer, message))?;
            }
            _ => {}
        }
    }
}
