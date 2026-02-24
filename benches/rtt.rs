use std::net::SocketAddr;
use std::time::{Duration, Instant};

use peerlink::{Command, Config, Event};

#[derive(Debug)]
struct Message(Vec<u8>);

impl peerlink::Message for Message {
    const MAX_SIZE: usize = 100 * 1024 * 1024;

    fn encode(&self, dest: &mut impl std::io::Write) -> usize {
        let mut written = 0;
        written += dest.write(&(self.0.len() as u64).to_le_bytes()).unwrap();
        dest.write_all(&self.0).unwrap();
        written += self.0.len();
        written
    }

    fn decode(buffer: &[u8]) -> Result<(Self, usize), peerlink::DecodeError> {
        if buffer.len() >= 8 {
            let size = u64::from_le_bytes(buffer[..8].try_into().unwrap()) as usize;

            match buffer.get(8..8 + size) {
                Some(data) => Ok((Self(data.to_owned()), 8 + size)),
                None => Err(peerlink::DecodeError::NotEnoughData),
            }
        } else {
            Err(peerlink::DecodeError::NotEnoughData)
        }
    }
}

fn server() -> Result<(), Error> {
    let bind_addr = "127.0.0.1:8080".parse().unwrap();
    println!("Server: starting to listen on address {}", bind_addr);

    let handle = peerlink::run::<Message>(Config {
        bind_addr: vec![bind_addr],
        stream_config: peerlink::StreamConfig::default(),
        ..Default::default()
    })?;

    loop {
        match handle.recv_blocking().unwrap() {
            Event::ConnectedFrom { peer, addr, .. } => {
                println!("Inbound peer connect: peer_id={} ip={}", peer, addr);
            }

            Event::Disconnected { peer, reason } => {
                println!(
                    "Inbound peer disconnect: peer_id={}, reason={:?}",
                    peer, reason
                );
            }

            Event::Message { peer, message, .. } => {
                handle.send(Command::Message(peer, message)).unwrap();
            }

            _ => {}
        }
    }
}

fn client(mut args: pico_args::Arguments) -> Result<(), Error> {
    let rounds: u32 = args.value_from_str("--rounds")?;
    let size: u32 = args.value_from_str("--size")?;

    let handle = peerlink::run(Config::default())?;

    let server_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    handle.send(Command::connect(server_addr))?;

    let peer_id = match handle.recv_blocking()? {
        Event::ConnectedTo {
            target,
            result: Ok(peer_id),
        } if target == server_addr.into() => {
            println!("Connected to server at {}", target);
            peer_id
        }

        event => panic!("Unexpected event: {:?}", event),
    };

    handle.send(Command::Message(peer_id, Message(vec![1; size as usize])))?;
    let mut start = Instant::now();
    let mut rtt = Vec::with_capacity(rounds as usize);

    let mut round = 0;
    let mut total_size: u64 = 0;

    loop {
        match handle.recv_blocking()? {
            Event::Message {
                message,
                peer,
                size,
            } => {
                if round > 0 {
                    rtt.push(Instant::now() - start);
                }
                handle.send(Command::Message(peer, message))?;
                start = Instant::now();
                round += 1;
                total_size += size as u64;

                if round > rounds {
                    break;
                }
            }

            Event::Disconnected { .. } => {
                println!("The server has disconnected, exiting.");
                break;
            }

            event => panic!("Unexpected event: {:?}", event),
        }
    }

    let stats = Stats::analyze(rtt);
    stats.print();
    println!("Total size: {total_size} bytes");

    handle.shutdown().0.join().unwrap().unwrap();

    Ok(())
}

fn main() -> Result<(), Error> {
    env_logger::init();
    let mut args = pico_args::Arguments::from_env();
    let command = args.subcommand()?;

    match command.as_deref() {
        Some("client") => client(args),
        Some("server") => server(),
        _ => {
            eprintln!("The first arg must be either 'client' or 'server'");
            Ok(())
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum Error {
    Args(pico_args::Error),
    Io(std::io::Error),
    Send(peerlink::SendError),
    Recv(peerlink::RecvError),
}

impl From<pico_args::Error> for Error {
    fn from(value: pico_args::Error) -> Self {
        Self::Args(value)
    }
}

impl From<peerlink::SendError> for Error {
    fn from(value: peerlink::SendError) -> Self {
        Self::Send(value)
    }
}

impl From<peerlink::RecvError> for Error {
    fn from(value: peerlink::RecvError) -> Self {
        Self::Recv(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

struct Stats {
    min: Duration,
    max: Duration,
    mean: Duration,
    median: Duration,
    p95: Duration,
}

impl Stats {
    fn analyze(mut data: Vec<Duration>) -> Self {
        data.sort();

        let sum: Duration = data.iter().sum();
        let len = data.len();

        Self {
            min: data[0],
            max: data[len - 1],
            mean: sum / len as u32,
            median: data[len / 2],
            p95: data[(len as f64 * 0.95) as usize],
        }
    }

    fn print(&self) {
        println!("---- stats ----");
        println!("min: {} μs", self.min.as_micros());
        println!("max: {} μs", self.max.as_micros());
        println!("avg: {} μs", self.mean.as_micros());
        println!("med: {} μs", self.median.as_micros());
        println!("p95: {} μs", self.p95.as_micros());
    }
}
