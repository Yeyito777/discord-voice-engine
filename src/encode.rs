use anyhow::{Context, Result};
use opus::{Application, Bandwidth, Bitrate, Channels, Decoder, Encoder, Signal};

use crate::audio::{float_to_i16, write_wav_i16};
use crate::{AudioMode, EngineConfig, FRAME_SAMPLES, SAMPLE_RATE};

fn opus_channels(channels: u8) -> Channels {
    if channels == 1 {
        Channels::Mono
    } else {
        Channels::Stereo
    }
}

fn opus_application(mode: AudioMode) -> Application {
    match mode {
        AudioMode::Voice => Application::Voip,
        AudioMode::Music => Application::Audio,
    }
}

pub fn configure_encoder(config: &EngineConfig) -> Result<Encoder> {
    let mut encoder = Encoder::new(
        SAMPLE_RATE,
        opus_channels(config.channels),
        opus_application(config.mode),
    )
    .context("create Opus encoder")?;
    encoder
        .set_bitrate(Bitrate::Bits(config.bitrate))
        .context("set Opus bitrate")?;
    encoder.set_complexity(10).context("set Opus complexity")?;
    encoder.set_vbr(true).context("enable Opus VBR")?;
    encoder
        .set_vbr_constraint(false)
        .context("disable constrained VBR")?;
    encoder
        .set_max_bandwidth(Bandwidth::Fullband)
        .context("set Opus max bandwidth")?;
    encoder
        .set_bandwidth(Bandwidth::Fullband)
        .context("set Opus bandwidth")?;
    encoder
        .set_signal(match config.mode {
            AudioMode::Voice => Signal::Voice,
            AudioMode::Music => Signal::Music,
        })
        .context("set Opus signal hint")?;
    encoder.set_dtx(false).context("disable Opus DTX")?;
    encoder.set_inband_fec(false).context("disable Opus FEC")?;
    encoder
        .set_packet_loss_perc(0)
        .context("set Opus packet loss percentage")?;
    Ok(encoder)
}

pub fn encode_float_frames(samples: &[f32], config: &EngineConfig) -> Result<Vec<Vec<u8>>> {
    let channels = config.channels as usize;
    let frame_len = FRAME_SAMPLES * channels;
    let mut encoder = configure_encoder(config)?;
    let mut encoded = Vec::with_capacity(samples.len() / frame_len + 1);
    let mut output = vec![0u8; 4096];
    for frame in samples.chunks_exact(frame_len) {
        let len = encoder
            .encode_float(frame, &mut output)
            .context("encode Opus frame")?;
        encoded.push(output[..len].to_vec());
    }
    Ok(encoded)
}

pub fn encode_i16_frame(encoder: &mut Encoder, frame: &[i16]) -> Result<Vec<u8>> {
    let mut output = vec![0u8; 4096];
    let len = encoder
        .encode(frame, &mut output)
        .context("encode Opus PCM frame")?;
    output.truncate(len);
    Ok(output)
}

pub fn encode_float_frame(encoder: &mut Encoder, frame: &[f32]) -> Result<Vec<u8>> {
    let mut output = vec![0u8; 4096];
    let len = encoder
        .encode_float(frame, &mut output)
        .context("encode Opus float PCM frame")?;
    output.truncate(len);
    Ok(output)
}

pub fn decode_frames_to_wav(
    path: &std::path::Path,
    payloads: &[Vec<u8>],
    channels: u8,
) -> Result<()> {
    let mut decoder =
        Decoder::new(SAMPLE_RATE, opus_channels(channels)).context("create Opus decoder")?;
    let mut decoded: Vec<i16> = Vec::new();
    let mut frame = vec![0.0f32; FRAME_SAMPLES * channels as usize * 3];
    for payload in payloads {
        let samples_per_channel = decoder
            .decode_float(payload, &mut frame, false)
            .context("decode Opus frame")?;
        let count = samples_per_channel * channels as usize;
        decoded.extend(frame[..count].iter().map(|sample| float_to_i16(*sample)));
    }
    write_wav_i16(path, &decoded, channels, SAMPLE_RATE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioMode, DEFAULT_PAYLOAD_TYPE};

    #[test]
    fn encodes_and_decodes_a_tone_frame() {
        let config = EngineConfig::new(AudioMode::Music, 2, Some(160_000), DEFAULT_PAYLOAD_TYPE, 1);
        let mut samples = Vec::new();
        for n in 0..FRAME_SAMPLES {
            let sample =
                ((n as f32 / SAMPLE_RATE as f32) * 440.0 * std::f32::consts::TAU).sin() * 0.2;
            samples.push(sample);
            samples.push(sample);
        }
        let payloads = encode_float_frames(&samples, &config).unwrap();
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].len() > 16);

        let mut decoder = Decoder::new(SAMPLE_RATE, Channels::Stereo).unwrap();
        let mut out = vec![0.0; FRAME_SAMPLES * 2 * 3];
        let decoded = decoder.decode_float(&payloads[0], &mut out, false).unwrap();
        assert_eq!(decoded, FRAME_SAMPLES);
    }
}
