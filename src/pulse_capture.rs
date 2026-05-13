use std::io::{self, BufRead, Read, Write};
use std::net::{ToSocketAddrs, UdpSocket};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::audio::{interleaved_i16_to_mono, write_wav_i16};
use crate::encode::{configure_encoder, encode_i16_frame};
use crate::noise_suppression::{
    NoiseSuppressionMode, NoiseSuppressor, parse_noise_suppression_mode,
};
use crate::rtp::build_rtp_packet;
use crate::{EngineConfig, FRAME_SAMPLES, RTP_CLOCK_INCREMENT, SAMPLE_RATE};

#[derive(Debug, Clone)]
pub struct CaptureOptions<'a> {
    pub device: Option<&'a str>,
    pub rtp_addr: &'a str,
    pub meter_stdout: bool,
    pub duration_ms: Option<u64>,
    pub dump_input_pcm: Option<&'a Path>,
    pub stats_json: Option<&'a Path>,
    pub noise_suppression: NoiseSuppressionMode,
}

#[derive(Debug, Serialize)]
pub struct CaptureStats {
    pub mode: &'static str,
    pub source: Option<String>,
    pub sample_rate: u32,
    pub channels: u8,
    pub frames: usize,
    pub packets: usize,
    pub duration_ms: u64,
    pub payload_bytes: usize,
    pub bitrate: i32,
    pub capture_backend: &'static str,
    pub noise_suppression: &'static str,
    pub noise_suppression_changes: usize,
}

pub fn build_parec_command(device: Option<&str>, channels: u8) -> Vec<String> {
    let mut args = vec![
        "--record".to_string(),
        "--raw".to_string(),
        "--client-name=discord-voice-engine".to_string(),
        "--stream-name=Discord voice microphone".to_string(),
        "--format=s16le".to_string(),
        format!("--rate={SAMPLE_RATE}"),
        format!("--channels={}", channels.clamp(1, 2)),
        "--latency-msec=20".to_string(),
        "--process-time-msec=20".to_string(),
    ];
    if let Some(device) = device.filter(|device| !device.is_empty() && *device != "default") {
        args.push(format!("--device={device}"));
    }
    args
}

pub fn capture_mic_to_rtp(
    config: &EngineConfig,
    options: CaptureOptions<'_>,
) -> Result<CaptureStats> {
    let source = options
        .device
        .filter(|device| !device.is_empty() && *device != "default");
    let args = build_parec_command(source, config.channels);
    let mut child = Command::new("parec")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start parec low-latency PulseAudio capture")?;
    let mut capture_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("parec stdout was not piped"))?;
    let mut child_stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    install_child_signal_handler(&child).context("install capture child signal handler")?;

    let socket = UdpSocket::bind(("127.0.0.1", 0)).context("bind local RTP sender")?;
    let mut addrs = options
        .rtp_addr
        .to_socket_addrs()
        .with_context(|| format!("resolve RTP address {}", options.rtp_addr))?;
    let addr = addrs
        .next()
        .ok_or_else(|| anyhow::anyhow!("RTP address did not resolve: {}", options.rtp_addr))?;
    let mut encoder = configure_encoder(config)?;
    let mut stdout = io::stdout().lock();
    let mut input_dump: Vec<i16> = Vec::new();
    let requested_noise_suppression =
        Arc::new(AtomicU8::new(noise_mode_to_u8(options.noise_suppression)));
    install_stdin_control_handler(Arc::clone(&requested_noise_suppression));
    let mut noise_suppressor = NoiseSuppressor::new(options.noise_suppression);
    let mut noise_suppression_changes = 0usize;

    let samples_per_packet = FRAME_SAMPLES * config.channels as usize;
    let bytes_per_packet = samples_per_packet * 2;
    let mut bytes = vec![0u8; bytes_per_packet];
    let deadline = options
        .duration_ms
        .map(|duration| Instant::now() + Duration::from_millis(duration));
    let mut sequence = 0u16;
    let mut timestamp = 0u32;
    let mut packets = 0usize;
    let mut payload_bytes = 0usize;
    let started = Instant::now();
    let debug_capture = std::env::var_os("DVE_CAPTURE_DEBUG").is_some();
    let result = loop {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            break Ok(());
        }
        if let Err(error) = capture_stdout.read_exact(&mut bytes) {
            break Err(error).context("read PCM from parec");
        }
        let mut pcm = Vec::with_capacity(samples_per_packet);
        for chunk in bytes.chunks_exact(2) {
            pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        if options.dump_input_pcm.is_some() {
            input_dump.extend_from_slice(&pcm);
        }
        let requested_mode =
            noise_mode_from_u8(requested_noise_suppression.load(Ordering::Relaxed));
        if requested_mode != noise_suppressor.mode() {
            noise_suppressor.set_mode(requested_mode);
            noise_suppression_changes += 1;
            eprintln!(
                "discord-voice-engine capture-mic: noise suppression set to {}",
                requested_mode.as_str()
            );
        }
        noise_suppressor.process_interleaved_i16_frame(&mut pcm, config.channels);
        if options.meter_stdout {
            write_meter_pcm(&mut stdout, &pcm, config.channels)?;
        }

        let payload = encode_i16_frame(&mut encoder, &pcm)?;
        payload_bytes += payload.len();
        let packet = build_rtp_packet(
            config.payload_type,
            sequence,
            timestamp,
            config.ssrc,
            &payload,
        );
        socket
            .send_to(&packet, addr)
            .context("send capture RTP packet")?;
        packets += 1;
        if debug_capture {
            eprintln!(
                "discord-voice-engine capture debug: packet={packets} elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
        sequence = sequence.wrapping_add(1);
        timestamp = timestamp.wrapping_add(RTP_CLOCK_INCREMENT);
    };

    terminate_child(&child);
    let mut stderr = String::new();
    if let Some(mut child_stderr) = child_stderr.take() {
        let _ = child_stderr.read_to_string(&mut stderr);
    }
    result.with_context(|| {
        format!(
            "parec failed{}",
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        )
    })?;

    if let Some(path) = options.dump_input_pcm {
        write_wav_i16(path, &input_dump, config.channels, SAMPLE_RATE)
            .with_context(|| format!("write input PCM dump {}", path.display()))?;
    }

    let stats = CaptureStats {
        mode: "capture-mic",
        source: source.map(str::to_string),
        sample_rate: SAMPLE_RATE,
        channels: config.channels,
        frames: packets,
        packets,
        duration_ms: started.elapsed().as_millis() as u64,
        payload_bytes,
        bitrate: config.bitrate,
        capture_backend: "parec",
        noise_suppression: noise_suppressor.mode().as_str(),
        noise_suppression_changes,
    };
    if let Some(path) = options.stats_json {
        std::fs::write(path, serde_json::to_vec_pretty(&stats)?)
            .with_context(|| format!("write stats {}", path.display()))?;
    }
    eprintln!(
        "discord-voice-engine capture-mic: sent {packets} RTP packet(s), {payload_bytes} Opus byte(s) via parec, noise_suppression={} changes={noise_suppression_changes}",
        noise_suppressor.mode().as_str()
    );
    Ok(stats)
}

fn noise_mode_to_u8(mode: NoiseSuppressionMode) -> u8 {
    match mode {
        NoiseSuppressionMode::Off => 0,
        NoiseSuppressionMode::Simple => 1,
    }
}

fn noise_mode_from_u8(value: u8) -> NoiseSuppressionMode {
    match value {
        1 => NoiseSuppressionMode::Simple,
        _ => NoiseSuppressionMode::Off,
    }
}

fn install_stdin_control_handler(requested_noise_suppression: Arc<AtomicU8>) {
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if let Some(mode) = parse_noise_suppression_control_line(&line) {
                requested_noise_suppression.store(noise_mode_to_u8(mode), Ordering::Relaxed);
            }
        }
    });
}

fn parse_noise_suppression_control_line(line: &str) -> Option<NoiseSuppressionMode> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(value) = trimmed.strip_prefix("noise-suppression ") {
        return parse_noise_suppression_mode(value);
    }
    if let Some(value) = trimmed.strip_prefix("set noise-suppression ") {
        return parse_noise_suppression_mode(value);
    }
    None
}

fn write_meter_pcm<W: Write>(writer: &mut W, pcm: &[i16], channels: u8) -> Result<()> {
    let mono = interleaved_i16_to_mono(pcm, channels);
    let mut meter_bytes = Vec::with_capacity(mono.len() * 2);
    for sample in mono {
        meter_bytes.extend_from_slice(&sample.to_le_bytes());
    }
    match writer.write_all(&meter_bytes) {
        Ok(()) => {
            let _ = writer.flush();
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error).context("write capture meter PCM to stdout"),
    }
}

fn install_child_signal_handler(child: &Arc<Mutex<Child>>) -> Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    let child = Arc::clone(child);
    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            terminate_child(&child);
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}

fn terminate_child(child: &Arc<Mutex<Child>>) {
    let Ok(mut child) = child.lock() else {
        return;
    };
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioMode, DEFAULT_PAYLOAD_TYPE};

    #[test]
    fn builds_low_latency_parec_command() {
        assert_eq!(
            build_parec_command(None, 2),
            vec![
                "--record",
                "--raw",
                "--client-name=discord-voice-engine",
                "--stream-name=Discord voice microphone",
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=20",
                "--process-time-msec=20",
            ]
        );
        assert!(
            build_parec_command(Some("alsa_input.test"), 1)
                .contains(&"--device=alsa_input.test".to_string())
        );
    }

    #[test]
    fn capture_stats_are_serializable() {
        let stats = CaptureStats {
            mode: "capture-mic",
            source: Some("default".to_string()),
            sample_rate: SAMPLE_RATE,
            channels: 2,
            frames: 3,
            packets: 3,
            duration_ms: 60,
            payload_bytes: 123,
            bitrate: AudioMode::Voice.default_bitrate(2),
            capture_backend: "parec",
            noise_suppression: "off",
            noise_suppression_changes: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("capture-mic"));
        assert!(json.contains("96000"));
        assert!(json.contains("parec"));

        let config = EngineConfig::new(AudioMode::Voice, 2, None, DEFAULT_PAYLOAD_TYPE, 1);
        assert_eq!(config.bitrate, 96_000);
    }

    #[test]
    fn parses_stdin_noise_control_lines() {
        assert_eq!(
            parse_noise_suppression_control_line("noise-suppression simple"),
            Some(NoiseSuppressionMode::Simple)
        );
        assert_eq!(parse_noise_suppression_control_line("volume 10"), None);
    }

    #[test]
    fn writes_meter_pcm_as_mono_s16le() {
        let mut bytes = Vec::new();
        write_meter_pcm(&mut bytes, &[100, 300, -100, -300], 2).unwrap();
        assert_eq!(
            bytes,
            [200i16.to_le_bytes(), (-200i16).to_le_bytes()].concat()
        );
    }
}
