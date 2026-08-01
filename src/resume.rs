use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"TSUMAP3\0";
const HEADER_LEN: usize = 8 + 8 + 8 + 32;

pub(crate) struct ResumeState {
    pub(crate) bitmap: Vec<u64>,
    pub(crate) received_count: u64,
    part_path: PathBuf,
    map_path: PathBuf,
    size: u64,
    chunks: u64,
    hash: [u8; 32],
}

impl ResumeState {
    pub(crate) fn open(
        destination: &Path,
        size: u64,
        chunks: u64,
        hash_hex: &str,
    ) -> Result<(File, Self), Box<dyn std::error::Error + Send + Sync>> {
        let hash = decode_hash(hash_hex)?;
        let part_path = appended_path(destination, ".part");
        let map_path = appended_path(destination, ".part.map");
        let bitmap_words = usize::try_from(chunks.div_ceil(64))
            .map_err(|_| "file has too many chunks for this platform")?;

        if let Some(bitmap) = load_map(&map_path, size, chunks, hash, bitmap_words)?
            && part_path
                .metadata()
                .is_ok_and(|metadata| metadata.len() == size)
        {
            let received_count = bitmap.iter().map(|word| word.count_ones() as u64).sum();
            let file = OpenOptions::new().read(true).write(true).open(&part_path)?;
            return Ok((
                file,
                Self {
                    bitmap,
                    received_count,
                    part_path,
                    map_path,
                    size,
                    chunks,
                    hash,
                },
            ));
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&part_path)?;
        file.set_len(size)?;
        let state = Self {
            bitmap: vec![0; bitmap_words],
            received_count: 0,
            part_path,
            map_path,
            size,
            chunks,
            hash,
        };
        state.checkpoint(&file)?;
        Ok((file, state))
    }

    pub(crate) fn checkpoint(
        &self,
        file: &File,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // The data file must reach stable storage before the bitmap claims those
        // chunks. A crash can therefore cause harmless retransmission, never a
        // bitmap entry that refers to unwritten data.
        file.sync_data()?;
        let temporary = appended_path(&self.map_path, &format!(".tmp.{}", std::process::id()));
        let mut stream = File::create(&temporary)?;
        stream.write_all(MAGIC)?;
        stream.write_all(&self.size.to_be_bytes())?;
        stream.write_all(&self.chunks.to_be_bytes())?;
        stream.write_all(&self.hash)?;
        for word in &self.bitmap {
            stream.write_all(&word.to_be_bytes())?;
        }
        stream.sync_all()?;
        fs::rename(temporary, &self.map_path)?;
        Ok(())
    }

    pub(crate) fn install(
        &self,
        destination: &Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        fs::rename(&self.part_path, destination)?;
        match fs::remove_file(&self.map_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn discard(&self) {
        let _ = fs::remove_file(&self.part_path);
        let _ = fs::remove_file(&self.map_path);
    }
}

fn load_map(
    path: &Path,
    size: u64,
    chunks: u64,
    hash: [u8; 32],
    bitmap_words: usize,
) -> Result<Option<Vec<u64>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = match File::open(path) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let expected_len = HEADER_LEN
        .checked_add(
            bitmap_words
                .checked_mul(8)
                .ok_or("resume map is too large")?,
        )
        .ok_or("resume map is too large")?;
    if stream.metadata()?.len() != expected_len as u64 {
        return Ok(None);
    }
    let mut header = [0_u8; HEADER_LEN];
    stream.read_exact(&mut header)?;
    if &header[..8] != MAGIC
        || u64::from_be_bytes(header[8..16].try_into()?) != size
        || u64::from_be_bytes(header[16..24].try_into()?) != chunks
        || header[24..56] != hash
    {
        return Ok(None);
    }
    let mut bitmap = Vec::with_capacity(bitmap_words);
    let mut bytes = [0_u8; 8];
    for _ in 0..bitmap_words {
        stream.read_exact(&mut bytes)?;
        bitmap.push(u64::from_be_bytes(bytes));
    }
    if chunks & 63 != 0
        && bitmap.last().is_some_and(|word| {
            let valid_mask = (1_u64 << (chunks % 64)) - 1;
            word & !valid_mask != 0
        })
    {
        return Ok(None);
    }
    Ok(Some(bitmap))
}

fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    value.into()
}

fn decode_hash(value: &str) -> Result<[u8; 32], Box<dyn std::error::Error + Send + Sync>> {
    if value.len() != 64 {
        return Err("SHA-256 value must contain 64 hexadecimal characters".into());
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

    fn test_destination(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tsunami-udp-{label}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    #[test]
    fn hash_decoder_rejects_malformed_values() {
        assert!(decode_hash(&"00".repeat(32)).is_ok());
        assert!(decode_hash("00").is_err());
        assert!(decode_hash(&"gg".repeat(32)).is_err());
    }

    #[test]
    fn durable_bitmap_round_trips() {
        let destination = test_destination("resume-round-trip");
        let hash = "11".repeat(32);
        let (file, mut state) = ResumeState::open(&destination, 2364, 2, &hash).unwrap();
        state.bitmap[0] = 1;
        state.received_count = 1;
        state.checkpoint(&file).unwrap();
        drop(file);
        drop(state);

        let (file, state) = ResumeState::open(&destination, 2364, 2, &hash).unwrap();
        assert_eq!(state.bitmap, vec![1]);
        assert_eq!(state.received_count, 1);
        state.discard();
        drop(file);
    }

    #[test]
    fn metadata_mismatch_discards_old_receipts() {
        let destination = test_destination("resume-mismatch");
        let old_hash = "22".repeat(32);
        let new_hash = "33".repeat(32);
        let (file, mut state) = ResumeState::open(&destination, 2364, 2, &old_hash).unwrap();
        state.bitmap[0] = 3;
        state.received_count = 2;
        state.checkpoint(&file).unwrap();
        drop(file);
        drop(state);

        let (file, state) = ResumeState::open(&destination, 2364, 2, &new_hash).unwrap();
        assert_eq!(state.bitmap, vec![0]);
        assert_eq!(state.received_count, 0);
        state.discard();
        drop(file);
    }

    #[test]
    fn truncated_map_is_never_trusted() {
        let destination = test_destination("resume-truncated");
        let hash = "44".repeat(32);
        let (file, state) = ResumeState::open(&destination, 2364, 2, &hash).unwrap();
        let map_path = state.map_path.clone();
        drop(file);
        drop(state);
        fs::write(&map_path, b"torn checkpoint").unwrap();

        let (file, state) = ResumeState::open(&destination, 2364, 2, &hash).unwrap();
        assert_eq!(state.bitmap, vec![0]);
        assert_eq!(state.received_count, 0);
        state.discard();
        drop(file);
    }
}
