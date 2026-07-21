# discord-voice-engine

> Archived: this standalone repository is no longer the canonical home for `discord-voice-engine`.

`discord-voice-engine` has been folded into the Record monorepo and now lives at:

```text
record/native/discord-voice-engine/
```

Canonical repository:

```text
https://github.com/Yeyito777/record
```

The binary name and integration contract are unchanged: Record and other tools, including `discord-cli`, should use the installed `discord-voice-engine` binary from `PATH`, or an explicit `DISCORD_VOICE_ENGINE` path.

This repository is kept read-only for historical reference. Future development should happen in the Record monorepo.

## Playback runtime controls

`play-rtp` keeps stdin open for newline-delimited runtime controls. The C playback loop reads stdin nonblocking and accepts:

```text
user-volume <ssrc> <percent>
gain-db <db>
```

`user-volume` uses a decimal RTP SSRC and a numeric percentage. Percentages are clamped to `0..200`, and SSRCs without an explicit setting use `100`. The multiplier is applied to that SSRC's decoded PCM before streams are mixed. Settings persist if an SSRC's playback stream goes idle and is later recreated.

`gain-db` changes the global gain applied to the mixed playback PCM. `play-rtp --gain-db <db>` selects the initial global gain and defaults to `0` dB. Runtime and initial dB values must be finite numbers.
