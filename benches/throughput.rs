use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use peerlink::{Command, Config, Event, MaxMessageSizeMultiple, Message};

#[derive(Clone)]
enum Msg {
    Start { size: u32, count: u32 },
    Data(Arc<[u8]>),
    Done,
}

impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start { size, count } => f
                .debug_struct("Start")
                .field("size", size)
                .field("count", count)
                .finish(),
            Self::Data(data) => f.debug_struct("Data").field("size", &data.len()).finish(),
            Self::Done => f.debug_tuple("Done").finish(),
        }
    }
}

impl peerlink::Message for Msg {
    const MAX_SIZE: usize = 1024 * 1024;

    fn encode(&self, dest: &mut impl std::io::Write) {
        match self {
            Self::Start { size, count } => {
                dest.write_all(&[0]).unwrap();
                dest.write_all(&size.to_le_bytes()).unwrap();
                dest.write_all(&count.to_le_bytes()).unwrap();
            }
            Self::Data(data) => {
                dest.write_all(&[1]).unwrap();
                dest.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
                dest.write_all(&data).unwrap();
            }
            Self::Done => {
                dest.write_all(&[2]).unwrap();
            }
        }
    }

    fn decode(buffer: &[u8]) -> Result<(Self, usize), peerlink::DecodeError> {
        let kind = *buffer.get(0).ok_or(peerlink::DecodeError::NotEnoughData)?;
        match kind {
            0 => {
                let size = buffer
                    .get(1..5)
                    .ok_or(peerlink::DecodeError::NotEnoughData)?;
                let count = buffer
                    .get(5..9)
                    .ok_or(peerlink::DecodeError::NotEnoughData)?;
                Ok((
                    Self::Start {
                        size: u32::from_le_bytes(size.try_into().unwrap()),
                        count: u32::from_le_bytes(count.try_into().unwrap()),
                    },
                    1 + 4 + 4,
                ))
            }
            1 => {
                let length = buffer
                    .get(1..5)
                    .ok_or(peerlink::DecodeError::NotEnoughData)?;
                let length = u32::from_le_bytes(length.try_into().unwrap()) as usize;
                let data = buffer
                    .get(5..5 + length)
                    .ok_or(peerlink::DecodeError::NotEnoughData)?;
                Ok((Self::Data(data.into()), 1 + 4 + length))
            }
            2 => Ok((Self::Done, 1)),
            x => unreachable!("no message type {x}"),
        }
    }

    fn wire_size(&self) -> usize {
        match self {
            Self::Start { .. } => 1 + 4 + 4,
            Self::Data(items) => 1 + 4 + items.len(),
            Self::Done => 1,
        }
    }
}

fn sender(mut args: pico_args::Arguments) -> Result<(), Error> {
    let index: u16 = args.value_from_str("--index")?;

    let bind_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080 + index);
    println!("sender {}: binding to {}", index, bind_addr);

    let handle = peerlink::run::<_>(Config {
        bind_addr: vec![bind_addr.into()],
        stream_config: peerlink::StreamConfig {
            tx_buf_min_size: 1024 * 1024,
            tx_buf_max_size: MaxMessageSizeMultiple(4.try_into().unwrap()),
            outbound_telemetry: true,
            ..Default::default()
        },
        ..Default::default()
    })?;

    let peer = match handle.recv_blocking().unwrap() {
        Event::ConnectedFrom { peer, .. } => peer,
        _ => unreachable!(),
    };

    let (size, count) = match handle.recv_blocking().unwrap() {
        Event::Message {
            message: Msg::Start { size, count },
            ..
        } => (size, count),
        _ => unreachable!(),
    };

    println!(
        "sender {}: peer requesting {} messages of size {}",
        index, count, size
    );

    let bytes: Arc<[u8]> = vec![1; size as usize].into();
    let data = Msg::Data(bytes.clone());
    let mut remaining = count as usize - 1;
    let mut backpressure = Backpressure::new(4 * 1024 * 1024);
    let mut actual_sent = 1;
    let mut actual_acked = 0;

    handle.send(Command::Message(peer, data.clone())).unwrap();
    backpressure.sent(data.wire_size());

    loop {
        match handle.recv_blocking().unwrap() {
            Event::OutboundTelemetry { delta, .. } => {
                if delta > 0 {
                    actual_acked += 1;
                }

                backpressure.delta(delta);
                let n = (backpressure.capacity() / data.wire_size()).min(remaining);
                if n > 0 {
                    for _ in 0..n {
                        handle.send(Command::Message(peer, data.clone())).unwrap();
                        backpressure.sent(data.wire_size());
                    }
                    actual_sent += n;
                    remaining = remaining.saturating_sub(n);
                }
                if actual_acked == count {
                    break;
                }
            }
            Event::QueueRejected { .. } => {
                println!("sender {}: queue rejected at count {}", index, remaining);
                unreachable!("queue rejected?")
            }
            Event::Message {
                message: Msg::Done, ..
            } => {
                println!("sender {}: received done signal", index);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(actual_acked, actual_acked);
    println!("sender {}: sent {} messages", index, actual_sent);
    handle
        .shutdown(peerlink::Termination::TryFlush(Duration::from_secs(5)))
        .0
        .join()
        .unwrap()
        .unwrap();

    Ok(())
}

fn sink(mut args: pico_args::Arguments) -> Result<(), Error> {
    let size: u32 = args.value_from_str("--size")?;
    let count: u32 = args.value_from_str("--count")?;
    let senders: u16 = args.value_from_str("--senders")?;
    let peer_addrs = (0..senders).map(|offset| {
        let port = 8080 + offset;
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
    });

    let handle = peerlink::run::<Msg>(Config::default())?;

    for peer in peer_addrs {
        handle.send(Command::Connect((peer).into()))?;
    }

    let mut connected_peers = 0;
    let mut peers = Vec::with_capacity(senders.into());

    loop {
        match handle.recv_blocking().unwrap() {
            Event::ConnectedTo { result, target } => {
                let peer_id = result.unwrap();
                peers.push(peer_id);
                println!("sink: connected to target={} peer_id={} ", target, peer_id);

                connected_peers += 1;
                if connected_peers == senders {
                    break;
                }
            }

            _ => {}
        }
    }

    for peer in &peers {
        handle.send(Command::Message(*peer, Msg::Start { size, count }))?;
    }

    let metrics = BenchMetrics::start();

    let mut bytes_received = 0;
    let mut msg_count = 0;
    let mut first_time = None;
    loop {
        match handle.recv_blocking().unwrap() {
            Event::Message {
                size,
                message: Msg::Data(_),
                ..
            } => {
                if first_time.is_none() {
                    let _ = first_time.insert(Instant::now());
                }
                bytes_received += size;
                msg_count += 1;
                if count * peers.len() as u32 == msg_count {
                    metrics.stop(bytes_received as u64, msg_count as u64);
                    for peer in &peers {
                        handle.send(Command::Message(*peer, Msg::Done)).unwrap();
                    }
                    break;
                }
            }

            _ => {}
        }
    }

    handle
        .shutdown(peerlink::Termination::Immediate)
        .0
        .join()
        .unwrap()
        .unwrap();

    Ok(())
}

struct Backpressure {
    ghost: usize,
    queue: usize,
    limit: usize,
}

impl Backpressure {
    fn new(limit: usize) -> Self {
        Self {
            ghost: 0,
            queue: 0,
            limit,
        }
    }

    fn sent(&mut self, n: usize) {
        self.ghost += n;
    }

    fn delta(&mut self, n: isize) {
        if n > 0 {
            self.ghost = self.ghost.strict_sub_signed(n);
            self.queue += n as usize;
        } else if n < 0 {
            self.queue = self.queue.strict_sub_signed(n.abs());
        }
    }

    fn capacity(&self) -> usize {
        self.limit.strict_sub(self.ghost + self.queue)
    }
}

fn main() -> Result<(), Error> {
    let _ = env_logger::builder().is_test(true).try_init();
    let mut args = pico_args::Arguments::from_env();
    let command = args.subcommand()?;

    match command.as_deref() {
        Some("sender") => sender(args),
        Some("sink") => sink(args),
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

pub struct BenchMetrics {
    start_time: Instant,
    start_usage: libc::rusage,
}

impl BenchMetrics {
    pub fn start() -> Self {
        unsafe {
            let mut usage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            Self {
                start_time: Instant::now(),
                start_usage: usage,
            }
        }
    }

    pub fn stop(&self, total_bytes: u64, total_msgs: u64) {
        let end_time = Instant::now();
        let mut end_usage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut end_usage) };

        self.print_report(total_bytes, total_msgs, end_time, end_usage);
    }

    fn timeval_delta_to_duration(start: libc::timeval, end: libc::timeval) -> Duration {
        let start_dur = Duration::new(start.tv_sec as u64, start.tv_usec as u32 * 1000);
        let end_dur = Duration::new(end.tv_sec as u64, end.tv_usec as u32 * 1000);
        end_dur.saturating_sub(start_dur)
    }

    fn print_report(
        &self,
        total_bytes: u64,
        total_msgs: u64,
        end_time: Instant,
        end_usage: libc::rusage,
    ) {
        fn fmt_bytes(bytes: u64) -> String {
            humansize::format_size(bytes, humansize::BINARY)
        }

        let wall_time = end_time.duration_since(self.start_time);

        // Calculate deltas for CPU time
        let user_time =
            Self::timeval_delta_to_duration(self.start_usage.ru_utime, end_usage.ru_utime);
        let sys_time =
            Self::timeval_delta_to_duration(self.start_usage.ru_stime, end_usage.ru_stime);
        let total_cpu = user_time + sys_time;

        // Calculate deltas for context switches
        let vol_ctx = end_usage.ru_nvcsw - self.start_usage.ru_nvcsw;
        let invol_ctx = end_usage.ru_nivcsw - self.start_usage.ru_nivcsw;

        #[cfg(target_os = "linux")]
        let ru_multiplier = 1024;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let ru_multiplier = 1;
        let max_rss_bytes = end_usage.ru_maxrss as u64 * ru_multiplier;

        let wall_secs = wall_time.as_secs_f64().max(0.001);
        let cpu_secs = total_cpu.as_secs_f64();
        let cpu_usage_pct = (cpu_secs / wall_secs) * 100.0;
        let bytes_per_sec = (total_bytes as f64 / wall_secs) as u64;

        // Derived Efficiency Metrics
        let mib_transferred = total_bytes as f64 / (1024.0 * 1024.0);
        let cpu_ms_per_mb = cpu_secs * 1000.0 / mib_transferred;
        let cpu_micros_per_msg = (cpu_secs * 1e6) / total_msgs as f64;
        let msgs_per_ctx_switch = total_msgs as f64 / (vol_ctx + invol_ctx) as f64;

        println!("\n{:-^60}", " THROUGHPUT BENCHMARK REPORT ");

        // --- SECTION 1: EXECUTION TIME ---
        println!("\n[ EXECUTION TIME ]");
        println!("{:<30} : {:.3}s", "Wall Time", wall_time.as_secs_f64());
        println!("{:<30} : {:.3}s", "User Time", user_time.as_secs_f64());
        println!("{:<30} : {:.3}s", "Kernel Time", sys_time.as_secs_f64());
        println!("{:<30} : {:.2}%", "Total CPU Utilization", cpu_usage_pct);

        // --- SECTION 2: NETWORK PERFORMANCE ---
        println!("\n[ NETWORK PERFORMANCE ]");
        println!(
            "{:<30} : {}",
            "Average message size",
            fmt_bytes(total_bytes / total_msgs)
        );
        println!("{:<30} : {}", "Total Data", fmt_bytes(total_bytes));
        println!("{:<30} : {}", "Total Message Count", total_msgs);
        println!(
            "{:<30} : {}/s",
            "Throughput (Bandwidth)",
            fmt_bytes(bytes_per_sec)
        );
        println!(
            "{:<30} : {:.2} msgs/s",
            "Message Rate",
            total_msgs as f64 / wall_secs
        );

        // --- SECTION 3: SYSTEM RESOURCES ---
        println!("\n[ RESOURCE UTILIZATION ]");
        println!("{:<30} : {}", "Peak RSS", fmt_bytes(max_rss_bytes));
        println!("{:<30} : {}", "Voluntary Ctx Switches", vol_ctx);
        println!("{:<30} : {}", "Involuntary Ctx Switches", invol_ctx);

        // --- SECTION 4: PROTOCOL EFFICIENCY ---
        println!("\n[ PROTOCOL EFFICIENCY ]");
        println!(
            "{:<30} : {:.2} ms/MiB",
            "Compute Cost (per MiB)", cpu_ms_per_mb
        );
        println!(
            "{:<30} : {:.2} µs/msg",
            "Compute Cost (per Message)", cpu_micros_per_msg
        );
        println!(
            "{:<30} : {:.2} msgs/switch",
            "I/O Batching Efficiency", msgs_per_ctx_switch
        );

        // System Overhead Ratio: How much of our CPU time was spent in the Kernel vs User?
        let sys_ratio = (sys_time.as_secs_f64() / cpu_secs) * 100.0;
        println!("{:<30} : {:.2}%", "Kernel Overhead Ratio", sys_ratio);

        println!("\n{:-^60}\n", "");
    }
}
