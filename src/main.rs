use std::{fs::File, io::Seek};

use anyhow::{ensure, Context, Result};
use binrw::BinRead;
use clap::{Args, Parser, Subcommand};
use prost::Message;
use update_metadata::{
    install_operation::Type as OperationType, DeltaArchiveManifest, InstallOperation,
    PartitionUpdate,
};

mod extract;
mod inspect;

// cli

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand, Debug)]
enum Action {
    #[command(name = "extract")]
    /// Extract image files from the payload file
    Extract(ExtractArgs),
    #[command(name = "inspect")]
    /// Show information about included partition updates
    Inspect(InspectArgs),
}

impl Action {
    fn get_file(&self) -> &str {
        match self {
            Action::Extract(inner) => &inner.file,
            Action::Inspect(inner) => &inner.file,
        }
    }
}

#[derive(Debug, Args)]
struct ExtractArgs {
    #[arg()]
    /// The payload.bin file
    file: String,
    #[arg(long)]
    /// The folder which contains the image files before the update (only needed for incremental OTAs)
    src: Option<String>,
    #[arg(long)]
    /// The folder which will contain the image files after the update
    dst: String,
    #[arg(long)]
    /// The parts to extract; defaults to all parts
    parts: Option<Option<String>>,
    #[arg(long)]
    /// Disable hash checking for src images and payload data
    skip_hash: bool,
}

#[derive(Debug, Args)]
struct InspectArgs {
    #[arg()]
    /// The payload.bin file
    file: String,
    #[arg(long)]
    /// The parts to list operations for; leave empty for all parts
    dump_ops: Option<Option<String>>,
}

// payload

pub mod update_metadata {
    pub const DEFAULT_BLOCK_SIZE: u32 = 4096;
    include!(concat!(env!("OUT_DIR"), "/chromeos_update_engine.rs"));
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum UpdateType {
    Unknown,
    Incremental,
    Full,
}

pub trait HasUpdateType {
    fn get_update_type(&self) -> UpdateType;
}

impl HasUpdateType for OperationType {
    fn get_update_type(&self) -> UpdateType {
        use UpdateType::*;
        match self {
            // deprecated
            OperationType::Move => Incremental,
            OperationType::Bsdiff => Incremental,
            // full
            OperationType::Replace => Full,
            OperationType::ReplaceBz => Full,
            OperationType::ReplaceXz => Full,
            OperationType::Zero => Full,
            OperationType::Discard => Full,
            // incremental
            OperationType::SourceCopy => Incremental,
            OperationType::SourceBsdiff => Incremental,
            OperationType::BrotliBsdiff => Incremental,
            OperationType::Puffdiff => Incremental,
            OperationType::Zucchini => Incremental,
            OperationType::Lz4diffBsdiff => Incremental,
            OperationType::Lz4diffPuffdiff => Incremental,
        }
    }
}

impl HasUpdateType for InstallOperation {
    fn get_update_type(&self) -> UpdateType {
        update_metadata::install_operation::Type::try_from(self.r#type)
            .as_ref()
            .map(HasUpdateType::get_update_type)
            .unwrap_or(UpdateType::Unknown)
    }
}

impl HasUpdateType for PartitionUpdate {
    fn get_update_type(&self) -> UpdateType {
        self.operations.iter().map(HasUpdateType::get_update_type).min().unwrap_or(UpdateType::Full)
    }
}

impl HasUpdateType for DeltaArchiveManifest {
    fn get_update_type(&self) -> UpdateType {
        self.partitions.iter().map(HasUpdateType::get_update_type).min().unwrap_or(UpdateType::Full)
    }
}

#[derive(BinRead)]
#[br(magic = b"CrAU", big)]
struct PayloadFile {
    file_format_version: u64,
    _manifest_size: u64,
    #[br(if(file_format_version >= 2))]
    _metadata_signature_size: u32,
    #[br(count = _manifest_size)]
    manifest: Vec<u8>,
    #[br(count = _metadata_signature_size)]
    _metadata_signature_message: Vec<u8>,
}

pub fn parse_parts(parts: &Option<Option<String>>) -> Option<Vec<&str>> {
    parts.as_ref().map(|parts| {
        parts
            .as_deref()
            .map(|parts| parts.split(",").map(|part| part.trim()).collect::<Vec<_>>())
            .unwrap_or_default()
    })
}

fn main() -> Result<()> {
    let args = Cli::parse();
    let file_name = args.command.get_file();
    let mut file = File::open(file_name)
        .with_context(|| format!("Failed to open file payload file {}", file_name))?;
    let payload = PayloadFile::read(&mut file)
        .with_context(|| format!("Failed to parse file payload file {}", file_name))?;
    ensure!(
        payload.file_format_version == 2,
        "unsupported file version {}, only version 2 is supported",
        payload.file_format_version
    );

    let data_offset = file.stream_position()?;
    let manifest = DeltaArchiveManifest::decode(&*payload.manifest)
        .with_context(|| format!("Failed to parse file payload file manifest for payload file"))?;

    match args.command {
        Action::Extract(extract_args) => extract::extract(&manifest, &extract_args, data_offset)
            .with_context(|| format!("Failed to extract images"))?,
        Action::Inspect(inspect_args) => inspect::inspect(&manifest, &inspect_args, data_offset)
            .with_context(|| format!("Failed to inspect payload"))?,
    };

    Ok(())
}
