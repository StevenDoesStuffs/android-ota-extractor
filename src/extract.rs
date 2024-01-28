use anyhow::{anyhow, bail, Context, Result};
use base64::prelude::*;
use bzip2::read::BzDecoder;
use cast::{i64, u64, usize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::Path,
};
use xz2::read::XzDecoder;

use crate::{
    extract::bspatch::bspatch,
    parse_parts,
    update_metadata::{
        install_operation::Type as OperationType, DeltaArchiveManifest, PartitionUpdate,
        DEFAULT_BLOCK_SIZE,
    },
    ExtractArgs,
};

use self::extent::{convert_extents, ExtentStream};

mod bspatch;
mod extent;

trait StreamRead: Read + Seek {}
impl<T: Read + Seek> StreamRead for T {}

trait StreamWrite: Write + Seek {}
impl<T: Write + Seek> StreamWrite for T {}

pub fn calculate_rel(start: u64, pos: u64, offset: i64) -> Result<u64, i64> {
    let abs_pos = i64(pos).unwrap() + offset;
    if let Ok(abs_pos) = u64(abs_pos) {
        if abs_pos >= start {
            return Ok(abs_pos);
        }
    }
    Err(abs_pos)
}

fn copy_padded(src: &mut impl Read, dst: &mut impl Write, len: usize) -> io::Result<()> {
    let written = io::copy(src, dst)?;
    io::copy(&mut io::repeat(0).take(u64(len).saturating_sub(written)), dst)?;
    Ok(())
}

fn check_hash(stream: &mut (impl Read + Seek), expected_hash: &[u8]) -> Result<()> {
    let pos = stream.stream_position()?;
    let mut hasher = Sha256::new();
    io::copy(stream, &mut hasher)?;
    stream.seek(io::SeekFrom::Start(pos))?;
    let hash = hasher.finalize();
    if hash.as_slice() != expected_hash {
        bail!(
            "Found hash {} but expected {}",
            BASE64_STANDARD.encode(hash),
            BASE64_STANDARD.encode(expected_hash)
        )
    }
    Ok(())
}

fn process_part(
    manifest: &DeltaArchiveManifest,
    part: &PartitionUpdate,
    data: &mut (impl Read + Seek),
    mut src: Option<&mut (impl Read + Seek)>,
    dst: &mut (impl Write + Seek),
    skip_hash: bool,
) -> Result<()> {
    let block_size = usize(manifest.block_size.unwrap_or(DEFAULT_BLOCK_SIZE));
    for i in 0..part.operations.len() {
        let op = &part.operations[i];
        // Unsupported operations:
        // - Discard: not sure what this does
        // - Move, Bsdiff: deprecated non A/B versions of SourceCopy and SourceBsdiff
        // - Puffdiff, Zucchini, Lz4diffBsdiff, Lz4diffPuffdiff - TODO maybe
        let op_type = OperationType::try_from(op.r#type)
            .map_err(|_| anyhow!("Invalid operation type {} for op {}", op.r#type, i))?;
        println!("applying operation #{}: {:?}", i, op_type);

        let mut src = src
            .as_deref_mut()
            .map(|src| -> Result<_> {
                Ok(ExtentStream::new(
                    src,
                    convert_extents(&op.src_extents, block_size)
                        .with_context(|| format!("Failed to parse src_extents"))?,
                )?)
            })
            .transpose()
            .with_context(|| format!("Error while constructing src stream"))?
            .flatten();
        let mut dst = ExtentStream::new(
            &mut *dst,
            convert_extents(&op.dst_extents, block_size)
                .with_context(|| format!("Failed to parse dst_extents"))?,
        )
        .with_context(|| format!("Error while constructing dst stream"))?
        .ok_or_else(|| anyhow!("No dst extents"))?;
        let mut data = op
            .data_offset
            .zip(op.data_length)
            .map(|(data_offset, data_len)| {
                ExtentStream::new_range(&mut *data, usize(data_offset), usize(data_len))
            })
            .transpose()
            .with_context(|| format!("Error while constructing data stream"))?;

        if !skip_hash {
            if let (Some(src), Some(hash)) = (src.as_mut(), op.src_sha256_hash.as_deref()) {
                check_hash(src, hash)
                    .with_context(|| format!("Error ocurred while checking src hash"))?;
            }
            if let (Some(data), Some(hash)) = (data.as_mut(), op.data_sha256_hash.as_deref()) {
                check_hash(data, hash)
                    .with_context(|| format!("Error ocurred while checking data hash"))?;
            }
        }

        let dst_len = dst.len();

        match &op_type {
            // replace: data -> dst
            OperationType::Replace | OperationType::ReplaceBz | OperationType::ReplaceXz => {
                let data = data.ok_or_else(|| anyhow!("No data given for replace operation"))?;

                let mut data: Box<dyn Read> = match &op_type {
                    OperationType::Replace => Box::new(data),
                    OperationType::ReplaceBz => Box::new(BzDecoder::new(data)),
                    OperationType::ReplaceXz => Box::new(XzDecoder::new(data)),
                    _ => unreachable!(),
                };

                copy_padded(&mut data, &mut dst, dst_len)
                    .with_context(|| format!("Error while writing output"))?;
            }
            // remove: _ -> dst
            OperationType::Zero => {
                copy_padded(&mut io::empty(), &mut dst, dst_len)
                    .with_context(|| format!("Error while writing output"))?;
            }

            // diff ops; require src
            // copy: src -> dst
            OperationType::SourceCopy => {
                let mut src = src.ok_or_else(|| anyhow!("No src given for copy operation"))?;

                copy_padded(&mut src, &mut dst, dst_len)
                    .with_context(|| format!("Error while writing output"))?;
            }
            // bsdiff: src + data -> dst
            OperationType::SourceBsdiff | OperationType::BrotliBsdiff => {
                let mut src = src.ok_or_else(|| anyhow!("No src given for bsdiff operation"))?;
                let mut data_vec = vec![];
                data.ok_or_else(|| anyhow!("No data given for bsdiff operation"))?
                    .read_to_end(&mut data_vec)
                    .with_context(|| format!("Error ocurred while reading patch data"))?;

                bspatch(&mut src, &mut dst, &data_vec)
                    .with_context(|| format!("Error ocurred applying patch"))?;
            }
            _ => bail!("Unsupported operation type {} for op {}", op.r#type, i),
        }
    }
    println!();

    Ok(())
}

fn extract_part(
    manifest: &DeltaArchiveManifest,
    args: &ExtractArgs,
    data: &mut (impl Read + Seek),
    part: &PartitionUpdate,
) -> Result<()> {
    let name = &part.partition_name;
    println!("processing partition: {}", name);
    let name_img = format!("{}.img", name);

    let mut src = args
        .src
        .as_ref()
        .map(|src_path| File::open(Path::new(src_path).join(&name_img)))
        .transpose()?;
    let mut dst = File::create(Path::new(&args.dst).join(&name_img))?;
    process_part(manifest, part, data, src.as_mut(), &mut dst, args.skip_hash)
}

pub fn extract(
    manifest: &DeltaArchiveManifest,
    args: &ExtractArgs,
    data_offset: u64,
) -> Result<()> {
    let parts = parse_parts(&args.parts);
    let mut data = ExtentStream::new_suffix(File::open(&args.file)?, usize(data_offset))?;
    fs::create_dir_all(&args.dst)?;
    for part in &manifest.partitions {
        if let Some(parts) = &parts {
            if !parts.contains(&part.partition_name.as_str()) {
                continue;
            }
        }

        extract_part(manifest, args, &mut data, part).with_context(|| {
            format!("Error ocurred while processing partition {}", part.partition_name)
        })?;
    }
    Ok(())
}
