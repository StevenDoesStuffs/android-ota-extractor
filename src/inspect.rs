use base64::prelude::*;
use std::fmt::{self, Debug, Display, Formatter};

use crate::{
    parse_parts,
    update_metadata::{
        install_operation::Type as OperationType, DeltaArchiveManifest, Extent as RawExtent,
        InstallOperation, DEFAULT_BLOCK_SIZE,
    },
    HasUpdateType, InspectArgs,
};
use anyhow::Result;

fn print_option<T: Display>(val: Option<&T>, unknown: &str) -> String {
    val.map(|v| format!("{}", v)).unwrap_or_else(|| unknown.to_string())
}

fn print_option_hash(hash: Option<&[u8]>) -> String {
    hash.map(|hash| BASE64_STANDARD.encode(hash)).unwrap_or_else(|| "none".to_string())
}

struct PrettyExtent<'a>(&'a RawExtent);

impl<'a> Debug for PrettyExtent<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match (self.0.start_block, self.0.num_blocks) {
            (Some(start), Some(len)) if start != u64::MAX => {
                write!(f, "blk{}..blk{} ({} blks)", start, start + len, len)?
            }
            _ => write!(f, "invalid")?,
        }
        Ok(())
    }
}

impl Display for InstallOperation {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{{ ")?;
        if let Ok(op_type) = OperationType::try_from(self.r#type) {
            write!(f, "type: {:?} ({:?}), ", op_type, op_type.get_update_type())?;
        } else {
            write!(f, "type: invalid, ",)?;
        }
        write!(
            f,
            "data: {}, ",
            if let (Some(offset), Some(len)) = (self.data_offset, self.data_length) {
                format!("0x{:x}..0x{:x} ({} B)", offset, offset + len, len)
            } else if self.data_offset.or(self.data_length).is_some() {
                "Invalid".to_string()
            } else {
                "None".to_string()
            }
        )?;
        write!(f, "src_sha256: {}, ", print_option_hash(self.src_sha256_hash.as_deref()))?;
        write!(f, "data_sha256: {}, ", print_option_hash(self.data_sha256_hash.as_deref()))?;
        write!(
            f,
            "src_extents: {:?}, ",
            self.src_extents.iter().map(PrettyExtent).collect::<Vec<_>>()
        )?;
        write!(
            f,
            "dst_extents: {:?}",
            self.dst_extents.iter().map(PrettyExtent).collect::<Vec<_>>()
        )?;
        write!(f, " }}")?;
        Ok(())
    }
}

pub fn inspect(
    manifest: &DeltaArchiveManifest,
    args: &InspectArgs,
    data_offset: u64,
) -> Result<()> {
    let list_ops = parse_parts(&args.dump_ops);
    println!("update_type: {:?}", manifest.get_update_type());
    println!("block_size: {0} (0x{0:x})", manifest.block_size.unwrap_or(DEFAULT_BLOCK_SIZE));
    println!("minor_version: {}", print_option(manifest.minor_version.as_ref(), "unknown"));
    println!(
        "security_patch_level: {}",
        print_option(manifest.security_patch_level.as_ref(), "unknown")
    );
    println!("data_offset: 0x{:x}", data_offset);
    println!();
    println!("==========");
    println!();

    for partition in &manifest.partitions {
        let name = &partition.partition_name;
        println!("name: {}", name);
        println!("update_type: {:?}", partition.get_update_type());
        println!(
            "postinstall: {}",
            if partition.run_postinstall == Some(true) {
                partition.postinstall_path.clone().unwrap_or_else(|| format!("postinst"))
            } else {
                format!("None")
            }
        );
        println!("num_operations: {}", partition.operations.len());

        let mut print_ops = false;
        if let Some(list_ops) = &list_ops {
            print_ops = list_ops.is_empty() || list_ops.contains(&name.as_str());
        }
        if !print_ops {
            println!();
            continue;
        }

        println!("operations: ");
        for op in &partition.operations {
            println!("- {}", op);
        }
        println!();
    }
    Ok(())
}
