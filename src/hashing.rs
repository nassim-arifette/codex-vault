//! Streaming SHA-256 and zstd compression helpers.

use crate::error::Result;
use crate::rollout::open_rollout_reader;
use crate::util::CHUNK_SIZE;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write as IoWrite};
use std::path::Path;
use zstd::stream::{Decoder, Encoder};

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
