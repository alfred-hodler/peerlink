use std::net::SocketAddr;
use std::time::{Duration, Instant};

use peerlink::{Command, Config, Event};

#[derive(Debug)]
struct Msg(bytes::Bytes);

impl peerlink::Message for Msg {
    const MAX_SIZE: usize = 4 + 1024 * 1024;

    fn encode(&self, dest: &mut impl bytes::BufMut) {
        dest.put_u32_le(self.0.len() as u32);
        dest.put_slice(&self.0);
    }

    fn decode(buffer: &mut impl bytes::Buf) -> Result<Self, peerlink::DecodeError> {
        let size = buffer
            .try_get_u32_le()
            .map_err(|_| peerlink::DecodeError::Partial)? as usize;

        if buffer.remaining() >= size {
            let data = buffer.copy_to_bytes(size);
            Ok(Self(data))
        } else {
            Err(peerlink::DecodeError::Partial)
        }
    }

    fn wire_size(&self) -> usize {
        4 + self.0.len()
    }
}

fn responder(_: pico_args::Arguments) -> Result<(), Error> {
    let bind_addr = "127.0.0.1:8080".parse().unwrap();
    eprintln!("Server: starting to listen on address {}", bind_addr);

    let handle = peerlink::run::<Msg>(Config {
        bind_addr: vec![bind_addr],
        stream_config: peerlink::StreamConfig::default(),
        ..Default::default()
    })?;

    loop {
        match handle.recv_blocking().unwrap() {
            Event::ConnectedFrom { peer, addr, .. } => {
                eprintln!("Inbound peer connect: peer_id={} ip={}", peer, addr);
            }

            Event::Disconnected { peer, reason } => {
                eprintln!(
                    "Inbound peer disconnect: peer_id={}, reason={:?}",
                    peer, reason
                );
                break;
            }

            Event::Message { peer, message, .. } => {
                handle.send(Command::Message(peer, message)).unwrap();
            }

            _ => {}
        }
    }

    Ok(())
}

fn initiator(mut args: pico_args::Arguments) -> Result<(), Error> {
    let count: u32 = args.value_from_str("--count")?;
    let size: u32 = args.value_from_str("--size")?;

    let handle = peerlink::run(Config::default())?;

    let server_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    handle.send(Command::connect(server_addr))?;

    let peer_id = match handle.recv_blocking()? {
        Event::ConnectedTo {
            target,
            result: Ok(peer_id),
        } if target == server_addr.into() => {
            eprintln!("Connected to server at {}", target);
            peer_id
        }

        event => panic!("Unexpected event: {:?}", event),
    };

    let mut start = Instant::now();
    let mut rtt = Vec::with_capacity(count as usize);

    let metrics = BenchMetrics::start();
    let data: bytes::Bytes = vec![1; size as usize].into();
    handle.send(Command::Message(peer_id, Msg(data.clone())))?;

    let mut round = 0;
    let mut total_size: u64 = 0;

    loop {
        match handle.recv_blocking()? {
            Event::Message {
                message,
                peer,
                size,
                time: _,
            } => {
                if round > 0 {
                    rtt.push(Instant::now() - start);
                }
                handle.send(Command::Message(peer, message))?;
                start = Instant::now();
                round += 1;
                total_size += size as u64;

                if round == count {
                    break;
                }
            }

            Event::Disconnected { .. } => {
                eprintln!("The server has disconnected, exiting.");
                break;
            }

            event => panic!("Unexpected event: {:?}", event),
        }
    }

    metrics.stop(total_size as u64, round as u64, rtt);

    handle
        .shutdown(peerlink::Termination::TryFlush(Duration::from_secs(5)))
        .0
        .join()
        .unwrap()
        .unwrap();

    Ok(())
}

fn main() -> Result<(), Error> {
    env_logger::init();
    let mut args = pico_args::Arguments::from_env();
    let command = args.subcommand()?;

    match command.as_deref() {
        Some("initiator") => initiator(args),
        Some("responder") => responder(args),
        _ => {
            eprintln!("The first arg must be either 'initator' or 'responder'");
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

    pub fn stop(&self, total_bytes: u64, total_msgs: u64, timings: Vec<Duration>) {
        let end_time = Instant::now();
        let mut end_usage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut end_usage) };

        self.print_report(total_bytes, total_msgs, end_time, end_usage, timings);
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
        timings: Vec<Duration>,
    ) {
        fn fmt_bytes(bytes: u64) -> String {
            format!("{:.2}", bytesize::ByteSize::b(bytes).display().iec())
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

        let wall_secs = wall_time.as_secs_f64().max(0.001);
        let cpu_secs = total_cpu.as_secs_f64();
        let cpu_usage_pct = (cpu_secs / wall_secs) * 100.0;
        let bytes_per_sec = (total_bytes as f64 / wall_secs) as u64;

        // Derived Efficiency Metrics
        let mib_transferred = total_bytes as f64 / (1024.0 * 1024.0);
        let cpu_ms_per_mb = cpu_secs * 1000.0 / mib_transferred;
        let cpu_micros_per_msg = (cpu_secs * 1e6) / total_msgs as f64;
        let msgs_per_ctx_switch = total_msgs as f64 / (vol_ctx + invol_ctx) as f64;

        // Timings
        let timings = Stats::analyze(timings);

        println!("{:-^60}", " ROUND TRIP BENCHMARK REPORT ");

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

        // --- SECTION 5: TIMINGS STATS ---
        println!("\n[ TIMINGS ]");
        println!("{:<30} : {:.2} µs", "Min", timings.min.as_micros());
        println!("{:<30} : {:.2} µs", "Max", timings.max.as_micros());
        println!("{:<30} : {:.2} µs", "Avg", timings.mean.as_micros());
        println!("{:<30} : {:.2} µs", "Med", timings.median.as_micros());
        println!("{:<30} : {:.2} µs", "p95", timings.p95.as_micros());

        println!("\n{:-^60}\n", "");
    }
}
