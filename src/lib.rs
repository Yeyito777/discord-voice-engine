pub mod audio;
pub mod encode;
pub mod file_input;
pub mod noise_suppression;
pub mod parent_watchdog;
pub mod playback;
pub mod playback_c;
pub mod pulse_capture;
pub mod rtp;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_MS: u32 = 20;
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;
pub const DEFAULT_PAYLOAD_TYPE: u8 = 120;
pub const RTP_CLOCK_INCREMENT: u32 = FRAME_SAMPLES as u32;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AudioMode {
    Voice,
    Music,
}

impl AudioMode {
    pub fn default_bitrate(self, channels: u8) -> i32 {
        match self {
            // High enough to avoid the low-bitrate, processed sound we were getting from
            // ffmpeg's default VoIP settings while still being reasonable for Discord voice.
            AudioMode::Voice => {
                if channels >= 2 {
                    96_000
                } else {
                    64_000
                }
            }
            // Music/file playback should use a fullband audio profile. 192k stereo is a
            // conservative high-quality default for Opus without asking Discord to carry
            // silly lossless-sized packets.
            AudioMode::Music => {
                if channels >= 2 {
                    192_000
                } else {
                    128_000
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub sequence: u16,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub mode: AudioMode,
    pub channels: u8,
    pub bitrate: i32,
    pub payload_type: u8,
    pub ssrc: u32,
}

impl EngineConfig {
    pub fn new(
        mode: AudioMode,
        channels: u8,
        bitrate: Option<i32>,
        payload_type: u8,
        ssrc: u32,
    ) -> Self {
        let channels = channels.clamp(1, 2);
        Self {
            mode,
            channels,
            bitrate: bitrate.unwrap_or_else(|| mode.default_bitrate(channels)),
            payload_type,
            ssrc,
        }
    }
}
