# discord-voice-engine

Shared outgoing-audio engine for Discord voice clients.

The engine exists so Record and discord-cli do not each maintain their own ad-hoc ffmpeg voice sender. It owns the audio-production side:

- file decode with Symphonia
- high-quality 48 kHz resampling with Rubato
- Opus encoding through libopus
- 20 ms Discord RTP cadence and timestamps
- low-latency PulseAudio/PipeWire microphone capture via `parec` raw PCM

It intentionally does **not** talk to Discord. Record and discord-cli still own Discord gateway, DAVE, RTP transport encryption, UDP sockets, and call lifecycle.

## Commands

### Encode/send a file as local plain RTP

```sh
discord-voice-engine encode-file \
  --input song.mp3 \
  --rtp 127.0.0.1:50000 \
  --mode music \
  --channels 2 \
  --bitrate 192000
```

Defaults for music are 48 kHz stereo, Opus `application=audio`, fullband, high complexity, VBR, and 192 kbps stereo.

Useful diagnostics:

```sh
--dump-input-pcm /tmp/input.wav
--dump-decoded-opus /tmp/decoded.wav
--stats-json /tmp/stats.json
--no-realtime
```

### Capture microphone as local plain RTP

```sh
discord-voice-engine capture-mic \
  --rtp 127.0.0.1:50000 \
  --mode voice \
  --channels 2 \
  --bitrate 96000 \
  --meter-stdout
```

Microphone capture uses `parec` with raw 48 kHz signed 16-bit PCM and explicit 20 ms latency/process-time requests. Rust/libopus still owns encoding, RTP headers, timing counters, and diagnostics. `--meter-stdout` writes mono signed 16-bit little-endian PCM for Record's local speaking meter.

## Integration contract

Both clients discover the binary in this order:

1. `DISCORD_VOICE_ENGINE`
2. `PATH` lookup for `discord-voice-engine`
3. client-specific legacy fallback, if any

The UDP RTP emitted by the engine is intentionally plain local RTP. Consumers wrap the Opus payloads with their existing DAVE and Discord voice transport layers.
