use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::AnyResult;

pub(crate) const MAX_MANIFEST_ENTRIES: usize = 1_000_000;
const MAX_RELATIVE_PATH_BYTES: usize = 4_096;
pub(crate) const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

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
    pub(crate) root_mode: u32,
    pub(crate) root_modified_seconds: i64,
    pub(crate) root_modified_nanoseconds: i64,
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
    let root_mode = metadata.mode() & 0o777;
    let root_modified_seconds = metadata.mtime();
    let root_modified_nanoseconds = metadata.mtime_nsec();
    let body = canonical_body(
        &entries,
        root_mode,
        root_modified_seconds,
        root_modified_nanoseconds,
    );
    if body.len().saturating_add("HASH ".len() + 64 + 1) > MAX_MANIFEST_BYTES {
        return Err("manifest byte limit exceeded".into());
    }
    Ok(Manifest {
        entries,
        root_mode,
        root_modified_seconds,
        root_modified_nanoseconds,
        hash: Sha256::digest(body.as_bytes()).into(),
    })
}

pub(crate) fn encode(manifest: &Manifest) -> String {
    let body = canonical_body(
        &manifest.entries,
        manifest.root_mode,
        manifest.root_modified_seconds,
        manifest.root_modified_nanoseconds,
    );
    format!("{body}HASH {}\n", crate::auth::hex(&manifest.hash))
}

pub(crate) fn parse(encoded: &str) -> AnyResult<Manifest> {
    if encoded.len() > MAX_MANIFEST_BYTES || !encoded.ends_with('\n') || encoded.contains('\r') {
        return Err("manifest must use canonical newline termination".into());
    }
    let mut lines = encoded.lines();
    let header = lines.next().ok_or("missing manifest header")?;
    let fields: Vec<_> = header.split(' ').collect();
    if fields.len() != 5 || fields[0] != "PTM1" {
        return Err("invalid manifest header".into());
    }
    let count = parse_usize(fields[1])?;
    let root_mode = parse_mode(fields[2])?;
    let root_modified_seconds = parse_i64(fields[3])?;
    let root_modified_nanoseconds = parse_nanoseconds(fields[4])?;
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
    if canonical_body(
        &entries,
        root_mode,
        root_modified_seconds,
        root_modified_nanoseconds,
    ) != body
    {
        return Err("manifest entries are not canonically encoded".into());
    }
    Ok(Manifest {
        entries,
        root_mode,
        root_modified_seconds,
        root_modified_nanoseconds,
        hash,
    })
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
        mode: metadata.mode() & 0o777,
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

fn canonical_body(
    entries: &[ManifestEntry],
    root_mode: u32,
    root_modified_seconds: i64,
    root_modified_nanoseconds: i64,
) -> String {
    let mut result = format!(
        "PTM1 {} {root_mode} {root_modified_seconds} {root_modified_nanoseconds}\n",
        entries.len()
    );
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
        if entry.mode > 0o777 {
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
    if mode > 0o777 {
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

pub(crate) fn prepare_staging(manifest: &Manifest, destination: &Path) -> AnyResult<PathBuf> {
    require_absent(destination, "directory destination already exists")?;
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    require_real_directory(parent)?;
    let destination_hash = Sha256::digest(destination.as_os_str().as_bytes());
    let staging = parent.join(format!(
        ".packet-tide-{}-{}.part",
        crate::auth::hex(&manifest.hash),
        crate::auth::hex(&destination_hash),
    ));
    let encoded = encode(manifest);
    match fs::symlink_metadata(&staging) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err("directory staging path is not a real directory".into());
            }
            let marker = read_nofollow(&staging.join(".packet-tide-manifest"))?;
            if marker != encoded.as_bytes() {
                return Err("directory staging manifest does not match".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&staging)?;
            let marker_path = staging.join(".packet-tide-manifest");
            let mut marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(marker_path)?;
            marker.write_all(encoded.as_bytes())?;
            marker.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    for entry in &manifest.entries {
        if entry.kind != EntryKind::Directory {
            continue;
        }
        let path = staging.join(path_from_bytes(&entry.path));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err("staging manifest directory is unsafe".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    validate_staging_tree(manifest, &staging, true)?;
    Ok(staging)
}

pub(crate) fn file_destination(
    manifest: &Manifest,
    staging: &Path,
    entry_index: usize,
) -> AnyResult<PathBuf> {
    let entry = manifest
        .entries
        .get(entry_index)
        .ok_or("manifest file index is out of range")?;
    if entry.kind != EntryKind::File {
        return Err("manifest entry is not a file".into());
    }
    require_real_directory(staging)?;
    let mut parent = staging.to_owned();
    let path = path_from_bytes(&entry.path);
    if let Some(relative_parent) = path.parent() {
        for component in relative_parent.components() {
            parent.push(component);
            require_real_directory(&parent)?;
        }
    }
    let destination = staging.join(path);
    if let Ok(metadata) = fs::symlink_metadata(&destination)
        && metadata.file_type().is_symlink()
    {
        return Err("staging file destination is a symlink".into());
    }
    Ok(destination)
}

pub(crate) fn source_file(
    manifest: &Manifest,
    root: &Path,
    entry_index: usize,
) -> AnyResult<PathBuf> {
    let entry = manifest
        .entries
        .get(entry_index)
        .ok_or("manifest file index is out of range")?;
    if entry.kind != EntryKind::File {
        return Err("manifest entry is not a file".into());
    }
    require_real_directory(root)?;
    let relative = path_from_bytes(&entry.path);
    let mut parent = root.to_owned();
    if let Some(relative_parent) = relative.parent() {
        for component in relative_parent.components() {
            parent.push(component);
            require_real_directory(&parent)?;
        }
    }
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("manifest source file changed type".into());
    }
    Ok(path)
}

pub(crate) fn candidate_file(
    manifest: &Manifest,
    root: &Path,
    entry_index: usize,
) -> AnyResult<Option<PathBuf>> {
    let entry = manifest
        .entries
        .get(entry_index)
        .ok_or("manifest file index is out of range")?;
    if entry.kind != EntryKind::File {
        return Err("manifest entry is not a file".into());
    }
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err("reuse root is not a real directory".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let relative = path_from_bytes(&entry.path);
    let mut parent = root.to_owned();
    if let Some(relative_parent) = relative.parent() {
        for component in relative_parent.components() {
            parent.push(component);
            match fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => return Err("reuse path contains an unsafe parent".into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
    }
    let path = root.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(Some(path)),
        Ok(_) => Err("reuse candidate is not a real file".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn install(manifest: &Manifest, staging: &Path, destination: &Path) -> AnyResult<()> {
    validate_staging_tree(manifest, staging, false)?;
    require_absent(destination, "directory destination already exists")?;
    for (index, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::File {
            continue;
        }
        let path = file_destination(manifest, staging, index)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.size() != entry.size {
            return Err("staged file size or type does not match manifest".into());
        }
        let (_, hash) = hash_file(&mut file)?;
        if Some(hash) != entry.hash {
            return Err("staged file hash does not match manifest".into());
        }
        file.set_permissions(fs::Permissions::from_mode(entry.mode))?;
        file.set_times(fs::FileTimes::new().set_modified(system_time(
            entry.modified_seconds,
            entry.modified_nanoseconds,
        )?))?;
        file.sync_all()?;
    }
    for entry in manifest.entries.iter().rev() {
        if entry.kind != EntryKind::Directory {
            continue;
        }
        let path = staging.join(path_from_bytes(&entry.path));
        let directory = open_directory(&path)?;
        directory.set_permissions(fs::Permissions::from_mode(entry.mode))?;
        directory.set_times(fs::FileTimes::new().set_modified(system_time(
            entry.modified_seconds,
            entry.modified_nanoseconds,
        )?))?;
        directory.sync_all()?;
    }
    fs::remove_file(staging.join(".packet-tide-manifest"))?;
    let root = open_directory(staging)?;
    root.set_permissions(fs::Permissions::from_mode(manifest.root_mode))?;
    root.set_times(fs::FileTimes::new().set_modified(system_time(
        manifest.root_modified_seconds,
        manifest.root_modified_nanoseconds,
    )?))?;
    root.sync_all()?;
    fs::rename(staging, destination)?;
    if let Some(parent) = destination.parent() {
        open_directory(parent)?.sync_all()?;
    }
    Ok(())
}

fn validate_staging_tree(
    manifest: &Manifest,
    staging: &Path,
    allow_partial_files: bool,
) -> AnyResult<()> {
    require_real_directory(staging)?;
    let directories: HashSet<_> = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory)
        .map(|entry| entry.path.clone())
        .collect();
    let files: HashSet<_> = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .map(|entry| entry.path.clone())
        .collect();
    let partial_files: HashSet<_> = if allow_partial_files {
        files
            .iter()
            .flat_map(|file| {
                [
                    appended_bytes(file, b".part"),
                    appended_bytes(file, b".part.map"),
                ]
            })
            .collect()
    } else {
        HashSet::new()
    };
    for entry in &manifest.entries {
        if entry.kind == EntryKind::Directory {
            require_real_directory(&staging.join(path_from_bytes(&entry.path)))?;
        }
    }
    validate_staging_directory(staging, staging, &directories, &files, &partial_files)?;
    Ok(())
}

fn validate_staging_directory(
    root: &Path,
    directory: &Path,
    directories: &HashSet<Vec<u8>>,
    files: &HashSet<Vec<u8>>,
    partial_files: &HashSet<Vec<u8>>,
) -> AnyResult<()> {
    for child in fs::read_dir(directory)? {
        let child = child?;
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err("staging tree contains a symlink".into());
        }
        let relative = path.strip_prefix(root)?.as_os_str().as_bytes().to_vec();
        if metadata.is_dir() {
            if !directories.contains(&relative) {
                return Err("staging tree contains an unexpected directory".into());
            }
            validate_staging_directory(root, &path, directories, files, partial_files)?;
        } else if metadata.is_file() {
            let expected = files.contains(&relative)
                || relative == b".packet-tide-manifest"
                || partial_files.contains(&relative);
            if !expected {
                return Err("staging tree contains an unexpected file".into());
            }
        } else {
            return Err("staging tree contains an unsupported object".into());
        }
    }
    Ok(())
}

fn appended_bytes(path: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(path.len() + suffix.len());
    result.extend_from_slice(path);
    result.extend_from_slice(suffix);
    result
}

fn require_real_directory(path: &Path) -> AnyResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("path component is not a real directory".into());
    }
    Ok(())
}

fn require_absent(path: &Path, message: &'static str) -> AnyResult<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(message.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn open_directory(path: &Path) -> AnyResult<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)?)
}

fn read_nofollow(path: &Path) -> AnyResult<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let mut result = Vec::new();
    file.take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut result)?;
    if result.len() > MAX_MANIFEST_BYTES {
        return Err("manifest byte limit exceeded".into());
    }
    Ok(result)
}

fn path_from_bytes(path: &[u8]) -> PathBuf {
    PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()))
}

fn system_time(seconds: i64, nanoseconds: i64) -> AnyResult<SystemTime> {
    if !(0..1_000_000_000).contains(&nanoseconds) {
        return Err("modification nanoseconds are out of range".into());
    }
    if seconds >= 0 {
        Ok(UNIX_EPOCH + Duration::new(seconds as u64, nanoseconds as u32))
    } else if nanoseconds == 0 {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(seconds.unsigned_abs()))
            .ok_or_else(|| "modification time is out of range".into())
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::new(
                seconds.unsigned_abs() - 1,
                1_000_000_000 - nanoseconds as u32,
            ))
            .ok_or_else(|| "modification time is out of range".into())
    }
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
    fn manifest_strips_special_permission_bits() {
        let root = temporary("special-mode");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("file");
        fs::write(&path, b"contents").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o6754)).unwrap();
        let manifest = build(&root).unwrap();
        let file = manifest
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::File)
            .unwrap();
        assert_eq!(file.mode, 0o754);
        fs::remove_dir_all(root).unwrap();
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

    #[test]
    fn staging_resumes_same_manifest_and_installs_atomically() {
        let root = temporary("install-source");
        let destination = temporary("install-destination");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&destination);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/file"), b"contents").unwrap();
        let manifest = build(&root).unwrap();
        let staging = prepare_staging(&manifest, &destination).unwrap();
        assert_eq!(prepare_staging(&manifest, &destination).unwrap(), staging);
        for (index, entry) in manifest.entries.iter().enumerate() {
            if entry.kind == EntryKind::File {
                let target = file_destination(&manifest, &staging, index).unwrap();
                fs::copy(root.join(path_from_bytes(&entry.path)), target).unwrap();
            }
        }
        install(&manifest, &staging, &destination).unwrap();
        assert_eq!(fs::read(destination.join("a/b/file")).unwrap(), b"contents");
        assert!(!staging.exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn staging_allows_only_hash_bound_resume_artifacts() {
        let root = temporary("resume-source");
        let destination = temporary("resume-destination");
        let other_destination = temporary("resume-other-destination");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&destination);
        let _ = fs::remove_dir_all(&other_destination);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file"), b"contents").unwrap();
        let manifest = build(&root).unwrap();
        let staging = prepare_staging(&manifest, &destination).unwrap();
        fs::write(staging.join("file.part"), b"partial").unwrap();
        fs::write(staging.join("file.part.map"), b"receipt").unwrap();
        assert_eq!(prepare_staging(&manifest, &destination).unwrap(), staging);
        let other = prepare_staging(&manifest, &other_destination).unwrap();
        assert_ne!(staging, other);
        fs::write(staging.join("unexpected"), b"no").unwrap();
        assert!(prepare_staging(&manifest, &destination).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(staging).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn staging_symlink_escape_and_destination_conflict_fail_closed() {
        let root = temporary("escape-source");
        let destination = temporary("escape-destination");
        let outside = temporary("escape-outside");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&destination);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/file"), b"contents").unwrap();
        fs::create_dir_all(&outside).unwrap();
        let manifest = build(&root).unwrap();
        let staging = prepare_staging(&manifest, &destination).unwrap();
        fs::remove_dir(staging.join("a")).unwrap();
        symlink(&outside, staging.join("a")).unwrap();
        let file_index = manifest
            .entries
            .iter()
            .position(|entry| entry.kind == EntryKind::File)
            .unwrap();
        assert!(file_destination(&manifest, &staging, file_index).is_err());
        assert!(install(&manifest, &staging, &destination).is_err());
        fs::write(&destination, b"conflict").unwrap();
        assert!(prepare_staging(&manifest, &destination).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(staging).unwrap();
        fs::remove_dir_all(outside).unwrap();
        fs::remove_file(destination).unwrap();
    }
}
