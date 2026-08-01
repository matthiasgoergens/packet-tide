use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

mod auth;
mod resume;

use auth::{ControlReader, ControlWriter, Direction, SecretKey, SessionAuth};
use resume::ResumeState;

const DATAGRAM_SIZE: usize = 1200;
const HEADER_SIZE: usize = 28;
const PAYLOAD_SIZE: usize = DATAGRAM_SIZE - HEADER_SIZE;
const REPORT_INTERVAL: Duration = Duration::from_millis(50);
const MAX_RANGES_PER_REPORT: usize = 512;
const TCP4_LANES: usize = 4;
const RESUME_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_QUEUED_REPAIRS: usize = 65_536;
const MAX_BITMAP_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHUNKS: u64 = (MAX_BITMAP_BYTES as u64) * 8;

type AnyResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transport {
    Tcp,
    Tcp4,
    Udp,
}

impl Transport {
    fn parse(value: &str) -> AnyResult<Self> {
        match value {
            "tcp" => Ok(Self::Tcp),
            "tcp4" => Ok(Self::Tcp4),
            "udp" => Ok(Self::Udp),
            _ => Err(format!("unknown transport {value:?}; expected tcp, tcp4, or udp").into()),
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Self::Tcp => "TCP",
            Self::Tcp4 => "TCP4",
            Self::Udp => "UDP",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Tcp4 => "tcp4",
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
    key_file: PathBuf,
}

#[derive(Debug)]
struct ReceiveArgs {
    listen: SocketAddr,
    udp: SocketAddr,
    out: PathBuf,
    key_file: PathBuf,
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
        Some("keygen") => {
            let args: Vec<_> = args.collect();
            validate_options(&args, &["--out"])?;
            SecretKey::generate(Path::new(&option(&args, "--out")?))?;
            Ok(())
        }
        Some("--help" | "-h" | "help") => {
            usage();
            Ok(())
        }
        Some("--version" | "-V") => {
            println!("tsunami-udp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            usage();
            Err("expected send, receive, or keygen subcommand".into())
        }
    }
}

fn usage() {
    eprintln!(
        "usage:\n  tsunami-udp receive --listen ADDR --udp ADDR --out PATH --key-file PATH\n  \
         tsunami-udp send --connect ADDR --udp-target ADDR --file PATH \
         --transport tcp|tcp4|udp --key-file PATH [--rate-mbps N] [--repair-cooldown-ms N]\n  \
         tsunami-udp keygen --out PATH"
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

fn validate_options(args: &[String], allowed: &[&str]) -> AnyResult<()> {
    if args.len() & 1 != 0 {
        return Err("every option must have a value".into());
    }
    let mut seen = HashSet::new();
    for pair in args.chunks_exact(2) {
        if !allowed.contains(&pair[0].as_str()) {
            return Err(format!("unknown option {}", pair[0]).into());
        }
        if !seen.insert(&pair[0]) {
            return Err(format!("duplicate option {}", pair[0]).into());
        }
    }
    Ok(())
}

fn resolve_socket(value: &str) -> AnyResult<SocketAddr> {
    value
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| format!("address {value:?} did not resolve").into())
}

fn parse_send(args: Vec<String>) -> AnyResult<SendArgs> {
    validate_options(
        &args,
        &[
            "--connect",
            "--udp-target",
            "--file",
            "--transport",
            "--rate-mbps",
            "--repair-cooldown-ms",
            "--key-file",
        ],
    )?;
    Ok(SendArgs {
        connect: resolve_socket(&option(&args, "--connect")?)?,
        udp_target: resolve_socket(&option(&args, "--udp-target")?)?,
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
        key_file: option(&args, "--key-file")?.into(),
    })
}

fn parse_receive(args: Vec<String>) -> AnyResult<ReceiveArgs> {
    validate_options(&args, &["--listen", "--udp", "--out", "--key-file"])?;
    Ok(ReceiveArgs {
        listen: resolve_socket(&option(&args, "--listen")?)?,
        udp: resolve_socket(&option(&args, "--udp")?)?,
        out: option(&args, "--out")?.into(),
        key_file: option(&args, "--key-file")?.into(),
    })
}

fn send(args: SendArgs) -> AnyResult<()> {
    let key = SecretKey::load(&args.key_file)?;
    let size = fs::metadata(&args.file)?.len();
    let expected_hash = hash_file(&args.file)?;
    let offered_session = auth::random_session()?;
    let chunks = size.div_ceil(PAYLOAD_SIZE as u64);
    if chunks > MAX_CHUNKS {
        return Err(format!(
            "file requires {chunks} chunks; maximum is {MAX_CHUNKS} to keep receipt memory bounded"
        )
        .into());
    }

    let mut control = TcpStream::connect(args.connect)?;
    control.set_nodelay(true)?;
    let hello = format!(
        "{} {} {} {} {} {}",
        args.transport.wire_name(),
        size,
        expected_hash,
        auth::hex(&offered_session),
        chunks,
        (args.repair_cooldown / 2).max(REPORT_INTERVAL).as_millis()
    );
    let session_auth = auth::client_handshake(&mut control, &key, &hello)?;
    let mut control_reader = ControlReader::new(
        control.try_clone()?,
        session_auth.clone(),
        Direction::ServerToClient,
    );
    let mut control_writer = ControlWriter::new(
        control.try_clone()?,
        session_auth.clone(),
        Direction::ClientToServer,
    );

    let (durable_chunks, already_complete) =
        read_acceptance(&mut control_reader, args.transport, chunks)?;
    let resumed_chunks = durable_chunks
        .iter()
        .map(|word| word.count_ones() as u64)
        .sum::<u64>();
    if already_complete {
        println!(
            "{{\"transport\":\"{}\",\"bytes\":{},\"elapsed_ms\":0.0,\
             \"goodput_mbps\":0.0,\"datagrams\":0,\"repairs\":0,\
             \"udp_ip_bytes_offered\":0,\"resumed_chunks\":{}}}",
            args.transport.display_name(),
            size,
            chunks
        );
        return Ok(());
    }

    let started = Instant::now();
    let (datagrams, repairs, udp_ip_bytes_offered) = match args.transport {
        Transport::Tcp => {
            send_tcp(&args.file, &mut control)?;
            expect_completion(&mut control_reader)?;
            (0, 0, 0)
        }
        Transport::Tcp4 => {
            send_tcp4(&args.file, args.connect, &session_auth)?;
            expect_completion(&mut control_reader)?;
            (0, 0, 0)
        }
        Transport::Udp => {
            let config = UdpSendConfig {
                size,
                auth: session_auth.clone(),
                chunks,
                target: args.udp_target,
                rate_mbps: args.rate_mbps,
                repair_cooldown: args.repair_cooldown,
                durable_chunks,
            };
            send_udp(&args.file, &config, &mut control_writer, control_reader)?
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
         \"goodput_mbps\":{:.3},\"datagrams\":{},\"repairs\":{},\
         \"udp_ip_bytes_offered\":{},\"resumed_chunks\":{}}}",
        args.transport.display_name(),
        size,
        elapsed.as_secs_f64() * 1000.0,
        goodput_mbps,
        datagrams,
        repairs,
        udp_ip_bytes_offered,
        resumed_chunks
    );
    Ok(())
}

fn expect_completion(control: &mut ControlReader) -> AnyResult<()> {
    let completion = control.recv()?;
    if completion != "COMPLETE" {
        return Err(format!("transfer did not complete: {completion}").into());
    }
    Ok(())
}

fn read_acceptance(
    control: &mut ControlReader,
    transport: Transport,
    chunks: u64,
) -> AnyResult<(Vec<u64>, bool)> {
    let mut line = control.recv()?;
    if line == "COMPLETE" {
        return Ok((Vec::new(), true));
    }
    if line != "READY" {
        return Err(format!("receiver rejected transfer: {line}").into());
    }
    if transport != Transport::Udp {
        return Ok((Vec::new(), false));
    }

    let words = usize::try_from(chunks.div_ceil(64))?;
    let mut bitmap = vec![0_u64; words];
    loop {
        line = control.recv()?;
        let trimmed = line.as_str();
        if trimmed == "GO" {
            break;
        }
        let fields: Vec<_> = trimmed.split_whitespace().collect();
        if fields.len() != 3 || fields[0] != "H" {
            return Err(format!("invalid resume response: {trimmed}").into());
        }
        let index: usize = fields[1].parse()?;
        let word = u64::from_str_radix(fields[2], 16)?;
        if index >= bitmap.len() || bitmap[index] != 0 || word == 0 {
            return Err("invalid or duplicate resume bitmap word".into());
        }
        bitmap[index] = word;
    }
    if chunks & 63 != 0
        && bitmap.last().is_some_and(|word| {
            let valid_mask = (1_u64 << (chunks % 64)) - 1;
            word & !valid_mask != 0
        })
    {
        return Err("resume bitmap contains chunks beyond the file".into());
    }
    Ok((bitmap, false))
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

fn send_tcp4(path: &Path, connect: SocketAddr, auth: &SessionAuth) -> AnyResult<()> {
    let size = fs::metadata(path)?.len();
    let mut lanes = Vec::with_capacity(TCP4_LANES);
    for lane in 0..TCP4_LANES {
        let mut stream = TcpStream::connect(connect)?;
        stream.set_nodelay(true)?;
        let mac = auth::lane_mac(&auth.lane, &auth.session, lane);
        writeln!(
            stream,
            "TSU2D {} {lane} {}",
            auth::hex(&auth.session),
            auth::hex(&mac)
        )?;
        stream.flush()?;
        lanes.push((lane, stream));
    }

    let mut workers = Vec::with_capacity(TCP4_LANES);
    for (lane, stream) in lanes {
        let path = path.to_owned();
        let (start, end) = tcp4_lane_range(size, lane);
        workers.push(thread::spawn(move || {
            send_tcp_range(&path, stream, start, end)
        }));
    }
    for worker in workers {
        worker.join().map_err(|_| "TCP4 sender thread panicked")??;
    }
    Ok(())
}

fn send_tcp_range(path: &Path, mut stream: TcpStream, start: u64, end: u64) -> AnyResult<()> {
    let file = File::open(path)?;
    let mut offset = start;
    let mut buffer = vec![0_u8; 1024 * 1024];
    while offset < end {
        let wanted = (end - offset).min(buffer.len() as u64) as usize;
        let count = file.read_at(&mut buffer[..wanted], offset)?;
        if count == 0 {
            return Err("unexpected EOF while sending TCP4 range".into());
        }
        stream.write_all(&buffer[..count])?;
        offset += count as u64;
    }
    stream.flush()?;
    Ok(())
}

fn tcp4_lane_range(size: u64, lane: usize) -> (u64, u64) {
    debug_assert!(lane < TCP4_LANES);
    let lanes = TCP4_LANES as u64;
    let base = size / lanes;
    let remainder = size % lanes;
    let start = base * lane as u64 + remainder.min(lane as u64);
    let length = base + if (lane as u64) < remainder { 1 } else { 0 };
    (start, start + length)
}

struct UdpSendConfig {
    size: u64,
    auth: SessionAuth,
    chunks: u64,
    target: SocketAddr,
    rate_mbps: f64,
    repair_cooldown: Duration,
    durable_chunks: Vec<u64>,
}

fn send_udp(
    path: &Path,
    config: &UdpSendConfig,
    control: &mut ControlWriter,
    control_reader: ControlReader,
) -> AnyResult<(u64, u64, u64)> {
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

    let pending_for_reader = Arc::clone(&pending);
    let complete_for_reader = Arc::clone(&complete);
    let error_for_reader = Arc::clone(&feedback_error);
    let chunks_for_reader = config.chunks;
    let feedback_thread = thread::spawn(move || {
        if let Err(error) = read_feedback(
            control_reader,
            pending_for_reader,
            complete_for_reader,
            chunks_for_reader,
        ) {
            *error_for_reader
                .lock()
                .expect("feedback error mutex poisoned") = Some(error.to_string());
        }
    });

    let mut pacer = Pacer::new(config.rate_mbps * 1_000_000.0 / 8.0);
    let mut packet = vec![0_u8; DATAGRAM_SIZE];
    let mut datagrams = 0_u64;
    let mut repairs = 0_u64;
    let mut udp_ip_bytes_offered = 0_u64;

    for sequence in 0..config.chunks {
        if bitmap_contains(&config.durable_chunks, sequence) {
            continue;
        }
        let packet_len = build_packet(
            &file,
            config.size,
            &config.auth,
            sequence,
            false,
            &mut packet,
        )?;
        socket.send(&packet[..packet_len])?;
        datagrams += 1;
        udp_ip_bytes_offered += packet_len as u64 + 28;
        pacer.account_and_wait(packet_len + 28);

        if sequence % 16 == 15 {
            for repair in take_repairs(&pending, 4) {
                let packet_len =
                    build_packet(&file, config.size, &config.auth, repair, true, &mut packet)?;
                socket.send(&packet[..packet_len])?;
                datagrams += 1;
                repairs += 1;
                udp_ip_bytes_offered += packet_len as u64 + 28;
                pacer.account_and_wait(packet_len + 28);
            }
        }
    }

    control.send("END")?;

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
            let packet_len =
                build_packet(&file, config.size, &config.auth, repair, true, &mut packet)?;
            socket.send(&packet[..packet_len])?;
            datagrams += 1;
            repairs += 1;
            udp_ip_bytes_offered += packet_len as u64 + 28;
            pacer.account_and_wait(packet_len + 28);
        }
    }

    feedback_thread
        .join()
        .map_err(|_| "feedback thread panicked")?;
    Ok((datagrams, repairs, udp_ip_bytes_offered))
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
    mut reader: ControlReader,
    pending: Arc<Mutex<RepairQueue>>,
    complete: Arc<AtomicBool>,
    chunks: u64,
) -> AnyResult<()> {
    loop {
        let line = reader.recv()?;
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
            enqueue_repairs(&mut queue, ranges, chunks)?;
        } else if let Some(message) = line.strip_prefix("ERROR ") {
            return Err(format!("receiver error: {message}").into());
        }
    }
}

fn enqueue_repairs(queue: &mut RepairQueue, ranges: &str, chunks: u64) -> AnyResult<()> {
    'reports: for range in ranges.split(',').filter(|part| !part.is_empty()) {
        let (start, end) = parse_range(range)?;
        if end >= chunks {
            return Err("receiver requested a chunk beyond the file".into());
        }
        for sequence in start..=end {
            if queue.queued.len() >= MAX_QUEUED_REPAIRS {
                break 'reports;
            }
            if !queue.last_sent.contains_key(&sequence) && queue.queued.insert(sequence) {
                queue.queue.push_back(sequence);
            }
        }
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
        if pending.last_sent.len() >= MAX_QUEUED_REPAIRS
            && let Some(oldest) = pending.last_sent.keys().next().copied()
        {
            pending.last_sent.remove(&oldest);
        }
        pending.last_sent.insert(sequence, Instant::now());
        result.push(sequence);
    }
    result
}

fn build_packet(
    file: &File,
    size: u64,
    auth: &SessionAuth,
    sequence: u64,
    repair: bool,
    packet: &mut [u8],
) -> AnyResult<usize> {
    let offset = sequence
        .checked_mul(PAYLOAD_SIZE as u64)
        .ok_or("packet offset overflow")?;
    if offset >= size && size != 0 {
        return Err(format!("sequence {sequence} is outside the file").into());
    }
    let payload_len = (size.saturating_sub(offset)).min(PAYLOAD_SIZE as u64) as usize;
    packet[0] = 2;
    packet[1..9].copy_from_slice(&sequence.to_be_bytes());
    packet[9] = u8::from(repair);
    packet[10..12].copy_from_slice(&(payload_len as u16).to_be_bytes());
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
    let tag = auth::udp_tag_parts(
        &auth.udp,
        &packet[..12],
        &packet[HEADER_SIZE..HEADER_SIZE + payload_len],
    );
    packet[12..28].copy_from_slice(&tag);
    Ok(HEADER_SIZE + payload_len)
}

struct Pacer {
    bytes_per_second: f64,
    started: Instant,
    bytes_sent: u64,
    bytes_since_wait: usize,
    batch_bytes: usize,
}

impl Pacer {
    fn new(bytes_per_second: f64) -> Self {
        // Sleeping once per datagram makes sub-millisecond scheduler wake-up
        // overhead part of the configured rate. Pace in short bounded batches
        // instead: this keeps the long-run byte rate while allowing the kernel
        // qdisc and NIC to handle a small burst efficiently.
        let batch_bytes = (bytes_per_second * 0.002).clamp(1_200.0, 65_536.0) as usize;
        Self {
            bytes_per_second,
            started: Instant::now(),
            bytes_sent: 0,
            bytes_since_wait: 0,
            batch_bytes,
        }
    }

    fn account_and_wait(&mut self, bytes: usize) {
        self.bytes_sent += bytes as u64;
        self.bytes_since_wait += bytes;
        if self.bytes_since_wait < self.batch_bytes {
            return;
        }
        self.bytes_since_wait = 0;
        let target = Duration::from_secs_f64(self.bytes_sent as f64 / self.bytes_per_second);
        let elapsed = self.started.elapsed();
        if target > elapsed {
            thread::sleep(target - elapsed);
        }
    }
}

fn receive(args: ReceiveArgs) -> AnyResult<()> {
    let key = SecretKey::load(&args.key_file)?;
    let udp = UdpSocket::bind(args.udp)?;
    udp.set_read_timeout(Some(Duration::from_millis(20)))?;
    let listener = TcpListener::bind(args.listen)?;
    let (mut control, peer) = listener.accept()?;
    control.set_nodelay(true)?;
    let (hello_body, session_auth) = auth::server_handshake(&mut control, &key)?;
    let mut hello = parse_hello(&hello_body)?;
    hello.session = session_auth.session;
    let mut control_writer = ControlWriter::new(
        control.try_clone()?,
        session_auth.clone(),
        Direction::ServerToClient,
    );
    let control_reader = ControlReader::new(
        control.try_clone()?,
        session_auth.clone(),
        Direction::ClientToServer,
    );

    if let Some(parent) = args.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(&args.out);
    let result = match hello.transport {
        Transport::Tcp => {
            control_writer.send("READY")?;
            receive_tcp(
                &mut control,
                &mut control_writer,
                &temporary,
                &args.out,
                &hello,
            )
        }
        Transport::Tcp4 => {
            control_writer.send("READY")?;
            receive_tcp4(
                &listener,
                &mut control_writer,
                &temporary,
                &args.out,
                &hello,
                &session_auth,
            )
        }
        Transport::Udp => receive_udp(
            control_writer,
            control_reader,
            udp,
            &args.out,
            &hello,
            &session_auth,
        ),
    };
    if let Err(error) = &result {
        if hello.transport != Transport::Udp {
            let _ = fs::remove_file(&temporary);
        }
        eprintln!("receive from {peer} failed: {error}");
    }
    result
}

struct Hello {
    transport: Transport,
    size: u64,
    hash: String,
    session: [u8; 16],
    chunks: u64,
    repair_grace: Duration,
}

fn parse_hello(line: &str) -> AnyResult<Hello> {
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 6 {
        return Err("invalid authenticated TSU2 greeting body".into());
    }
    let transport = match parts[0] {
        "TCP" => Transport::Tcp,
        "TCP4" => Transport::Tcp4,
        "UDP" => Transport::Udp,
        _ => return Err("invalid transport in greeting".into()),
    };
    let hello = Hello {
        transport,
        size: parts[1].parse()?,
        hash: parts[2].to_owned(),
        session: auth::decode_array::<16>(parts[3])?,
        chunks: parts[4].parse()?,
        repair_grace: Duration::from_millis(parts[5].parse()?),
    };
    auth::decode_array::<32>(&hello.hash)?;
    let expected_chunks = hello.size.div_ceil(PAYLOAD_SIZE as u64);
    if hello.chunks != expected_chunks {
        return Err("chunk count does not match file size".into());
    }
    if hello.chunks > MAX_CHUNKS {
        return Err("file exceeds the bounded receipt-map limit".into());
    }
    if hello.repair_grace > Duration::from_secs(60) {
        return Err("repair grace exceeds 60 seconds".into());
    }
    Ok(hello)
}

fn receive_tcp(
    control: &mut TcpStream,
    control_writer: &mut ControlWriter,
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
        control_writer.send("ERROR hash-mismatch")?;
        return Err("received TCP file hash did not match".into());
    }
    install_file(temporary, destination)?;
    control_writer.send("COMPLETE")?;
    Ok(())
}

fn receive_tcp4(
    listener: &TcpListener,
    control: &mut ControlWriter,
    temporary: &Path,
    destination: &Path,
    hello: &Hello,
    session_auth: &SessionAuth,
) -> AnyResult<()> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(temporary)?;
    file.set_len(hello.size)?;

    let mut lanes: Vec<Option<TcpStream>> = (0..TCP4_LANES).map(|_| None).collect();
    for _ in 0..TCP4_LANES {
        let (mut stream, _) = listener.accept()?;
        stream.set_nodelay(true)?;
        let lane = read_tcp4_hello(&mut stream, session_auth)?;
        if lane >= TCP4_LANES || lanes[lane].is_some() {
            return Err("invalid or duplicate TCP4 data connection".into());
        }
        lanes[lane] = Some(stream);
    }

    let mut workers = Vec::with_capacity(TCP4_LANES);
    for (lane, stream) in lanes.into_iter().enumerate() {
        let stream = stream.ok_or("missing TCP4 data connection")?;
        let file = file.try_clone()?;
        let (start, end) = tcp4_lane_range(hello.size, lane);
        workers.push(thread::spawn(move || {
            receive_tcp4_range(stream, file, start, end)
        }));
    }
    for worker in workers {
        worker
            .join()
            .map_err(|_| "TCP4 receiver thread panicked")??;
    }

    file.sync_all()?;
    let actual_hash = hash_file(temporary)?;
    if actual_hash != hello.hash {
        control.send("ERROR hash-mismatch")?;
        return Err("received TCP4 file hash did not match".into());
    }
    install_file(temporary, destination)?;
    control.send("COMPLETE")?;
    Ok(())
}

fn read_tcp4_hello(stream: &mut TcpStream, session_auth: &SessionAuth) -> AnyResult<usize> {
    let mut greeting = Vec::with_capacity(64);
    while greeting.len() < 128 {
        let mut byte = [0_u8; 1];
        if stream.read(&mut byte)? == 0 {
            return Err("TCP4 data connection ended before its greeting".into());
        }
        if byte[0] == b'\n' {
            break;
        }
        greeting.push(byte[0]);
    }
    if greeting.len() == 128 {
        return Err("TCP4 data greeting is too long".into());
    }
    let greeting = std::str::from_utf8(&greeting)?;
    let parts: Vec<_> = greeting.split_whitespace().collect();
    if parts.len() != 4 || parts[0] != "TSU2D" {
        return Err("invalid TCP4 data greeting".into());
    }
    let session = auth::decode_array::<16>(parts[1])?;
    let lane: usize = parts[2].parse()?;
    let supplied = auth::decode_array::<32>(parts[3])?;
    if session != session_auth.session || lane >= TCP4_LANES {
        return Err("invalid TCP4 session or lane".into());
    }
    let expected = auth::lane_mac(&session_auth.lane, &session, lane);
    if !auth::constant_time_verify(&supplied, &expected) {
        return Err("TCP4 lane authentication failed".into());
    }
    Ok(lane)
}

fn receive_tcp4_range(mut stream: TcpStream, file: File, start: u64, end: u64) -> AnyResult<()> {
    let mut offset = start;
    let mut buffer = vec![0_u8; 1024 * 1024];
    while offset < end {
        let wanted = (end - offset).min(buffer.len() as u64) as usize;
        let count = stream.read(&mut buffer[..wanted])?;
        if count == 0 {
            return Err("TCP4 data ended before its complete range arrived".into());
        }
        write_all_at(&file, &buffer[..count], offset)?;
        offset += count as u64;
    }
    Ok(())
}

fn receive_udp(
    mut control: ControlWriter,
    control_reader: ControlReader,
    udp: UdpSocket,
    destination: &Path,
    hello: &Hello,
    session_auth: &SessionAuth,
) -> AnyResult<()> {
    if destination
        .metadata()
        .is_ok_and(|metadata| metadata.len() == hello.size)
        && hash_file(destination)? == hello.hash
    {
        control.send("COMPLETE")?;
        return Ok(());
    }

    let (file, mut state) = ResumeState::open(destination, hello.size, hello.chunks, &hello.hash)?;
    let mut mapped_file = MappedFile::new(&file, hello.size)?;
    control.send("READY")?;
    for (index, word) in state.bitmap.iter().copied().enumerate() {
        if word != 0 {
            control.send(&format!("H {index} {word:016x}"))?;
        }
    }
    control.send("GO")?;

    let mut highest = highest_received(&state.bitmap);
    let mut ended = false;
    let mut ended_at = None::<Instant>;
    let mut reordering_observed = false;
    let mut last_report = Instant::now();
    let mut frontier_history = VecDeque::new();
    frontier_history.push_back((Instant::now(), highest.map_or(0, |value| value + 1)));
    let mut last_checkpoint = Instant::now();
    let mut checkpoint_dirty = false;
    let (control_tx, control_rx) = mpsc::channel();
    let control_thread = thread::spawn(move || {
        let mut reader = control_reader;
        loop {
            match reader.recv() {
                Ok(line) if line == "END" => {
                    let _ = control_tx.send(ControlEvent::End);
                }
                Ok(_) => {
                    let _ = control_tx
                        .send(ControlEvent::Error("unexpected control message".to_owned()));
                    break;
                }
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
                if packet[0] != 2 {
                    continue;
                }
                let sequence = u64::from_be_bytes(packet[1..9].try_into()?);
                let repair = match packet[9] {
                    0 => false,
                    1 => true,
                    _ => continue,
                };
                let payload_len = u16::from_be_bytes(packet[10..12].try_into()?) as usize;
                if sequence >= hello.chunks
                    || payload_len > PAYLOAD_SIZE
                    || count != HEADER_SIZE + payload_len
                {
                    continue;
                }
                if !auth::verify_udp_tag_parts(
                    &session_auth.udp,
                    &packet[..12],
                    &packet[HEADER_SIZE..count],
                    &packet[12..28],
                ) {
                    continue;
                }
                let offset = sequence * PAYLOAD_SIZE as u64;
                let expected_len =
                    (hello.size.saturating_sub(offset)).min(PAYLOAD_SIZE as u64) as usize;
                if payload_len != expected_len {
                    continue;
                }
                if !bitmap_contains(&state.bitmap, sequence) {
                    if !repair && highest.is_some_and(|frontier| sequence < frontier) {
                        reordering_observed = true;
                    }
                    mapped_file
                        .write_at(&packet[HEADER_SIZE..HEADER_SIZE + payload_len], offset)?;
                    bitmap_insert(&mut state.bitmap, sequence);
                    state.received_count += 1;
                    checkpoint_dirty = true;
                }
                highest = Some(highest.map_or(sequence, |old| old.max(sequence)));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                if checkpoint_dirty {
                    mapped_file.flush()?;
                    state.checkpoint(&file)?;
                }
                return Err(error.into());
            }
        }

        while let Ok(event) = control_rx.try_recv() {
            match event {
                ControlEvent::End => {
                    ended = true;
                    ended_at.get_or_insert_with(Instant::now);
                }
                ControlEvent::Error(error) => {
                    if checkpoint_dirty {
                        mapped_file.flush()?;
                        state.checkpoint(&file)?;
                    }
                    return Err(error.into());
                }
            }
        }

        if ended && state.received_count == hello.chunks {
            mapped_file.flush()?;
            file.sync_all()?;
            let actual_hash = hash_open_file(&file)?;
            if actual_hash != hello.hash {
                control.send("ERROR hash-mismatch")?;
                state.discard();
                return Err("received UDP file hash did not match".into());
            }
            state.install(destination)?;
            control.send("COMPLETE")?;
            break;
        }

        if checkpoint_dirty && last_checkpoint.elapsed() >= RESUME_CHECKPOINT_INTERVAL {
            mapped_file.flush()?;
            state.checkpoint(&file)?;
            checkpoint_dirty = false;
            last_checkpoint = Instant::now();
        }

        if last_report.elapsed() >= REPORT_INTERVAL {
            let now = Instant::now();
            let active_grace = if reordering_observed {
                hello.repair_grace
            } else {
                Duration::ZERO
            };
            let mature_end = mature_frontier(
                &mut frontier_history,
                now,
                highest.map_or(0, |value| value + 1),
                active_grace,
                hello.repair_grace,
            );
            let report_end = if ended
                && ended_at.is_some_and(|value| now.duration_since(value) >= active_grace)
            {
                hello.chunks
            } else {
                mature_end
            };
            let ranges = missing_ranges(&state.bitmap, report_end, MAX_RANGES_PER_REPORT);
            let mut report = String::from("M ");
            for (index, (start, end)) in ranges.iter().enumerate() {
                if index > 0 {
                    report.push(',');
                }
                if start == end {
                    report.push_str(&start.to_string());
                } else {
                    report.push_str(&format!("{start}-{end}"));
                }
            }
            control.send(&report)?;
            last_report = Instant::now();
        }
    }

    control_thread
        .join()
        .map_err(|_| "control reader thread panicked")?;
    Ok(())
}

fn highest_received(bitmap: &[u64]) -> Option<u64> {
    bitmap
        .iter()
        .enumerate()
        .rev()
        .find(|(_, word)| **word != 0)
        .map(|(index, word)| index as u64 * 64 + (63 - word.leading_zeros() as u64))
}

fn mature_frontier(
    history: &mut VecDeque<(Instant, u64)>,
    now: Instant,
    current: u64,
    grace: Duration,
    retention: Duration,
) -> u64 {
    if history.back().is_none_or(|(_, value)| *value != current) {
        history.push_back((now, current));
    }
    let cutoff = now.checked_sub(grace).unwrap_or(now);
    let retention_cutoff = now.checked_sub(retention).unwrap_or(now);
    while history.len() > 1
        && history
            .get(1)
            .is_some_and(|(seen, _)| *seen <= retention_cutoff)
    {
        history.pop_front();
    }
    history
        .iter()
        .rev()
        .find(|(seen, _)| *seen <= cutoff)
        .map_or(0, |(_, value)| *value)
}

fn hash_open_file(file: &File) -> AnyResult<String> {
    let mut file = file.try_clone()?;
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

enum ControlEvent {
    End,
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

struct MappedFile {
    address: *mut libc::c_void,
    len: usize,
}

impl MappedFile {
    fn new(file: &File, len: u64) -> AnyResult<Self> {
        let len = usize::try_from(len).map_err(|_| "file is too large to map on this platform")?;
        if len == 0 {
            return Ok(Self {
                address: std::ptr::null_mut(),
                len,
            });
        }
        // SAFETY: the file has already been extended to `len`; the returned
        // mapping is owned by this value and unmapped exactly once in Drop.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { address, len })
    }

    fn write_at(&mut self, bytes: &[u8], offset: u64) -> AnyResult<()> {
        let offset = usize::try_from(offset).map_err(|_| "file offset is too large")?;
        let end = offset
            .checked_add(bytes.len())
            .ok_or("mapped write offset overflow")?;
        if end > self.len {
            return Err("mapped write extends beyond the file".into());
        }
        // SAFETY: the bounds check above proves both regions are valid and the
        // packet buffer cannot overlap the file mapping.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.address.cast::<u8>().add(offset),
                bytes.len(),
            );
        }
        Ok(())
    }

    fn flush(&self) -> AnyResult<()> {
        if self.len == 0 {
            return Ok(());
        }
        // SAFETY: `address..address+len` remains a live MAP_SHARED mapping.
        if unsafe { libc::msync(self.address, self.len, libc::MS_SYNC) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: this mapping is owned by self and Drop runs once.
            let _ = unsafe { libc::munmap(self.address, self.len) };
        }
    }
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

    #[test]
    fn packet_kind_distinguishes_originals_and_repairs() {
        let path =
            std::env::temp_dir().join(format!("tsunami-udp-packet-kind-{}", std::process::id()));
        fs::write(&path, b"payload").unwrap();
        let file = File::open(&path).unwrap();
        let auth = auth::test_session();
        let mut packet = vec![0_u8; DATAGRAM_SIZE];
        let original = build_packet(&file, 7, &auth, 0, false, &mut packet).unwrap();
        assert_eq!(packet[9], 0);
        assert_eq!(u16::from_be_bytes(packet[10..12].try_into().unwrap()), 7);
        assert_eq!(original, HEADER_SIZE + 7);
        let repair = build_packet(&file, 7, &auth, 0, true, &mut packet).unwrap();
        assert_eq!(packet[9], 1);
        assert_eq!(repair, original);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn repair_queue_has_a_hard_entry_limit() {
        let mut queue = RepairQueue::new(Duration::from_secs(60));
        enqueue_repairs(&mut queue, "0-999999", 1_000_000).unwrap();
        assert_eq!(queue.queue.len(), MAX_QUEUED_REPAIRS);
        assert_eq!(queue.queued.len(), MAX_QUEUED_REPAIRS);
        assert!(enqueue_repairs(&mut queue, "1000000", 1_000_000).is_err());

        let pending = Arc::new(Mutex::new(queue));
        let repairs = take_repairs(&pending, MAX_QUEUED_REPAIRS);
        assert_eq!(repairs.len(), MAX_QUEUED_REPAIRS);
        assert!(
            pending
                .lock()
                .expect("repair queue mutex poisoned")
                .last_sent
                .len()
                <= MAX_QUEUED_REPAIRS
        );
    }

    #[test]
    fn frontier_becomes_reportable_only_after_the_grace() {
        let base = Instant::now();
        let grace = Duration::from_millis(100);
        let mut history = VecDeque::from([(base, 0)]);
        assert_eq!(
            mature_frontier(
                &mut history,
                base + Duration::from_millis(50),
                100,
                grace,
                grace,
            ),
            0
        );
        assert_eq!(
            mature_frontier(
                &mut history,
                base + Duration::from_millis(120),
                200,
                grace,
                grace,
            ),
            0
        );
        assert_eq!(
            mature_frontier(
                &mut history,
                base + Duration::from_millis(160),
                200,
                grace,
                grace,
            ),
            100
        );
        assert_eq!(
            mature_frontier(
                &mut history,
                base + Duration::from_millis(230),
                200,
                grace,
                grace,
            ),
            200
        );
    }

    #[test]
    fn frontier_history_survives_switch_to_a_longer_grace() {
        let base = Instant::now();
        let short = Duration::from_millis(50);
        let long = Duration::from_millis(125);
        let mut history = VecDeque::from([(base, 0)]);
        assert_eq!(
            mature_frontier(
                &mut history,
                base + Duration::from_millis(50),
                100,
                short,
                long,
            ),
            0
        );
        assert_eq!(
            mature_frontier(
                &mut history,
                base + Duration::from_millis(100),
                200,
                short,
                long,
            ),
            100
        );
        assert_eq!(
            mature_frontier(
                &mut history,
                base + Duration::from_millis(120),
                200,
                long,
                long,
            ),
            0
        );
        assert_eq!(
            mature_frontier(
                &mut history,
                base + Duration::from_millis(180),
                200,
                long,
                long,
            ),
            100
        );
    }

    #[test]
    fn tcp4_ranges_are_contiguous_and_cover_the_file() {
        let size = 11;
        let ranges: Vec<_> = (0..TCP4_LANES)
            .map(|lane| tcp4_lane_range(size, lane))
            .collect();
        assert_eq!(ranges, vec![(0, 3), (3, 6), (6, 9), (9, 11)]);
    }

    #[test]
    fn tcp4_transport_round_trips_through_the_wire_name() {
        assert_eq!(Transport::parse("tcp4").unwrap(), Transport::Tcp4);
        assert_eq!(Transport::Tcp4.wire_name(), "TCP4");
    }

    #[test]
    fn pacer_uses_two_millisecond_bounded_batches() {
        assert_eq!(Pacer::new(12_500_000.0).batch_bytes, 25_000);
        assert_eq!(Pacer::new(1.0).batch_bytes, 1_200);
        assert_eq!(Pacer::new(1_000_000_000.0).batch_bytes, 65_536);
    }

    #[test]
    fn mapped_file_writes_are_visible_after_flush() {
        let path =
            std::env::temp_dir().join(format!("tsunami-udp-mapped-file-{}", std::process::id()));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(16).unwrap();
        let mut mapped = MappedFile::new(&file, 16).unwrap();
        mapped.write_at(b"payload", 4).unwrap();
        mapped.flush().unwrap();
        drop(mapped);
        assert_eq!(&fs::read(&path).unwrap()[4..11], b"payload");
        fs::remove_file(path).unwrap();
    }
}
