use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::AnyResult;

const MAX_MANIFEST_ENTRIES: usize = 1_000_000;
const MAX_RELATIVE_PATH_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManifestEntry {
    pub(crate) path: Vec<u8>,
    pub(crate) kind: EntryKind,
    pub(crate) mode: u32,
    pub(crate) modified_seconds: i64,
    pub(crate) modified_nanoseconds: i64,
    pub(crate) size: u64,
    pub(crate) hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Manifest {
    pub(crate) entries: Vec<ManifestEntry>,
    pub(crate) hash: [u8; 32],
}

pub(crate) fn build(root: &Path) -> AnyResult<Manifest> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("directory manifest root must be a real directory".into());
    }
    let mut entries = Vec::new();
    scan(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    validate_entries(&entries)?;
    let body = canonical_body(&entries);
    Ok(Manifest {
        entries,
        hash: Sha256::digest(body.as_bytes()).into(),
    })
}

pub(crate) fn encode(manifest: &Manifest) -> String {
    let body = canonical_body(&manifest.entries);
    format!("{body}HASH {}\n", crate::auth::hex(&manifest.hash))
}

pub(crate) fn parse(encoded: &str) -> AnyResult<Manifest> {
    if !encoded.ends_with('\n') || encoded.contains('\r') {
        return Err("manifest must use canonical newline termination".into());
    }
    let mut lines = encoded.lines();
    let header = lines.next().ok_or("missing manifest header")?;
    let fields: Vec<_> = header.split(' ').collect();
    if fields.len() != 2 || fields[0] != "PTM1" {
        return Err("invalid manifest header".into());
    }
    let count = parse_usize(fields[1])?;
    if count > MAX_MANIFEST_ENTRIES {
        return Err("manifest entry limit exceeded".into());
    }
    let mut entries = Vec::with_capacity(count);
    let mut body = format!("{header}\n");
    for _ in 0..count {
        let line = lines.next().ok_or("manifest ended before all entries")?;
        body.push_str(line);
        body.push('\n');
        entries.push(parse_entry(line)?);
    }
    let footer = lines.next().ok_or("missing manifest hash")?;
    if lines.next().is_some() {
        return Err("trailing manifest data".into());
    }
    let supplied = footer
        .strip_prefix("HASH ")
        .ok_or("invalid manifest hash footer")?;
    let hash = decode_hash(supplied)?;
    let expected: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    if hash != expected {
        return Err("manifest hash mismatch".into());
    }
    validate_entries(&entries)?;
    if canonical_body(&entries) != body {
        return Err("manifest entries are not canonically encoded".into());
    }
    Ok(Manifest { entries, hash })
}

fn scan(root: &Path, directory: &Path, entries: &mut Vec<ManifestEntry>) -> AnyResult<()> {
    let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name().as_bytes().to_vec());
    for child in children {
        if entries.len() >= MAX_MANIFEST_ENTRIES {
            return Err("manifest entry limit exceeded".into());
        }
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)?;
        let relative = path.strip_prefix(root)?;
        let path_bytes = relative.as_os_str().as_bytes().to_vec();
        validate_relative_path(&path_bytes)?;
        if metadata.file_type().is_symlink() {
            return Err(format!("symlinks are not supported: {}", relative.display()).into());
        }
        if metadata.is_dir() {
            entries.push(entry(path_bytes, EntryKind::Directory, &metadata, 0, None));
            scan(root, &path, entries)?;
        } else if metadata.is_file() {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&path)?;
            let opened_metadata = file.metadata()?;
            if !opened_metadata.is_file()
                || opened_metadata.dev() != metadata.dev()
                || opened_metadata.ino() != metadata.ino()
            {
                return Err("manifest source changed while it was scanned".into());
            }
            let (size, hash) = hash_file(&mut file)?;
            let final_metadata = file.metadata()?;
            if final_metadata.size() != size
                || final_metadata.mtime() != opened_metadata.mtime()
                || final_metadata.mtime_nsec() != opened_metadata.mtime_nsec()
            {
                return Err("manifest source changed while it was hashed".into());
            }
            entries.push(entry(
                path_bytes,
                EntryKind::File,
                &final_metadata,
                size,
                Some(hash),
            ));
        } else {
            return Err(format!("unsupported filesystem entry: {}", relative.display()).into());
        }
    }
    Ok(())
}

fn entry(
    path: Vec<u8>,
    kind: EntryKind,
    metadata: &Metadata,
    size: u64,
    hash: Option<[u8; 32]>,
) -> ManifestEntry {
    ManifestEntry {
        path,
        kind,
        mode: metadata.mode() & 0o7777,
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        size,
        hash,
    }
}

fn hash_file(file: &mut File) -> AnyResult<(u64, [u8; 32])> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size = size.checked_add(count as u64).ok_or("file size overflow")?;
    }
    Ok((size, hasher.finalize().into()))
}

fn canonical_body(entries: &[ManifestEntry]) -> String {
    let mut result = format!("PTM1 {}\n", entries.len());
    for entry in entries {
        let path = hex(&entry.path);
        match entry.kind {
            EntryKind::Directory => result.push_str(&format!(
                "D {path} {} {} {}\n",
                entry.mode, entry.modified_seconds, entry.modified_nanoseconds
            )),
            EntryKind::File => result.push_str(&format!(
                "F {path} {} {} {} {} {}\n",
                entry.size,
                crate::auth::hex(entry.hash.as_ref().expect("file entry hash")),
                entry.mode,
                entry.modified_seconds,
                entry.modified_nanoseconds
            )),
        }
    }
    result
}

fn parse_entry(line: &str) -> AnyResult<ManifestEntry> {
    let fields: Vec<_> = line.split(' ').collect();
    match fields.as_slice() {
        ["D", path, mode, seconds, nanoseconds] => Ok(ManifestEntry {
            path: decode_path(path)?,
            kind: EntryKind::Directory,
            mode: parse_mode(mode)?,
            modified_seconds: parse_i64(seconds)?,
            modified_nanoseconds: parse_nanoseconds(nanoseconds)?,
            size: 0,
            hash: None,
        }),
        ["F", path, size, hash, mode, seconds, nanoseconds] => Ok(ManifestEntry {
            path: decode_path(path)?,
            kind: EntryKind::File,
            mode: parse_mode(mode)?,
            modified_seconds: parse_i64(seconds)?,
            modified_nanoseconds: parse_nanoseconds(nanoseconds)?,
            size: parse_u64(size)?,
            hash: Some(decode_hash(hash)?),
        }),
        _ => Err("invalid manifest entry".into()),
    }
}

fn validate_entries(entries: &[ManifestEntry]) -> AnyResult<()> {
    if entries.len() > MAX_MANIFEST_ENTRIES {
        return Err("manifest entry limit exceeded".into());
    }
    let mut previous: Option<&[u8]> = None;
    let mut kinds = HashMap::new();
    for entry in entries {
        validate_relative_path(&entry.path)?;
        if previous.is_some_and(|value| value >= entry.path.as_slice()) {
            return Err("manifest paths must be unique and strictly sorted".into());
        }
        if entry.modified_nanoseconds < 0 || entry.modified_nanoseconds >= 1_000_000_000 {
            return Err("manifest modification nanoseconds are out of range".into());
        }
        if entry.mode > 0o7777 {
            return Err("manifest mode is out of range".into());
        }
        if entry.kind == EntryKind::Directory && (entry.size != 0 || entry.hash.is_some()) {
            return Err("directory entry carries file data".into());
        }
        if entry.kind == EntryKind::File && entry.hash.is_none() {
            return Err("file entry has no hash".into());
        }
        for parent in parents(&entry.path) {
            if kinds.get(parent) != Some(&EntryKind::Directory) {
                return Err("manifest parent is absent or is not a directory".into());
            }
        }
        kinds.insert(entry.path.as_slice(), entry.kind);
        previous = Some(&entry.path);
    }
    Ok(())
}

fn parents(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(index, _)| &path[..index])
}

fn validate_relative_path(path: &[u8]) -> AnyResult<()> {
    if path.is_empty()
        || path.len() > MAX_RELATIVE_PATH_BYTES
        || path[0] == b'/'
        || path.contains(&0)
        || path
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err("unsafe manifest path".into());
    }
    Ok(())
}

fn parse_mode(value: &str) -> AnyResult<u32> {
    let mode = parse_u64(value)?;
    if mode > 0o7777 {
        return Err("manifest mode is out of range".into());
    }
    Ok(mode as u32)
}

fn parse_nanoseconds(value: &str) -> AnyResult<i64> {
    let result = parse_i64(value)?;
    if !(0..1_000_000_000).contains(&result) {
        return Err("manifest modification nanoseconds are out of range".into());
    }
    Ok(result)
}

fn parse_usize(value: &str) -> AnyResult<usize> {
    Ok(usize::try_from(parse_u64(value)?)?)
}

fn parse_u64(value: &str) -> AnyResult<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err("noncanonical unsigned manifest integer".into());
    }
    Ok(value.parse()?)
}

fn parse_i64(value: &str) -> AnyResult<i64> {
    if value == "-0"
        || value.is_empty()
        || value.strip_prefix('-').unwrap_or(value).is_empty()
        || !value
            .strip_prefix('-')
            .unwrap_or(value)
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        || value.strip_prefix('-').unwrap_or(value).len() > 1
            && value.strip_prefix('-').unwrap_or(value).starts_with('0')
    {
        return Err("noncanonical signed manifest integer".into());
    }
    Ok(value.parse()?)
}

fn decode_path(value: &str) -> AnyResult<Vec<u8>> {
    let path = decode_hex(value)?;
    validate_relative_path(&path)?;
    Ok(path)
}

fn decode_hash(value: &str) -> AnyResult<[u8; 32]> {
    let bytes = decode_hex(value)?;
    bytes
        .try_into()
        .map_err(|_| "manifest hash must contain 32 bytes".into())
}

fn decode_hex(value: &str) -> AnyResult<Vec<u8>> {
    if value.is_empty()
        || value.len() & 1 != 0
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return Err("noncanonical manifest hex".into());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn print(root: &Path) -> AnyResult<()> {
    let encoded = encode(&build(root)?);
    parse(&encoded)?;
    print!("{encoded}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "packet-tide-directory-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn manifest_round_trips_regular_files_and_directories() {
        let root = temporary("roundtrip");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/file"), b"contents").unwrap();
        fs::write(root.join("empty"), b"").unwrap();
        let manifest = build(&root).unwrap();
        assert_eq!(parse(&encode(&manifest)).unwrap(), manifest);
        assert_eq!(manifest.entries.len(), 4);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parser_rejects_traversal_and_absolute_paths() {
        for path in [b"..".as_slice(), b"../x", b"a/../x", b"/absolute", b"a//b"] {
            assert!(validate_relative_path(path).is_err());
        }
    }

    #[test]
    fn scanner_rejects_symlinks_instead_of_following_them() {
        let root = temporary("symlink");
        let outside = temporary("outside");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(&outside);
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, b"secret").unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        assert!(build(&root).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_file(outside).unwrap();
    }

    #[test]
    fn validator_rejects_file_ancestors_and_missing_directories() {
        let file = ManifestEntry {
            path: b"a".to_vec(),
            kind: EntryKind::File,
            mode: 0o600,
            modified_seconds: 0,
            modified_nanoseconds: 0,
            size: 0,
            hash: Some([0; 32]),
        };
        let child = ManifestEntry {
            path: b"a/b".to_vec(),
            ..file.clone()
        };
        assert!(validate_entries(&[file, child]).is_err());
        assert!(
            validate_entries(&[ManifestEntry {
                path: b"missing/child".to_vec(),
                ..ManifestEntry {
                    path: b"ignored".to_vec(),
                    kind: EntryKind::File,
                    mode: 0o600,
                    modified_seconds: 0,
                    modified_nanoseconds: 0,
                    size: 0,
                    hash: Some([0; 32]),
                }
            }])
            .is_err()
        );
    }

    #[test]
    fn parser_rejects_tampering_noncanonical_order_and_duplicates() {
        let root = temporary("tamper");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a"), b"a").unwrap();
        fs::write(root.join("b"), b"b").unwrap();
        let encoded = encode(&build(&root).unwrap());
        assert!(parse(&encoded.replacen("F 61", "F 2e2e", 1)).is_err());
        let mut lines: Vec<_> = encoded.lines().collect();
        lines.swap(1, 2);
        assert!(parse(&(lines.join("\n") + "\n")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
