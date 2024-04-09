use peerlink::reactor::Event;
use peerlink::{Command, Config, Reactor};

// This example consists of a client and server. The server waits for inbound connections from
// clients and replies to client pings with pongs.
//
// Start the server with `cargo run --example pingpong server` and then start any number of
// clients with `cargo run --example pingpong client`.

// First we define a set of messages describing our protocol.
#[derive(Debug)]
enum Message {
    Ping(u64),
    Pong(u64),
}

// Then we define the codec for the messages we plan on sending and receiving. The encoding is
// entirely arbitrary.
impl peerlink::Message for Message {
    fn encode(&self, dest: &mut impl std::io::Write) -> usize {
        let (msg_type, value) = match &self {
            Message::Ping(p) => (b"ping", p),
            Message::Pong(p) => (b"pong", p),
        };

        let mut written = 0;
        written += dest.write(msg_type).unwrap();
        written += dest.write(&value.to_le_bytes()).unwrap();
        written
    }

    fn decode(buffer: &[u8]) -> Result<(Self, usize), peerlink::DecodeError> {
        const VALUE_OFFSET: usize = 4;
        const MSG_LEN: usize = 12;

        if buffer.len() >= MSG_LEN {
            let value = u64::from_le_bytes(buffer[VALUE_OFFSET..MSG_LEN].try_into().unwrap());

            match &buffer[0..4] {
                b"ping" => Ok((Message::Ping(value), MSG_LEN)),
                b"pong" => Ok((Message::Pong(value), MSG_LEN)),
                _ => Err(peerlink::DecodeError::MalformedMessage),
            }
        } else {
            Err(peerlink::DecodeError::NotEnoughData)
        }
    }
}

// The server consists of a reactor listening to events in a loop and replying to pings with pongs.
fn server() -> std::io::Result<()> {
    // Define the listen address for inbound peers to connect to.
    let bind_addr = "127.0.0.1:8080".parse().unwrap();
    println!("Server: starting to listen on address {}", bind_addr);

    // Create the reactor and get its handle.
    let (reactor, handle) = Reactor::<_, _, String>::new(Config {
        bind_addr: vec![bind_addr],
        ..Default::default()
    })?;

    let _join_handle = reactor.run();

    // Start processing events.
    loop {
        match handle.receive_blocking().unwrap() {
            Event::ConnectedFrom { peer, addr, .. } => {
                println!("Inbound peer connect: peer_id={} ip={}", peer, addr);
            }

            Event::Disconnected { peer, reason } => {
                println!(
                    "Inbound peer disconnect: peer_id={}, reason={:?}",
                    peer, reason
                );
            }

            Event::Message { peer, message } => match message {
                Message::Ping(p) => {
                    println!("Incoming ping: peer={}, value={}", peer, p);
                    handle.send(Command::Message(peer, Message::Pong(p)))?;
                }
                Message::Pong(p) => {
                    println!("Incoming pong: peer={}, value={}", peer, p);
                }
            },

            _ => {}
        }
    }
}

// The client consists of a reactor connecting to a server and periodically sending out pings until
// the server disappears.
fn client() -> std::io::Result<()> {
    // Create the reactor and get its handle.
    let (reactor, handle) = Reactor::new(Config::default())?;

    let _join_handle = reactor.run();

    // Connect to our server.
    let server_addr = "127.0.0.1:8080".to_string();
    handle.send(Command::Connect(server_addr.clone()))?;

    let peer_id = match handle.receive_blocking()? {
        Event::ConnectedTo {
            target,
            result: Ok(peer_id),
        } if target == server_addr => {
            println!("Connected to server at {}", target);
            peer_id
        }
        event => panic!("Unexpected event: {:?}", event),
    };

    let mut ping = 0;

    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));

        handle.send(Command::Message(peer_id, Message::Ping(ping)))?;
        println!("Sending a ping: value={}", ping);

        match handle.receive_blocking()? {
            Event::Message {
                message: Message::Pong(pong),
                ..
            } => {
                println!("Received a pong: value={}", pong);
            }

            Event::Disconnected { .. } => {
                println!("The server has disconnected, exiting.");
                break Ok(());
            }

            event => panic!("Unexpected event: {:?}", event),
        }

        ping += 1;
    }
}

fn main() -> std::io::Result<()> {
    env_logger::init();

    match std::env::args().nth(1).as_deref() {
        Some("client") => client(),
        Some("server") => server(),
        _ => {
            eprintln!("The first arg must be either 'client' or 'server'");
            Ok(())
        }
    }
}
