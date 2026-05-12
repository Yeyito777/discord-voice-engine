use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail};
use symphonia::core::audio::{AudioBufferRef, Signal, SignalSpec};
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::conv::IntoSample;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::audio::{PcmAudio, convert_channels, resample_to_48k};

pub fn load_audio_file(path: &Path, output_channels: u8) -> Result<PcmAudio> {
    let source = File::open(path).with_context(|| format!("open audio file {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(extension);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("probe audio file {}", path.display()))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow::anyhow!("no supported audio track in {}", path.display()))?
        .clone();
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .with_context(|| format!("create decoder for {}", path.display()))?;

    let mut input_samples: Vec<f32> = Vec::new();
    let mut input_spec: Option<SignalSpec> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => bail!("audio stream reset is not supported yet"),
            Err(error) => {
                return Err(error).with_context(|| format!("read packet from {}", path.display()));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("decode packet from {}", path.display()));
            }
        };
        let spec = *decoded.spec();
        if let Some(existing) = input_spec {
            if existing.rate != spec.rate || existing.channels != spec.channels {
                bail!("audio stream changed format mid-file, which is not supported yet");
            }
        } else {
            input_spec = Some(spec);
        }
        append_audio_buffer(decoded, &mut input_samples);
    }

    let spec =
        input_spec.ok_or_else(|| anyhow::anyhow!("no decodable audio in {}", path.display()))?;
    let input_channels = spec.channels.count() as u8;
    let converted = convert_channels(&input_samples, input_channels, output_channels)?;
    resample_to_48k(PcmAudio {
        samples: converted,
        sample_rate: spec.rate,
        channels: output_channels,
    })
}

fn append_audio_buffer(buffer: AudioBufferRef<'_>, output: &mut Vec<f32>) {
    match buffer {
        AudioBufferRef::F32(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| buf.chan(channel)[frame],
            output,
        ),
        AudioBufferRef::U8(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
        AudioBufferRef::U16(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
        AudioBufferRef::U24(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
        AudioBufferRef::U32(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
        AudioBufferRef::S8(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
        AudioBufferRef::S16(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
        AudioBufferRef::S24(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
        AudioBufferRef::S32(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
        AudioBufferRef::F64(buf) => append_planes(
            buf.spec().channels.count(),
            buf.frames(),
            |channel, frame| IntoSample::<f32>::into_sample(buf.chan(channel)[frame]),
            output,
        ),
    }
}

fn append_planes<F>(channels: usize, frames: usize, mut sample: F, output: &mut Vec<f32>)
where
    F: FnMut(usize, usize) -> f32,
{
    output.reserve(frames * channels);
    for frame in 0..frames {
        for channel in 0..channels {
            output.push(sample(channel, frame));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn decodes_wav_to_requested_48k_stereo() {
        let path = std::env::temp_dir().join(format!(
            "discord-voice-engine-test-{}.wav",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            for n in 0..4410 {
                let sample = ((n as f32 / 44_100.0) * 440.0 * std::f32::consts::TAU).sin()
                    * i16::MAX as f32
                    * 0.2;
                writer.write_sample(sample as i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        let decoded = load_audio_file(&path, 2).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 2);
        assert!(decoded.frames() >= 4_790 && decoded.frames() <= 4_810);
        assert_eq!(decoded.samples.len(), decoded.frames() * 2);
    }
}
