use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use clap::{Parser, Subcommand, ValueEnum};
use opus::{Channels, Decoder};
use serde::{Deserialize, Serialize};

use discord_voice_engine::audio::{
    float_to_i16, pad_to_full_opus_frames, write_wav_f32, write_wav_i16,
};
use discord_voice_engine::encode::{decode_frames_to_wav, encode_float_frames};
use discord_voice_engine::file_input::load_audio_file;
use discord_voice_engine::noise_suppression::NoiseSuppressionMode;
use discord_voice_engine::parent_watchdog::install_parent_exit_watchdog;
use discord_voice_engine::playback::{
    PlaybackRecoveryStats, RtpAudioPacket, recover_ordered_packets_to_pcm,
};
use discord_voice_engine::playback_c::{CPlaybackOptions, CPlaybackOutput, play_rtp_c};
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
        #[arg(long, value_enum, default_value_t = NoiseSuppressionArg::Off)]
        noise_suppression: NoiseSuppressionArg,
        /// Initial microphone capture gain in dB. 0 dB is neutral/default.
        #[arg(long, default_value_t = 0.0)]
        gain_db: f32,
        /// Exit automatically if the Record process that launched this helper disappears.
        #[arg(long)]
        parent_pid: Option<u32>,
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
    /// Receive plain Opus RTP, run jitter-buffered Opus PLC recovery, and play decoded PCM.
    PlayRtp {
        #[arg(long)]
        rtp: String,
        #[arg(long, default_value_t = 2)]
        channels: u8,
        #[arg(long, default_value_t = DEFAULT_PAYLOAD_TYPE)]
        payload_type: u8,
        #[arg(long, default_value_t = 240)]
        jitter_ms: u64,
        #[arg(long, default_value_t = 350)]
        idle_timeout_ms: u64,
        #[arg(long, default_value_t = 10)]
        max_plc_packets: usize,
        #[arg(long, value_enum, default_value_t = PlaybackOutputArg::Pipewire)]
        output: PlaybackOutputArg,
        #[arg(long)]
        output_wav: Option<PathBuf>,
        #[arg(long)]
        duration_ms: Option<u64>,
        #[arg(long)]
        stats_json: Option<PathBuf>,
        #[arg(long)]
        ready_file: Option<PathBuf>,
        #[arg(long)]
        fec: bool,
        /// Exit automatically if the Record process that launched this helper disappears.
        #[arg(long)]
        parent_pid: Option<u32>,
    },
    /// Deterministically inject Opus RTP packet loss and verify decoder-state PLC recovery.
    TestPlaybackRecovery {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(long, default_value_t = 10)]
        iterations: usize,
        #[arg(long, default_value_t = 20)]
        loss_per_mille: u32,
        #[arg(long, default_value_t = 3)]
        max_burst: usize,
        #[arg(long, default_value_t = 0x5eed_u64)]
        seed: u64,
        #[arg(long, default_value_t = 2)]
        channels: u8,
        #[arg(long)]
        bitrate: Option<i32>,
        #[arg(long, default_value_t = DEFAULT_PAYLOAD_TYPE)]
        payload_type: u8,
        #[arg(long)]
        output_dir: Option<PathBuf>,
        #[arg(long)]
        stats_json: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    Voice,
    Music,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PlaybackOutputArg {
    Pipewire,
    Pulse,
    Null,
    Wav,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum NoiseSuppressionArg {
    Off,
    Simple,
}

impl From<NoiseSuppressionArg> for NoiseSuppressionMode {
    fn from(value: NoiseSuppressionArg) -> Self {
        match value {
            NoiseSuppressionArg::Off => NoiseSuppressionMode::Off,
            NoiseSuppressionArg::Simple => NoiseSuppressionMode::Simple,
        }
    }
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

#[derive(Debug, Serialize)]
struct PlaybackRecoveryHarnessSummary {
    mode: &'static str,
    input: String,
    iterations: usize,
    loss_per_mille: u32,
    max_burst: usize,
    seed: u64,
    channels: u8,
    payload_type: u8,
    encoded_packets: usize,
    total_dropped_packets: usize,
    total_concealed_packets: usize,
    total_decode_errors: usize,
    max_consecutive_missing_packets: usize,
    min_loss_window_relative_rms: f64,
    min_concealed_rms: f64,
    iterations_with_loss: usize,
    failures: Vec<String>,
    iteration_stats: Vec<PlaybackRecoveryHarnessIteration>,
}

#[derive(Debug, Serialize)]
struct PlaybackRecoveryHarnessIteration {
    iteration: usize,
    seed: u64,
    dropped_packets: usize,
    dropped_ranges: Vec<PacketRange>,
    loss_window_relative_rms: f64,
    concealed_rms: f64,
    output_duration_ms: u64,
    stats: PlaybackRecoveryStats,
    output_wav: Option<String>,
}

#[derive(Debug, Serialize)]
struct PacketRange {
    start: usize,
    end: usize,
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
            noise_suppression,
            gain_db,
            parent_pid,
        } => {
            install_parent_exit_watchdog(parent_pid, "capture-mic");
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
                    noise_suppression: NoiseSuppressionMode::from(noise_suppression),
                    gain_db,
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
        Command::PlayRtp {
            rtp,
            channels,
            payload_type,
            jitter_ms,
            idle_timeout_ms,
            max_plc_packets,
            output,
            output_wav,
            duration_ms,
            stats_json,
            ready_file,
            fec,
            parent_pid,
        } => {
            install_parent_exit_watchdog(parent_pid, "play-rtp");
            let output = c_playback_output_from_args(output, output_wav.as_deref())?;
            play_rtp_c(CPlaybackOptions {
                rtp_addr: &rtp,
                channels,
                payload_type,
                jitter_ms,
                idle_timeout_ms,
                max_plc_packets,
                output,
                output_wav: output_wav.as_deref(),
                duration_ms,
                stats_json: stats_json.as_deref(),
                ready_file: ready_file.as_deref(),
                use_fec: fec,
            })?;
        }
        Command::TestPlaybackRecovery {
            input,
            iterations,
            loss_per_mille,
            max_burst,
            seed,
            channels,
            bitrate,
            payload_type,
            output_dir,
            stats_json,
        } => {
            test_playback_recovery_command(
                &input,
                iterations,
                loss_per_mille,
                max_burst,
                seed,
                channels,
                bitrate,
                payload_type,
                output_dir.as_deref(),
                stats_json,
            )?;
        }
    }
    Ok(())
}

fn encode_file_command(
    input: &Path,
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
    input: &Path,
    output: &Path,
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

fn c_playback_output_from_args(
    output: PlaybackOutputArg,
    output_wav: Option<&std::path::Path>,
) -> Result<CPlaybackOutput> {
    Ok(match output {
        PlaybackOutputArg::Pipewire => CPlaybackOutput::PipeWire,
        PlaybackOutputArg::Pulse => CPlaybackOutput::Pulse,
        PlaybackOutputArg::Null => CPlaybackOutput::Null,
        PlaybackOutputArg::Wav => {
            if output_wav.is_none() {
                bail!("--output wav requires --output-wav");
            }
            CPlaybackOutput::Wav
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn test_playback_recovery_command(
    input: &Path,
    iterations: usize,
    loss_per_mille: u32,
    max_burst: usize,
    seed: u64,
    channels: u8,
    bitrate: Option<i32>,
    payload_type: u8,
    output_dir: Option<&std::path::Path>,
    stats_json: Option<PathBuf>,
) -> Result<PlaybackRecoveryHarnessSummary> {
    let channels = channels.clamp(1, 2);
    let config = EngineConfig::new(AudioMode::Music, channels, bitrate, payload_type, 1);
    let mut audio = load_audio_file(input, channels)
        .with_context(|| format!("load playback recovery input {}", input.display()))?;
    pad_to_full_opus_frames(&mut audio.samples, audio.channels);
    let payloads = encode_float_frames(&audio.samples, &config)?;
    let frames = frame_payloads_to_rtp(payloads);
    let packets: Vec<RtpAudioPacket> = frames
        .iter()
        .map(|frame| RtpAudioPacket {
            sequence: frame.sequence,
            timestamp: frame.timestamp,
            ssrc: config.ssrc,
            payload_type: config.payload_type,
            payload: frame.payload.clone(),
        })
        .collect();
    let (reference_pcm, reference_stats) =
        recover_ordered_packets_to_pcm(&packets, channels, payload_type, false)?;
    if reference_stats.decode_errors != 0 {
        bail!(
            "reference no-loss decode had {} decode error(s)",
            reference_stats.decode_errors
        );
    }
    if let Some(dir) = output_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create playback recovery output dir {}", dir.display()))?;
        write_wav_i16(
            &dir.join("reference.wav"),
            &reference_pcm,
            channels,
            SAMPLE_RATE,
        )
        .with_context(|| format!("write playback recovery reference WAV in {}", dir.display()))?;
    }

    let mut total_dropped_packets = 0usize;
    let mut total_concealed_packets = 0usize;
    let mut total_decode_errors = 0usize;
    let mut max_consecutive_missing_packets = 0usize;
    let mut min_loss_window_relative_rms = f64::INFINITY;
    let mut min_concealed_rms = f64::INFINITY;
    let mut iterations_with_loss = 0usize;
    let mut failures = Vec::new();
    let mut iteration_stats = Vec::new();

    for iteration in 0..iterations {
        let iteration_seed =
            seed.wrapping_add((iteration as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let dropped = choose_loss_indices(packets.len(), loss_per_mille, max_burst, iteration_seed);
        let survivors: Vec<_> = packets
            .iter()
            .enumerate()
            .filter(|(index, _packet)| !dropped[*index])
            .map(|(_index, packet)| packet.clone())
            .collect();
        let dropped_count = dropped.iter().filter(|value| **value).count();
        if dropped_count > 0 {
            iterations_with_loss += 1;
        }
        let (recovered_pcm, stats) =
            recover_ordered_packets_to_pcm(&survivors, channels, payload_type, false)?;
        let dropped_ranges = packet_ranges(&dropped);
        let expected_missing = dropped_count;
        if stats.missing_packets != expected_missing {
            failures.push(format!(
                "iteration {iteration}: expected {expected_missing} missing packet(s), recovery reported {}",
                stats.missing_packets
            ));
        }
        if stats.concealed_packets != expected_missing {
            failures.push(format!(
                "iteration {iteration}: expected {expected_missing} concealed packet(s), recovery reported {}",
                stats.concealed_packets
            ));
        }
        if recovered_pcm.len() != reference_pcm.len() {
            failures.push(format!(
                "iteration {iteration}: recovered PCM length {} != no-loss length {}",
                recovered_pcm.len(),
                reference_pcm.len()
            ));
        }
        if stats.decode_errors != 0 {
            failures.push(format!(
                "iteration {iteration}: {} Opus decode error(s)",
                stats.decode_errors
            ));
        }
        let concealed_rms = dropped_window_rms(&recovered_pcm, &dropped, channels);
        let reference_loss_rms = dropped_window_rms(&reference_pcm, &dropped, channels);
        let loss_window_relative_rms = if reference_loss_rms <= f64::EPSILON {
            1.0
        } else {
            concealed_rms / reference_loss_rms
        };
        if dropped_count > 0 {
            min_concealed_rms = min_concealed_rms.min(concealed_rms);
            min_loss_window_relative_rms =
                min_loss_window_relative_rms.min(loss_window_relative_rms);
            // This is intentionally conservative. It catches hard-silence insertion, while still
            // allowing Opus PLC to decay during long/bursty gaps.
            if reference_loss_rms > 0.02 && concealed_rms < 0.001 {
                failures.push(format!(
                    "iteration {iteration}: concealed window RMS {concealed_rms:.6} looks like hard silence"
                ));
            }
        }
        let output_wav = if let Some(dir) = output_dir {
            let path = dir.join(format!("recovered-{iteration:03}.wav"));
            write_wav_i16(&path, &recovered_pcm, channels, SAMPLE_RATE)
                .with_context(|| format!("write recovered playback WAV {}", path.display()))?;
            Some(path.display().to_string())
        } else {
            None
        };

        total_dropped_packets += dropped_count;
        total_concealed_packets += stats.concealed_packets;
        total_decode_errors += stats.decode_errors;
        max_consecutive_missing_packets =
            max_consecutive_missing_packets.max(stats.max_consecutive_missing_packets);
        iteration_stats.push(PlaybackRecoveryHarnessIteration {
            iteration,
            seed: iteration_seed,
            dropped_packets: dropped_count,
            dropped_ranges,
            loss_window_relative_rms,
            concealed_rms,
            output_duration_ms: stats.output_duration_ms,
            stats,
            output_wav,
        });
    }
    if min_loss_window_relative_rms.is_infinite() {
        min_loss_window_relative_rms = 1.0;
    }
    if min_concealed_rms.is_infinite() {
        min_concealed_rms = 0.0;
    }
    let summary = PlaybackRecoveryHarnessSummary {
        mode: "test-playback-recovery",
        input: input.display().to_string(),
        iterations,
        loss_per_mille,
        max_burst,
        seed,
        channels,
        payload_type,
        encoded_packets: packets.len(),
        total_dropped_packets,
        total_concealed_packets,
        total_decode_errors,
        max_consecutive_missing_packets,
        min_loss_window_relative_rms,
        min_concealed_rms,
        iterations_with_loss,
        failures,
        iteration_stats,
    };
    if let Some(path) = stats_json.as_deref() {
        std::fs::write(path, serde_json::to_vec_pretty(&summary)?)
            .with_context(|| format!("write playback recovery stats {}", path.display()))?;
    }
    eprintln!(
        "discord-voice-engine test-playback-recovery: {} iteration(s), {} dropped, {} concealed, {} failure(s)",
        summary.iterations,
        summary.total_dropped_packets,
        summary.total_concealed_packets,
        summary.failures.len(),
    );
    if !summary.failures.is_empty() {
        bail!(
            "playback recovery harness failed: {}",
            summary.failures.join("; ")
        );
    }
    Ok(summary)
}

fn choose_loss_indices(
    packet_count: usize,
    loss_per_mille: u32,
    max_burst: usize,
    seed: u64,
) -> Vec<bool> {
    let mut dropped = vec![false; packet_count];
    if packet_count <= 2 || loss_per_mille == 0 {
        return dropped;
    }
    let mut rng = XorShift64::new(seed);
    let mut index = 1usize;
    let max_index = packet_count - 1;
    while index < max_index {
        if rng.next_u32() % 1000 < loss_per_mille.min(1000) {
            let burst = 1 + (rng.next_u32() as usize % max_burst.max(1));
            for offset in 0..burst {
                let drop_index = index + offset;
                if drop_index >= max_index {
                    break;
                }
                dropped[drop_index] = true;
            }
            index += burst.max(1);
        } else {
            index += 1;
        }
    }
    dropped
}

fn packet_ranges(dropped: &[bool]) -> Vec<PacketRange> {
    let mut ranges = Vec::new();
    let mut index = 0usize;
    while index < dropped.len() {
        if !dropped[index] {
            index += 1;
            continue;
        }
        let start = index;
        while index < dropped.len() && dropped[index] {
            index += 1;
        }
        ranges.push(PacketRange {
            start,
            end: index - 1,
        });
    }
    ranges
}

fn dropped_window_rms(pcm: &[i16], dropped: &[bool], channels: u8) -> f64 {
    let channels = channels.max(1) as usize;
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for (packet_index, is_dropped) in dropped.iter().enumerate() {
        if !*is_dropped {
            continue;
        }
        let start = packet_index * FRAME_SAMPLES * channels;
        let end = (start + (FRAME_SAMPLES * channels)).min(pcm.len());
        for sample in &pcm[start..end] {
            let normalized = f64::from(*sample) / 32768.0;
            sum += normalized * normalized;
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
    }
}

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        (x >> 32) as u32
    }
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
