use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

type AnyResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 32;
const MAX_HANDSHAKE_LINE: usize = 2048;
const MAX_CONTROL_LINE: usize = 64 * 1024;

pub(crate) const UDP_TAG_LEN: usize = 16;

pub(crate) struct SecretKey([u8; KEY_LEN]);

impl SecretKey {
    pub(crate) fn load(path: &Path) -> AnyResult<Self> {
        let metadata = fs::metadata(path)?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "key file {} must not be accessible by group or others (chmod 600)",
                path.display()
            )
            .into());
        }
        let bytes = fs::read(path)?;
        if bytes.len() != KEY_LEN {
            return Err(format!("key file must contain exactly {KEY_LEN} raw bytes").into());
        }
        let mut key = [0_u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        Ok(Self(key))
    }

    pub(crate) fn generate(path: &Path) -> AnyResult<()> {
        let mut key = [0_u8; KEY_LEN];
        random_bytes(&mut key)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&key)?;
        file.sync_all()?;
        key.fill(0);
        Ok(())
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone)]
pub(crate) struct SessionAuth {
    pub(crate) session: [u8; 16],
    c2s: [u8; 32],
    s2c: [u8; 32],
    pub(crate) udp: [u8; 32],
    pub(crate) lane: [u8; 32],
}

impl Drop for SessionAuth {
    fn drop(&mut self) {
        self.c2s.fill(0);
        self.s2c.fill(0);
        self.udp.fill(0);
        self.lane.fill(0);
    }
}

pub(crate) enum Direction {
    ClientToServer,
    ServerToClient,
}

impl SessionAuth {
    fn control_key(&self, direction: &Direction) -> &[u8; 32] {
        match direction {
            Direction::ClientToServer => &self.c2s,
            Direction::ServerToClient => &self.s2c,
        }
    }
}

pub(crate) struct ControlWriter {
    stream: TcpStream,
    auth: SessionAuth,
    direction: Direction,
    sequence: u64,
}

impl ControlWriter {
    pub(crate) fn new(stream: TcpStream, auth: SessionAuth, direction: Direction) -> Self {
        Self {
            stream,
            auth,
            direction,
            sequence: 0,
        }
    }

    pub(crate) fn send(&mut self, payload: &str) -> AnyResult<()> {
        if payload.contains('\n') || payload.len() > MAX_CONTROL_LINE / 2 {
            return Err("invalid authenticated control payload".into());
        }
        let message =
            control_mac_input(&self.auth.session, &self.direction, self.sequence, payload);
        let mac = hmac_sha256(self.auth.control_key(&self.direction), &message);
        writeln!(self.stream, "A {} {} {}", self.sequence, payload, hex(&mac))?;
        self.stream.flush()?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("control sequence exhausted")?;
        Ok(())
    }
}

pub(crate) struct ControlReader {
    stream: TcpStream,
    auth: SessionAuth,
    direction: Direction,
    sequence: u64,
}

impl ControlReader {
    pub(crate) fn new(stream: TcpStream, auth: SessionAuth, direction: Direction) -> Self {
        Self {
            stream,
            auth,
            direction,
            sequence: 0,
        }
    }

    pub(crate) fn recv(&mut self) -> AnyResult<String> {
        let line = read_line_limited(&mut self.stream, MAX_CONTROL_LINE)?;
        let mut fields = line.splitn(3, ' ');
        if fields.next() != Some("A") {
            return Err("invalid authenticated control frame".into());
        }
        let sequence: u64 = fields.next().ok_or("missing control sequence")?.parse()?;
        if sequence != self.sequence {
            return Err("replayed or out-of-order control frame".into());
        }
        let tail = fields.next().ok_or("missing control payload")?;
        let (payload, mac_hex) = tail.rsplit_once(' ').ok_or("missing control MAC")?;
        let supplied = decode_array::<32>(mac_hex)?;
        let input = control_mac_input(&self.auth.session, &self.direction, sequence, payload);
        let expected = hmac_sha256(self.auth.control_key(&self.direction), &input);
        if !constant_time_eq(&supplied, &expected) {
            return Err("control authentication failed".into());
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("control sequence exhausted")?;
        Ok(payload.to_owned())
    }
}

pub(crate) fn client_handshake(
    stream: &mut TcpStream,
    key: &SecretKey,
    hello: &str,
) -> AnyResult<SessionAuth> {
    let challenge_line = read_line_limited(stream, MAX_HANDSHAKE_LINE)?;
    let challenge_hex = challenge_line
        .strip_prefix("TSU2C ")
        .ok_or("receiver did not offer a TSU2 challenge")?;
    let challenge = decode_array::<NONCE_LEN>(challenge_hex)?;
    let hello_mac = hmac_sha256(
        &key.0,
        &joined(b"tsu2 client hello\0", &[&challenge, hello.as_bytes()]),
    );
    writeln!(stream, "TSU2 {hello} {}", hex(&hello_mac))?;
    stream.flush()?;

    let server_line = read_line_limited(stream, MAX_HANDSHAKE_LINE)?;
    let fields: Vec<_> = server_line.split_whitespace().collect();
    if fields.len() != 3 || fields[0] != "TSU2S" {
        return Err("invalid TSU2 server response".into());
    }
    let server_nonce = decode_array::<NONCE_LEN>(fields[1])?;
    let supplied = decode_array::<32>(fields[2])?;
    let transcript = joined(
        b"tsu2 transcript\0",
        &[&challenge, hello.as_bytes(), &server_nonce],
    );
    let expected = hmac_sha256(&key.0, &joined(b"tsu2 server hello\0", &[&transcript]));
    if !constant_time_eq(&supplied, &expected) {
        return Err("receiver authentication failed".into());
    }
    let finish = hmac_sha256(&key.0, &joined(b"tsu2 client finish\0", &[&transcript]));
    writeln!(stream, "TSU2F {}", hex(&finish))?;
    stream.flush()?;
    Ok(derive_session(&key.0, &transcript))
}

pub(crate) fn server_handshake(
    stream: &mut TcpStream,
    key: &SecretKey,
) -> AnyResult<(String, SessionAuth)> {
    let mut challenge = [0_u8; NONCE_LEN];
    random_bytes(&mut challenge)?;
    writeln!(stream, "TSU2C {}", hex(&challenge))?;
    stream.flush()?;

    let client_line = read_line_limited(stream, MAX_HANDSHAKE_LINE)?;
    let body = client_line
        .strip_prefix("TSU2 ")
        .ok_or("invalid TSU2 client greeting")?;
    let (hello, mac_hex) = body.rsplit_once(' ').ok_or("missing client greeting MAC")?;
    let supplied = decode_array::<32>(mac_hex)?;
    let expected = hmac_sha256(
        &key.0,
        &joined(b"tsu2 client hello\0", &[&challenge, hello.as_bytes()]),
    );
    if !constant_time_eq(&supplied, &expected) {
        return Err("client authentication failed".into());
    }

    let mut server_nonce = [0_u8; NONCE_LEN];
    random_bytes(&mut server_nonce)?;
    let transcript = joined(
        b"tsu2 transcript\0",
        &[&challenge, hello.as_bytes(), &server_nonce],
    );
    let response = hmac_sha256(&key.0, &joined(b"tsu2 server hello\0", &[&transcript]));
    writeln!(stream, "TSU2S {} {}", hex(&server_nonce), hex(&response))?;
    stream.flush()?;
    let finish_line = read_line_limited(stream, MAX_HANDSHAKE_LINE)?;
    let supplied_finish = finish_line
        .strip_prefix("TSU2F ")
        .ok_or("invalid client finish")?;
    let supplied_finish = decode_array::<32>(supplied_finish)?;
    let expected_finish = hmac_sha256(&key.0, &joined(b"tsu2 client finish\0", &[&transcript]));
    if !constant_time_eq(&supplied_finish, &expected_finish) {
        return Err("client finish authentication failed".into());
    }
    Ok((hello.to_owned(), derive_session(&key.0, &transcript)))
}

pub(crate) fn random_session() -> AnyResult<[u8; 16]> {
    let mut value = [0_u8; 16];
    random_bytes(&mut value)?;
    Ok(value)
}

pub(crate) fn lane_mac(key: &[u8; 32], session: &[u8; 16], lane: usize) -> [u8; 32] {
    hmac_sha256(
        key,
        &joined(b"tsu2 lane\0", &[session, &(lane as u64).to_be_bytes()]),
    )
}

#[cfg(test)]
pub(crate) fn udp_tag(key: &[u8; 32], packet_without_tag: &[u8]) -> [u8; UDP_TAG_LEN] {
    udp_tag_parts(key, packet_without_tag, &[])
}

pub(crate) fn udp_tag_parts(key: &[u8; 32], header: &[u8], payload: &[u8]) -> [u8; UDP_TAG_LEN] {
    let full = hmac_sha256_parts(key, &[b"tsu2 udp\0", header, payload]);
    full[..UDP_TAG_LEN].try_into().expect("fixed tag length")
}

#[cfg(test)]
pub(crate) fn verify_udp_tag(key: &[u8; 32], packet_without_tag: &[u8], tag: &[u8]) -> bool {
    constant_time_eq(&udp_tag(key, packet_without_tag), tag)
}

pub(crate) fn verify_udp_tag_parts(
    key: &[u8; 32],
    header: &[u8],
    payload: &[u8],
    tag: &[u8],
) -> bool {
    constant_time_eq(&udp_tag_parts(key, header, payload), tag)
}

pub(crate) fn constant_time_verify(left: &[u8], right: &[u8]) -> bool {
    constant_time_eq(left, right)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}

pub(crate) fn decode_array<const N: usize>(value: &str) -> AnyResult<[u8; N]> {
    if value.len() != N * 2 {
        return Err("invalid hexadecimal field length".into());
    }
    let mut result = [0_u8; N];
    for (index, byte) in result.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(result)
}

fn derive_session(psk: &[u8; 32], transcript: &[u8]) -> SessionAuth {
    let root = hmac_sha256(psk, &joined(b"tsu2 session root\0", &[transcript]));
    let derive = |label: &[u8]| hmac_sha256(&root, label);
    let session_full = derive(b"session id");
    SessionAuth {
        session: session_full[..16].try_into().expect("fixed session length"),
        c2s: derive(b"client to server control"),
        s2c: derive(b"server to client control"),
        udp: derive(b"udp packets"),
        lane: derive(b"tcp4 lanes"),
    }
}

#[cfg(test)]
pub(crate) fn test_session() -> SessionAuth {
    derive_session(&[7_u8; 32], b"test transcript")
}

fn control_mac_input(
    session: &[u8; 16],
    direction: &Direction,
    sequence: u64,
    payload: &str,
) -> Vec<u8> {
    let direction = match direction {
        Direction::ClientToServer => b'c',
        Direction::ServerToClient => b's',
    };
    joined(
        b"tsu2 control\0",
        &[
            session,
            &[direction],
            &sequence.to_be_bytes(),
            payload.as_bytes(),
        ],
    )
}

fn joined(prefix: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let capacity = prefix.len() + parts.iter().map(|part| part.len()).sum::<usize>();
    let mut result = Vec::with_capacity(capacity);
    result.extend_from_slice(prefix);
    for part in parts {
        result.extend_from_slice(part);
    }
    result
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    hmac_sha256_parts(key, &[message])
}

fn hmac_sha256_parts(key: &[u8], messages: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut normalized = [0_u8; BLOCK];
    if key.len() > BLOCK {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for message in messages {
        inner.update(message);
    }
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn random_bytes(output: &mut [u8]) -> AnyResult<()> {
    let mut random = File::open("/dev/urandom")?;
    random.read_exact(output)?;
    Ok(())
}

fn read_line_limited(stream: &mut TcpStream, limit: usize) -> AnyResult<String> {
    let mut bytes = Vec::new();
    while bytes.len() < limit {
        let mut byte = [0_u8; 1];
        if stream.read(&mut byte)? == 0 {
            return Err("connection closed during authenticated message".into());
        }
        if byte[0] == b'\n' {
            return Ok(String::from_utf8(bytes)?);
        }
        if byte[0] == b'\r' {
            return Err("carriage returns are not allowed in protocol messages".into());
        }
        bytes.push(byte[0]);
    }
    Err("authenticated protocol line is too long".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let connector = thread::spawn(move || TcpStream::connect(address).unwrap());
        let (server, _) = listener.accept().unwrap();
        (connector.join().unwrap(), server)
    }

    #[test]
    fn hmac_matches_rfc_4231_vector() {
        let key = [0x0b_u8; 20];
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn udp_tag_detects_tampering_and_wrong_key() {
        let key = [7_u8; 32];
        let other = [8_u8; 32];
        let packet = b"authenticated packet";
        let tag = udp_tag(&key, packet);
        assert!(verify_udp_tag(&key, packet, &tag));
        assert!(!verify_udp_tag(&key, b"authenticated packeu", &tag));
        assert!(!verify_udp_tag(&other, packet, &tag));
    }

    #[test]
    fn derivation_separates_keys_and_sessions() {
        let first = derive_session(&[1_u8; 32], b"first transcript");
        let second = derive_session(&[1_u8; 32], b"second transcript");
        assert_ne!(first.session, second.session);
        assert_ne!(first.c2s, first.s2c);
        assert_ne!(first.udp, first.lane);
    }

    #[test]
    fn mutual_handshake_derives_the_same_fresh_session() {
        let (mut client, mut server) = tcp_pair();
        let server_thread =
            thread::spawn(move || server_handshake(&mut server, &SecretKey([9_u8; 32])).unwrap());
        let client_auth = client_handshake(
            &mut client,
            &SecretKey([9_u8; 32]),
            "UDP 7 0000000000000000000000000000000000000000000000000000000000000000 00000000000000000000000000000000 1 50",
        )
        .unwrap();
        let (_, server_auth) = server_thread.join().unwrap();
        assert_eq!(client_auth.session, server_auth.session);
        assert_eq!(client_auth.udp, server_auth.udp);
    }

    #[test]
    fn control_reader_rejects_replay() {
        let (mut sender, receiver) = tcp_pair();
        let auth = derive_session(&[3_u8; 32], b"control replay test");
        let input = control_mac_input(&auth.session, &Direction::ServerToClient, 0, "READY");
        let mac = hmac_sha256(auth.control_key(&Direction::ServerToClient), &input);
        let frame = format!("A 0 READY {}\n", hex(&mac));
        sender.write_all(frame.as_bytes()).unwrap();
        sender.write_all(frame.as_bytes()).unwrap();
        let mut reader = ControlReader::new(receiver, auth, Direction::ServerToClient);
        assert_eq!(reader.recv().unwrap(), "READY");
        assert!(reader.recv().is_err());
    }
}
