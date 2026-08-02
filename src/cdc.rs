use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::AnyResult;

pub(crate) const MIN_CHUNK_BYTES: usize = 16 * 1024;
pub(crate) const AVERAGE_CHUNK_BYTES: usize = 64 * 1024;
pub(crate) const MAX_CHUNK_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CHUNKS: usize = 1_000_000;
pub(crate) const MAX_ENCODED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Chunk {
    pub(crate) offset: u64,
    pub(crate) length: u32,
    pub(crate) hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Manifest {
    pub(crate) size: u64,
    pub(crate) chunks: Vec<Chunk>,
    pub(crate) hash: [u8; 32],
}

pub(crate) fn scan(path: &Path) -> AnyResult<Manifest> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err("content-defined source is not a regular file".into());
    }
    let mut chunks = Vec::new();
    let mut input = vec![0_u8; 1024 * 1024];
    let mut chunk = Vec::with_capacity(MAX_CHUNK_BYTES);
    let mut rolling = 0_u64;
    let mut offset = 0_u64;
    loop {
        let count = file.read(&mut input)?;
        if count == 0 {
            break;
        }
        for byte in &input[..count] {
            rolling = (rolling << 1).wrapping_add(gear(*byte));
            chunk.push(*byte);
            if chunk.len() >= MIN_CHUNK_BYTES
                && (rolling & (AVERAGE_CHUNK_BYTES as u64 - 1) == 0
                    || chunk.len() >= MAX_CHUNK_BYTES)
            {
                push_chunk(&mut chunks, offset, &chunk)?;
                offset = offset
                    .checked_add(chunk.len() as u64)
                    .ok_or("content-defined offset overflow")?;
                chunk.clear();
                rolling = 0;
            }
        }
    }
    if !chunk.is_empty() {
        push_chunk(&mut chunks, offset, &chunk)?;
        offset = offset
            .checked_add(chunk.len() as u64)
            .ok_or("content-defined size overflow")?;
    }
    if offset != metadata.len() {
        return Err("content-defined source changed while it was scanned".into());
    }
    let body = canonical_body(offset, &chunks);
    if body.len().saturating_add(70) > MAX_ENCODED_BYTES {
        return Err("content-defined manifest byte limit exceeded".into());
    }
    Ok(Manifest {
        size: offset,
        chunks,
        hash: Sha256::digest(body.as_bytes()).into(),
    })
}

pub(crate) fn encode(manifest: &Manifest) -> String {
    let body = canonical_body(manifest.size, &manifest.chunks);
    format!("{body}CHASH {}\n", crate::auth::hex(&manifest.hash))
}

pub(crate) fn parse(encoded: &str) -> AnyResult<Manifest> {
    if encoded.len() > MAX_ENCODED_BYTES || !encoded.ends_with('\n') || encoded.contains('\r') {
        return Err("content-defined manifest is not canonically terminated".into());
    }
    let mut lines = encoded.lines();
    let header = lines.next().ok_or("missing content-defined header")?;
    let fields: Vec<_> = header.split(' ').collect();
    if fields.len() != 3 || fields[0] != "CDC1" {
        return Err("invalid content-defined header".into());
    }
    let size = parse_u64(fields[1])?;
    let count = usize::try_from(parse_u64(fields[2])?)?;
    if count > MAX_CHUNKS {
        return Err("content-defined chunk limit exceeded".into());
    }
    let mut body = format!("{header}\n");
    let mut chunks = Vec::with_capacity(count);
    let mut expected_offset = 0_u64;
    for _ in 0..count {
        let line = lines.next().ok_or("content-defined manifest ended early")?;
        body.push_str(line);
        body.push('\n');
        let fields: Vec<_> = line.split(' ').collect();
        if fields.len() != 4 || fields[0] != "C" {
            return Err("invalid content-defined chunk".into());
        }
        let offset = parse_u64(fields[1])?;
        let length = u32::try_from(parse_u64(fields[2])?)?;
        if offset != expected_offset
            || length == 0
            || length as usize > MAX_CHUNK_BYTES
            || (length as usize) < MIN_CHUNK_BYTES && offset + u64::from(length) != size
        {
            return Err("invalid content-defined chunk layout".into());
        }
        expected_offset = offset
            .checked_add(u64::from(length))
            .ok_or("content-defined offset overflow")?;
        chunks.push(Chunk {
            offset,
            length,
            hash: decode_hash(fields[3])?,
        });
    }
    if expected_offset != size || (size == 0) != chunks.is_empty() {
        return Err("content-defined chunks do not cover the object".into());
    }
    let footer = lines
        .next()
        .ok_or("missing content-defined manifest hash")?;
    if lines.next().is_some() {
        return Err("trailing content-defined manifest data".into());
    }
    let hash = decode_hash(
        footer
            .strip_prefix("CHASH ")
            .ok_or("invalid content-defined hash footer")?,
    )?;
    let expected: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    if hash != expected || canonical_body(size, &chunks) != body {
        return Err("content-defined manifest hash or encoding mismatch".into());
    }
    Ok(Manifest { size, chunks, hash })
}

pub(crate) fn print(path: &Path) -> AnyResult<()> {
    let encoded = encode(&scan(path)?);
    parse(&encoded)?;
    print!("{encoded}");
    Ok(())
}

fn push_chunk(chunks: &mut Vec<Chunk>, offset: u64, bytes: &[u8]) -> AnyResult<()> {
    if chunks.len() >= MAX_CHUNKS {
        return Err("content-defined chunk limit exceeded".into());
    }
    chunks.push(Chunk {
        offset,
        length: u32::try_from(bytes.len())?,
        hash: Sha256::digest(bytes).into(),
    });
    Ok(())
}

fn canonical_body(size: u64, chunks: &[Chunk]) -> String {
    let mut result = format!("CDC1 {size} {}\n", chunks.len());
    for chunk in chunks {
        result.push_str(&format!(
            "C {} {} {}\n",
            chunk.offset,
            chunk.length,
            crate::auth::hex(&chunk.hash)
        ));
    }
    result
}

fn gear(byte: u8) -> u64 {
    let mut value = u64::from(byte).wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn parse_u64(value: &str) -> AnyResult<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.len() > 1 && value.starts_with('0')
    {
        return Err("noncanonical content-defined integer".into());
    }
    Ok(value.parse()?)
}

fn decode_hash(value: &str) -> AnyResult<[u8; 32]> {
    if value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return Err("noncanonical content-defined hash".into());
    }
    let mut result = [0_u8; 32];
    for (index, byte) in result.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};

    fn temporary(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("packet-tide-cdc-{name}-{}", std::process::id()))
    }

    fn deterministic_bytes(length: usize) -> Vec<u8> {
        let mut state = 0x1234_5678_9abc_def0_u64;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[test]
    fn manifest_round_trips_and_covers_the_object() {
        let path = temporary("roundtrip");
        fs::write(&path, deterministic_bytes(3 * 1024 * 1024)).unwrap();
        let manifest = scan(&path).unwrap();
        assert_eq!(parse(&encode(&manifest)).unwrap(), manifest);
        assert_eq!(manifest.size, 3 * 1024 * 1024);
        assert!(manifest.chunks.len() > 10);
        assert!(
            manifest
                .chunks
                .iter()
                .all(|chunk| chunk.length as usize <= MAX_CHUNK_BYTES)
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn insertion_preserves_most_content_chunk_identities() {
        let original = temporary("insert-original");
        let edited = temporary("insert-edited");
        let bytes = deterministic_bytes(4 * 1024 * 1024);
        fs::write(&original, &bytes).unwrap();
        let mut changed = bytes[..1024 * 1024].to_vec();
        changed.extend_from_slice(b"inserted bytes");
        changed.extend_from_slice(&bytes[1024 * 1024..]);
        fs::write(&edited, changed).unwrap();
        let before = scan(&original).unwrap();
        let after = scan(&edited).unwrap();
        let identities: HashSet<_> = before
            .chunks
            .iter()
            .map(|chunk| (chunk.length, chunk.hash))
            .collect();
        let reused: u64 = after
            .chunks
            .iter()
            .filter(|chunk| identities.contains(&(chunk.length, chunk.hash)))
            .map(|chunk| u64::from(chunk.length))
            .sum();
        assert!(
            reused > before.size * 3 / 4,
            "reused {reused} of {}",
            before.size
        );
        fs::remove_file(original).unwrap();
        fs::remove_file(edited).unwrap();
    }

    #[test]
    fn local_edits_and_truncation_change_only_affected_chunks() {
        let original = temporary("edit-original");
        let edited = temporary("edit-changed");
        fs::write(&original, deterministic_bytes(2 * 1024 * 1024)).unwrap();
        fs::copy(&original, &edited).unwrap();
        let mut file = OpenOptions::new().write(true).open(&edited).unwrap();
        file.seek(SeekFrom::Start(700_000)).unwrap();
        file.write_all(b"changed").unwrap();
        file.set_len(1_800_000).unwrap();
        let before = scan(&original).unwrap();
        let after = scan(&edited).unwrap();
        assert_ne!(before.hash, after.hash);
        assert!(after.chunks.iter().any(|right| {
            before
                .chunks
                .iter()
                .any(|left| left.length == right.length && left.hash == right.hash)
        }));
        fs::remove_file(original).unwrap();
        fs::remove_file(edited).unwrap();
    }

    #[test]
    fn parser_rejects_layout_and_hash_tampering() {
        let path = temporary("tamper");
        fs::write(&path, deterministic_bytes(512 * 1024)).unwrap();
        let encoded = encode(&scan(&path).unwrap());
        assert!(parse(&encoded.replacen("C 0 ", "C 1 ", 1)).is_err());
        assert!(parse(&encoded.replacen("CHASH ", "CHASH 0", 1)).is_err());
        fs::remove_file(path).unwrap();
    }
}
