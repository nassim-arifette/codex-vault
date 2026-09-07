//! Streaming SHA-256 and zstd compression helpers.

use crate::error::Result;
use crate::rollout::open_rollout_reader;
use crate::util::CHUNK_SIZE;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write as IoWrite};
use std::path::Path;
use zstd::stream::{Decoder, Encoder};

struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

pub fn sha256_reader<R: Read>(reader: &mut R) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    sha256_reader(&mut File::open(path)?)
}

pub fn sha256_rollout_prefix(path: &Path, length: u64) -> Result<String> {
    let mut reader = open_rollout_reader(path)?;
    let mut limited = (&mut reader).take(length);
    sha256_reader(&mut limited)
}

/// Verify one zstd archive pass while hashing both its stored bytes and decoded content.
///
/// The decoder consumes concatenated frames until EOF. The underlying hashing reader therefore
/// sees the archive bytes that are actually read back from storage, while the decoded stream gets
/// its own SHA-256 and size. Keeping both checks in this one read-back pass removes a redundant
/// second full archive read without weakening either integrity guarantee.
pub fn verify_zstd_archive(path: &Path) -> Result<(String, String, u64)> {
    let source = HashingReader::new(File::open(path)?);
    let mut decoder = Decoder::new(source)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total = 0u64;
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total = total.saturating_add(n as u64);
    }
    // Force the wrapped reader to EOF as well. Usually the decoder already did this, but draining
    // the returned BufReader makes the compressed-byte hash explicitly cover any buffered/trailing
    // bytes instead of depending on zstd's internal read-ahead behavior.
    let mut raw = decoder.finish();
    io::copy(&mut raw, &mut io::sink())?;
    let compressed_sha = raw.into_inner().finish();
    Ok((compressed_sha, format!("{:x}", hasher.finalize()), total))
}

pub fn sha256_zstd_decompressed_with_size(path: &Path) -> Result<(String, u64)> {
    let mut decoder = Decoder::new(File::open(path)?)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total = 0u64;
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total = total.saturating_add(n as u64);
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

pub fn sha256_zstd_decompressed(path: &Path) -> Result<String> {
    Ok(sha256_zstd_decompressed_with_size(path)?.0)
}

pub fn compress_file_with_input_sha(src: &Path, dst: &Path, level: i32) -> Result<String> {
    let mut source = File::open(src)?;
    let target = crate::fsatomic::create_private_file(dst)?;
    let mut encoder = Encoder::new(target, level)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        encoder.write_all(&buf[..n])?;
    }
    let target = encoder.finish()?;
    target.sync_all()?;
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn decompress_file(src: &Path, dst: &Path) -> Result<()> {
    let mut decoder = Decoder::new(File::open(src)?)?;
    let mut target = crate::fsatomic::create_private_file(dst)?;
    io::copy(&mut decoder, &mut target)?;
    target.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zstd_verification_hashes_stored_and_decoded_bytes_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.jsonl");
        let packed = dir.path().join("plain.jsonl.zst");
        std::fs::write(
            &plain,
            b"{\"type\":\"session_meta\"}\n{\"type\":\"event_msg\"}\n",
        )
        .unwrap();
        let input_sha = compress_file_with_input_sha(&plain, &packed, 3).unwrap();

        let (compressed_sha, decoded_sha, decoded_size) = verify_zstd_archive(&packed).unwrap();

        assert_eq!(decoded_sha, input_sha);
        assert_eq!(decoded_sha, sha256_file(&plain).unwrap());
        assert_eq!(compressed_sha, sha256_file(&packed).unwrap());
        assert_eq!(decoded_size, std::fs::metadata(&plain).unwrap().len());
    }

    #[test]
    fn zstd_verification_covers_every_concatenated_frame() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let first_zstd = dir.path().join("first.zst");
        let second_zstd = dir.path().join("second.zst");
        let joined = dir.path().join("joined.zst");
        std::fs::write(&first, b"first frame\n").unwrap();
        std::fs::write(&second, b"second frame\n").unwrap();
        compress_file_with_input_sha(&first, &first_zstd, 3).unwrap();
        compress_file_with_input_sha(&second, &second_zstd, 3).unwrap();
        let mut bytes = std::fs::read(&first_zstd).unwrap();
        bytes.extend(std::fs::read(&second_zstd).unwrap());
        std::fs::write(&joined, &bytes).unwrap();

        let (compressed_sha, decoded_sha, decoded_size) = verify_zstd_archive(&joined).unwrap();
        let decoded = b"first frame\nsecond frame\n";

        assert_eq!(compressed_sha, sha256_file(&joined).unwrap());
        assert_eq!(decoded_sha, format!("{:x}", Sha256::digest(decoded)));
        assert_eq!(decoded_size, decoded.len() as u64);
    }
}
