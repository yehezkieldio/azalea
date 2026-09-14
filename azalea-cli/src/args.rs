//! Hand-rolled CLI argument parsing.
//!
//! ## Rationale
//! The workspace has no CLI-parsing dependency anywhere; `azalea-probe`
//! (`azalea/src/bin/azalea_probe.rs`) already established a manual
//! `Args::parse()` convention for a small flag surface. This follows it
//! rather than introducing `clap` for a handful of flags.

use anyhow::{Context, Result, bail};
use azalea_core::config::{HardwareAcceleration, QualityPreset, TargetVideoCodec};
use std::path::PathBuf;

/// User-facing codec selection. `Auto` resolves to the fastest usable
/// *hardware* encoder for its tier, not "always try AV1" — see the CLI's
/// target-hardware notes: neither of this project's reference machines has
/// hardware AV1 encode, so `Auto` means HEVC in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecChoice {
    Auto,
    Av1,
    Hevc,
    H264,
    /// Skip transcoding entirely; save the downloaded file as-is.
    Copy,
}

impl CodecChoice {
    pub fn target_codec(self) -> Option<TargetVideoCodec> {
        match self {
            Self::Auto | Self::Hevc => Some(TargetVideoCodec::Hevc),
            Self::Av1 => Some(TargetVideoCodec::Av1),
            Self::H264 => Some(TargetVideoCodec::H264),
            Self::Copy => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwChoice {
    /// OS-based heuristic tuned for this project's two reference machines
    /// (NVIDIA on Linux, AMD on Windows), not general GPU detection.
    Auto,
    None,
    Nvenc,
    Vaapi,
    VideoToolbox,
    Qsv,
    Amf,
}

impl HwChoice {
    pub fn resolve(self) -> HardwareAcceleration {
        match self {
            Self::Auto => {
                if cfg!(target_os = "windows") {
                    HardwareAcceleration::Amf
                } else {
                    HardwareAcceleration::Nvenc
                }
            }
            Self::None => HardwareAcceleration::None,
            Self::Nvenc => HardwareAcceleration::Nvenc,
            Self::Vaapi => HardwareAcceleration::Vaapi,
            Self::VideoToolbox => HardwareAcceleration::VideoToolbox,
            Self::Qsv => HardwareAcceleration::Qsv,
            Self::Amf => HardwareAcceleration::Amf,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub urls: Vec<String>,
    pub input_file: Option<PathBuf>,
    pub output_dir: PathBuf,
    pub codec: CodecChoice,
    pub quality: QualityPreset,
    pub crf: Option<u8>,
    pub max_size_bytes: Option<u64>,
    pub hw: HwChoice,
    /// Concurrent download+transcode jobs. Defaults to 1 — chosen for the
    /// tightest reference machine's RAM headroom, not CPU count.
    pub concurrency: u32,
    /// Reuse the bot's Discord-shaped `optimize::optimize` ladder (splits
    /// into multiple parts, targets `max_upload_bytes`) instead of the local
    /// single-file path.
    pub discord_cap: bool,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            urls: Vec::new(),
            input_file: None,
            output_dir: PathBuf::from("."),
            codec: CodecChoice::Auto,
            quality: QualityPreset::Balanced,
            crf: None,
            max_size_bytes: None,
            hw: HwChoice::Auto,
            concurrency: 1,
            discord_cap: false,
        }
    }
}

impl CliArgs {
    pub fn parse() -> Result<Self> {
        let mut args = Self {
            output_dir: std::env::current_dir().context("resolve current directory")?,
            ..Self::default()
        };

        let mut values = std::env::args().skip(1);
        while let Some(arg) = values.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--input" => {
                    args.input_file = Some(PathBuf::from(next_value(&mut values, "--input")?));
                }
                "-o" | "--output" => {
                    args.output_dir = PathBuf::from(next_value(&mut values, "--output")?);
                }
                "--codec" => {
                    args.codec = parse_codec(&next_value(&mut values, "--codec")?)?;
                }
                "--quality" => {
                    args.quality = parse_quality(&next_value(&mut values, "--quality")?)?;
                }
                "--crf" => {
                    let raw = next_value(&mut values, "--crf")?;
                    args.crf = Some(
                        raw.parse()
                            .with_context(|| format!("invalid --crf value: {raw}"))?,
                    );
                }
                "--max-size" => {
                    let raw = next_value(&mut values, "--max-size")?;
                    args.max_size_bytes = Some(parse_size(&raw)?);
                }
                "--hw" => {
                    args.hw = parse_hw(&next_value(&mut values, "--hw")?)?;
                }
                "-j" | "--concurrency" => {
                    let raw = next_value(&mut values, "--concurrency")?;
                    args.concurrency = raw
                        .parse()
                        .with_context(|| format!("invalid --concurrency value: {raw}"))?;
                    if args.concurrency == 0 {
                        bail!("--concurrency must be at least 1");
                    }
                }
                "--discord-cap" => args.discord_cap = true,
                value if value.starts_with('-') => bail!("unknown option: {value}"),
                value => args.urls.push(value.to_owned()),
            }
        }

        // No URLs and no --input: enter interactive mode (see `run::run_interactive`)
        // instead of erroring, so re-running with different links doesn't need
        // retyping every flag each time.
        Ok(args)
    }

    /// Whether no URL source was given on the command line, so `run` should
    /// prompt for links interactively instead of processing a fixed batch.
    pub fn is_interactive(&self) -> bool {
        self.urls.is_empty() && self.input_file.is_none()
    }
}

fn next_value(values: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    values
        .next()
        .with_context(|| format!("{flag} requires a value"))
}

fn parse_codec(raw: &str) -> Result<CodecChoice> {
    match raw {
        "auto" => Ok(CodecChoice::Auto),
        "av1" => Ok(CodecChoice::Av1),
        "hevc" | "h265" => Ok(CodecChoice::Hevc),
        "h264" => Ok(CodecChoice::H264),
        "copy" => Ok(CodecChoice::Copy),
        other => bail!("unknown --codec value: {other} (expected auto|av1|hevc|h264|copy)"),
    }
}

fn parse_quality(raw: &str) -> Result<QualityPreset> {
    match raw {
        "fast" => Ok(QualityPreset::Fast),
        "balanced" => Ok(QualityPreset::Balanced),
        "quality" => Ok(QualityPreset::Quality),
        "size" => Ok(QualityPreset::Size),
        other => bail!("unknown --quality value: {other} (expected fast|balanced|quality|size)"),
    }
}

fn parse_hw(raw: &str) -> Result<HwChoice> {
    match raw {
        "auto" => Ok(HwChoice::Auto),
        "none" => Ok(HwChoice::None),
        "nvenc" => Ok(HwChoice::Nvenc),
        "vaapi" => Ok(HwChoice::Vaapi),
        "videotoolbox" => Ok(HwChoice::VideoToolbox),
        "qsv" => Ok(HwChoice::Qsv),
        "amf" => Ok(HwChoice::Amf),
        other => bail!(
            "unknown --hw value: {other} (expected auto|none|nvenc|vaapi|videotoolbox|qsv|amf)"
        ),
    }
}

fn parse_size(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    let (digits, multiplier) = if let Some(prefix) = lower.strip_suffix("gb") {
        (prefix, 1024 * 1024 * 1024)
    } else if let Some(prefix) = lower.strip_suffix("mb") {
        (prefix, 1024 * 1024)
    } else if let Some(prefix) = lower.strip_suffix("kb") {
        (prefix, 1024)
    } else if let Some(prefix) = lower.strip_suffix('b') {
        (prefix, 1)
    } else {
        (lower.as_str(), 1)
    };

    let value: f64 = digits
        .trim()
        .parse()
        .with_context(|| format!("invalid --max-size value: {raw}"))?;
    if !value.is_finite() || value <= 0.0 {
        bail!("--max-size must be a positive number: {raw}");
    }

    Ok((value * multiplier as f64) as u64)
}

fn print_usage() {
    println!(
        "usage: azalea-cli [OPTIONS] [URL]...\n\n\
         With no URLs and no --input, enters interactive mode: paste one link\n\
         per line and it downloads immediately, so options don't need retyping.\n\n\
         Options:\n\
         \x20 --input <FILE>        Read newline-delimited URLs from FILE\n\
         \x20 -o, --output <DIR>    Output directory (default: current directory)\n\
         \x20 --codec <MODE>        auto|av1|hevc|h264|copy (default: auto = hardware HEVC)\n\
         \x20 --quality <PRESET>    fast|balanced|quality|size (default: balanced)\n\
         \x20 --crf <N>             Override the quality preset's CRF/CQ value\n\
         \x20 --max-size <SIZE>     Target output size (e.g. 50MB); default is CRF mode\n\
         \x20 --hw <BACKEND>        auto|none|nvenc|vaapi|videotoolbox|qsv|amf (default: auto)\n\
         \x20 -j, --concurrency <N> Concurrent jobs (default: 1)\n\
         \x20 --discord-cap         Use the bot's Discord upload-size ladder instead\n\
         \x20 -h, --help            Print this message"
    );
}
