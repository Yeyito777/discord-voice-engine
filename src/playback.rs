use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Write};
use std::net::UdpSocket;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use opus::{Channels, Decoder};
use serde::Serialize;

use crate::audio::{float_to_i16, write_wav_i16};
use crate::rtp::RTP_HEADER_LEN;
use crate::{DEFAULT_PAYLOAD_TYPE, FRAME_MS, FRAME_SAMPLES, SAMPLE_RATE};

const DEFAULT_JITTER_MS: u64 = 240;
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 350;
const DEFAULT_MAX_PLC_PACKETS: usize = 10;
const MAX_PACKET_SIZE: usize = 4096;

#[derive(Debug, Clone)]
pub struct PlaybackOptions<'a> {
    pub rtp_addr: &'a str,
    pub channels: u8,
    pub payload_type: u8,
    pub jitter_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_plc_packets: usize,
    pub output: PlaybackOutput<'a>,
    pub duration_ms: Option<u64>,
    pub stats_json: Option<&'a Path>,
    pub ready_file: Option<&'a Path>,
    pub use_fec: bool,
}

impl<'a> PlaybackOptions<'a> {
    pub fn new(rtp_addr: &'a str) -> Self {
        Self {
            rtp_addr,
            channels: 2,
            payload_type: DEFAULT_PAYLOAD_TYPE,
            jitter_ms: DEFAULT_JITTER_MS,
            idle_timeout_ms: DEFAULT_IDLE_TIMEOUT_MS,
            max_plc_packets: DEFAULT_MAX_PLC_PACKETS,
            output: PlaybackOutput::PipeWire,
            duration_ms: None,
            stats_json: None,
            ready_file: None,
            use_fec: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PlaybackOutput<'a> {
    PipeWire,
    Pulse,
    Wav(&'a Path),
    Null,
}

#[derive(Debug, Clone)]
pub struct RtpAudioPacket {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PlaybackRecoveryStats {
    pub mode: &'static str,
    pub sample_rate: u32,
    pub channels: u8,
    pub received_packets: usize,
    pub decoded_packets: usize,
    pub normal_packets: usize,
    pub concealed_packets: usize,
    pub fec_attempts: usize,
    pub sequence_gap_events: usize,
    pub missing_packets: usize,
    pub max_consecutive_missing_packets: usize,
    pub duplicate_packets: usize,
    pub out_of_order_packets: usize,
    pub late_packets: usize,
    pub dropped_wrong_payload_packets: usize,
    pub dropped_ssrc_packets: usize,
    pub decode_errors: usize,
    pub streams_started: usize,
    pub streams_ended: usize,
    pub output_frames: usize,
    pub output_duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PacketInsertResult {
    Inserted,
    Duplicate,
    TooOld,
}

pub fn parse_rtp_audio_packet(packet: &[u8]) -> Option<RtpAudioPacket> {
    if packet.len() < RTP_HEADER_LEN || (packet[0] >> 6) != 2 {
        return None;
    }
    let csrc_count = (packet[0] & 0x0f) as usize;
    let has_extension = (packet[0] & 0x10) != 0;
    let mut header_len = RTP_HEADER_LEN + (csrc_count * 4);
    if packet.len() < header_len {
        return None;
    }
    if has_extension {
        if packet.len() < header_len + 4 {
            return None;
        }
        let extension_len =
            u16::from_be_bytes([packet[header_len + 2], packet[header_len + 3]]) as usize * 4;
        header_len += 4 + extension_len;
        if packet.len() < header_len {
            return None;
        }
    }
    if packet.len() <= header_len {
        return None;
    }
    Some(RtpAudioPacket {
        sequence: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        payload_type: packet[1] & 0x7f,
        payload: packet[header_len..].to_vec(),
    })
}

pub fn recover_ordered_packets_to_pcm(
    packets: &[RtpAudioPacket],
    channels: u8,
    payload_type: u8,
    use_fec: bool,
) -> Result<(Vec<i16>, PlaybackRecoveryStats)> {
    let channels = channels.clamp(1, 2);
    let mut stats = PlaybackRecoveryStats {
        mode: "recover-offline",
        sample_rate: SAMPLE_RATE,
        channels,
        ..PlaybackRecoveryStats::default()
    };
    let mut streams: HashMap<u32, OfflineStream> = HashMap::new();
    for packet in packets {
        stats.received_packets += 1;
        if packet.payload_type != payload_type {
            stats.dropped_wrong_payload_packets += 1;
            continue;
        }
        let stream = streams
            .entry(packet.ssrc)
            .or_insert_with(|| OfflineStream::new(packet.sequence));
        match stream.insert(packet.clone()) {
            PacketInsertResult::Inserted => {}
            PacketInsertResult::Duplicate => stats.duplicate_packets += 1,
            PacketInsertResult::TooOld => stats.out_of_order_packets += 1,
        }
    }
    if streams.is_empty() {
        return Ok((Vec::new(), stats));
    }
    if streams.len() > 1 {
        // Offline recovery harnesses currently score one RTP audio stream at a time.
        // Keep the newest/longest stream rather than mixing so packet-loss metrics are unambiguous.
        let keep = streams
            .iter()
            .max_by_key(|(_ssrc, stream)| stream.packets.len())
            .map(|(ssrc, _)| *ssrc)
            .unwrap();
        let dropped: usize = streams
            .iter()
            .filter(|(ssrc, _)| **ssrc != keep)
            .map(|(_, stream)| stream.packets.len())
            .sum();
        stats.dropped_ssrc_packets += dropped;
        streams.retain(|ssrc, _| *ssrc == keep);
    }
    let (_ssrc, stream) = streams.into_iter().next().unwrap();
    let mut decoder = OpusStreamDecoder::new(channels)?;
    let mut pcm = Vec::new();
    let mut expected = stream.first_sequence;
    let last = stream.last_sequence;
    let mut missing_run = 0usize;
    loop {
        if let Some(packet) = stream.packets.get(&expected) {
            decoder.decode_payload(&packet.payload, &mut pcm, &mut stats)?;
            missing_run = 0;
        } else {
            missing_run += 1;
            stats.missing_packets += 1;
            stats.sequence_gap_events += usize::from(missing_run == 1);
            stats.max_consecutive_missing_packets =
                stats.max_consecutive_missing_packets.max(missing_run);
            if use_fec {
                let next = expected.wrapping_add(1);
                if let Some(next_packet) = stream.packets.get(&next) {
                    decoder.decode_fec_or_plc(&next_packet.payload, &mut pcm, &mut stats)?;
                } else {
                    decoder.decode_plc(&mut pcm, &mut stats)?;
                }
            } else {
                decoder.decode_plc(&mut pcm, &mut stats)?;
            }
        }
        if expected == last {
            break;
        }
        expected = expected.wrapping_add(1);
    }
    stats.output_frames = if channels == 0 {
        0
    } else {
        pcm.len() / channels as usize
    };
    stats.output_duration_ms = (stats.output_frames as u64 * 1000) / SAMPLE_RATE as u64;
    Ok((pcm, stats))
}

pub fn play_rtp(options: PlaybackOptions<'_>) -> Result<PlaybackRecoveryStats> {
    let channels = options.channels.clamp(1, 2);
    let socket = UdpSocket::bind(options.rtp_addr)
        .with_context(|| format!("bind playback RTP socket {}", options.rtp_addr))?;
    socket
        .set_nonblocking(true)
        .context("set playback RTP socket nonblocking")?;
    if let Some(path) = options.ready_file {
        std::fs::write(path, b"ready\n")
            .with_context(|| format!("write playback ready file {}", path.display()))?;
    }
    let mut sink = create_sink(&options.output, channels)?;
    let started = Instant::now();
    let deadline = options
        .duration_ms
        .map(|ms| started + Duration::from_millis(ms));
    let tick = Duration::from_millis(FRAME_MS as u64);
    let mut next_tick: Option<Instant> = None;
    let mut streams: HashMap<u32, LiveStream> = HashMap::new();
    let mut stats = PlaybackRecoveryStats {
        mode: "play-rtp",
        sample_rate: SAMPLE_RATE,
        channels,
        ..PlaybackRecoveryStats::default()
    };
    let mut recv_buffer = vec![0u8; MAX_PACKET_SIZE];

    loop {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            break;
        }
        if streams.is_empty() {
            match socket.recv_from(&mut recv_buffer) {
                Ok((len, _addr)) => {
                    ingest_live_packet(
                        &recv_buffer[..len],
                        channels,
                        options.payload_type,
                        options.jitter_ms,
                        &mut streams,
                        &mut stats,
                    )?;
                    if next_tick.is_none() {
                        next_tick = Some(Instant::now() + Duration::from_millis(options.jitter_ms));
                    }
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => return Err(error).context("receive playback RTP packet"),
            }
        }

        let target =
            next_tick.unwrap_or_else(|| Instant::now() + Duration::from_millis(options.jitter_ms));
        drain_until(
            &socket,
            &mut recv_buffer,
            target,
            channels,
            options.payload_type,
            options.jitter_ms,
            &mut streams,
            &mut stats,
        )?;

        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }

        let mut mix = vec![0.0f32; FRAME_SAMPLES * channels as usize];
        let mut active_streams = 0usize;
        let mut ended = Vec::new();
        for (ssrc, stream) in streams.iter_mut() {
            if target < stream.start_at {
                active_streams += 1;
                continue;
            }
            match stream.decode_next_frame(
                target,
                options.idle_timeout_ms,
                options.max_plc_packets,
                options.use_fec,
                &mut stats,
            )? {
                Some(frame) => {
                    active_streams += 1;
                    mix_frame(&mut mix, &frame);
                }
                None => ended.push(*ssrc),
            }
        }
        for ssrc in ended {
            streams.remove(&ssrc);
            stats.streams_ended += 1;
        }
        if active_streams > 0 {
            sink.write_frame(&mix)?;
            stats.output_frames += FRAME_SAMPLES;
        }
        next_tick = if streams.is_empty() {
            None
        } else {
            Some(target + tick)
        };
    }

    stats.output_duration_ms = (stats.output_frames as u64 * 1000) / SAMPLE_RATE as u64;
    sink.finish()?;
    if let Some(path) = options.stats_json {
        std::fs::write(path, serde_json::to_vec_pretty(&stats)?)
            .with_context(|| format!("write playback stats {}", path.display()))?;
    }
    eprintln!(
        "discord-voice-engine play-rtp: received {} packet(s), decoded {}, concealed {}, missing {}",
        stats.received_packets,
        stats.normal_packets,
        stats.concealed_packets,
        stats.missing_packets
    );
    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
fn drain_until(
    socket: &UdpSocket,
    recv_buffer: &mut [u8],
    deadline: Instant,
    channels: u8,
    payload_type: u8,
    jitter_ms: u64,
    streams: &mut HashMap<u32, LiveStream>,
    stats: &mut PlaybackRecoveryStats,
) -> Result<()> {
    loop {
        match socket.recv_from(recv_buffer) {
            Ok((len, _addr)) => ingest_live_packet(
                &recv_buffer[..len],
                channels,
                payload_type,
                jitter_ms,
                streams,
                stats,
            )?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error).context("receive playback RTP packet"),
        }
    }
    Ok(())
}

fn ingest_live_packet(
    packet: &[u8],
    channels: u8,
    payload_type: u8,
    jitter_ms: u64,
    streams: &mut HashMap<u32, LiveStream>,
    stats: &mut PlaybackRecoveryStats,
) -> Result<()> {
    let Some(packet) = parse_rtp_audio_packet(packet) else {
        stats.dropped_wrong_payload_packets += 1;
        return Ok(());
    };
    stats.received_packets += 1;
    if packet.payload_type != payload_type {
        stats.dropped_wrong_payload_packets += 1;
        return Ok(());
    }
    let now = Instant::now();
    let stream = match streams.get_mut(&packet.ssrc) {
        Some(stream) => stream,
        None => {
            stats.streams_started += 1;
            streams.insert(
                packet.ssrc,
                LiveStream::new(
                    channels,
                    packet.sequence,
                    now + Duration::from_millis(jitter_ms),
                )?,
            );
            streams.get_mut(&packet.ssrc).unwrap()
        }
    };
    match stream.insert(packet, now) {
        PacketInsertResult::Inserted => {}
        PacketInsertResult::Duplicate => stats.duplicate_packets += 1,
        PacketInsertResult::TooOld => stats.late_packets += 1,
    }
    Ok(())
}

fn create_sink(output: &PlaybackOutput<'_>, channels: u8) -> Result<Box<dyn PcmSink>> {
    match output {
        PlaybackOutput::PipeWire => {
            ChildPcmSink::pipewire(channels).map(|sink| Box::new(sink) as Box<dyn PcmSink>)
        }
        PlaybackOutput::Pulse => {
            ChildPcmSink::pulse(channels).map(|sink| Box::new(sink) as Box<dyn PcmSink>)
        }
        PlaybackOutput::Wav(path) => Ok(Box::new(WavPcmSink::new(path, channels))),
        PlaybackOutput::Null => Ok(Box::new(NullPcmSink)),
    }
}

trait PcmSink {
    fn write_frame(&mut self, frame: &[f32]) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}

struct NullPcmSink;

impl PcmSink for NullPcmSink {
    fn write_frame(&mut self, _frame: &[f32]) -> Result<()> {
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

struct WavPcmSink {
    path: std::path::PathBuf,
    channels: u8,
    samples: Vec<i16>,
}

impl WavPcmSink {
    fn new(path: &Path, channels: u8) -> Self {
        Self {
            path: path.to_path_buf(),
            channels,
            samples: Vec::new(),
        }
    }
}

impl PcmSink for WavPcmSink {
    fn write_frame(&mut self, frame: &[f32]) -> Result<()> {
        self.samples.extend(frame.iter().copied().map(float_to_i16));
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        write_wav_i16(&self.path, &self.samples, self.channels, SAMPLE_RATE)
            .with_context(|| format!("write playback WAV {}", self.path.display()))
    }
}

struct ChildPcmSink {
    child: Child,
    stdin: ChildStdin,
}

impl ChildPcmSink {
    fn pipewire(channels: u8) -> Result<Self> {
        let mut child = Command::new("pw-cat")
            .args([
                "--playback",
                "--raw",
                "--rate",
                &SAMPLE_RATE.to_string(),
                "--channels",
                &channels.to_string(),
                "--format",
                "s16",
                "--latency",
                "50ms",
                "-",
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start pw-cat playback sink")?;
        let stdin = child.stdin.take().context("pw-cat stdin was not piped")?;
        Ok(Self { child, stdin })
    }

    fn pulse(channels: u8) -> Result<Self> {
        let mut child = Command::new("pacat")
            .args([
                "--playback",
                "--raw",
                "--rate=48000",
                "--format=s16le",
                &format!("--channels={channels}"),
                "--latency-msec=50",
                "--process-time-msec=20",
                "--client-name=discord-voice-engine",
                "--stream-name=Discord voice playback",
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start pacat playback sink")?;
        let stdin = child.stdin.take().context("pacat stdin was not piped")?;
        Ok(Self { child, stdin })
    }
}

impl PcmSink for ChildPcmSink {
    fn write_frame(&mut self, frame: &[f32]) -> Result<()> {
        let mut bytes = Vec::with_capacity(frame.len() * 2);
        for sample in frame {
            bytes.extend_from_slice(&float_to_i16(*sample).to_le_bytes());
        }
        self.stdin
            .write_all(&bytes)
            .context("write PCM playback frame")
    }

    fn finish(&mut self) -> Result<()> {
        self.stdin.flush().ok();
        Ok(())
    }
}

impl Drop for ChildPcmSink {
    fn drop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct OfflineStream {
    first_sequence: u16,
    last_sequence: u16,
    packets: BTreeMap<u16, RtpAudioPacket>,
}

impl OfflineStream {
    fn new(first_sequence: u16) -> Self {
        Self {
            first_sequence,
            last_sequence: first_sequence,
            packets: BTreeMap::new(),
        }
    }

    fn insert(&mut self, packet: RtpAudioPacket) -> PacketInsertResult {
        if self.packets.contains_key(&packet.sequence) {
            return PacketInsertResult::Duplicate;
        }
        if sequence_distance(packet.sequence, self.first_sequence) >= 0x8000 {
            return PacketInsertResult::TooOld;
        }
        if sequence_distance(packet.sequence, self.last_sequence) < 0x8000 {
            self.last_sequence = packet.sequence;
        }
        self.packets.insert(packet.sequence, packet);
        PacketInsertResult::Inserted
    }
}

struct LiveStream {
    decoder: OpusStreamDecoder,
    expected_sequence: u16,
    buffer: BTreeMap<u16, RtpAudioPacket>,
    // Opus RTP packets are not guaranteed to decode to exactly one 20 ms
    // playback tick. Discord commonly sends 20 ms packets, but 40/60 ms packets
    // are legal and do show up in the wild. Keep decoded PCM here and drain it
    // on the fixed local output cadence so longer packets are not truncated.
    pending_samples: VecDeque<f32>,
    start_at: Instant,
    last_receive: Instant,
    consecutive_plc: usize,
}

impl LiveStream {
    fn new(channels: u8, first_sequence: u16, start_at: Instant) -> Result<Self> {
        Ok(Self {
            decoder: OpusStreamDecoder::new(channels)?,
            expected_sequence: first_sequence,
            buffer: BTreeMap::new(),
            pending_samples: VecDeque::new(),
            start_at,
            last_receive: Instant::now(),
            consecutive_plc: 0,
        })
    }

    fn insert(&mut self, packet: RtpAudioPacket, arrival: Instant) -> PacketInsertResult {
        let delta = sequence_distance(packet.sequence, self.expected_sequence);
        if delta >= 0x8000 {
            return PacketInsertResult::TooOld;
        }
        if self.buffer.contains_key(&packet.sequence) {
            return PacketInsertResult::Duplicate;
        }
        self.last_receive = arrival;
        self.buffer.insert(packet.sequence, packet);
        PacketInsertResult::Inserted
    }

    fn decode_next_frame(
        &mut self,
        now: Instant,
        idle_timeout_ms: u64,
        max_plc_packets: usize,
        use_fec: bool,
        stats: &mut PlaybackRecoveryStats,
    ) -> Result<Option<Vec<f32>>> {
        let frame_samples = FRAME_SAMPLES * self.decoder.channels as usize;
        while self.pending_samples.len() < frame_samples {
            if let Some(packet) = self.buffer.remove(&self.expected_sequence) {
                let mut decoded = Vec::new();
                self.decoder
                    .decode_payload_f32(&packet.payload, &mut decoded, stats)?;
                self.pending_samples.extend(decoded);
                self.expected_sequence = self.expected_sequence.wrapping_add(1);
                self.consecutive_plc = 0;
                continue;
            }

            if self.buffer.is_empty()
                && now.duration_since(self.last_receive) >= Duration::from_millis(idle_timeout_ms)
                && self.consecutive_plc > 0
                && self.pending_samples.is_empty()
            {
                return Ok(None);
            }

            self.consecutive_plc += 1;
            if self.consecutive_plc > max_plc_packets && self.buffer.is_empty() {
                if self.pending_samples.is_empty() {
                    return Ok(None);
                }
                break;
            }
            stats.missing_packets += 1;
            stats.sequence_gap_events += usize::from(self.consecutive_plc == 1);
            stats.max_consecutive_missing_packets = stats
                .max_consecutive_missing_packets
                .max(self.consecutive_plc);

            let mut concealed = Vec::new();
            if use_fec {
                let next = self.expected_sequence.wrapping_add(1);
                if let Some(next_packet) = self.buffer.get(&next) {
                    self.decoder.decode_fec_or_plc_f32(
                        &next_packet.payload,
                        &mut concealed,
                        stats,
                    )?;
                } else {
                    self.decoder.decode_plc_f32(&mut concealed, stats)?;
                }
            } else {
                self.decoder.decode_plc_f32(&mut concealed, stats)?;
            }
            self.pending_samples.extend(concealed);
            self.expected_sequence = self.expected_sequence.wrapping_add(1);
        }

        if self.pending_samples.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.pop_output_frame(frame_samples)))
    }

    fn pop_output_frame(&mut self, frame_samples: usize) -> Vec<f32> {
        let mut frame = Vec::with_capacity(frame_samples);
        for _ in 0..frame_samples {
            match self.pending_samples.pop_front() {
                Some(sample) => frame.push(sample),
                None => frame.push(0.0),
            }
        }
        frame
    }
}

struct OpusStreamDecoder {
    decoder: Decoder,
    channels: u8,
}

impl OpusStreamDecoder {
    fn new(channels: u8) -> Result<Self> {
        let channels = channels.clamp(1, 2);
        let opus_channels = if channels == 1 {
            Channels::Mono
        } else {
            Channels::Stereo
        };
        Ok(Self {
            decoder: Decoder::new(SAMPLE_RATE, opus_channels)
                .context("create Opus playback decoder")?,
            channels,
        })
    }

    fn decode_payload(
        &mut self,
        payload: &[u8],
        pcm: &mut Vec<i16>,
        stats: &mut PlaybackRecoveryStats,
    ) -> Result<()> {
        let mut frame = Vec::new();
        self.decode_payload_f32(payload, &mut frame, stats)?;
        pcm.extend(frame.into_iter().map(float_to_i16));
        Ok(())
    }

    fn decode_fec_or_plc(
        &mut self,
        payload: &[u8],
        pcm: &mut Vec<i16>,
        stats: &mut PlaybackRecoveryStats,
    ) -> Result<()> {
        let mut frame = Vec::new();
        self.decode_fec_or_plc_f32(payload, &mut frame, stats)?;
        pcm.extend(frame.into_iter().map(float_to_i16));
        Ok(())
    }

    fn decode_plc(&mut self, pcm: &mut Vec<i16>, stats: &mut PlaybackRecoveryStats) -> Result<()> {
        let mut frame = Vec::new();
        self.decode_plc_f32(&mut frame, stats)?;
        pcm.extend(frame.into_iter().map(float_to_i16));
        Ok(())
    }

    fn decode_payload_f32(
        &mut self,
        payload: &[u8],
        frame: &mut Vec<f32>,
        stats: &mut PlaybackRecoveryStats,
    ) -> Result<()> {
        let mut out = vec![0.0f32; FRAME_SAMPLES * self.channels as usize * 6];
        match self.decoder.decode_float(payload, &mut out, false) {
            Ok(samples_per_channel) => {
                let count = samples_per_channel * self.channels as usize;
                frame.extend_from_slice(&out[..count]);
                stats.normal_packets += 1;
                stats.decoded_packets += 1;
                Ok(())
            }
            Err(error) => {
                stats.decode_errors += 1;
                Err(error).context("decode Opus playback packet")
            }
        }
    }

    fn decode_fec_or_plc_f32(
        &mut self,
        payload: &[u8],
        frame: &mut Vec<f32>,
        stats: &mut PlaybackRecoveryStats,
    ) -> Result<()> {
        let mut out = vec![0.0f32; FRAME_SAMPLES * self.channels as usize];
        match self.decoder.decode_float(payload, &mut out, true) {
            Ok(samples_per_channel) => {
                let count = samples_per_channel * self.channels as usize;
                frame.extend_from_slice(&out[..count]);
                stats.fec_attempts += 1;
                stats.concealed_packets += 1;
                stats.decoded_packets += 1;
                Ok(())
            }
            Err(error) => {
                stats.decode_errors += 1;
                Err(error).context("decode Opus FEC/PLC playback packet")
            }
        }
    }

    fn decode_plc_f32(
        &mut self,
        frame: &mut Vec<f32>,
        stats: &mut PlaybackRecoveryStats,
    ) -> Result<()> {
        let mut out = vec![0.0f32; FRAME_SAMPLES * self.channels as usize];
        match self.decoder.decode_float(&[], &mut out, false) {
            Ok(samples_per_channel) => {
                let count = samples_per_channel * self.channels as usize;
                frame.extend_from_slice(&out[..count]);
                stats.concealed_packets += 1;
                stats.decoded_packets += 1;
                Ok(())
            }
            Err(error) => {
                stats.decode_errors += 1;
                Err(error).context("decode Opus PLC playback frame")
            }
        }
    }
}

fn mix_frame(mix: &mut [f32], frame: &[f32]) {
    for (dst, src) in mix.iter_mut().zip(frame.iter()) {
        *dst = (*dst + *src).clamp(-1.0, 1.0);
    }
}

fn sequence_distance(sequence: u16, previous: u16) -> u16 {
    sequence.wrapping_sub(previous)
}

pub fn write_recovered_wav(
    path: &Path,
    packets: &[RtpAudioPacket],
    channels: u8,
    payload_type: u8,
) -> Result<PlaybackRecoveryStats> {
    let (pcm, stats) = recover_ordered_packets_to_pcm(packets, channels, payload_type, false)?;
    write_wav_i16(path, &pcm, channels, SAMPLE_RATE)
        .with_context(|| format!("write recovered playback WAV {}", path.display()))?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encode_float_frames;
    use crate::rtp::build_rtp_packet;
    use crate::{AudioMode, EngineConfig};
    use opus::{Application, Bitrate, Channels, Encoder};

    #[test]
    fn parses_plain_rtp_packet() {
        let packet = build_rtp_packet(120, 0x1234, 0x1020_3040, 0x5566_7788, &[1, 2, 3]);
        let parsed = parse_rtp_audio_packet(&packet).unwrap();
        assert_eq!(parsed.payload_type, 120);
        assert_eq!(parsed.sequence, 0x1234);
        assert_eq!(parsed.timestamp, 0x1020_3040);
        assert_eq!(parsed.ssrc, 0x5566_7788);
        assert_eq!(parsed.payload, vec![1, 2, 3]);
    }

    #[test]
    fn opus_plc_conceals_missing_music_frame_without_shortening_stream() {
        let config = EngineConfig::new(AudioMode::Music, 2, Some(160_000), DEFAULT_PAYLOAD_TYPE, 7);
        let frames = 80usize;
        let mut samples = Vec::with_capacity(frames * FRAME_SAMPLES * 2);
        for n in 0..frames * FRAME_SAMPLES {
            let t = n as f32 / SAMPLE_RATE as f32;
            let sample = ((t * 440.0 * std::f32::consts::TAU).sin() * 0.25)
                + ((t * 880.0 * std::f32::consts::TAU).sin() * 0.05);
            samples.push(sample);
            samples.push(sample);
        }
        let payloads = encode_float_frames(&samples, &config).unwrap();
        let mut packets = Vec::new();
        for (index, payload) in payloads.into_iter().enumerate() {
            if matches!(index, 20 | 21 | 55) {
                continue;
            }
            packets.push(RtpAudioPacket {
                sequence: index as u16,
                timestamp: (index as u32) * crate::RTP_CLOCK_INCREMENT,
                ssrc: 99,
                payload_type: DEFAULT_PAYLOAD_TYPE,
                payload,
            });
        }
        let (pcm, stats) =
            recover_ordered_packets_to_pcm(&packets, 2, DEFAULT_PAYLOAD_TYPE, false).unwrap();
        assert_eq!(stats.missing_packets, 3);
        assert_eq!(stats.concealed_packets, 3);
        assert_eq!(stats.sequence_gap_events, 2);
        assert_eq!(stats.max_consecutive_missing_packets, 2);
        assert_eq!(stats.decoded_packets, frames);
        assert_eq!(pcm.len(), frames * FRAME_SAMPLES * 2);

        let missing_start = 20 * FRAME_SAMPLES * 2;
        let missing_end = 22 * FRAME_SAMPLES * 2;
        let concealed_energy: i64 = pcm[missing_start..missing_end]
            .iter()
            .map(|sample| i64::from(*sample).abs())
            .sum();
        assert!(
            concealed_energy > 1_000,
            "PLC should synthesize non-silent continuity"
        );
    }

    #[test]
    fn live_playback_splits_multi_frame_opus_packets_across_ticks() {
        let channels = 2u8;
        let packet_frames = 3usize;
        let mut samples = Vec::with_capacity(packet_frames * FRAME_SAMPLES * channels as usize);
        for n in 0..packet_frames * FRAME_SAMPLES {
            let t = n as f32 / SAMPLE_RATE as f32;
            let sample = ((t * 330.0 * std::f32::consts::TAU).sin() * 0.20)
                + ((t * 660.0 * std::f32::consts::TAU).sin() * 0.05);
            samples.push(sample);
            samples.push(sample);
        }

        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio).unwrap();
        encoder.set_bitrate(Bitrate::Bits(160_000)).unwrap();
        let mut encoded = vec![0u8; 4096];
        let len = encoder.encode_float(&samples, &mut encoded).unwrap();
        encoded.truncate(len);

        let start_at = Instant::now();
        let mut stream = LiveStream::new(channels, 77, start_at).unwrap();
        let arrival = Instant::now();
        assert_eq!(
            stream.insert(
                RtpAudioPacket {
                    sequence: 77,
                    timestamp: 0,
                    ssrc: 99,
                    payload_type: DEFAULT_PAYLOAD_TYPE,
                    payload: encoded,
                },
                arrival,
            ),
            PacketInsertResult::Inserted
        );

        let mut stats = PlaybackRecoveryStats::default();
        for tick in 0..packet_frames {
            let frame = stream
                .decode_next_frame(
                    arrival + Duration::from_millis((tick as u64) * FRAME_MS as u64),
                    1_000,
                    10,
                    false,
                    &mut stats,
                )
                .unwrap()
                .unwrap();
            assert_eq!(frame.len(), FRAME_SAMPLES * channels as usize);
            assert!(
                frame.iter().any(|sample| sample.abs() > 0.001),
                "decoded packet chunk {tick} should contain real audio, not silence/PLC"
            );
        }

        assert_eq!(stats.normal_packets, 1);
        assert_eq!(stats.concealed_packets, 0);
        assert_eq!(stats.missing_packets, 0);
        assert_eq!(stream.pending_samples.len(), 0);
    }
}
