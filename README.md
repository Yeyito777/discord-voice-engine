# discord-voice-engine

Shared audio engine for Discord voice clients.

The engine exists so Record and discord-cli do not each maintain their own ad-hoc ffmpeg/ffplay voice paths. It owns the audio-production side:

- file decode with Symphonia
- high-quality 48 kHz resampling with Rubato
- Opus encoding through libopus
- 20 ms Discord RTP cadence and timestamps
- low-latency PulseAudio/PipeWire microphone capture via `parec` raw PCM
- incoming Opus RTP jitter buffering and decoder-state packet loss concealment

It intentionally does **not** talk to Discord. Record and discord-cli still own Discord gateway, DAVE, RTP transport encryption, UDP sockets, and call lifecycle.

## Build requirements

The live playback backend is built from C and links against libopus and PulseAudio's simple client library through `pkg-config` (`opus`, `libpulse-simple`, and `libpulse`).

On Debian/Ubuntu-style systems:

```sh
sudo apt install build-essential pkg-config libopus-dev libpulse-dev
```

Build and install with:

```sh
make
make install
```

By default `make install` writes `discord-voice-engine` to `~/.local/bin`. Override with `PREFIX=/usr/local` or another prefix if needed.

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
  --gain-db -20 \
  --meter-stdout
```

Microphone capture uses `parec` with raw 48 kHz signed 16-bit PCM and explicit 20 ms latency/process-time requests. Rust/libopus still owns gain (`--gain-db`, plus stdin control lines like `gain-db -12`), encoding, RTP headers, timing counters, and diagnostics. `--meter-stdout` writes mono signed 16-bit little-endian PCM for Record's local speaking meter.

### Play incoming local plain RTP with recovery

```sh
discord-voice-engine play-rtp \
  --rtp 127.0.0.1:50001 \
  --channels 2 \
  --payload-type 120 \
  --jitter-ms 240 \
  --output pipewire
```

`play-rtp` is the native replacement for handing RTP to `ffplay`. It receives decrypted/DAVE-decoded local Opus RTP from the client, buffers it on a fixed playout cadence, and uses libopus decoder-state PLC (`decode_float` with an empty packet) for missing frames. The live playout backend is implemented in C inside this binary so it can parse RTP, maintain decoder state, mix streams, and feed Pulse/PipeWire directly without a `pw-cat` subprocess. Metrics such as received packets, missing packets, concealed packets, late packets, decode errors, output underruns, and Opus packet duration histograms are available through `--stats-json`.

### Packet-loss recovery harness

```sh
discord-voice-engine test-playback-recovery \
  --input song.mp3 \
  --iterations 100 \
  --loss-per-mille 20 \
  --max-burst 3 \
  --stats-json /tmp/recovery.json
```

The harness encodes the input to Opus RTP, deterministically injects packet loss/bursts, decodes through the same recovery path, and fails if the stream shortens, decode errors occur, missing packets are not concealed, or concealed windows collapse to hard silence.

## Integration contract

Both clients discover the binary in this order:

1. `DISCORD_VOICE_ENGINE`
2. `PATH` lookup for `discord-voice-engine`
3. client-specific legacy fallback, if any

The UDP RTP emitted by the engine is intentionally plain local RTP. Consumers wrap the Opus payloads with their existing DAVE and Discord voice transport layers.

For incoming playback, consumers send decrypted/DAVE-decoded plain RTP to `play-rtp`; the engine owns jitter buffering, Opus decoding, PLC, mixing, and local PipeWire/Pulse/WAV/null output.

## License

MIT
