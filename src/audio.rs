use anyhow::{Result, bail};

use crate::{FRAME_SAMPLES, SAMPLE_RATE};

#[derive(Debug, Clone)]
pub struct PcmAudio {
    /// Interleaved floating-point PCM in the range expected by libopus (-1.0..=1.0).
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u8,
}

impl PcmAudio {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            (self.frames() as u64 * 1000) / self.sample_rate as u64
        }
    }
}

pub fn convert_channels(
    input: &[f32],
    input_channels: u8,
    output_channels: u8,
) -> Result<Vec<f32>> {
    let input_channels = input_channels as usize;
    let output_channels = output_channels as usize;
    if !(1..=8).contains(&input_channels) {
        bail!("unsupported input channel count: {input_channels}");
    }
    if !(1..=2).contains(&output_channels) {
        bail!("unsupported output channel count: {output_channels}");
    }
    if !input.len().is_multiple_of(input_channels) {
        bail!("input sample count is not divisible by channel count");
    }
    if input_channels == output_channels {
        return Ok(input.to_vec());
    }

    let frames = input.len() / input_channels;
    let mut output = Vec::with_capacity(frames * output_channels);
    for frame in input.chunks_exact(input_channels) {
        match output_channels {
            1 => {
                let sum: f32 = frame.iter().copied().sum();
                output.push(sum / input_channels as f32);
            }
            2 => {
                if input_channels == 1 {
                    output.push(frame[0]);
                    output.push(frame[0]);
                } else {
                    // For surround-ish sources, keep the main left/right pair.  This avoids
                    // surprising phase/level artifacts from naively averaging every channel.
                    output.push(frame[0]);
                    output.push(frame[1]);
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(output)
}

pub fn resample_to_48k(input: PcmAudio) -> Result<PcmAudio> {
    if input.sample_rate == SAMPLE_RATE {
        return Ok(input);
    }
    use rubato::audioadapter_buffers::direct::InterleavedSlice;
    use rubato::{Fft, FixedSync, Resampler};

    let channels = input.channels as usize;
    let input_frames = input.frames();
    if input_frames == 0 {
        return Ok(PcmAudio {
            samples: Vec::new(),
            sample_rate: SAMPLE_RATE,
            channels: input.channels,
        });
    }

    let mut resampler = Fft::<f32>::new(
        input.sample_rate as usize,
        SAMPLE_RATE as usize,
        1024,
        1,
        channels,
        FixedSync::Both,
    )?;
    let input_adapter = InterleavedSlice::new(&input.samples, channels, input_frames)?;
    let expected_output_frames = ((input_frames as u128 * SAMPLE_RATE as u128)
        + (input.sample_rate as u128 / 2))
        / input.sample_rate as u128;
    let expected_output_frames = expected_output_frames as usize;
    let output_capacity = resampler
        .process_all_needed_output_len(input_frames)
        .max(expected_output_frames);
    let mut output = vec![0.0f32; output_capacity * channels];
    let mut output_adapter = InterleavedSlice::new_mut(&mut output, channels, output_capacity)?;
    let (_input_used, output_frames) = resampler.process_all_into_buffer(
        &input_adapter,
        &mut output_adapter,
        input_frames,
        None,
    )?;
    let wanted_frames = output_frames.min(expected_output_frames);
    output.truncate(wanted_frames * channels);
    if wanted_frames < expected_output_frames {
        output.resize(expected_output_frames * channels, 0.0);
    }
    Ok(PcmAudio {
        samples: output,
        sample_rate: SAMPLE_RATE,
        channels: input.channels,
    })
}

pub fn pad_to_full_opus_frames(samples: &mut Vec<f32>, channels: u8) {
    let frame_len = FRAME_SAMPLES * channels as usize;
    let remainder = samples.len() % frame_len;
    if remainder != 0 {
        samples.resize(samples.len() + frame_len - remainder, 0.0);
    }
}

pub fn float_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    if clamped >= 0.0 {
        (clamped * i16::MAX as f32).round() as i16
    } else {
        (clamped * 32768.0).round() as i16
    }
}

pub fn write_wav_f32(
    path: &std::path::Path,
    samples: &[f32],
    channels: u8,
    sample_rate: u32,
) -> Result<()> {
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for sample in samples {
        writer.write_sample(float_to_i16(*sample))?;
    }
    writer.finalize()?;
    Ok(())
}

pub fn write_wav_i16(
    path: &std::path::Path,
    samples: &[i16],
    channels: u8,
    sample_rate: u32,
) -> Result<()> {
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for sample in samples {
        writer.write_sample(*sample)?;
    }
    writer.finalize()?;
    Ok(())
}

pub fn interleaved_i16_to_mono(frame: &[i16], channels: u8) -> Vec<i16> {
    let channels = channels as usize;
    if channels == 1 {
        return frame.to_vec();
    }
    frame
        .chunks_exact(channels)
        .map(|samples| {
            let sum: i32 = samples.iter().map(|sample| *sample as i32).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_mono_to_stereo_by_duplication() {
        assert_eq!(
            convert_channels(&[0.1, -0.2], 1, 2).unwrap(),
            vec![0.1, 0.1, -0.2, -0.2]
        );
    }

    #[test]
    fn converts_stereo_to_mono_by_averaging() {
        assert_eq!(
            convert_channels(&[0.4, 0.2, -0.2, -0.4], 2, 1).unwrap(),
            vec![0.3, -0.3]
        );
    }

    #[test]
    fn pads_to_full_opus_frames() {
        let mut samples = vec![0.0; 7];
        pad_to_full_opus_frames(&mut samples, 1);
        assert_eq!(samples.len(), FRAME_SAMPLES);
    }

    #[test]
    fn makes_stereo_meter_mono() {
        assert_eq!(
            interleaved_i16_to_mono(&[100, 300, -100, -300], 2),
            vec![200, -200]
        );
    }
}
