use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::Engine as _;
use clap::{Parser, Subcommand, ValueEnum};
use opus::{Channels, Decoder};
use serde::{Deserialize, Serialize};

use discord_voice_engine::audio::{
    float_to_i16, pad_to_full_opus_frames, write_wav_f32, write_wav_i16,
};
use discord_voice_engine::encode::{decode_frames_to_wav, encode_float_frames};
use discord_voice_engine::file_input::load_audio_file;
use discord_voice_engine::pulse_capture::{CaptureOptions, capture_mic_to_rtp};
use discord_voice_engine::rtp::{frame_payloads_to_rtp, send_rtp_frames};
use discord_voice_engine::{
    AudioMode, DEFAULT_PAYLOAD_TYPE, EngineConfig, FRAME_SAMPLES, SAMPLE_RATE,
};

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
    /// Decode Record's RECORD_PLAYBACK_TRACE_DIR JSONL trace into WAV and packet-loss stats.
    DecodeTrace {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, default_value_t = 2)]
        channels: u8,
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

#[derive(Debug, Deserialize)]
struct PlaybackTraceFrame {
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    payload: String,
}

#[derive(Debug, Serialize)]
struct DecodeTraceStats {
    mode: &'static str,
    input: String,
    output: String,
    sample_rate: u32,
    channels: u8,
    received_packets: usize,
    decoded_packets: usize,
    concealed_lost_packets: usize,
    sequence_gap_events: usize,
    max_consecutive_lost_packets: usize,
    duplicate_packets: usize,
    out_of_order_packets: usize,
    first_sequence: Option<u16>,
    last_sequence: Option<u16>,
    first_timestamp: Option<u32>,
    last_timestamp: Option<u32>,
    output_duration_ms: u64,
    gaps: Vec<DecodeTraceGap>,
    gaps_truncated: bool,
}

#[derive(Debug, Serialize)]
struct DecodeTraceGap {
    previous_sequence: u16,
    sequence: u16,
    missing_packets: usize,
    offset_ms: u64,
    missing_duration_ms: u64,
}

const MAX_REPORTED_TRACE_GAPS: usize = 100;

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
        Command::DecodeTrace {
            input,
            output,
            channels,
            stats_json,
        } => {
            decode_trace_command(&input, &output, channels, stats_json)?;
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

fn decode_trace_command(
    input: &PathBuf,
    output: &PathBuf,
    channels: u8,
    stats_json: Option<PathBuf>,
) -> Result<DecodeTraceStats> {
    let text = std::fs::read_to_string(input)
        .with_context(|| format!("read playback trace {}", input.display()))?;
    let mut decoder =
        Decoder::new(SAMPLE_RATE, opus_channels(channels)).context("create Opus trace decoder")?;
    let mut frame_buffer = vec![0.0f32; FRAME_SAMPLES * channels as usize * 3];
    let mut decoded = Vec::<i16>::new();
    let mut received_packets = 0usize;
    let mut decoded_packets = 0usize;
    let mut concealed_lost_packets = 0usize;
    let mut sequence_gap_events = 0usize;
    let mut max_consecutive_lost_packets = 0usize;
    let mut duplicate_packets = 0usize;
    let mut out_of_order_packets = 0usize;
    let mut first_sequence = None;
    let mut last_sequence = None;
    let mut first_timestamp = None;
    let mut last_timestamp = None;
    let mut gaps = Vec::new();
    let mut gaps_truncated = false;

    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let frame: PlaybackTraceFrame = serde_json::from_str(line)
            .with_context(|| format!("parse trace line {}", line_number + 1))?;
        let payload = base64::engine::general_purpose::STANDARD
            .decode(frame.payload.as_bytes())
            .with_context(|| format!("decode payload on trace line {}", line_number + 1))?;
        received_packets += 1;

        if first_sequence.is_none() {
            first_sequence = Some(frame.sequence);
            first_timestamp = Some(frame.timestamp);
        } else if let Some(previous_sequence) = last_sequence {
            let delta = rtp_sequence_delta(frame.sequence, previous_sequence);
            if delta == 0 {
                duplicate_packets += 1;
                continue;
            }
            if delta >= 0x8000 {
                out_of_order_packets += 1;
                continue;
            }
            if delta > 1 {
                let missing = (delta - 1) as usize;
                sequence_gap_events += 1;
                concealed_lost_packets += missing;
                max_consecutive_lost_packets = max_consecutive_lost_packets.max(missing);
                if gaps.len() < MAX_REPORTED_TRACE_GAPS {
                    gaps.push(DecodeTraceGap {
                        previous_sequence,
                        sequence: frame.sequence,
                        missing_packets: missing,
                        offset_ms: decoded_packets as u64 * 20,
                        missing_duration_ms: missing as u64 * 20,
                    });
                } else {
                    gaps_truncated = true;
                }
                for _ in 0..missing {
                    decode_one_trace_packet(
                        &mut decoder,
                        &[],
                        channels,
                        &mut frame_buffer,
                        &mut decoded,
                    )
                    .context("decode Opus packet-loss concealment frame")?;
                    decoded_packets += 1;
                }
            }
        }

        decode_one_trace_packet(
            &mut decoder,
            &payload,
            channels,
            &mut frame_buffer,
            &mut decoded,
        )
        .with_context(|| format!("decode Opus trace line {}", line_number + 1))?;
        decoded_packets += 1;
        last_sequence = Some(frame.sequence);
        last_timestamp = Some(frame.timestamp);
        let _ = frame.ssrc;
    }

    write_wav_i16(output, &decoded, channels, SAMPLE_RATE)
        .with_context(|| format!("write decoded trace WAV {}", output.display()))?;
    let output_duration_ms = if channels == 0 {
        0
    } else {
        ((decoded.len() / channels as usize) as u64 * 1000) / SAMPLE_RATE as u64
    };
    let stats = DecodeTraceStats {
        mode: "decode-trace",
        input: input.display().to_string(),
        output: output.display().to_string(),
        sample_rate: SAMPLE_RATE,
        channels,
        received_packets,
        decoded_packets,
        concealed_lost_packets,
        sequence_gap_events,
        max_consecutive_lost_packets,
        duplicate_packets,
        out_of_order_packets,
        first_sequence,
        last_sequence,
        first_timestamp,
        last_timestamp,
        output_duration_ms,
        gaps,
        gaps_truncated,
    };
    if let Some(path) = stats_json.as_deref() {
        std::fs::write(path, serde_json::to_vec_pretty(&stats)?)
            .with_context(|| format!("write stats {}", path.display()))?;
    }
    eprintln!(
        "discord-voice-engine decode-trace: decoded {} packet(s), concealed {} lost packet(s) across {} gap(s)",
        received_packets, concealed_lost_packets, sequence_gap_events,
    );
    Ok(stats)
}

fn decode_one_trace_packet(
    decoder: &mut Decoder,
    payload: &[u8],
    channels: u8,
    frame_buffer: &mut [f32],
    decoded: &mut Vec<i16>,
) -> Result<()> {
    let max_samples = if payload.is_empty() {
        FRAME_SAMPLES * channels as usize
    } else {
        frame_buffer.len()
    };
    let samples_per_channel =
        decoder.decode_float(payload, &mut frame_buffer[..max_samples], false)?;
    let count = samples_per_channel * channels as usize;
    decoded.extend(
        frame_buffer[..count]
            .iter()
            .map(|sample| float_to_i16(*sample)),
    );
    Ok(())
}

fn opus_channels(channels: u8) -> Channels {
    if channels == 1 {
        Channels::Mono
    } else {
        Channels::Stereo
    }
}

fn rtp_sequence_delta(sequence: u16, previous: u16) -> u16 {
    sequence.wrapping_sub(previous)
}
