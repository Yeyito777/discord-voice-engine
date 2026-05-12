use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use discord_voice_engine::audio::{pad_to_full_opus_frames, write_wav_f32};
use discord_voice_engine::encode::{decode_frames_to_wav, encode_float_frames};
use discord_voice_engine::file_input::load_audio_file;
use discord_voice_engine::pulse_capture::{CaptureOptions, capture_mic_to_rtp};
use discord_voice_engine::rtp::{frame_payloads_to_rtp, send_rtp_frames};
use discord_voice_engine::{AudioMode, DEFAULT_PAYLOAD_TYPE, EngineConfig, SAMPLE_RATE};

#[derive(Debug, Parser)]
#[command(name = "discord-voice-engine")]
#[command(about = "Native audio-to-Opus RTP engine for Discord voice clients")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Decode an audio file, encode it with libopus, and optionally send plain RTP to a local UDP relay.
    EncodeFile {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(long)]
        rtp: Option<String>,
        #[arg(long, value_enum, default_value_t = ModeArg::Music)]
        mode: ModeArg,
        #[arg(long, default_value_t = 2)]
        channels: u8,
        #[arg(long)]
        bitrate: Option<i32>,
        #[arg(long, default_value_t = DEFAULT_PAYLOAD_TYPE)]
        payload_type: u8,
        #[arg(long, default_value_t = 1)]
        ssrc: u32,
        /// Send packets as fast as possible instead of pacing at 20 ms per frame.
        #[arg(long)]
        no_realtime: bool,
        #[arg(long)]
        dump_input_pcm: Option<PathBuf>,
        #[arg(long)]
        dump_decoded_opus: Option<PathBuf>,
        #[arg(long)]
        stats_json: Option<PathBuf>,
    },
    /// Capture the PulseAudio default source, encode it with libopus, and send plain RTP to a local UDP relay.
    CaptureMic {
        #[arg(long)]
        rtp: String,
        #[arg(long, value_enum, default_value_t = ModeArg::Voice)]
        mode: ModeArg,
        #[arg(long, default_value = "default")]
        device: String,
        #[arg(long, default_value_t = 2)]
        channels: u8,
        #[arg(long)]
        bitrate: Option<i32>,
        #[arg(long, default_value_t = DEFAULT_PAYLOAD_TYPE)]
        payload_type: u8,
        #[arg(long, default_value_t = 1)]
        ssrc: u32,
        /// Write mono signed 16-bit little-endian meter PCM to stdout for speech-level detection.
        #[arg(long)]
        meter_stdout: bool,
        #[arg(long)]
        duration_ms: Option<u64>,
        #[arg(long)]
        dump_input_pcm: Option<PathBuf>,
        #[arg(long)]
        stats_json: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    Voice,
    Music,
}

impl From<ModeArg> for AudioMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Voice => AudioMode::Voice,
            ModeArg::Music => AudioMode::Music,
        }
    }
}

#[derive(Debug, Serialize)]
struct EncodeFileStats {
    mode: &'static str,
    input: String,
    input_duration_ms: u64,
    sample_rate: u32,
    channels: u8,
    frames: usize,
    packets: usize,
    payload_bytes: usize,
    average_payload_bytes: f64,
    bitrate: i32,
    sent_packets: usize,
    realtime: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::EncodeFile {
            input,
            rtp,
            mode,
            channels,
            bitrate,
            payload_type,
            ssrc,
            no_realtime,
            dump_input_pcm,
            dump_decoded_opus,
            stats_json,
        } => {
            let mode = AudioMode::from(mode);
            let config = EngineConfig::new(mode, channels, bitrate, payload_type, ssrc);
            encode_file_command(
                &input,
                rtp.as_deref(),
                config,
                !no_realtime,
                dump_input_pcm,
                dump_decoded_opus,
                stats_json,
            )?;
        }
        Command::CaptureMic {
            rtp,
            mode,
            device,
            channels,
            bitrate,
            payload_type,
            ssrc,
            meter_stdout,
            duration_ms,
            dump_input_pcm,
            stats_json,
        } => {
            let mode = AudioMode::from(mode);
            let config = EngineConfig::new(mode, channels, bitrate, payload_type, ssrc);
            capture_mic_to_rtp(
                &config,
                CaptureOptions {
                    device: Some(device.as_str()),
                    rtp_addr: &rtp,
                    meter_stdout,
                    duration_ms,
                    dump_input_pcm: dump_input_pcm.as_deref(),
                    stats_json: stats_json.as_deref(),
                },
            )?;
        }
    }
    Ok(())
}

fn encode_file_command(
    input: &PathBuf,
    rtp: Option<&str>,
    config: EngineConfig,
    realtime: bool,
    dump_input_pcm: Option<PathBuf>,
    dump_decoded_opus: Option<PathBuf>,
    stats_json: Option<PathBuf>,
) -> Result<EncodeFileStats> {
    let mut audio = load_audio_file(input, config.channels)
        .with_context(|| format!("load {}", input.display()))?;
    let input_duration_ms = audio.duration_ms();
    pad_to_full_opus_frames(&mut audio.samples, audio.channels);

    if let Some(path) = dump_input_pcm.as_deref() {
        write_wav_f32(path, &audio.samples, audio.channels, audio.sample_rate)
            .with_context(|| format!("write input PCM dump {}", path.display()))?;
    }

    let payloads = encode_float_frames(&audio.samples, &config)?;
    if let Some(path) = dump_decoded_opus.as_deref() {
        decode_frames_to_wav(path, &payloads, config.channels)
            .with_context(|| format!("write decoded Opus dump {}", path.display()))?;
    }
    let frames = frame_payloads_to_rtp(payloads);
    let payload_bytes: usize = frames.iter().map(|frame| frame.payload.len()).sum();
    let sent_packets = if let Some(addr) = rtp {
        send_rtp_frames(addr, &frames, &config, realtime)?
    } else {
        0
    };
    let stats = EncodeFileStats {
        mode: "encode-file",
        input: input.display().to_string(),
        input_duration_ms,
        sample_rate: SAMPLE_RATE,
        channels: config.channels,
        frames: frames.len(),
        packets: frames.len(),
        payload_bytes,
        average_payload_bytes: if frames.is_empty() {
            0.0
        } else {
            payload_bytes as f64 / frames.len() as f64
        },
        bitrate: config.bitrate,
        sent_packets,
        realtime,
    };
    if let Some(path) = stats_json.as_deref() {
        std::fs::write(path, serde_json::to_vec_pretty(&stats)?)
            .with_context(|| format!("write stats {}", path.display()))?;
    }
    eprintln!(
        "discord-voice-engine encode-file: encoded {} frame(s), {} Opus byte(s), sent {} RTP packet(s)",
        frames.len(),
        payload_bytes,
        sent_packets,
    );
    Ok(stats)
}
