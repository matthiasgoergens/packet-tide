use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

mod auth;
mod cdc;
mod directory;
#[allow(dead_code)]
mod fountain;
mod resume;

use auth::{ControlReader, ControlWriter, Direction, SecretKey, SessionAuth};
use resume::ResumeState;

const HEADER_SIZE: usize = 28;
const DEFAULT_UDP_PAYLOAD_BYTES: usize = 1172;
const MIN_UDP_PAYLOAD_BYTES: usize = 256;
const MAX_UDP_PAYLOAD_BYTES: usize = 1424;
const DEFAULT_REPORT_INTERVAL: Duration = Duration::from_millis(50);
const MIN_REPORT_INTERVAL: Duration = Duration::from_millis(10);
const MAX_REPORT_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_IDLE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_RANGES_PER_REPORT: usize = 512;
const MAX_FEEDBACK_REPORT_SIZE: usize = 24 * 1024;
const TCP4_LANES: usize = 4;
const RESUME_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_QUEUED_REPAIRS: usize = 65_536;
const MAX_BITMAP_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHUNKS: u64 = (MAX_BITMAP_BYTES as u64) * 8;
const DEFAULT_AUTO_MIN_RATE_MBPS: f64 = 10.0;
const DEFAULT_AUTO_MAX_RATE_MBPS: f64 = 10_000.0;
const MAX_RATE_DECISIONS: usize = 4_096;

type AnyResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ReceiverTelemetry {
    received_chunks: u64,
    frontier_chunks: u64,
    accepted_datagrams: u64,
    valid_datagrams: u64,
    duplicate_datagrams: u64,
    invalid_datagrams: u64,
    repair_datagrams: u64,
    socket_drops: Option<u64>,
    reports: u64,
}

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
    rate_mbps: Option<f64>,
    min_rate_mbps: f64,
    max_rate_mbps: f64,
    repair_cooldown: Duration,
    udp_payload_bytes: usize,
    report_interval: Duration,
    idle_timeout: Duration,
    key_file: PathBuf,
    reuse_chunks: bool,
}

#[derive(Debug)]
struct ReceiveArgs {
    listen: SocketAddr,
    udp: SocketAddr,
    out: PathBuf,
    idle_timeout: Duration,
    key_file: PathBuf,
    reuse_from: Option<PathBuf>,
}

#[derive(Debug)]
struct SendDirArgs {
    connect: SocketAddr,
    data_connect: SocketAddr,
    udp_target: SocketAddr,
    root: PathBuf,
    rate_mbps: Option<f64>,
    min_rate_mbps: f64,
    max_rate_mbps: f64,
    repair_cooldown: Duration,
    udp_payload_bytes: usize,
    report_interval: Duration,
    idle_timeout: Duration,
    key_file: PathBuf,
    reuse_chunks: bool,
}

#[derive(Debug)]
struct ReceiveDirArgs {
    listen: SocketAddr,
    data_listen: SocketAddr,
    udp: SocketAddr,
    out: PathBuf,
    idle_timeout: Duration,
    key_file: PathBuf,
    reuse_from: Option<PathBuf>,
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
        Some("send-dir") => send_dir(parse_send_dir(args.collect())?),
        Some("receive-dir") => receive_dir(parse_receive_dir(args.collect())?),
        Some("keygen") => {
            let args: Vec<_> = args.collect();
            validate_options(&args, &["--out"])?;
            SecretKey::generate(Path::new(&option(&args, "--out")?))?;
            Ok(())
        }
        Some("manifest") => {
            let args: Vec<_> = args.collect();
            validate_options(&args, &["--root"])?;
            directory::print(Path::new(&option(&args, "--root")?))
        }
        Some("chunks") => {
            let args: Vec<_> = args.collect();
            validate_options(&args, &["--file"])?;
            cdc::print(Path::new(&option(&args, "--file")?))
        }
        Some("--help" | "-h" | "help") => {
            usage();
            Ok(())
        }
        Some("--version" | "-V") => {
            println!("packet-tide {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            usage();
            Err(
                "expected send, receive, send-dir, receive-dir, manifest, chunks, or keygen subcommand"
                    .into(),
            )
        }
    }
}

fn usage() {
    eprintln!(
        "usage:\n  packet-tide receive --listen ADDR --udp ADDR --out PATH --key-file PATH \
         [--idle-timeout-ms N] [--reuse-from PATH]\n  \
         packet-tide send --connect ADDR --udp-target ADDR --file PATH \
         --transport tcp|tcp4|udp --key-file PATH [--rate-mbps N] \
         [--min-rate-mbps N] [--max-rate-mbps N] \
         [--repair-cooldown-ms N] [--udp-payload-bytes N] \
         [--feedback-interval-ms N] [--idle-timeout-ms N] [--reuse-chunks true|false]\n  \
         packet-tide receive-dir --listen ADDR --data-listen ADDR --udp ADDR \
         --out PATH --key-file PATH [--idle-timeout-ms N] [--reuse-from PATH]\n  \
         packet-tide send-dir --connect ADDR --data-connect ADDR --udp-target ADDR \
         --root PATH --key-file PATH [--rate-mbps N] \
         [--min-rate-mbps N] [--max-rate-mbps N] \
         [--repair-cooldown-ms N] [--udp-payload-bytes N] \
         [--feedback-interval-ms N] [--idle-timeout-ms N] [--reuse-chunks true|false]\n  \
         packet-tide manifest --root PATH\n  \
         packet-tide chunks --file PATH\n  \
         packet-tide keygen --out PATH"
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
            "--min-rate-mbps",
            "--max-rate-mbps",
            "--repair-cooldown-ms",
            "--udp-payload-bytes",
            "--feedback-interval-ms",
            "--idle-timeout-ms",
            "--key-file",
            "--reuse-chunks",
        ],
    )?;
    let parsed = SendArgs {
        connect: resolve_socket(&option(&args, "--connect")?)?,
        udp_target: resolve_socket(&option(&args, "--udp-target")?)?,
        file: option(&args, "--file")?.into(),
        transport: Transport::parse(&option(&args, "--transport")?)?,
        rate_mbps: optional_option(&args, "--rate-mbps")
            .map(|value| value.parse())
            .transpose()?,
        min_rate_mbps: optional_option(&args, "--min-rate-mbps")
            .unwrap_or_else(|| DEFAULT_AUTO_MIN_RATE_MBPS.to_string())
            .parse()?,
        max_rate_mbps: optional_option(&args, "--max-rate-mbps")
            .unwrap_or_else(|| DEFAULT_AUTO_MAX_RATE_MBPS.to_string())
            .parse()?,
        repair_cooldown: Duration::from_millis(
            optional_option(&args, "--repair-cooldown-ms")
                .unwrap_or_else(|| "250".to_owned())
                .parse()?,
        ),
        udp_payload_bytes: parse_udp_payload_bytes(&args)?,
        report_interval: parse_report_interval(&args)?,
        idle_timeout: parse_idle_timeout(&args)?,
        key_file: option(&args, "--key-file")?.into(),
        reuse_chunks: parse_bool_option(&args, "--reuse-chunks")?,
    };
    if parsed.report_interval >= parsed.idle_timeout {
        return Err("--feedback-interval-ms must be shorter than --idle-timeout-ms".into());
    }
    if parsed.reuse_chunks && parsed.transport != Transport::Udp {
        return Err("--reuse-chunks is supported only by UDP".into());
    }
    if let Some(rate) = parsed.rate_mbps
        && (!rate.is_finite() || rate <= 0.0)
    {
        return Err("--rate-mbps must be positive and finite".into());
    }
    if !parsed.min_rate_mbps.is_finite()
        || !parsed.max_rate_mbps.is_finite()
        || parsed.min_rate_mbps <= 0.0
        || parsed.max_rate_mbps < parsed.min_rate_mbps
    {
        return Err("automatic rate bounds must be positive, finite, and ordered".into());
    }
    if parsed.rate_mbps.is_some()
        && (optional_option(&args, "--min-rate-mbps").is_some()
            || optional_option(&args, "--max-rate-mbps").is_some())
    {
        return Err("--rate-mbps is a fixed-rate override and cannot be combined with automatic rate bounds".into());
    }
    Ok(parsed)
}

fn parse_send_dir(args: Vec<String>) -> AnyResult<SendDirArgs> {
    validate_options(
        &args,
        &[
            "--connect",
            "--data-connect",
            "--udp-target",
            "--root",
            "--rate-mbps",
            "--min-rate-mbps",
            "--max-rate-mbps",
            "--repair-cooldown-ms",
            "--udp-payload-bytes",
            "--feedback-interval-ms",
            "--idle-timeout-ms",
            "--key-file",
            "--reuse-chunks",
        ],
    )?;
    let parsed = SendDirArgs {
        connect: resolve_socket(&option(&args, "--connect")?)?,
        data_connect: resolve_socket(&option(&args, "--data-connect")?)?,
        udp_target: resolve_socket(&option(&args, "--udp-target")?)?,
        root: option(&args, "--root")?.into(),
        rate_mbps: optional_option(&args, "--rate-mbps")
            .map(|value| value.parse())
            .transpose()?,
        min_rate_mbps: optional_option(&args, "--min-rate-mbps")
            .unwrap_or_else(|| DEFAULT_AUTO_MIN_RATE_MBPS.to_string())
            .parse()?,
        max_rate_mbps: optional_option(&args, "--max-rate-mbps")
            .unwrap_or_else(|| DEFAULT_AUTO_MAX_RATE_MBPS.to_string())
            .parse()?,
        repair_cooldown: Duration::from_millis(
            optional_option(&args, "--repair-cooldown-ms")
                .unwrap_or_else(|| "250".to_owned())
                .parse()?,
        ),
        udp_payload_bytes: parse_udp_payload_bytes(&args)?,
        report_interval: parse_report_interval(&args)?,
        idle_timeout: parse_idle_timeout(&args)?,
        key_file: option(&args, "--key-file")?.into(),
        reuse_chunks: parse_bool_option(&args, "--reuse-chunks")?,
    };
    validate_rate_configuration(
        parsed.rate_mbps,
        parsed.min_rate_mbps,
        parsed.max_rate_mbps,
        optional_option(&args, "--min-rate-mbps").is_some()
            || optional_option(&args, "--max-rate-mbps").is_some(),
    )?;
    if parsed.report_interval >= parsed.idle_timeout {
        return Err("--feedback-interval-ms must be shorter than --idle-timeout-ms".into());
    }
    Ok(parsed)
}

fn validate_rate_configuration(
    fixed: Option<f64>,
    minimum: f64,
    maximum: f64,
    explicit_bounds: bool,
) -> AnyResult<()> {
    if fixed.is_some_and(|rate| !rate.is_finite() || rate <= 0.0) {
        return Err("--rate-mbps must be positive and finite".into());
    }
    if !minimum.is_finite() || !maximum.is_finite() || minimum <= 0.0 || maximum < minimum {
        return Err("automatic rate bounds must be positive, finite, and ordered".into());
    }
    if fixed.is_some() && explicit_bounds {
        return Err("--rate-mbps is a fixed-rate override and cannot be combined with automatic rate bounds".into());
    }
    Ok(())
}

fn parse_bool_option(args: &[String], name: &str) -> AnyResult<bool> {
    match optional_option(args, name).as_deref() {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(format!("{name} must be true or false").into()),
    }
}

fn parse_udp_payload_bytes(args: &[String]) -> AnyResult<usize> {
    let value = optional_option(args, "--udp-payload-bytes")
        .unwrap_or_else(|| DEFAULT_UDP_PAYLOAD_BYTES.to_string())
        .parse()?;
    if !(MIN_UDP_PAYLOAD_BYTES..=MAX_UDP_PAYLOAD_BYTES).contains(&value) {
        return Err(format!(
            "--udp-payload-bytes must be between {MIN_UDP_PAYLOAD_BYTES} and {MAX_UDP_PAYLOAD_BYTES}"
        )
        .into());
    }
    Ok(value)
}

fn parse_report_interval(args: &[String]) -> AnyResult<Duration> {
    let value = Duration::from_millis(
        optional_option(args, "--feedback-interval-ms")
            .unwrap_or_else(|| DEFAULT_REPORT_INTERVAL.as_millis().to_string())
            .parse()?,
    );
    if !(MIN_REPORT_INTERVAL..=MAX_REPORT_INTERVAL).contains(&value) {
        return Err(format!(
            "--feedback-interval-ms must be between {} and {}",
            MIN_REPORT_INTERVAL.as_millis(),
            MAX_REPORT_INTERVAL.as_millis()
        )
        .into());
    }
    Ok(value)
}

fn parse_receive(args: Vec<String>) -> AnyResult<ReceiveArgs> {
    validate_options(
        &args,
        &[
            "--listen",
            "--udp",
            "--out",
            "--key-file",
            "--idle-timeout-ms",
            "--reuse-from",
        ],
    )?;
    Ok(ReceiveArgs {
        listen: resolve_socket(&option(&args, "--listen")?)?,
        udp: resolve_socket(&option(&args, "--udp")?)?,
        out: option(&args, "--out")?.into(),
        idle_timeout: parse_idle_timeout(&args)?,
        key_file: option(&args, "--key-file")?.into(),
        reuse_from: optional_option(&args, "--reuse-from").map(Into::into),
    })
}

fn parse_receive_dir(args: Vec<String>) -> AnyResult<ReceiveDirArgs> {
    validate_options(
        &args,
        &[
            "--listen",
            "--data-listen",
            "--udp",
            "--out",
            "--key-file",
            "--idle-timeout-ms",
            "--reuse-from",
        ],
    )?;
    Ok(ReceiveDirArgs {
        listen: resolve_socket(&option(&args, "--listen")?)?,
        data_listen: resolve_socket(&option(&args, "--data-listen")?)?,
        udp: resolve_socket(&option(&args, "--udp")?)?,
        out: option(&args, "--out")?.into(),
        idle_timeout: parse_idle_timeout(&args)?,
        key_file: option(&args, "--key-file")?.into(),
        reuse_from: optional_option(&args, "--reuse-from").map(Into::into),
    })
}

fn parse_idle_timeout(args: &[String]) -> AnyResult<Duration> {
    let timeout = Duration::from_millis(
        optional_option(args, "--idle-timeout-ms")
            .unwrap_or_else(|| DEFAULT_IDLE_TIMEOUT.as_millis().to_string())
            .parse()?,
    );
    if !(MIN_IDLE_TIMEOUT..=MAX_IDLE_TIMEOUT).contains(&timeout) {
        return Err(format!(
            "--idle-timeout-ms must be between {} and {}",
            MIN_IDLE_TIMEOUT.as_millis(),
            MAX_IDLE_TIMEOUT.as_millis()
        )
        .into());
    }
    Ok(timeout)
}

fn send_dir(args: SendDirArgs) -> AnyResult<()> {
    let manifest = directory::build(&args.root)?;
    let encoded = directory::encode(&manifest);
    let key = SecretKey::load(&args.key_file)?;
    let mut control = TcpStream::connect(args.connect)?;
    control.set_nodelay(true)?;
    control.set_read_timeout(Some(args.idle_timeout))?;
    control.set_write_timeout(Some(args.idle_timeout))?;
    let hello = format!(
        "DIR {} {}",
        auth::hex(&manifest.hash),
        manifest.entries.len()
    );
    let session = auth::client_handshake(&mut control, &key, &hello)?;
    let mut writer = ControlWriter::new(
        control.try_clone()?,
        session.clone(),
        Direction::ClientToServer,
    );
    let mut reader = ControlReader::new(control.try_clone()?, session, Direction::ServerToClient);
    for line in encoded.lines() {
        writer.send(line)?;
    }
    writer.send("MANIFEST_END")?;
    let expected = format!("MANIFEST_OK {}", auth::hex(&manifest.hash));
    let response = reader.recv()?;
    if response != expected {
        return Err(format!("receiver rejected directory manifest: {response}").into());
    }
    drop(reader);
    drop(writer);
    drop(control);

    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for (index, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != directory::EntryKind::File {
            continue;
        }
        let path = directory::source_file(&manifest, &args.root, index)?;
        send(SendArgs {
            connect: args.data_connect,
            udp_target: args.udp_target,
            file: path,
            transport: Transport::Udp,
            rate_mbps: args.rate_mbps,
            min_rate_mbps: args.min_rate_mbps,
            max_rate_mbps: args.max_rate_mbps,
            repair_cooldown: args.repair_cooldown,
            udp_payload_bytes: args.udp_payload_bytes,
            report_interval: args.report_interval,
            idle_timeout: args.idle_timeout,
            key_file: args.key_file.clone(),
            reuse_chunks: args.reuse_chunks,
        })?;
        files = files.saturating_add(1);
        bytes = bytes
            .checked_add(entry.size)
            .ok_or("directory byte count overflow")?;
    }
    println!(
        "{{\"schema_version\":1,\"role\":\"directory-sender\",\"manifest_sha256\":\"{}\",\"entries\":{},\"files\":{},\"bytes\":{}}}",
        auth::hex(&manifest.hash),
        manifest.entries.len(),
        files,
        bytes
    );
    Ok(())
}

fn receive_dir(args: ReceiveDirArgs) -> AnyResult<()> {
    let key = SecretKey::load(&args.key_file)?;
    let manifest_listener = TcpListener::bind(args.listen)?;
    let data_listener = TcpListener::bind(args.data_listen)?;
    let udp = UdpSocket::bind(args.udp)?;
    udp.set_read_timeout(Some(Duration::from_millis(20)))?;
    let (mut control, _) = manifest_listener.accept()?;
    control.set_nodelay(true)?;
    control.set_read_timeout(Some(args.idle_timeout))?;
    control.set_write_timeout(Some(args.idle_timeout))?;
    let (hello, session) = auth::server_handshake(&mut control, &key)?;
    let fields: Vec<_> = hello.split(' ').collect();
    if fields.len() != 3 || fields[0] != "DIR" {
        return Err("invalid directory manifest greeting".into());
    }
    let expected_hash = auth::decode_array::<32>(fields[1])?;
    let expected_entries = usize::try_from(parse_canonical_u64(fields[2])?)?;
    if expected_entries > directory::MAX_MANIFEST_ENTRIES {
        return Err("directory manifest entry limit exceeded".into());
    }
    let mut reader = ControlReader::new(
        control.try_clone()?,
        session.clone(),
        Direction::ClientToServer,
    );
    let mut writer = ControlWriter::new(control.try_clone()?, session, Direction::ServerToClient);
    let mut encoded = String::new();
    for _ in 0..expected_entries.saturating_add(2) {
        let line = reader.recv()?;
        if encoded.len().saturating_add(line.len()).saturating_add(1)
            > directory::MAX_MANIFEST_BYTES
        {
            return Err("directory manifest byte limit exceeded".into());
        }
        encoded.push_str(&line);
        encoded.push('\n');
    }
    if reader.recv()? != "MANIFEST_END" {
        return Err("directory manifest did not terminate correctly".into());
    }
    let manifest = directory::parse(&encoded)?;
    if manifest.hash != expected_hash || manifest.entries.len() != expected_entries {
        return Err("directory manifest differs from authenticated greeting".into());
    }
    let staging = directory::prepare_staging(&manifest, &args.out)?;
    writer.send(&format!("MANIFEST_OK {}", auth::hex(&manifest.hash)))?;
    drop(reader);
    drop(writer);
    drop(control);

    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for (index, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != directory::EntryKind::File {
            continue;
        }
        let out = directory::file_destination(&manifest, &staging, index)?;
        let reuse_from = match &args.reuse_from {
            Some(root) => directory::candidate_file(&manifest, root, index)?,
            None => None,
        };
        receive_on(
            &ReceiveArgs {
                listen: args.data_listen,
                udp: args.udp,
                out,
                idle_timeout: args.idle_timeout,
                key_file: args.key_file.clone(),
                reuse_from,
            },
            &data_listener,
            &udp,
        )?;
        files = files.saturating_add(1);
        bytes = bytes
            .checked_add(entry.size)
            .ok_or("directory byte count overflow")?;
    }
    directory::install(&manifest, &staging, &args.out)?;
    println!(
        "{{\"schema_version\":1,\"role\":\"directory-receiver\",\"manifest_sha256\":\"{}\",\"entries\":{},\"files\":{},\"bytes\":{}}}",
        auth::hex(&manifest.hash),
        manifest.entries.len(),
        files,
        bytes
    );
    Ok(())
}

fn send(args: SendArgs) -> AnyResult<()> {
    let key = SecretKey::load(&args.key_file)?;
    let size = fs::metadata(&args.file)?.len();
    let expected_hash = hash_file(&args.file)?;
    let reuse_manifest = if args.reuse_chunks {
        Some(cdc::scan(&args.file)?)
    } else {
        None
    };
    let offered_session = auth::random_session()?;
    let chunks = size.div_ceil(args.udp_payload_bytes as u64);
    if chunks > MAX_CHUNKS {
        return Err(format!(
            "file requires {chunks} chunks; maximum is {MAX_CHUNKS} to keep receipt memory bounded"
        )
        .into());
    }

    let mut control = TcpStream::connect(args.connect)?;
    control.set_nodelay(true)?;
    control.set_read_timeout(Some(args.idle_timeout))?;
    control.set_write_timeout(Some(args.idle_timeout))?;
    let reuse_greeting = reuse_manifest.as_ref().map_or_else(
        || "R0".to_owned(),
        |manifest| format!("R1 {} {}", manifest.chunks.len(), auth::hex(&manifest.hash)),
    );
    let hello = format!(
        "{} {} {} {} {} {} {} {} {}",
        args.transport.wire_name(),
        size,
        expected_hash,
        auth::hex(&offered_session),
        chunks,
        args.udp_payload_bytes,
        args.report_interval.as_millis(),
        (args.repair_cooldown / 2)
            .max(args.report_interval)
            .as_millis(),
        reuse_greeting,
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

    if let Some(manifest) = &reuse_manifest {
        for line in cdc::encode(manifest).lines() {
            control_writer.send(line)?;
        }
        control_writer.send("CHUNKS_END")?;
    }

    let acceptance = read_acceptance(
        &mut control_reader,
        args.transport,
        chunks,
        args.udp_payload_bytes,
        args.report_interval,
    )?;
    let resumed_chunks = acceptance
        .durable_chunks
        .iter()
        .map(|word| word.count_ones() as u64)
        .sum::<u64>();
    if acceptance.already_complete {
        control_writer.send("COMPLETE_ACK")?;
        let socket_drops = acceptance
            .telemetry
            .socket_drops
            .map_or_else(|| "null".to_owned(), |drops| drops.to_string());
        let rate_controller_json = if args.transport == Transport::Udp {
            RateController::new(
                args.rate_mbps,
                args.min_rate_mbps,
                args.max_rate_mbps,
                args.report_interval,
            )
            .json()
        } else {
            "null".to_owned()
        };
        println!(
            "{{\"schema_version\":1,\"role\":\"sender\",\"transport\":\"{}\",\"bytes\":{},\
             \"udp_payload_bytes\":{},\"feedback_interval_ms\":{},\"elapsed_ms\":0.0,\
             \"goodput_mbps\":0.0,\"datagrams\":0,\"repairs\":0,\
             \"udp_ip_bytes_offered\":0,\"resumed_chunks\":{},\"reused_bytes\":{},\"reused_chunks\":{},\
             \"receiver_received_chunks\":{},\"receiver_frontier_chunks\":{},\
             \"receiver_accepted_datagrams\":{},\"receiver_valid_datagrams\":{},\
             \"receiver_duplicate_datagrams\":{},\"receiver_invalid_datagrams\":{},\
             \"receiver_repair_datagrams\":{},\"receiver_socket_drops\":{},\
             \"receiver_reports\":{},\"rate_controller\":{}}}",
            args.transport.display_name(),
            size,
            args.udp_payload_bytes,
            args.report_interval.as_millis(),
            chunks,
            acceptance.reused_bytes,
            acceptance.reused_chunks,
            acceptance.telemetry.received_chunks,
            acceptance.telemetry.frontier_chunks,
            acceptance.telemetry.accepted_datagrams,
            acceptance.telemetry.valid_datagrams,
            acceptance.telemetry.duplicate_datagrams,
            acceptance.telemetry.invalid_datagrams,
            acceptance.telemetry.repair_datagrams,
            socket_drops,
            acceptance.telemetry.reports,
            rate_controller_json,
        );
        return Ok(());
    }

    let started = Instant::now();
    let (datagrams, repairs, udp_ip_bytes_offered, receiver_telemetry, rate_controller_json) =
        match args.transport {
            Transport::Tcp => {
                send_tcp(&args.file, &mut control)?;
                expect_completion(&mut control_reader, &mut control_writer)?;
                (0, 0, 0, ReceiverTelemetry::default(), "null".to_owned())
            }
            Transport::Tcp4 => {
                send_tcp4(&args.file, args.connect, &session_auth, args.idle_timeout)?;
                expect_completion(&mut control_reader, &mut control_writer)?;
                (0, 0, 0, ReceiverTelemetry::default(), "null".to_owned())
            }
            Transport::Udp => {
                let config = UdpSendConfig {
                    size,
                    auth: session_auth.clone(),
                    chunks,
                    target: args.udp_target,
                    rate_mbps: args.rate_mbps,
                    min_rate_mbps: args.min_rate_mbps,
                    max_rate_mbps: args.max_rate_mbps,
                    repair_cooldown: args.repair_cooldown,
                    payload_bytes: args.udp_payload_bytes,
                    report_interval: args.report_interval,
                    idle_timeout: args.idle_timeout,
                    durable_chunks: acceptance.durable_chunks,
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
    let receiver_socket_drops = receiver_telemetry
        .socket_drops
        .map_or_else(|| "null".to_owned(), |drops| drops.to_string());
    println!(
        "{{\"schema_version\":1,\"role\":\"sender\",\"transport\":\"{}\",\"bytes\":{},\
         \"udp_payload_bytes\":{},\"feedback_interval_ms\":{},\"elapsed_ms\":{:.3},\
         \"goodput_mbps\":{:.3},\"datagrams\":{},\"repairs\":{},\
         \"udp_ip_bytes_offered\":{},\"resumed_chunks\":{},\"reused_bytes\":{},\"reused_chunks\":{},\
         \"receiver_received_chunks\":{},\"receiver_frontier_chunks\":{},\
         \"receiver_accepted_datagrams\":{},\"receiver_valid_datagrams\":{},\
         \"receiver_duplicate_datagrams\":{},\"receiver_invalid_datagrams\":{},\
         \"receiver_repair_datagrams\":{},\"receiver_socket_drops\":{},\
         \"receiver_reports\":{},\"rate_controller\":{}}}",
        args.transport.display_name(),
        size,
        args.udp_payload_bytes,
        args.report_interval.as_millis(),
        elapsed.as_secs_f64() * 1000.0,
        goodput_mbps,
        datagrams,
        repairs,
        udp_ip_bytes_offered,
        resumed_chunks,
        acceptance.reused_bytes,
        acceptance.reused_chunks,
        receiver_telemetry.received_chunks,
        receiver_telemetry.frontier_chunks,
        receiver_telemetry.accepted_datagrams,
        receiver_telemetry.valid_datagrams,
        receiver_telemetry.duplicate_datagrams,
        receiver_telemetry.invalid_datagrams,
        receiver_telemetry.repair_datagrams,
        receiver_socket_drops,
        receiver_telemetry.reports,
        rate_controller_json,
    );
    Ok(())
}

fn expect_completion(reader: &mut ControlReader, writer: &mut ControlWriter) -> AnyResult<()> {
    let completion = reader.recv()?;
    if completion != "COMPLETE" {
        return Err(format!("transfer did not complete: {completion}").into());
    }
    writer.send("COMPLETE_ACK")?;
    Ok(())
}

struct Acceptance {
    durable_chunks: Vec<u64>,
    already_complete: bool,
    telemetry: ReceiverTelemetry,
    reused_bytes: u64,
    reused_chunks: u64,
}

fn read_acceptance(
    control: &mut ControlReader,
    transport: Transport,
    chunks: u64,
    payload_bytes: usize,
    report_interval: Duration,
) -> AnyResult<Acceptance> {
    let mut line = control.recv()?;
    let mut telemetry = ReceiverTelemetry::default();
    if line.starts_with("M ") {
        if transport != Transport::Udp {
            return Err("non-UDP receiver sent datagram telemetry".into());
        }
        let (mut snapshot, ranges) = parse_feedback_report(&line, chunks)?;
        if !ranges.is_empty() {
            return Err("completed receiver reported missing chunks".into());
        }
        snapshot.reports = 1;
        telemetry = snapshot;
        line = control.recv()?;
    }
    if line == "COMPLETE" {
        return Ok(Acceptance {
            durable_chunks: Vec::new(),
            already_complete: true,
            telemetry,
            reused_bytes: 0,
            reused_chunks: 0,
        });
    }
    let expected_ready = format!("READY {payload_bytes} {}", report_interval.as_millis());
    if line != expected_ready {
        return Err(format!("receiver rejected transfer: {line}").into());
    }
    if transport != Transport::Udp {
        return Ok(Acceptance {
            durable_chunks: Vec::new(),
            already_complete: false,
            telemetry,
            reused_bytes: 0,
            reused_chunks: 0,
        });
    }

    let words = usize::try_from(chunks.div_ceil(64))?;
    let mut bitmap = vec![0_u64; words];
    line = control.recv()?;
    let reuse_fields: Vec<_> = line.split(' ').collect();
    if reuse_fields.len() != 3 || reuse_fields[0] != "REUSED" {
        return Err("receiver omitted authenticated reuse inventory".into());
    }
    let reused_bytes = parse_canonical_u64(reuse_fields[1])?;
    let reused_chunks = parse_canonical_u64(reuse_fields[2])?;
    if reused_bytes > chunks.saturating_mul(payload_bytes as u64) || reused_chunks > chunks {
        return Err("receiver reuse inventory exceeds the object".into());
    }
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
    if bitmap
        .iter()
        .map(|word| word.count_ones() as u64)
        .sum::<u64>()
        < reused_chunks
    {
        return Err("receiver reuse inventory exceeds durable chunks".into());
    }
    Ok(Acceptance {
        durable_chunks: bitmap,
        already_complete: false,
        telemetry,
        reused_bytes,
        reused_chunks,
    })
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

fn send_tcp4(
    path: &Path,
    connect: SocketAddr,
    auth: &SessionAuth,
    idle_timeout: Duration,
) -> AnyResult<()> {
    let size = fs::metadata(path)?.len();
    let mut lanes = Vec::with_capacity(TCP4_LANES);
    for lane in 0..TCP4_LANES {
        let mut stream = TcpStream::connect_timeout(&connect, idle_timeout)?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(idle_timeout))?;
        let mac = auth::lane_mac(&auth.lane, &auth.session, lane);
        writeln!(
            stream,
            "TSU4D {} {lane} {}",
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
    rate_mbps: Option<f64>,
    min_rate_mbps: f64,
    max_rate_mbps: f64,
    repair_cooldown: Duration,
    payload_bytes: usize,
    report_interval: Duration,
    idle_timeout: Duration,
    durable_chunks: Vec<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RateDecisionKind {
    Increase,
    Decrease,
    Hold,
}

#[derive(Clone, Debug)]
struct RateDecision {
    elapsed_ms: u128,
    kind: RateDecisionKind,
    old_rate_mbps: f64,
    new_rate_mbps: f64,
    useful_rate_mbps: f64,
    waste_ratio: f64,
    reason: &'static str,
}

#[derive(Debug)]
struct RateController {
    fixed: bool,
    rate_mbps: f64,
    min_rate_mbps: f64,
    max_rate_mbps: f64,
    decision_interval: Duration,
    started: Instant,
    last_decision: Instant,
    last_telemetry: ReceiverTelemetry,
    decisions: Vec<RateDecision>,
    decision_count: u64,
    increase_count: u64,
    decrease_count: u64,
    hold_count: u64,
}

impl RateController {
    fn new(
        fixed_rate_mbps: Option<f64>,
        min_rate_mbps: f64,
        max_rate_mbps: f64,
        report_interval: Duration,
    ) -> Self {
        let now = Instant::now();
        let fixed = fixed_rate_mbps.is_some();
        let rate_mbps = fixed_rate_mbps.unwrap_or(min_rate_mbps);
        Self {
            fixed,
            rate_mbps,
            min_rate_mbps: if fixed { rate_mbps } else { min_rate_mbps },
            max_rate_mbps: if fixed { rate_mbps } else { max_rate_mbps },
            decision_interval: (report_interval * 4).max(Duration::from_millis(200)),
            started: now,
            last_decision: now,
            last_telemetry: ReceiverTelemetry::default(),
            decisions: Vec::new(),
            decision_count: 0,
            increase_count: 0,
            decrease_count: 0,
            hold_count: 0,
        }
    }

    fn observe(&mut self, telemetry: &ReceiverTelemetry, now: Instant, payload_bytes: usize) {
        if self.fixed || now.duration_since(self.last_decision) < self.decision_interval {
            return;
        }
        let elapsed = now.duration_since(self.last_decision).as_secs_f64();
        let accepted = telemetry
            .accepted_datagrams
            .saturating_sub(self.last_telemetry.accepted_datagrams);
        let valid = telemetry
            .valid_datagrams
            .saturating_sub(self.last_telemetry.valid_datagrams);
        let socket_drops = match (telemetry.socket_drops, self.last_telemetry.socket_drops) {
            (Some(current), Some(previous)) => current.saturating_sub(previous),
            (Some(current), None) => current,
            _ => 0,
        };
        let duplicates = valid.saturating_sub(accepted);
        let useful_rate_mbps = accepted as f64 * payload_bytes as f64 * 8.0 / elapsed / 1_000_000.0;
        let waste_ratio = if valid == 0 {
            0.0
        } else {
            (duplicates.saturating_add(socket_drops)) as f64 / valid as f64
        };
        let old_rate = self.rate_mbps;
        let (candidate, reason) = if accepted == 0 {
            (old_rate * 0.7, "stalled_progress")
        } else if waste_ratio > 0.03 || socket_drops > 0 {
            (old_rate * 0.7, "receiver_waste")
        } else if useful_rate_mbps >= old_rate * 0.8 {
            (
                (old_rate * 1.25).max(old_rate + 1.0),
                "useful_throughput_followed_load",
            )
        } else if useful_rate_mbps < old_rate * 0.5 {
            (old_rate * 0.85, "throughput_fell_behind_load")
        } else {
            (old_rate, "insufficient_signal")
        };
        self.rate_mbps = candidate.clamp(self.min_rate_mbps, self.max_rate_mbps);
        let effective_kind = if self.rate_mbps > old_rate {
            RateDecisionKind::Increase
        } else if self.rate_mbps < old_rate {
            RateDecisionKind::Decrease
        } else {
            RateDecisionKind::Hold
        };
        self.decision_count = self.decision_count.saturating_add(1);
        match effective_kind {
            RateDecisionKind::Increase => {
                self.increase_count = self.increase_count.saturating_add(1)
            }
            RateDecisionKind::Decrease => {
                self.decrease_count = self.decrease_count.saturating_add(1)
            }
            RateDecisionKind::Hold => self.hold_count = self.hold_count.saturating_add(1),
        }
        let decision = RateDecision {
            elapsed_ms: now.duration_since(self.started).as_millis(),
            kind: effective_kind,
            old_rate_mbps: old_rate,
            new_rate_mbps: self.rate_mbps,
            useful_rate_mbps,
            waste_ratio,
            reason,
        };
        if self.decisions.len() < MAX_RATE_DECISIONS {
            self.decisions.push(decision);
        }
        self.last_decision = now;
        self.last_telemetry = telemetry.clone();
    }

    fn json(&self) -> String {
        let mode = if self.fixed { "fixed" } else { "auto" };
        let decisions = self
            .decisions
            .iter()
            .map(|decision| {
                let kind = match decision.kind {
                    RateDecisionKind::Increase => "increase",
                    RateDecisionKind::Decrease => "decrease",
                    RateDecisionKind::Hold => "hold",
                };
                format!(
                    "{{\"elapsed_ms\":{},\"decision\":\"{}\",\"old_rate_mbps\":{:.6},\"new_rate_mbps\":{:.6},\"useful_rate_mbps\":{:.6},\"waste_ratio\":{:.6},\"reason\":\"{}\"}}",
                    decision.elapsed_ms,
                    kind,
                    decision.old_rate_mbps,
                    decision.new_rate_mbps,
                    decision.useful_rate_mbps,
                    decision.waste_ratio,
                    decision.reason,
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"mode\":\"{mode}\",\"initial_rate_mbps\":{:.6},\"final_rate_mbps\":{:.6},\"minimum_rate_mbps\":{:.6},\"maximum_rate_mbps\":{:.6},\"decision_count\":{},\"increase_count\":{},\"decrease_count\":{},\"hold_count\":{},\"omitted_decision_count\":{},\"decisions\":[{}]}}",
            if self.fixed {
                self.rate_mbps
            } else {
                self.min_rate_mbps
            },
            self.rate_mbps,
            self.min_rate_mbps,
            self.max_rate_mbps,
            self.decision_count,
            self.increase_count,
            self.decrease_count,
            self.hold_count,
            self.decision_count
                .saturating_sub(self.decisions.len() as u64),
            decisions,
        )
    }
}

fn send_udp(
    path: &Path,
    config: &UdpSendConfig,
    control: &mut ControlWriter,
    control_reader: ControlReader,
) -> AnyResult<(u64, u64, u64, ReceiverTelemetry, String)> {
    let result = send_udp_inner(path, config, control, control_reader);
    if result.is_err() {
        let _ = control.send("CANCEL sender-error");
    }
    result
}

fn send_udp_inner(
    path: &Path,
    config: &UdpSendConfig,
    control: &mut ControlWriter,
    control_reader: ControlReader,
) -> AnyResult<(u64, u64, u64, ReceiverTelemetry, String)> {
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
    let receiver_telemetry = Arc::new(Mutex::new(ReceiverTelemetry::default()));
    let rate_controller = Arc::new(Mutex::new(RateController::new(
        config.rate_mbps,
        config.min_rate_mbps,
        config.max_rate_mbps,
        config.report_interval,
    )));
    let adaptive = config.rate_mbps.is_none();

    let pending_for_reader = Arc::clone(&pending);
    let complete_for_reader = Arc::clone(&complete);
    let error_for_reader = Arc::clone(&feedback_error);
    let telemetry_for_reader = Arc::clone(&receiver_telemetry);
    let controller_for_reader = adaptive.then(|| Arc::clone(&rate_controller));
    let chunks_for_reader = config.chunks;
    let payload_bytes_for_reader = config.payload_bytes;
    let feedback_thread = thread::spawn(move || {
        if let Err(error) = read_feedback(
            control_reader,
            pending_for_reader,
            complete_for_reader,
            telemetry_for_reader,
            controller_for_reader,
            chunks_for_reader,
            payload_bytes_for_reader,
        ) {
            *error_for_reader
                .lock()
                .expect("feedback error mutex poisoned") = Some(error.to_string());
        }
    });

    let mut pacer = Pacer::new(
        rate_controller
            .lock()
            .expect("rate controller mutex poisoned")
            .rate_mbps
            * 1_000_000.0
            / 8.0,
    );
    let mut packet = vec![0_u8; HEADER_SIZE + config.payload_bytes];
    let mut datagrams = 0_u64;
    let mut repairs = 0_u64;
    let mut udp_ip_bytes_offered = 0_u64;
    let heartbeat_interval = (config.idle_timeout / 3).min(Duration::from_secs(1));
    let mut last_heartbeat = Instant::now();

    for sequence in 0..config.chunks {
        check_feedback_error(&feedback_error)?;
        send_heartbeat_if_due(control, &mut last_heartbeat, heartbeat_interval)?;
        if bitmap_contains(&config.durable_chunks, sequence) {
            continue;
        }
        let packet_len = build_packet(
            &file,
            config.size,
            &config.auth,
            sequence,
            false,
            config.payload_bytes,
            &mut packet,
        )?;
        socket.send(&packet[..packet_len])?;
        datagrams += 1;
        udp_ip_bytes_offered += packet_len as u64 + 28;
        pace_with_controller(&mut pacer, &rate_controller, adaptive, packet_len + 28);

        if sequence % 16 == 15 {
            for repair in take_repairs(&pending, 4) {
                let packet_len = build_packet(
                    &file,
                    config.size,
                    &config.auth,
                    repair,
                    true,
                    config.payload_bytes,
                    &mut packet,
                )?;
                socket.send(&packet[..packet_len])?;
                datagrams += 1;
                repairs += 1;
                udp_ip_bytes_offered += packet_len as u64 + 28;
                pace_with_controller(&mut pacer, &rate_controller, adaptive, packet_len + 28);
            }
        }
    }

    control.send("END")?;

    while !complete.load(Ordering::Acquire) {
        check_feedback_error(&feedback_error)?;
        send_heartbeat_if_due(control, &mut last_heartbeat, heartbeat_interval)?;

        let repairs_now = take_repairs(&pending, 1024);
        if repairs_now.is_empty() {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        for repair in repairs_now {
            let packet_len = build_packet(
                &file,
                config.size,
                &config.auth,
                repair,
                true,
                config.payload_bytes,
                &mut packet,
            )?;
            socket.send(&packet[..packet_len])?;
            datagrams += 1;
            repairs += 1;
            udp_ip_bytes_offered += packet_len as u64 + 28;
            pace_with_controller(&mut pacer, &rate_controller, adaptive, packet_len + 28);
        }
    }

    feedback_thread
        .join()
        .map_err(|_| "feedback thread panicked")?;
    control.send("COMPLETE_ACK")?;
    let receiver_telemetry = receiver_telemetry
        .lock()
        .expect("receiver telemetry mutex poisoned")
        .clone();
    let controller_json = rate_controller
        .lock()
        .expect("rate controller mutex poisoned")
        .json();
    Ok((
        datagrams,
        repairs,
        udp_ip_bytes_offered,
        receiver_telemetry,
        controller_json,
    ))
}

fn pace_with_controller(
    pacer: &mut Pacer,
    controller: &Arc<Mutex<RateController>>,
    adaptive: bool,
    bytes: usize,
) {
    if adaptive {
        let rate_mbps = controller
            .lock()
            .expect("rate controller mutex poisoned")
            .rate_mbps;
        pacer.set_bytes_per_second(rate_mbps * 1_000_000.0 / 8.0);
    }
    pacer.account_and_wait(bytes);
}

fn check_feedback_error(feedback_error: &Mutex<Option<String>>) -> AnyResult<()> {
    if let Some(error) = feedback_error
        .lock()
        .expect("feedback error mutex poisoned")
        .clone()
    {
        return Err(error.into());
    }
    Ok(())
}

fn send_heartbeat_if_due(
    control: &mut ControlWriter,
    last_heartbeat: &mut Instant,
    interval: Duration,
) -> AnyResult<()> {
    if last_heartbeat.elapsed() >= interval {
        control.send("PING")?;
        *last_heartbeat = Instant::now();
    }
    Ok(())
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
    telemetry: Arc<Mutex<ReceiverTelemetry>>,
    controller: Option<Arc<Mutex<RateController>>>,
    chunks: u64,
    payload_bytes: usize,
) -> AnyResult<()> {
    loop {
        let line = reader
            .recv()
            .map_err(|error| format!("receiver feedback stopped: {error}"))?;
        if line == "COMPLETE" {
            complete.store(true, Ordering::Release);
            return Ok(());
        }
        if line.starts_with("M ") {
            let (mut snapshot, ranges) = parse_feedback_report(&line, chunks)?;
            if let Some(controller) = &controller {
                controller
                    .lock()
                    .expect("rate controller mutex poisoned")
                    .observe(&snapshot, Instant::now(), payload_bytes);
            }
            let mut current = telemetry.lock().expect("receiver telemetry mutex poisoned");
            snapshot.reports = current.reports.saturating_add(1);
            *current = snapshot;
            drop(current);
            let mut queue = pending.lock().expect("repair queue mutex poisoned");
            let now = Instant::now();
            let cooldown = queue.cooldown;
            queue
                .last_sent
                .retain(|_, sent_at| now.duration_since(*sent_at) < cooldown);
            enqueue_repairs(&mut queue, &ranges, chunks)?;
        } else if let Some(message) = line.strip_prefix("CANCEL ") {
            return Err(format!("receiver cancelled transfer: {message}").into());
        } else if let Some(message) = line.strip_prefix("ERROR ") {
            return Err(format!("receiver error: {message}").into());
        } else if line == "PONG" {
            continue;
        } else {
            return Err(format!("unexpected receiver feedback: {line}").into());
        }
    }
}

fn parse_feedback_report(line: &str, chunks: u64) -> AnyResult<(ReceiverTelemetry, String)> {
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() != 10 || fields[0] != "M" {
        return Err("invalid receiver feedback report".into());
    }
    let socket_drops = if fields[8] == "-" {
        None
    } else {
        Some(parse_canonical_u64(fields[8])?)
    };
    let telemetry = ReceiverTelemetry {
        received_chunks: parse_canonical_u64(fields[1])?,
        frontier_chunks: parse_canonical_u64(fields[2])?,
        accepted_datagrams: parse_canonical_u64(fields[3])?,
        valid_datagrams: parse_canonical_u64(fields[4])?,
        duplicate_datagrams: parse_canonical_u64(fields[5])?,
        invalid_datagrams: parse_canonical_u64(fields[6])?,
        repair_datagrams: parse_canonical_u64(fields[7])?,
        socket_drops,
        reports: 0,
    };
    if telemetry.received_chunks > chunks
        || telemetry.frontier_chunks > chunks
        || telemetry.accepted_datagrams > telemetry.received_chunks
        || telemetry.accepted_datagrams > telemetry.valid_datagrams
        || telemetry.duplicate_datagrams > telemetry.valid_datagrams
        || telemetry.repair_datagrams > telemetry.valid_datagrams
        || telemetry
            .accepted_datagrams
            .checked_add(telemetry.duplicate_datagrams)
            != Some(telemetry.valid_datagrams)
    {
        return Err("inconsistent receiver feedback counters".into());
    }
    let ranges = if fields[9] == "-" {
        String::new()
    } else {
        fields[9].to_owned()
    };
    Ok((telemetry, ranges))
}

fn parse_canonical_u64(value: &str) -> AnyResult<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(format!("non-canonical unsigned integer {value:?}").into());
    }
    Ok(value.parse()?)
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
    payload_bytes: usize,
    packet: &mut [u8],
) -> AnyResult<usize> {
    let offset = sequence
        .checked_mul(payload_bytes as u64)
        .ok_or("packet offset overflow")?;
    if offset >= size && size != 0 {
        return Err(format!("sequence {sequence} is outside the file").into());
    }
    let payload_len = (size.saturating_sub(offset)).min(payload_bytes as u64) as usize;
    packet[0] = 4;
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

    fn set_bytes_per_second(&mut self, bytes_per_second: f64) {
        if self.bytes_per_second == bytes_per_second {
            return;
        }
        self.bytes_per_second = bytes_per_second;
        self.started = Instant::now();
        self.bytes_sent = 0;
        self.bytes_since_wait = 0;
        self.batch_bytes = (bytes_per_second * 0.002).clamp(1_200.0, 65_536.0) as usize;
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
    let udp = UdpSocket::bind(args.udp)?;
    udp.set_read_timeout(Some(Duration::from_millis(20)))?;
    let listener = TcpListener::bind(args.listen)?;
    receive_on(&args, &listener, &udp)
}

fn receive_on(args: &ReceiveArgs, listener: &TcpListener, udp: &UdpSocket) -> AnyResult<()> {
    let key = SecretKey::load(&args.key_file)?;
    let udp = udp.try_clone()?;
    let (mut control, peer) = listener.accept()?;
    control.set_nodelay(true)?;
    control.set_read_timeout(Some(args.idle_timeout))?;
    control.set_write_timeout(Some(args.idle_timeout))?;
    let (hello_body, session_auth) = auth::server_handshake(&mut control, &key)?;
    let mut hello = parse_hello(&hello_body)?;
    hello.session = session_auth.session;
    let mut control_writer = ControlWriter::new(
        control.try_clone()?,
        session_auth.clone(),
        Direction::ServerToClient,
    );
    let mut control_reader = ControlReader::new(
        control.try_clone()?,
        session_auth.clone(),
        Direction::ClientToServer,
    );
    let reuse_manifest = read_reuse_manifest(&mut control_reader, &hello)?;

    if let Some(parent) = args.out.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(&args.out);
    let started = Instant::now();
    let result: AnyResult<Option<ReceiverTelemetry>> = match hello.transport {
        Transport::Tcp => {
            control_writer.send(&format!(
                "READY {} {}",
                hello.payload_bytes,
                hello.report_interval.as_millis()
            ))?;
            receive_tcp(
                &mut control,
                &mut control_writer,
                control_reader,
                &temporary,
                &args.out,
                &hello,
            )
            .map(|()| None)
        }
        Transport::Tcp4 => {
            control_writer.send(&format!(
                "READY {} {}",
                hello.payload_bytes,
                hello.report_interval.as_millis()
            ))?;
            receive_tcp4(
                listener,
                &mut control_writer,
                control_reader,
                (&temporary, &args.out),
                &hello,
                &session_auth,
                args.idle_timeout,
            )
            .map(|()| None)
        }
        Transport::Udp => receive_udp(
            &mut control_writer,
            control_reader,
            udp,
            (
                &args.out,
                args.reuse_from.as_deref(),
                reuse_manifest.as_ref(),
            ),
            &hello,
            &session_auth,
            args.idle_timeout,
        )
        .map(Some),
    };
    if let Err(error) = &result {
        if hello.transport != Transport::Udp {
            let _ = fs::remove_file(&temporary);
        }
        eprintln!("receive from {peer} failed: {error}");
    }
    if let Ok(telemetry) = &result {
        let telemetry = telemetry.clone().unwrap_or_default();
        let elapsed = started.elapsed();
        let goodput_mbps = if elapsed.is_zero() {
            0.0
        } else {
            hello.size as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0
        };
        let socket_drops = telemetry
            .socket_drops
            .map_or_else(|| "null".to_owned(), |drops| drops.to_string());
        println!(
            "{{\"schema_version\":1,\"role\":\"receiver\",\"transport\":\"{}\",\
             \"bytes\":{},\"udp_payload_bytes\":{},\"feedback_interval_ms\":{},\
             \"elapsed_ms\":{:.3},\"goodput_mbps\":{:.3},\
             \"received_chunks\":{},\"frontier_chunks\":{},\
             \"accepted_datagrams\":{},\"valid_datagrams\":{},\
             \"duplicate_datagrams\":{},\"invalid_datagrams\":{},\
             \"repair_datagrams\":{},\"socket_drops\":{},\"reports\":{}}}",
            hello.transport.display_name(),
            hello.size,
            hello.payload_bytes,
            hello.report_interval.as_millis(),
            elapsed.as_secs_f64() * 1000.0,
            goodput_mbps,
            telemetry.received_chunks,
            telemetry.frontier_chunks,
            telemetry.accepted_datagrams,
            telemetry.valid_datagrams,
            telemetry.duplicate_datagrams,
            telemetry.invalid_datagrams,
            telemetry.repair_datagrams,
            socket_drops,
            telemetry.reports
        );
    }
    result.map(|_| ())
}

struct Hello {
    transport: Transport,
    size: u64,
    hash: String,
    session: [u8; 16],
    chunks: u64,
    payload_bytes: usize,
    report_interval: Duration,
    repair_grace: Duration,
    reuse_manifest: Option<(usize, [u8; 32])>,
}

#[derive(Clone, Copy)]
struct ChunkLayout {
    size: u64,
    chunks: u64,
    payload_bytes: usize,
}

impl Hello {
    fn chunk_layout(&self) -> ChunkLayout {
        ChunkLayout {
            size: self.size,
            chunks: self.chunks,
            payload_bytes: self.payload_bytes,
        }
    }
}

fn parse_hello(line: &str) -> AnyResult<Hello> {
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return Err("invalid authenticated TSU4 greeting body".into());
    }
    let transport = match parts[0] {
        "TCP" => Transport::Tcp,
        "TCP4" => Transport::Tcp4,
        "UDP" => Transport::Udp,
        _ => return Err("invalid transport in greeting".into()),
    };
    let reuse_manifest = match parts[8] {
        "R0" if parts.len() == 9 => None,
        "R1" if parts.len() == 11 => {
            let count = usize::try_from(parse_canonical_u64(parts[9])?)?;
            if count > cdc::MAX_CHUNKS {
                return Err("content-defined chunk limit exceeded".into());
            }
            Some((count, auth::decode_array::<32>(parts[10])?))
        }
        _ => return Err("invalid content-reuse greeting".into()),
    };
    let hello = Hello {
        transport,
        size: parse_canonical_u64(parts[1])?,
        hash: parts[2].to_owned(),
        session: auth::decode_array::<16>(parts[3])?,
        chunks: parse_canonical_u64(parts[4])?,
        payload_bytes: usize::try_from(parse_canonical_u64(parts[5])?)?,
        report_interval: Duration::from_millis(parse_canonical_u64(parts[6])?),
        repair_grace: Duration::from_millis(parse_canonical_u64(parts[7])?),
        reuse_manifest,
    };
    auth::decode_array::<32>(&hello.hash)?;
    if !(MIN_UDP_PAYLOAD_BYTES..=MAX_UDP_PAYLOAD_BYTES).contains(&hello.payload_bytes) {
        return Err("UDP payload size is outside the supported bounds".into());
    }
    if !(MIN_REPORT_INTERVAL..=MAX_REPORT_INTERVAL).contains(&hello.report_interval) {
        return Err("feedback interval is outside the supported bounds".into());
    }
    let expected_chunks = hello.size.div_ceil(hello.payload_bytes as u64);
    if hello.chunks != expected_chunks {
        return Err("chunk count does not match file size".into());
    }
    if hello.chunks > MAX_CHUNKS {
        return Err("file exceeds the bounded receipt-map limit".into());
    }
    if hello.repair_grace < hello.report_interval || hello.repair_grace > Duration::from_secs(60) {
        return Err(
            "repair grace must cover at least one feedback interval and at most 60 seconds".into(),
        );
    }
    if hello.reuse_manifest.is_some() && hello.transport != Transport::Udp {
        return Err("content reuse is supported only by UDP".into());
    }
    Ok(hello)
}

fn read_reuse_manifest(
    control: &mut ControlReader,
    hello: &Hello,
) -> AnyResult<Option<cdc::Manifest>> {
    let Some((expected_count, expected_hash)) = hello.reuse_manifest else {
        return Ok(None);
    };
    let mut encoded = String::new();
    for _ in 0..expected_count.saturating_add(2) {
        let line = control.recv()?;
        if encoded.len().saturating_add(line.len()).saturating_add(1) > cdc::MAX_ENCODED_BYTES {
            return Err("content-defined manifest byte limit exceeded".into());
        }
        encoded.push_str(&line);
        encoded.push('\n');
    }
    if control.recv()? != "CHUNKS_END" {
        return Err("content-defined manifest did not terminate correctly".into());
    }
    let manifest = cdc::parse(&encoded)?;
    if manifest.size != hello.size
        || manifest.chunks.len() != expected_count
        || manifest.hash != expected_hash
    {
        return Err("content-defined manifest differs from authenticated greeting".into());
    }
    Ok(Some(manifest))
}

fn receive_tcp(
    control: &mut TcpStream,
    control_writer: &mut ControlWriter,
    mut control_reader: ControlReader,
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
    send_completion_and_wait(control_writer, &mut control_reader)?;
    Ok(())
}

fn receive_tcp4(
    listener: &TcpListener,
    control: &mut ControlWriter,
    mut control_reader: ControlReader,
    paths: (&Path, &Path),
    hello: &Hello,
    session_auth: &SessionAuth,
    idle_timeout: Duration,
) -> AnyResult<()> {
    let (temporary, destination) = paths;
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(temporary)?;
    file.set_len(hello.size)?;

    let mut lanes: Vec<Option<TcpStream>> = (0..TCP4_LANES).map(|_| None).collect();
    listener.set_nonblocking(true)?;
    let mut accept_deadline = Instant::now() + idle_timeout;
    while lanes.iter().any(Option::is_none) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nodelay(true)?;
                stream.set_read_timeout(Some(idle_timeout))?;
                let lane = read_tcp4_hello(&mut stream, session_auth)?;
                if lane >= TCP4_LANES || lanes[lane].is_some() {
                    return Err("invalid or duplicate TCP4 data connection".into());
                }
                lanes[lane] = Some(stream);
                accept_deadline = Instant::now() + idle_timeout;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= accept_deadline {
                    return Err("timed out waiting for TCP4 data connections".into());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
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
    send_completion_and_wait(control, &mut control_reader)?;
    Ok(())
}

fn send_completion_and_wait(
    control: &mut ControlWriter,
    reader: &mut ControlReader,
) -> AnyResult<()> {
    control.send("COMPLETE")?;
    let acknowledgement = reader.recv()?;
    if acknowledgement != "COMPLETE_ACK" {
        return Err(format!("expected completion acknowledgement, got {acknowledgement}").into());
    }
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
    if parts.len() != 4 || parts[0] != "TSU4D" {
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
    control: &mut ControlWriter,
    mut control_reader: ControlReader,
    udp: UdpSocket,
    paths: (&Path, Option<&Path>, Option<&cdc::Manifest>),
    hello: &Hello,
    session_auth: &SessionAuth,
    idle_timeout: Duration,
) -> AnyResult<ReceiverTelemetry> {
    let (destination, reuse_from, reuse_manifest) = paths;
    if destination
        .metadata()
        .is_ok_and(|metadata| metadata.len() == hello.size)
        && hash_file(destination)? == hello.hash
    {
        let telemetry = ReceiverTelemetry {
            received_chunks: hello.chunks,
            frontier_chunks: hello.chunks,
            socket_drops: udp_socket_drops(&udp),
            reports: 1,
            ..ReceiverTelemetry::default()
        };
        control.send(&format_feedback_report(&telemetry, &[]))?;
        send_completion_and_wait(control, &mut control_reader)?;
        return Ok(telemetry);
    }

    let (file, mut state) = ResumeState::open(
        destination,
        hello.size,
        hello.chunks,
        hello.payload_bytes,
        &hello.hash,
    )?;
    let (reused_bytes, reused_chunks) = match (reuse_manifest, reuse_from) {
        (Some(manifest), Some(candidate)) => {
            apply_reusable_content(&file, &mut state, manifest, candidate, hello.chunk_layout())?
        }
        _ => (0, 0),
    };
    let mut telemetry = ReceiverTelemetry {
        received_chunks: state.received_count,
        ..ReceiverTelemetry::default()
    };
    let mut object_hasher = Sha256::new();
    let mut hash_frontier = 0_u64;
    let mut hash_buffer = vec![0_u8; 1024 * 1024];
    advance_hash_frontier(
        &file,
        &state.bitmap,
        hello.chunk_layout(),
        &mut hash_frontier,
        &mut object_hasher,
        &mut hash_buffer,
    )?;
    control.send(&format!(
        "READY {} {}",
        hello.payload_bytes,
        hello.report_interval.as_millis()
    ))?;
    control.send(&format!("REUSED {reused_bytes} {reused_chunks}"))?;
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
                Ok(line) if line == "PING" => {
                    let _ = control_tx.send(ControlEvent::Ping);
                }
                Ok(line) if line == "COMPLETE_ACK" => {
                    let _ = control_tx.send(ControlEvent::CompleteAck);
                    break;
                }
                Ok(line) if line.starts_with("CANCEL ") => {
                    let reason = line.strip_prefix("CANCEL ").unwrap_or_default().to_owned();
                    let _ = control_tx.send(ControlEvent::Cancel(reason));
                    break;
                }
                Ok(_) => {
                    let _ = control_tx
                        .send(ControlEvent::Error("unexpected control message".to_owned()));
                    break;
                }
                Err(error) => {
                    let _ = control_tx.send(ControlEvent::Error(format!(
                        "sender control stopped: {error}"
                    )));
                    break;
                }
            }
        }
    });

    // One extra byte makes oversized datagrams observable: recv() otherwise
    // truncates them to the buffer length without reporting truncation.
    let mut packet = vec![0_u8; HEADER_SIZE + hello.payload_bytes + 1];
    loop {
        match udp.recv(&mut packet) {
            Ok(count) => 'packet: {
                if count < HEADER_SIZE {
                    telemetry.invalid_datagrams = telemetry.invalid_datagrams.saturating_add(1);
                    break 'packet;
                }
                if packet[0] != 4 {
                    telemetry.invalid_datagrams = telemetry.invalid_datagrams.saturating_add(1);
                    break 'packet;
                }
                let sequence = u64::from_be_bytes(packet[1..9].try_into()?);
                let repair = match packet[9] {
                    0 => false,
                    1 => true,
                    _ => {
                        telemetry.invalid_datagrams = telemetry.invalid_datagrams.saturating_add(1);
                        break 'packet;
                    }
                };
                let payload_len = u16::from_be_bytes(packet[10..12].try_into()?) as usize;
                if sequence >= hello.chunks
                    || payload_len > hello.payload_bytes
                    || count != HEADER_SIZE + payload_len
                {
                    telemetry.invalid_datagrams = telemetry.invalid_datagrams.saturating_add(1);
                    break 'packet;
                }
                if !auth::verify_udp_tag_parts(
                    &session_auth.udp,
                    &packet[..12],
                    &packet[HEADER_SIZE..count],
                    &packet[12..28],
                ) {
                    telemetry.invalid_datagrams = telemetry.invalid_datagrams.saturating_add(1);
                    break 'packet;
                }
                let offset = sequence * hello.payload_bytes as u64;
                let expected_len =
                    (hello.size.saturating_sub(offset)).min(hello.payload_bytes as u64) as usize;
                if payload_len != expected_len {
                    telemetry.invalid_datagrams = telemetry.invalid_datagrams.saturating_add(1);
                    break 'packet;
                }
                telemetry.valid_datagrams = telemetry.valid_datagrams.saturating_add(1);
                if repair {
                    telemetry.repair_datagrams = telemetry.repair_datagrams.saturating_add(1);
                }
                if !bitmap_contains(&state.bitmap, sequence) {
                    telemetry.accepted_datagrams = telemetry.accepted_datagrams.saturating_add(1);
                    if !repair && highest.is_some_and(|frontier| sequence < frontier) {
                        reordering_observed = true;
                    }
                    write_all_at(
                        &file,
                        &packet[HEADER_SIZE..HEADER_SIZE + payload_len],
                        offset,
                    )?;
                    bitmap_insert(&mut state.bitmap, sequence);
                    state.received_count += 1;
                    checkpoint_dirty = true;
                    if state.received_count % 64 == 0 || (repair && sequence == hash_frontier) {
                        advance_hash_frontier(
                            &file,
                            &state.bitmap,
                            hello.chunk_layout(),
                            &mut hash_frontier,
                            &mut object_hasher,
                            &mut hash_buffer,
                        )?;
                    }
                } else {
                    telemetry.duplicate_datagrams = telemetry.duplicate_datagrams.saturating_add(1);
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
                ControlEvent::Ping => control.send("PONG")?,
                ControlEvent::CompleteAck => {
                    return Err("received completion acknowledgement before completion".into());
                }
                ControlEvent::Cancel(reason) => {
                    if checkpoint_dirty {
                        state.checkpoint(&file)?;
                    }
                    return Err(format!("sender cancelled transfer: {reason}").into());
                }
                ControlEvent::Error(error) => {
                    if checkpoint_dirty {
                        state.checkpoint(&file)?;
                    }
                    return Err(error.into());
                }
            }
        }

        if ended && state.received_count == hello.chunks {
            advance_hash_frontier(
                &file,
                &state.bitmap,
                hello.chunk_layout(),
                &mut hash_frontier,
                &mut object_hasher,
                &mut hash_buffer,
            )?;
            if hash_frontier != hello.chunks {
                return Err("complete receipt map has a hash-frontier gap".into());
            }
            file.sync_all()?;
            let actual_hash = hex_digest(object_hasher.finalize().as_slice());
            if actual_hash != hello.hash {
                control.send("CANCEL hash-mismatch")?;
                state.discard();
                return Err("received UDP file hash did not match".into());
            }
            state.install(destination)?;
            update_receiver_telemetry(&mut telemetry, &state, highest, &udp);
            telemetry.reports = telemetry.reports.saturating_add(1);
            control.send(&format_feedback_report(&telemetry, &[]))?;
            control.send("COMPLETE")?;
            let acknowledgement_deadline = Instant::now() + idle_timeout;
            loop {
                let remaining = acknowledgement_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("timed out waiting for completion acknowledgement".into());
                }
                match control_rx.recv_timeout(remaining) {
                    Ok(ControlEvent::CompleteAck) => break,
                    Ok(ControlEvent::Ping) => control.send("PONG")?,
                    Ok(ControlEvent::End) => {}
                    Ok(ControlEvent::Cancel(reason)) => {
                        return Err(format!("sender cancelled transfer: {reason}").into());
                    }
                    Ok(ControlEvent::Error(error)) => return Err(error.into()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        return Err("timed out waiting for completion acknowledgement".into());
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(
                            "control reader stopped before completion acknowledgement".into()
                        );
                    }
                }
            }
            break;
        }

        if checkpoint_dirty && last_checkpoint.elapsed() >= RESUME_CHECKPOINT_INTERVAL {
            state.checkpoint(&file)?;
            checkpoint_dirty = false;
            last_checkpoint = Instant::now();
        }

        if last_report.elapsed() >= hello.report_interval {
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
            update_receiver_telemetry(&mut telemetry, &state, highest, &udp);
            telemetry.reports = telemetry.reports.saturating_add(1);
            control.send(&format_feedback_report(&telemetry, &ranges))?;
            last_report = Instant::now();
        }
    }

    control_thread
        .join()
        .map_err(|_| "control reader thread panicked")?;
    Ok(telemetry)
}

fn apply_reusable_content(
    destination: &File,
    state: &mut ResumeState,
    target: &cdc::Manifest,
    candidate_path: &Path,
    layout: ChunkLayout,
) -> AnyResult<(u64, u64)> {
    let candidate = match cdc::scan(candidate_path) {
        Ok(manifest) => manifest,
        Err(_error) if candidate_path.try_exists().is_ok_and(|exists| !exists) => {
            return Ok((0, 0));
        }
        Err(error) => return Err(error),
    };
    let source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(candidate_path)?;
    let mut available = HashMap::with_capacity(candidate.chunks.len());
    for chunk in &candidate.chunks {
        available
            .entry((chunk.length, chunk.hash))
            .or_insert(chunk.offset);
    }

    let mut copied = Vec::<(u64, u64)>::new();
    let mut buffer = vec![0_u8; cdc::MAX_CHUNK_BYTES];
    for chunk in &target.chunks {
        let Some(source_offset) = available.get(&(chunk.length, chunk.hash)).copied() else {
            continue;
        };
        let bytes = &mut buffer[..chunk.length as usize];
        read_exact_at(&source, bytes, source_offset)?;
        let actual: [u8; 32] = Sha256::digest(&*bytes).into();
        if actual != chunk.hash {
            continue;
        }
        write_all_at(destination, bytes, chunk.offset)?;
        copied.push((chunk.offset, chunk.offset + u64::from(chunk.length)));
    }
    if copied.is_empty() {
        return Ok((0, 0));
    }

    let mut merged = Vec::<(u64, u64)>::new();
    for range in copied {
        if let Some(last) = merged.last_mut()
            && range.0 <= last.1
        {
            last.1 = last.1.max(range.1);
        } else {
            merged.push(range);
        }
    }
    let mut range_index = 0_usize;
    let mut reused_bytes = 0_u64;
    let mut reused_chunks = 0_u64;
    for sequence in 0..layout.chunks {
        if bitmap_contains(&state.bitmap, sequence) {
            continue;
        }
        let start = sequence
            .checked_mul(layout.payload_bytes as u64)
            .ok_or("reuse packet offset overflow")?;
        let end = start
            .checked_add(layout.payload_bytes as u64)
            .ok_or("reuse packet offset overflow")?
            .min(layout.size);
        while range_index < merged.len() && merged[range_index].1 <= start {
            range_index += 1;
        }
        if merged
            .get(range_index)
            .is_some_and(|range| range.0 <= start && range.1 >= end)
        {
            bitmap_insert(&mut state.bitmap, sequence);
            state.received_count = state.received_count.saturating_add(1);
            reused_chunks = reused_chunks.saturating_add(1);
            reused_bytes = reused_bytes.saturating_add(end - start);
        }
    }
    if reused_chunks > 0 {
        state.checkpoint(destination)?;
    }
    Ok((reused_bytes, reused_chunks))
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> AnyResult<()> {
    while !buffer.is_empty() {
        let count = file.read_at(buffer, offset)?;
        if count == 0 {
            return Err("reuse candidate changed while it was read".into());
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or("reuse candidate offset overflow")?;
        buffer = &mut buffer[count..];
    }
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

fn advance_hash_frontier(
    file: &File,
    bitmap: &[u64],
    layout: ChunkLayout,
    frontier: &mut u64,
    hasher: &mut Sha256,
    buffer: &mut [u8],
) -> AnyResult<()> {
    let start = *frontier;
    let mut end = start;
    while end < layout.chunks && bitmap_contains(bitmap, end) {
        end += 1;
    }
    let mut offset = start
        .checked_mul(layout.payload_bytes as u64)
        .ok_or("hash frontier offset overflow")?;
    let byte_end = end
        .checked_mul(layout.payload_bytes as u64)
        .ok_or("hash frontier offset overflow")?
        .min(layout.size);
    while offset < byte_end {
        let wanted = (byte_end - offset).min(buffer.len() as u64) as usize;
        let count = file.read_at(&mut buffer[..wanted], offset)?;
        if count == 0 {
            return Err("short read while advancing object hash".into());
        }
        hasher.update(&buffer[..count]);
        offset += count as u64;
    }
    *frontier = end;
    Ok(())
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

enum ControlEvent {
    End,
    Ping,
    CompleteAck,
    Cancel(String),
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

fn update_receiver_telemetry(
    telemetry: &mut ReceiverTelemetry,
    state: &ResumeState,
    highest: Option<u64>,
    udp: &UdpSocket,
) {
    telemetry.received_chunks = state.received_count;
    telemetry.frontier_chunks = highest.map_or(0, |sequence| sequence + 1);
    telemetry.socket_drops = udp_socket_drops(udp);
}

fn format_feedback_report(telemetry: &ReceiverTelemetry, ranges: &[(u64, u64)]) -> String {
    let socket_drops = telemetry
        .socket_drops
        .map_or_else(|| "-".to_owned(), |drops| drops.to_string());
    let mut encoded_ranges = String::new();
    for (index, (start, end)) in ranges.iter().enumerate() {
        if index > 0 {
            encoded_ranges.push(',');
        }
        if start == end {
            encoded_ranges.push_str(&start.to_string());
        } else {
            encoded_ranges.push_str(&format!("{start}-{end}"));
        }
    }
    if encoded_ranges.is_empty() {
        encoded_ranges.push('-');
    }
    let report = format!(
        "M {} {} {} {} {} {} {} {} {}",
        telemetry.received_chunks,
        telemetry.frontier_chunks,
        telemetry.accepted_datagrams,
        telemetry.valid_datagrams,
        telemetry.duplicate_datagrams,
        telemetry.invalid_datagrams,
        telemetry.repair_datagrams,
        socket_drops,
        encoded_ranges
    );
    debug_assert!(report.len() <= MAX_FEEDBACK_REPORT_SIZE);
    report
}

fn udp_socket_drops(socket: &UdpSocket) -> Option<u64> {
    let link = fs::read_link(format!("/proc/self/fd/{}", socket.as_raw_fd())).ok()?;
    let link = link.to_str()?;
    let inode = link.strip_prefix("socket:[")?.strip_suffix(']')?;
    for table in ["/proc/net/udp", "/proc/net/udp6"] {
        let Ok(contents) = fs::read_to_string(table) else {
            continue;
        };
        for line in contents.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.get(9) == Some(&inode) {
                return fields.last()?.parse().ok();
            }
        }
    }
    None
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
    fn telemetry_reports_round_trip_and_reject_inconsistent_counters() {
        let telemetry = ReceiverTelemetry {
            received_chunks: 8,
            frontier_chunks: 10,
            accepted_datagrams: 7,
            valid_datagrams: 9,
            duplicate_datagrams: 2,
            invalid_datagrams: 3,
            repair_datagrams: 4,
            socket_drops: Some(5),
            reports: 0,
        };
        let line = format_feedback_report(&telemetry, &[(1, 2), (9, 9)]);
        assert_eq!(line, "M 8 10 7 9 2 3 4 5 1-2,9");
        let (decoded, ranges) = parse_feedback_report(&line, 10).unwrap();
        assert_eq!(decoded, telemetry);
        assert_eq!(ranges, "1-2,9");

        assert!(parse_feedback_report("M 8 10 7 8 2 3 4 5 -", 10).is_err());
        assert!(parse_feedback_report("M 08 10 7 9 2 3 4 5 -", 10).is_err());
        assert!(parse_feedback_report("M 8 11 7 9 2 3 4 5 -", 10).is_err());
    }

    #[test]
    fn empty_telemetry_report_uses_explicit_sentinels() {
        let line = format_feedback_report(&ReceiverTelemetry::default(), &[]);
        assert_eq!(line, "M 0 0 0 0 0 0 0 - -");
        let (decoded, ranges) = parse_feedback_report(&line, 0).unwrap();
        assert_eq!(decoded, ReceiverTelemetry::default());
        assert!(ranges.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_udp_drop_counter_is_discoverable() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(udp_socket_drops(&socket).is_some());
    }

    #[test]
    fn packet_kind_distinguishes_originals_and_repairs() {
        let path =
            std::env::temp_dir().join(format!("packet-tide-packet-kind-{}", std::process::id()));
        fs::write(&path, b"payload").unwrap();
        let file = File::open(&path).unwrap();
        let auth = auth::test_session();
        let payload_bytes = DEFAULT_UDP_PAYLOAD_BYTES;
        let mut packet = vec![0_u8; HEADER_SIZE + payload_bytes];
        let original = build_packet(&file, 7, &auth, 0, false, payload_bytes, &mut packet).unwrap();
        assert_eq!(packet[0], 4);
        assert_eq!(packet[9], 0);
        assert_eq!(u16::from_be_bytes(packet[10..12].try_into().unwrap()), 7);
        assert_eq!(original, HEADER_SIZE + 7);
        let repair = build_packet(&file, 7, &auth, 0, true, payload_bytes, &mut packet).unwrap();
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

    fn controller_step(
        controller: &mut RateController,
        telemetry: &mut ReceiverTelemetry,
        step: u32,
        capacity_mbps: f64,
    ) {
        let interval = 0.2;
        let payload_bytes = DEFAULT_UDP_PAYLOAD_BYTES;
        let offered =
            (controller.rate_mbps * 1_000_000.0 * interval / (payload_bytes as f64 * 8.0)) as u64;
        let accepted = (controller.rate_mbps.min(capacity_mbps) * 1_000_000.0 * interval
            / (payload_bytes as f64 * 8.0)) as u64;
        telemetry.received_chunks += accepted;
        telemetry.accepted_datagrams += accepted;
        telemetry.valid_datagrams += accepted;
        let dropped = offered.saturating_sub(accepted);
        telemetry.socket_drops = Some(telemetry.socket_drops.unwrap_or(0) + dropped);
        controller.observe(
            telemetry,
            controller.started + Duration::from_millis(u64::from(step) * 200),
            payload_bytes,
        );
    }

    #[test]
    fn automatic_rate_converges_and_tracks_a_capacity_drop() {
        let mut controller = RateController::new(None, 10.0, 200.0, Duration::from_millis(50));
        let mut telemetry = ReceiverTelemetry {
            socket_drops: Some(0),
            ..ReceiverTelemetry::default()
        };
        for step in 1..=14 {
            controller_step(&mut controller, &mut telemetry, step, 80.0);
        }
        assert!(controller.rate_mbps >= 40.0, "{}", controller.rate_mbps);
        for step in 15..=24 {
            controller_step(&mut controller, &mut telemetry, step, 20.0);
        }
        assert!(
            (10.0..=35.0).contains(&controller.rate_mbps),
            "{}",
            controller.rate_mbps
        );
        assert!(
            controller
                .decisions
                .iter()
                .any(|decision| decision.kind == RateDecisionKind::Increase)
        );
        assert!(
            controller
                .decisions
                .iter()
                .any(|decision| decision.kind == RateDecisionKind::Decrease)
        );
    }

    #[test]
    fn receiver_waste_causes_backoff_without_collapse() {
        let mut controller = RateController::new(None, 10.0, 100.0, Duration::from_millis(50));
        controller.rate_mbps = 50.0;
        let mut telemetry = ReceiverTelemetry {
            received_chunks: 100,
            accepted_datagrams: 100,
            valid_datagrams: 100,
            socket_drops: Some(10),
            ..ReceiverTelemetry::default()
        };
        controller.observe(
            &telemetry,
            controller.started + Duration::from_millis(200),
            DEFAULT_UDP_PAYLOAD_BYTES,
        );
        assert_eq!(controller.rate_mbps, 35.0);
        telemetry.socket_drops = Some(1_000);
        for step in 2..=20 {
            controller.observe(
                &telemetry,
                controller.started + Duration::from_millis(step as u64 * 200),
                DEFAULT_UDP_PAYLOAD_BYTES,
            );
        }
        assert_eq!(controller.rate_mbps, 10.0);
    }

    #[test]
    fn fixed_rate_controller_never_changes_or_records_decisions() {
        let mut controller =
            RateController::new(Some(123.0), 10.0, 1_000.0, Duration::from_millis(50));
        let telemetry = ReceiverTelemetry {
            socket_drops: Some(1_000),
            ..ReceiverTelemetry::default()
        };
        controller.observe(
            &telemetry,
            controller.started + Duration::from_secs(10),
            DEFAULT_UDP_PAYLOAD_BYTES,
        );
        assert_eq!(controller.rate_mbps, 123.0);
        assert!(controller.decisions.is_empty());
        assert!(controller.json().contains("\"mode\":\"fixed\""));
    }

    #[test]
    fn rate_decision_history_is_bounded_but_counts_every_decision() {
        let mut controller = RateController::new(None, 10.0, 100.0, Duration::from_millis(50));
        let telemetry = ReceiverTelemetry::default();
        for step in 1..=(MAX_RATE_DECISIONS as u64 + 10) {
            controller.observe(
                &telemetry,
                controller.started + Duration::from_millis(step * 200),
                DEFAULT_UDP_PAYLOAD_BYTES,
            );
        }
        assert_eq!(controller.decisions.len(), MAX_RATE_DECISIONS);
        assert_eq!(controller.decision_count, MAX_RATE_DECISIONS as u64 + 10);
        assert_eq!(controller.hold_count, controller.decision_count);
        assert!(controller.json().contains("\"omitted_decision_count\":10"));
    }

    fn minimal_send_args(extra: &[&str]) -> Vec<String> {
        let mut args = vec![
            "--connect",
            "127.0.0.1:9000",
            "--udp-target",
            "127.0.0.1:9001",
            "--file",
            "/tmp/source",
            "--transport",
            "udp",
            "--key-file",
            "/tmp/key",
        ];
        args.extend_from_slice(extra);
        args.into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn automatic_rate_is_default_and_fixed_rate_rejects_auto_bounds() {
        let automatic = parse_send(minimal_send_args(&[])).unwrap();
        assert_eq!(automatic.rate_mbps, None);
        assert_eq!(automatic.min_rate_mbps, DEFAULT_AUTO_MIN_RATE_MBPS);
        assert_eq!(automatic.max_rate_mbps, DEFAULT_AUTO_MAX_RATE_MBPS);

        let fixed = parse_send(minimal_send_args(&["--rate-mbps", "123"])).unwrap();
        assert_eq!(fixed.rate_mbps, Some(123.0));
        assert!(
            parse_send(minimal_send_args(&[
                "--rate-mbps",
                "123",
                "--max-rate-mbps",
                "500",
            ]))
            .is_err()
        );
    }

    #[test]
    fn idle_timeout_is_bounded_and_defaults_to_thirty_seconds() {
        assert_eq!(parse_idle_timeout(&[]).unwrap(), Duration::from_secs(30));
        assert_eq!(
            parse_idle_timeout(&["--idle-timeout-ms".into(), "500".into()]).unwrap(),
            Duration::from_millis(500)
        );
        assert!(parse_idle_timeout(&["--idle-timeout-ms".into(), "499".into()]).is_err());
        assert!(parse_idle_timeout(&["--idle-timeout-ms".into(), "3600001".into()]).is_err());
    }

    #[test]
    fn negotiated_udp_parameters_are_bounded() {
        assert_eq!(parse_udp_payload_bytes(&[]).unwrap(), 1172);
        assert_eq!(
            parse_report_interval(&[]).unwrap(),
            Duration::from_millis(50)
        );
        assert_eq!(
            parse_udp_payload_bytes(&["--udp-payload-bytes".into(), "256".into()]).unwrap(),
            256
        );
        assert_eq!(
            parse_udp_payload_bytes(&["--udp-payload-bytes".into(), "1424".into()]).unwrap(),
            1424
        );
        assert!(parse_udp_payload_bytes(&["--udp-payload-bytes".into(), "255".into()]).is_err());
        assert!(parse_udp_payload_bytes(&["--udp-payload-bytes".into(), "1425".into()]).is_err());
        assert!(parse_report_interval(&["--feedback-interval-ms".into(), "9".into()]).is_err());
        assert!(parse_report_interval(&["--feedback-interval-ms".into(), "10001".into()]).is_err());
    }

    #[test]
    fn authenticated_hello_binds_udp_parameters() {
        let hash = "00".repeat(32);
        let session = "11".repeat(16);
        let valid = format!("UDP 2000 {hash} {session} 4 512 20 125 R0");
        let hello = parse_hello(&valid).unwrap();
        assert_eq!(hello.chunks, 4);
        assert_eq!(hello.payload_bytes, 512);
        assert_eq!(hello.report_interval, Duration::from_millis(20));
        assert!(parse_hello(&format!("UDP 2000 {hash} {session} 4 0512 20 125 R0")).is_err());
        assert!(parse_hello(&format!("UDP 2000 {hash} {session} 3 512 20 125 R0")).is_err());
        assert!(parse_hello(&format!("UDP 2000 {hash} {session} 4 255 20 125 R0")).is_err());
        assert!(parse_hello(&format!("UDP 2000 {hash} {session} 4 512 9 125 R0")).is_err());
        assert!(parse_hello(&format!("UDP 2000 {hash} {session} 4 512 20 19 R0")).is_err());
        assert!(
            parse_hello(&format!(
                "TCP 2000 {hash} {session} 4 512 20 125 R1 1 {hash}"
            ))
            .is_err()
        );
        assert!(
            parse_hello(&format!(
                "UDP 2000 {hash} {session} 4 512 20 125 R1 01 {hash}"
            ))
            .is_err()
        );
    }

    #[test]
    fn object_hash_advances_only_over_contiguous_received_chunks() {
        let path =
            std::env::temp_dir().join(format!("packet-tide-hash-frontier-{}", std::process::id()));
        let payload_bytes = 512;
        let bytes = vec![0x5a; payload_bytes * 3];
        fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        let mut bitmap = vec![0_u64; 1];
        bitmap_insert(&mut bitmap, 0);
        bitmap_insert(&mut bitmap, 2);
        let mut frontier = 0;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 4096];
        advance_hash_frontier(
            &file,
            &bitmap,
            ChunkLayout {
                size: bytes.len() as u64,
                chunks: 3,
                payload_bytes,
            },
            &mut frontier,
            &mut hasher,
            &mut buffer,
        )
        .unwrap();
        assert_eq!(frontier, 1);
        assert_eq!(
            hex_digest(hasher.clone().finalize().as_slice()),
            hex_digest(Sha256::digest(&bytes[..payload_bytes]).as_slice())
        );

        bitmap_insert(&mut bitmap, 1);
        advance_hash_frontier(
            &file,
            &bitmap,
            ChunkLayout {
                size: bytes.len() as u64,
                chunks: 3,
                payload_bytes,
            },
            &mut frontier,
            &mut hasher,
            &mut buffer,
        )
        .unwrap();
        assert_eq!(frontier, 3);
        assert_eq!(
            hex_digest(hasher.finalize().as_slice()),
            hex_digest(Sha256::digest(&bytes).as_slice())
        );
        fs::remove_file(path).unwrap();
    }
}
