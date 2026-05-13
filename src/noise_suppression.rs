use nnnoiseless::DenoiseState;

use crate::audio::{float_to_i16, interleaved_i16_to_mono};
use crate::{FRAME_SAMPLES, SAMPLE_RATE};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NoiseSuppressionMode {
    Off,
    Simple,
}

impl NoiseSuppressionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Simple => "simple",
        }
    }
}

impl Default for NoiseSuppressionMode {
    fn default() -> Self {
        Self::Off
    }
}

pub fn parse_noise_suppression_mode(value: &str) -> Option<NoiseSuppressionMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "0" | "false" => Some(NoiseSuppressionMode::Off),
        "simple" | "rnnoise" | "on" | "1" | "true" => Some(NoiseSuppressionMode::Simple),
        _ => None,
    }
}

pub struct NoiseSuppressor {
    mode: NoiseSuppressionMode,
    rnnoise: Option<RnnoiseSuppressor>,
}

impl NoiseSuppressor {
    pub fn new(mode: NoiseSuppressionMode) -> Self {
        let rnnoise = match mode {
            NoiseSuppressionMode::Off => None,
            NoiseSuppressionMode::Simple => Some(RnnoiseSuppressor::new()),
        };
        Self { mode, rnnoise }
    }

    pub fn mode(&self) -> NoiseSuppressionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: NoiseSuppressionMode) {
        if self.mode == mode {
            return;
        }
        *self = Self::new(mode);
    }

    pub fn process_interleaved_i16_frame(&mut self, pcm: &mut [i16], channels: u8) {
        match self.mode {
            NoiseSuppressionMode::Off => {}
            NoiseSuppressionMode::Simple => {
                if let Some(rnnoise) = &mut self.rnnoise {
                    rnnoise.process_interleaved_i16_frame(pcm, channels);
                }
            }
        }
    }
}

struct RnnoiseSuppressor {
    state: Box<DenoiseState<'static>>,
    input: [f32; DenoiseState::FRAME_SIZE],
    output: [f32; DenoiseState::FRAME_SIZE],
}

impl RnnoiseSuppressor {
    fn new() -> Self {
        debug_assert_eq!(SAMPLE_RATE, 48_000);
        debug_assert_eq!(FRAME_SAMPLES % DenoiseState::FRAME_SIZE, 0);
        Self {
            state: DenoiseState::new(),
            input: [0.0; DenoiseState::FRAME_SIZE],
            output: [0.0; DenoiseState::FRAME_SIZE],
        }
    }

    fn process_interleaved_i16_frame(&mut self, pcm: &mut [i16], channels: u8) {
        if channels == 0 || pcm.is_empty() {
            return;
        }
        let channels = channels as usize;
        if pcm.len() != FRAME_SAMPLES * channels {
            return;
        }

        let mono = interleaved_i16_to_mono(pcm, channels as u8);
        let mut cleaned = Vec::with_capacity(mono.len());
        for chunk in mono.chunks_exact(DenoiseState::FRAME_SIZE) {
            for (dst, &src) in self.input.iter_mut().zip(chunk.iter()) {
                *dst = src as f32;
            }
            self.state.process_frame(&mut self.output, &self.input);
            cleaned.extend(
                self.output
                    .iter()
                    .map(|&sample| float_to_i16(sample / i16::MAX as f32)),
            );
        }

        if channels == 1 {
            pcm.copy_from_slice(&cleaned);
        } else {
            for (frame, &sample) in pcm.chunks_exact_mut(channels).zip(cleaned.iter()) {
                for channel_sample in frame {
                    *channel_sample = sample;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_noise_suppression_modes() {
        assert_eq!(
            parse_noise_suppression_mode("off"),
            Some(NoiseSuppressionMode::Off)
        );
        assert_eq!(
            parse_noise_suppression_mode("simple"),
            Some(NoiseSuppressionMode::Simple)
        );
        assert_eq!(parse_noise_suppression_mode("wat"), None);
    }

    #[test]
    fn off_mode_leaves_pcm_unchanged() {
        let mut ns = NoiseSuppressor::new(NoiseSuppressionMode::Off);
        let mut pcm = vec![100i16; FRAME_SAMPLES * 2];
        let before = pcm.clone();
        ns.process_interleaved_i16_frame(&mut pcm, 2);
        assert_eq!(pcm, before);
    }

    #[test]
    fn simple_mode_outputs_valid_stereo_frame() {
        let mut ns = NoiseSuppressor::new(NoiseSuppressionMode::Simple);
        let mut pcm = vec![0i16; FRAME_SAMPLES * 2];
        for (i, sample) in pcm.iter_mut().enumerate() {
            *sample = ((i as f32 * 0.01).sin() * 1000.0) as i16;
        }
        ns.process_interleaved_i16_frame(&mut pcm, 2);
        assert_eq!(pcm.len(), FRAME_SAMPLES * 2);
    }
}
