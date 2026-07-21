use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::path::Path;

use anyhow::{Context, Result, bail};

unsafe extern "C" {
    fn discord_voice_engine_c_play_rtp_main(argc: c_int, argv: *const *const c_char) -> c_int;
    fn discord_voice_engine_c_playback_self_test() -> c_int;
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CPlaybackOutput {
    PipeWire,
    Pulse,
    Wav,
    Null,
}

impl CPlaybackOutput {
    fn as_arg(self) -> &'static str {
        match self {
            Self::PipeWire => "pipewire",
            Self::Pulse => "pulse",
            Self::Wav => "wav",
            Self::Null => "null",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CPlaybackOptions<'a> {
    pub rtp_addr: &'a str,
    pub channels: u8,
    pub payload_type: u8,
    pub jitter_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_plc_packets: usize,
    pub gain_db: f32,
    pub output: CPlaybackOutput,
    pub output_wav: Option<&'a Path>,
    pub duration_ms: Option<u64>,
    pub stats_json: Option<&'a Path>,
    pub ready_file: Option<&'a Path>,
    pub use_fec: bool,
}

pub fn play_rtp_c(options: CPlaybackOptions<'_>) -> Result<()> {
    if options.output == CPlaybackOutput::Wav && options.output_wav.is_none() {
        bail!("--output wav requires --output-wav");
    }

    let mut args = Vec::new();
    push_arg(&mut args, "discord-voice-engine")?;
    push_arg(&mut args, "play-rtp")?;
    push_arg_pair(&mut args, "--rtp", options.rtp_addr)?;
    push_arg_pair(&mut args, "--channels", options.channels.to_string())?;
    push_arg_pair(
        &mut args,
        "--payload-type",
        options.payload_type.to_string(),
    )?;
    push_arg_pair(&mut args, "--jitter-ms", options.jitter_ms.to_string())?;
    push_arg_pair(
        &mut args,
        "--idle-timeout-ms",
        options.idle_timeout_ms.to_string(),
    )?;
    push_arg_pair(
        &mut args,
        "--max-plc-packets",
        options.max_plc_packets.to_string(),
    )?;
    push_arg_pair(&mut args, "--gain-db", options.gain_db.to_string())?;
    push_arg_pair(&mut args, "--output", options.output.as_arg())?;
    if let Some(path) = options.output_wav {
        push_arg_pair(&mut args, "--output-wav", path.display().to_string())?;
    }
    if let Some(duration_ms) = options.duration_ms {
        push_arg_pair(&mut args, "--duration-ms", duration_ms.to_string())?;
    }
    if let Some(path) = options.stats_json {
        push_arg_pair(&mut args, "--stats-json", path.display().to_string())?;
    }
    if let Some(path) = options.ready_file {
        push_arg_pair(&mut args, "--ready-file", path.display().to_string())?;
    }
    if options.use_fec {
        push_arg(&mut args, "--fec")?;
    }

    let argv: Vec<*const c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
    let code = unsafe { discord_voice_engine_c_play_rtp_main(argv.len() as c_int, argv.as_ptr()) };
    if code == 0 {
        Ok(())
    } else {
        bail!("C play-rtp backend exited with {code}")
    }
}

fn push_arg(args: &mut Vec<CString>, value: impl AsRef<str>) -> Result<()> {
    args.push(CString::new(value.as_ref()).context("argument contains a NUL byte")?);
    Ok(())
}

fn push_arg_pair(args: &mut Vec<CString>, key: &'static str, value: impl AsRef<str>) -> Result<()> {
    push_arg(args, key)?;
    push_arg(args, value)
}

pub fn playback_c_self_test() -> Result<()> {
    let code = unsafe { discord_voice_engine_c_playback_self_test() };
    if code == 0 {
        Ok(())
    } else {
        bail!("C playback self-test failed with {code}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_playback_self_test_validates_rtp_jitter_and_opus_handling() {
        playback_c_self_test().unwrap();
    }
}
