use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DATAGRAM_SIZE: usize = 1200;
const HEADER_SIZE: usize = 18;
const PAYLOAD_SIZE: usize = DATAGRAM_SIZE - HEADER_SIZE;
const REPORT_INTERVAL: Duration = Duration::from_millis(50);
const MAX_RANGES_PER_REPORT: usize = 512;

type AnyResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transport {
    Tcp,
    Udp,
}

impl Transport {
    fn parse(value: &str) -> AnyResult<Self> {
        match value {
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            _ => Err(format!("unknown transport {value:?}; expected tcp or udp").into()),
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

#[derive(Debug)]
struct SendArgs {
    connect: SocketAddr,
    udp_target: SocketAddr,
    file: PathBuf,
    transport: Transport,
    rate_mbps: f64,
    repair_cooldown: Duration,
}

#[derive(Debug)]
struct ReceiveArgs {
    listen: SocketAddr,
    udp: SocketAddr,
    out: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> AnyResult<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("send") => send(parse_send(args.collect())?),
        Some("receive") => receive(parse_receive(args.collect())?),
        _ => {
            usage();
            Err("expected send or receive subcommand".into())
        }
    }
}

fn usage() {
    eprintln!(
        "usage:\n  tsunami-udp receive --listen ADDR --udp ADDR --out PATH\n  \
         tsunami-udp send --connect ADDR --udp-target ADDR --file PATH \
         --transport tcp|udp [--rate-mbps N] [--repair-cooldown-ms N]"
    );
}

fn option(args: &[String], name: &str) -> AnyResult<String> {
    let index = args
        .iter()
        .position(|arg| arg == name)
        .ok_or_else(|| format!("missing required option {name}"))?;
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| format!("missing value for {name}").into())
}

fn optional_option(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn parse_send(args: Vec<String>) -> AnyResult<SendArgs> {
    Ok(SendArgs {
        connect: option(&args, "--connect")?.parse()?,
        udp_target: option(&args, "--udp-target")?.parse()?,
        file: option(&args, "--file")?.into(),
        transport: Transport::parse(&option(&args, "--transport")?)?,
        rate_mbps: optional_option(&args, "--rate-mbps")
            .unwrap_or_else(|| "100".to_owned())
            .parse()?,
        repair_cooldown: Duration::from_millis(
            optional_option(&args, "--repair-cooldown-ms")
                .unwrap_or_else(|| "250".to_owned())
                .parse()?,
        ),
    })
}

fn parse_receive(args: Vec<String>) -> AnyResult<ReceiveArgs> {
    Ok(ReceiveArgs {
        listen: option(&args, "--listen")?.parse()?,
        udp: option(&args, "--udp")?.parse()?,
        out: option(&args, "--out")?.into(),
    })
}

fn send(args: SendArgs) -> AnyResult<()> {
    let size = fs::metadata(&args.file)?.len();
    let expected_hash = hash_file(&args.file)?;
    let session = new_session_id();
    let chunks = size.div_ceil(PAYLOAD_SIZE as u64);

    let mut control = TcpStream::connect(args.connect)?;
    control.set_nodelay(true)?;
    writeln!(
        control,
        "TSU1 {} {} {} {} {}",
        args.transport.wire_name(),
        size,
        expected_hash,
        session,
        chunks
    )?;
    control.flush()?;

    let mut ready_reader = BufReader::new(control.try_clone()?);
    let mut ready = String::new();
    ready_reader.read_line(&mut ready)?;
    if ready.trim() != "READY" {
        return Err(format!("receiver rejected transfer: {}", ready.trim()).into());
    }

    let started = Instant::now();
    let (datagrams, repairs) = match args.transport {
        Transport::Tcp => {
            send_tcp(&args.file, &mut control)?;
            expect_completion(control.try_clone()?)?;
            (0, 0)
        }
        Transport::Udp => {
            let config = UdpSendConfig {
                size,
                session,
                chunks,
                target: args.udp_target,
                rate_mbps: args.rate_mbps,
                repair_cooldown: args.repair_cooldown,
            };
            send_udp(&args.file, &config, &mut control)?
        }
    };

    let elapsed = started.elapsed();
    let goodput_mbps = if elapsed.is_zero() {
        0.0
    } else {
        size as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0
    };
    println!(
        "{{\"transport\":\"{}\",\"bytes\":{},\"elapsed_ms\":{:.3},\
         \"goodput_mbps\":{:.3},\"datagrams\":{},\"repairs\":{}}}",
        args.transport.display_name(),
        size,
        elapsed.as_secs_f64() * 1000.0,
        goodput_mbps,
        datagrams,
        repairs
    );
    Ok(())
}

fn expect_completion(control: TcpStream) -> AnyResult<()> {
    let mut completion_reader = BufReader::new(control);
    let mut completion = String::new();
    completion_reader.read_line(&mut completion)?;
    if completion.trim() != "COMPLETE" {
        return Err(format!("transfer did not complete: {}", completion.trim()).into());
    }
    Ok(())
}

fn send_tcp(path: &Path, control: &mut TcpStream) -> AnyResult<()> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        control.write_all(&buffer[..count])?;
    }
    control.flush()?;
    Ok(())
}

struct UdpSendConfig {
    size: u64,
    session: u64,
    chunks: u64,
    target: SocketAddr,
    rate_mbps: f64,
    repair_cooldown: Duration,
}

fn send_udp(path: &Path, config: &UdpSendConfig, control: &mut TcpStream) -> AnyResult<(u64, u64)> {
    if !config.rate_mbps.is_finite() || config.rate_mbps <= 0.0 {
        return Err("--rate-mbps must be positive and finite".into());
    }

    let socket = UdpSocket::bind(if config.target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?;
    socket.connect(config.target)?;
    let file = File::open(path)?;
    let pending = Arc::new(Mutex::new(RepairQueue::new(config.repair_cooldown)));
    let complete = Arc::new(AtomicBool::new(false));
    let feedback_error = Arc::new(Mutex::new(None::<String>));

    let reader_stream = control.try_clone()?;
    let pending_for_reader = Arc::clone(&pending);
    let complete_for_reader = Arc::clone(&complete);
    let error_for_reader = Arc::clone(&feedback_error);
    let feedback_thread = thread::spawn(move || {
        if let Err(error) = read_feedback(reader_stream, pending_for_reader, complete_for_reader) {
            *error_for_reader
                .lock()
                .expect("feedback error mutex poisoned") = Some(error.to_string());
        }
    });

    let mut pacer = Pacer::new(config.rate_mbps * 1_000_000.0 / 8.0);
    let mut packet = vec![0_u8; DATAGRAM_SIZE];
    let mut datagrams = 0_u64;
    let mut repairs = 0_u64;

    for sequence in 0..config.chunks {
        let packet_len = build_packet(&file, config.size, config.session, sequence, &mut packet)?;
        socket.send(&packet[..packet_len])?;
        datagrams += 1;
        pacer.account_and_wait(packet_len);

        if sequence % 16 == 15 {
            for repair in take_repairs(&pending, 4) {
                let packet_len =
                    build_packet(&file, config.size, config.session, repair, &mut packet)?;
                socket.send(&packet[..packet_len])?;
                datagrams += 1;
                repairs += 1;
                pacer.account_and_wait(packet_len);
            }
        }
    }

    control.write_all(b"END\n")?;
    control.flush()?;

    while !complete.load(Ordering::Acquire) {
        if let Some(error) = feedback_error
            .lock()
            .expect("feedback error mutex poisoned")
            .clone()
        {
            return Err(error.into());
        }

        let repairs_now = take_repairs(&pending, 1024);
        if repairs_now.is_empty() {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        for repair in repairs_now {
            let packet_len = build_packet(&file, config.size, config.session, repair, &mut packet)?;
            socket.send(&packet[..packet_len])?;
            datagrams += 1;
            repairs += 1;
            pacer.account_and_wait(packet_len);
        }
    }

    feedback_thread
        .join()
        .map_err(|_| "feedback thread panicked")?;
    Ok((datagrams, repairs))
}

struct RepairQueue {
    queue: VecDeque<u64>,
    queued: HashSet<u64>,
    last_sent: std::collections::HashMap<u64, Instant>,
    cooldown: Duration,
}

impl RepairQueue {
    fn new(cooldown: Duration) -> Self {
        Self {
            queue: VecDeque::new(),
            queued: HashSet::new(),
            last_sent: std::collections::HashMap::new(),
            cooldown,
        }
    }
}

fn read_feedback(
    stream: TcpStream,
    pending: Arc<Mutex<RepairQueue>>,
    complete: Arc<AtomicBool>,
) -> AnyResult<()> {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line == "COMPLETE" {
            complete.store(true, Ordering::Release);
            return Ok(());
        }
        if let Some(ranges) = line.strip_prefix("M ") {
            let mut queue = pending.lock().expect("repair queue mutex poisoned");
            let now = Instant::now();
            let cooldown = queue.cooldown;
            queue
                .last_sent
                .retain(|_, sent_at| now.duration_since(*sent_at) < cooldown);
            for range in ranges.split(',').filter(|part| !part.is_empty()) {
                let (start, end) = parse_range(range)?;
                for sequence in start..=end {
                    if !queue.last_sent.contains_key(&sequence) && queue.queued.insert(sequence) {
                        queue.queue.push_back(sequence);
                    }
                }
            }
        } else if let Some(message) = line.strip_prefix("ERROR ") {
            return Err(format!("receiver error: {message}").into());
        }
    }
    if !complete.load(Ordering::Acquire) {
        return Err("control connection closed before completion".into());
    }
    Ok(())
}

fn parse_range(value: &str) -> AnyResult<(u64, u64)> {
    if let Some((start, end)) = value.split_once('-') {
        let start = start.parse()?;
        let end = end.parse()?;
        if end < start {
            return Err(format!("invalid missing range {value}").into());
        }
        Ok((start, end))
    } else {
        let sequence = value.parse()?;
        Ok((sequence, sequence))
    }
}

fn take_repairs(pending: &Arc<Mutex<RepairQueue>>, limit: usize) -> Vec<u64> {
    let mut pending = pending.lock().expect("repair queue mutex poisoned");
    let mut result = Vec::with_capacity(limit.min(pending.queue.len()));
    for _ in 0..limit {
        let Some(sequence) = pending.queue.pop_front() else {
            break;
        };
        pending.queued.remove(&sequence);
        pending.last_sent.insert(sequence, Instant::now());
        result.push(sequence);
    }
    result
}

fn build_packet(
    file: &File,
    size: u64,
    session: u64,
    sequence: u64,
    packet: &mut [u8],
) -> AnyResult<usize> {
    let offset = sequence
        .checked_mul(PAYLOAD_SIZE as u64)
        .ok_or("packet offset overflow")?;
    if offset >= size && size != 0 {
        return Err(format!("sequence {sequence} is outside the file").into());
    }
    let payload_len = (size.saturating_sub(offset)).min(PAYLOAD_SIZE as u64) as usize;
    packet[0..8].copy_from_slice(&session.to_be_bytes());
    packet[8..16].copy_from_slice(&sequence.to_be_bytes());
    packet[16..18].copy_from_slice(&(payload_len as u16).to_be_bytes());
    let mut read = 0;
    while read < payload_len {
        let count = file.read_at(
            &mut packet[HEADER_SIZE + read..HEADER_SIZE + payload_len],
            offset + read as u64,
        )?;
        if count == 0 {
            return Err("unexpected EOF while building UDP packet".into());
        }
        read += count;
    }
    Ok(HEADER_SIZE + payload_len)
}

struct Pacer {
    bytes_per_second: f64,
    started: Instant,
    bytes_sent: u64,
}

impl Pacer {
    fn new(bytes_per_second: f64) -> Self {
        Self {
            bytes_per_second,
            started: Instant::now(),
            bytes_sent: 0,
        }
    }

    fn account_and_wait(&mut self, bytes: usize) {
        self.bytes_sent += bytes as u64;
        let target = Duration::from_secs_f64(self.bytes_sent as f64 / self.bytes_per_second);
        let elapsed = self.started.elapsed();
        if target > elapsed {
            thread::sleep(target - elapsed);
        }
    }
}

fn receive(args: ReceiveArgs) -> AnyResult<()> {
    let udp = UdpSocket::bind(args.udp)?;
    udp.set_read_timeout(Some(Duration::from_millis(20)))?;
    let listener = TcpListener::bind(args.listen)?;
    let (control, peer) = listener.accept()?;
    control.set_nodelay(true)?;

    let mut reader = BufReader::new(control);
    let mut hello = String::new();
    reader.read_line(&mut hello)?;
    let hello = parse_hello(&hello)?;
    let mut control = reader.into_inner();
    control.write_all(b"READY\n")?;
    control.flush()?;

    if let Some(parent) = args.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(&args.out);
    let result = match hello.transport {
        Transport::Tcp => receive_tcp(&mut control, &temporary, &args.out, &hello),
        Transport::Udp => receive_udp(control, udp, &temporary, &args.out, &hello),
    };
    if let Err(error) = &result {
        let _ = fs::remove_file(&temporary);
        eprintln!("receive from {peer} failed: {error}");
    }
    result
}

struct Hello {
    transport: Transport,
    size: u64,
    hash: String,
    session: u64,
    chunks: u64,
}

fn parse_hello(line: &str) -> AnyResult<Hello> {
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 6 || parts[0] != "TSU1" {
        return Err("invalid TSU1 greeting".into());
    }
    let transport = match parts[1] {
        "TCP" => Transport::Tcp,
        "UDP" => Transport::Udp,
        _ => return Err("invalid transport in greeting".into()),
    };
    let hello = Hello {
        transport,
        size: parts[2].parse()?,
        hash: parts[3].to_owned(),
        session: parts[4].parse()?,
        chunks: parts[5].parse()?,
    };
    let expected_chunks = hello.size.div_ceil(PAYLOAD_SIZE as u64);
    if hello.chunks != expected_chunks {
        return Err("chunk count does not match file size".into());
    }
    Ok(hello)
}

fn receive_tcp(
    control: &mut TcpStream,
    temporary: &Path,
    destination: &Path,
    hello: &Hello,
) -> AnyResult<()> {
    let mut file = File::create(temporary)?;
    let mut remaining = hello.size;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    while remaining > 0 {
        let wanted = remaining.min(buffer.len() as u64) as usize;
        let count = control.read(&mut buffer[..wanted])?;
        if count == 0 {
            return Err("TCP data ended before the complete file arrived".into());
        }
        file.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    file.sync_all()?;
    let actual_hash = hex_digest(hasher.finalize().as_slice());
    if actual_hash != hello.hash {
        control.write_all(b"ERROR hash-mismatch\n")?;
        return Err("received TCP file hash did not match".into());
    }
    install_file(temporary, destination)?;
    control.write_all(b"COMPLETE\n")?;
    control.flush()?;
    Ok(())
}

fn receive_udp(
    mut control: TcpStream,
    udp: UdpSocket,
    temporary: &Path,
    destination: &Path,
    hello: &Hello,
) -> AnyResult<()> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(temporary)?;
    file.set_len(hello.size)?;
    let bitmap_words = hello.chunks.div_ceil(64) as usize;
    let mut received = vec![0_u64; bitmap_words];
    let mut received_count = 0_u64;
    let mut highest = None::<u64>;
    let mut ended = false;
    let mut last_report = Instant::now();
    let (control_tx, control_rx) = mpsc::channel();
    let control_reader = control.try_clone()?;
    let control_thread = thread::spawn(move || {
        let mut reader = BufReader::new(control_reader);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = control_tx.send(ControlEvent::Closed);
                    break;
                }
                Ok(_) if line.trim() == "END" => {
                    let _ = control_tx.send(ControlEvent::End);
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = control_tx.send(ControlEvent::Error(error.to_string()));
                    break;
                }
            }
        }
    });

    let mut packet = vec![0_u8; 65_535];
    loop {
        match udp.recv(&mut packet) {
            Ok(count) => {
                if count < HEADER_SIZE {
                    continue;
                }
                let session = u64::from_be_bytes(packet[0..8].try_into()?);
                if session != hello.session {
                    continue;
                }
                let sequence = u64::from_be_bytes(packet[8..16].try_into()?);
                let payload_len = u16::from_be_bytes(packet[16..18].try_into()?) as usize;
                if sequence >= hello.chunks
                    || payload_len > PAYLOAD_SIZE
                    || count != HEADER_SIZE + payload_len
                {
                    continue;
                }
                let offset = sequence * PAYLOAD_SIZE as u64;
                let expected_len =
                    (hello.size.saturating_sub(offset)).min(PAYLOAD_SIZE as u64) as usize;
                if payload_len != expected_len {
                    continue;
                }
                if !bitmap_contains(&received, sequence) {
                    write_all_at(
                        &file,
                        &packet[HEADER_SIZE..HEADER_SIZE + payload_len],
                        offset,
                    )?;
                    bitmap_insert(&mut received, sequence);
                    received_count += 1;
                }
                highest = Some(highest.map_or(sequence, |old| old.max(sequence)));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }

        while let Ok(event) = control_rx.try_recv() {
            match event {
                ControlEvent::End => ended = true,
                ControlEvent::Closed => {
                    return Err("control connection closed before UDP completion".into());
                }
                ControlEvent::Error(error) => return Err(error.into()),
            }
        }

        if ended && received_count == hello.chunks {
            file.sync_all()?;
            let actual_hash = hash_file(temporary)?;
            if actual_hash != hello.hash {
                control.write_all(b"ERROR hash-mismatch\n")?;
                return Err("received UDP file hash did not match".into());
            }
            install_file(temporary, destination)?;
            control.write_all(b"COMPLETE\n")?;
            control.flush()?;
            break;
        }

        if last_report.elapsed() >= REPORT_INTERVAL {
            let report_end = if ended {
                hello.chunks
            } else {
                highest.map_or(0, |value| value + 1)
            };
            let ranges = missing_ranges(&received, report_end, MAX_RANGES_PER_REPORT);
            write!(control, "M ")?;
            for (index, (start, end)) in ranges.iter().enumerate() {
                if index > 0 {
                    write!(control, ",")?;
                }
                if start == end {
                    write!(control, "{start}")?;
                } else {
                    write!(control, "{start}-{end}")?;
                }
            }
            writeln!(control)?;
            control.flush()?;
            last_report = Instant::now();
        }
    }

    control_thread
        .join()
        .map_err(|_| "control reader thread panicked")?;
    Ok(())
}

enum ControlEvent {
    End,
    Closed,
    Error(String),
}

fn bitmap_contains(bitmap: &[u64], sequence: u64) -> bool {
    let word = (sequence / 64) as usize;
    let bit = sequence % 64;
    bitmap
        .get(word)
        .is_some_and(|value| value & (1_u64 << bit) != 0)
}

fn bitmap_insert(bitmap: &mut [u64], sequence: u64) {
    let word = (sequence / 64) as usize;
    let bit = sequence % 64;
    bitmap[word] |= 1_u64 << bit;
}

fn missing_ranges(bitmap: &[u64], end: u64, max_ranges: usize) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut sequence = 0_u64;
    while sequence < end && ranges.len() < max_ranges {
        if bitmap_contains(bitmap, sequence) {
            sequence += 1;
            continue;
        }
        let start = sequence;
        while sequence + 1 < end && !bitmap_contains(bitmap, sequence + 1) {
            sequence += 1;
        }
        ranges.push((start, sequence));
        sequence += 1;
    }
    ranges
}

fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> AnyResult<()> {
    while !bytes.is_empty() {
        let count = file.write_at(bytes, offset)?;
        if count == 0 {
            return Err("short positional file write".into());
        }
        bytes = &bytes[count..];
        offset += count as u64;
    }
    Ok(())
}

fn temporary_path(destination: &Path) -> PathBuf {
    let mut value = destination.as_os_str().to_owned();
    value.push(format!(".part.{}", std::process::id()));
    value.into()
}

fn install_file(temporary: &Path, destination: &Path) -> AnyResult<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(temporary, destination)?;
    Ok(())
}

fn hash_file(path: &Path) -> AnyResult<String> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn new_session_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    nanos ^ (std::process::id() as u64).rotate_left(32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_range_encoding() {
        let mut bitmap = vec![0_u64; 1];
        for sequence in [0, 1, 4, 7, 8, 9] {
            bitmap_insert(&mut bitmap, sequence);
        }
        assert_eq!(missing_ranges(&bitmap, 10, 10), vec![(2, 3), (5, 6)]);
    }

    #[test]
    fn range_parser() {
        assert_eq!(parse_range("7").unwrap(), (7, 7));
        assert_eq!(parse_range("7-11").unwrap(), (7, 11));
        assert!(parse_range("11-7").is_err());
    }
}
