// discord-voice-engine play-rtp: native C RTP/Opus playback backend.
//
// This backend receives local, already transport/DAVE-decoded Opus RTP packets
// from Discord voice clients, reconstructs continuous 48 kHz PCM with a jitter
// buffer and a persistent libopus decoder per SSRC, and writes the result
// directly to the local audio server (Pulse/PipeWire's Pulse compatibility
// server) or to a WAV file/null sink for tests.

#define _POSIX_C_SOURCE 200809L

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <float.h>
#include <math.h>
#include <netdb.h>
#include <opus/opus.h>
#include <pulse/simple.h>
#include <pulse/error.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#define SAMPLE_RATE 48000
#define FRAME_MS 20
#define FRAME_SAMPLES ((SAMPLE_RATE * FRAME_MS) / 1000)
#define OPUS_MAX_FRAME_SAMPLES 5760
#define RTP_HEADER_LEN 12
#define MAX_PACKET_SIZE 4096
#define MAX_PAYLOAD_SIZE 4096
#define DEFAULT_PAYLOAD_TYPE 120
#define DEFAULT_CHANNELS 2
#define DEFAULT_JITTER_MS 240
#define DEFAULT_IDLE_TIMEOUT_MS 350
#define DEFAULT_MAX_PLC_PACKETS 10
#define DEFAULT_MAX_RESYNC_GAP 120
#define MAX_STREAMS 16
#define CONTROL_LINE_CAP 512
#define CONTROL_READ_CAP 1024
static volatile sig_atomic_t g_running = 1;
static int g_stdin_original_flags = -1;
static bool g_stdin_nonblocking_changed = false;
static bool g_stdin_restore_registered = false;

static void on_signal(int sig) {
  (void)sig;
  g_running = 0;
}

static void restore_stdin_flags(void) {
  if (g_stdin_nonblocking_changed && g_stdin_original_flags >= 0) {
    (void)fcntl(STDIN_FILENO, F_SETFL, g_stdin_original_flags);
  }
  g_stdin_original_flags = -1;
  g_stdin_nonblocking_changed = false;
}

static uint64_t monotonic_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ((uint64_t)ts.tv_sec * 1000u) + ((uint64_t)ts.tv_nsec / 1000000u);
}

static void sleep_until_ms(uint64_t target_ms) {
  while (g_running) {
    uint64_t now = monotonic_ms();
    if (now >= target_ms) return;
    uint64_t remain = target_ms - now;
    struct timespec ts;
    ts.tv_sec = (time_t)(remain / 1000u);
    ts.tv_nsec = (long)((remain % 1000u) * 1000000u);
    nanosleep(&ts, NULL);
  }
}

static void die(const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  vfprintf(stderr, fmt, ap);
  va_end(ap);
  fputc('\n', stderr);
  exit(1);
}

static int16_t float_to_i16(float sample) {
  if (!isfinite(sample)) return 0;
  if (sample > 1.0f) sample = 1.0f;
  if (sample < -1.0f) sample = -1.0f;
  float scaled = sample * 32767.0f;
  if (scaled >= 0.0f) return (int16_t)(scaled + 0.5f);
  return (int16_t)(scaled - 0.5f);
}

static uint16_t seq_forward_distance(uint16_t sequence, uint16_t expected) {
  return (uint16_t)(sequence - expected);
}

static bool seq_is_older(uint16_t sequence, uint16_t expected) {
  uint16_t delta = seq_forward_distance(sequence, expected);
  return delta >= 0x8000u;
}

static uint32_t timestamp_forward_distance(uint32_t timestamp, uint32_t expected) {
  return timestamp - expected;
}

static bool timestamp_is_older(uint32_t timestamp, uint32_t expected) {
  uint32_t delta = timestamp_forward_distance(timestamp, expected);
  return delta >= 0x80000000u;
}

typedef struct {
  uint16_t sequence;
  uint32_t timestamp;
  uint32_t ssrc;
  uint8_t payload_type;
  const uint8_t *payload;
  size_t payload_len;
  size_t header_len;
  bool has_extension;
} ParsedRtp;

static bool parse_rtp_packet(const uint8_t *packet, size_t len, ParsedRtp *out) {
  if (len < RTP_HEADER_LEN) return false;
  if ((packet[0] >> 6) != 2) return false;

  size_t csrc_count = packet[0] & 0x0f;
  bool has_extension = (packet[0] & 0x10) != 0;
  size_t offset = RTP_HEADER_LEN + (csrc_count * 4u);
  if (len < offset) return false;

  if (has_extension) {
    if (len < offset + 4u) return false;
    size_t ext_words = ((size_t)packet[offset + 2] << 8) | packet[offset + 3];
    offset += 4u + (ext_words * 4u);
    if (len < offset) return false;
  }
  if (len <= offset) return false;

  out->sequence = ((uint16_t)packet[2] << 8) | packet[3];
  out->timestamp = ((uint32_t)packet[4] << 24) | ((uint32_t)packet[5] << 16) |
                   ((uint32_t)packet[6] << 8) | packet[7];
  out->ssrc = ((uint32_t)packet[8] << 24) | ((uint32_t)packet[9] << 16) |
              ((uint32_t)packet[10] << 8) | packet[11];
  out->payload_type = packet[1] & 0x7f;
  out->payload = packet + offset;
  out->payload_len = len - offset;
  out->header_len = offset;
  out->has_extension = has_extension;
  return true;
}

typedef struct RtpPacketNode {
  uint16_t sequence;
  uint32_t timestamp;
  uint32_t ssrc;
  uint8_t payload_type;
  size_t payload_len;
  uint8_t payload[MAX_PAYLOAD_SIZE];
  struct RtpPacketNode *next;
} RtpPacketNode;

typedef struct {
  float *data;
  size_t len;
  size_t cap;
} FloatFifo;

static void fifo_free(FloatFifo *fifo) {
  free(fifo->data);
  fifo->data = NULL;
  fifo->len = 0;
  fifo->cap = 0;
}

static bool fifo_reserve(FloatFifo *fifo, size_t need) {
  if (need <= fifo->cap) return true;
  size_t cap = fifo->cap ? fifo->cap : 4096;
  while (cap < need) cap *= 2u;
  float *next = (float *)realloc(fifo->data, cap * sizeof(float));
  if (!next) return false;
  fifo->data = next;
  fifo->cap = cap;
  return true;
}

static bool fifo_append(FloatFifo *fifo, const float *samples, size_t count) {
  if (!fifo_reserve(fifo, fifo->len + count)) return false;
  memcpy(fifo->data + fifo->len, samples, count * sizeof(float));
  fifo->len += count;
  return true;
}

static void fifo_pop_frame(FloatFifo *fifo, float *out, size_t count) {
  size_t copy = fifo->len < count ? fifo->len : count;
  if (copy) memcpy(out, fifo->data, copy * sizeof(float));
  if (copy < count) memset(out + copy, 0, (count - copy) * sizeof(float));
  if (copy) {
    memmove(fifo->data, fifo->data + copy, (fifo->len - copy) * sizeof(float));
    fifo->len -= copy;
  }
}

typedef struct {
  const char *mode;
  uint32_t sample_rate;
  uint8_t channels;
  uint64_t received_packets;
  uint64_t decoded_packets;
  uint64_t normal_packets;
  uint64_t concealed_packets;
  uint64_t fec_attempts;
  uint64_t sequence_gap_events;
  uint64_t missing_packets;
  uint64_t max_consecutive_missing_packets;
  uint64_t duplicate_packets;
  uint64_t out_of_order_packets;
  uint64_t late_packets;
  uint64_t dropped_wrong_payload_packets;
  uint64_t dropped_ssrc_packets;
  uint64_t decode_errors;
  uint64_t decoder_resets;
  uint64_t resync_events;
  uint64_t streams_started;
  uint64_t streams_ended;
  uint64_t output_frames;
  uint64_t silent_output_frames;
  uint64_t output_duration_ms;
  uint64_t output_underruns;
  uint64_t opus_2_5ms_packets;
  uint64_t opus_5ms_packets;
  uint64_t opus_10ms_packets;
  uint64_t opus_20ms_packets;
  uint64_t opus_40ms_packets;
  uint64_t opus_60ms_packets;
  uint64_t opus_other_duration_packets;
} PlaybackStats;

static void stats_note_opus_samples(PlaybackStats *stats, int samples_per_channel) {
  switch (samples_per_channel) {
    case 120: stats->opus_2_5ms_packets++; break;
    case 240: stats->opus_5ms_packets++; break;
    case 480: stats->opus_10ms_packets++; break;
    case 960: stats->opus_20ms_packets++; break;
    case 1920: stats->opus_40ms_packets++; break;
    case 2880: stats->opus_60ms_packets++; break;
    default: stats->opus_other_duration_packets++; break;
  }
}

typedef struct {
  bool active;
  uint32_t ssrc;
  OpusDecoder *decoder;
  uint16_t expected_sequence;
  bool expected_sequence_set;
  uint32_t expected_timestamp;
  bool expected_timestamp_set;
  RtpPacketNode *packets;
  size_t buffered_packets;
  FloatFifo pending;
  uint64_t last_receive_ms;
  uint64_t consecutive_plc;
} PlaybackStream;

typedef struct UserVolumeControl {
  uint32_t ssrc;
  float percent;
  struct UserVolumeControl *next;
} UserVolumeControl;

typedef struct {
  UserVolumeControl *user_volumes;
  float gain_db;
  float gain_multiplier;
} PlaybackControls;

typedef enum {
  PLAYBACK_CONTROL_NONE = 0,
  PLAYBACK_CONTROL_USER_VOLUME = 1,
  PLAYBACK_CONTROL_GAIN_DB = 2,
} PlaybackControlKind;

typedef struct {
  PlaybackControlKind kind;
  uint32_t ssrc;
  float value;
} PlaybackControlCommand;

typedef struct {
  char line[CONTROL_LINE_CAP];
  size_t line_len;
  bool discarding_line;
  bool eof;
} ControlInput;

static void free_packet_list(RtpPacketNode *node) {
  while (node) {
    RtpPacketNode *next = node->next;
    free(node);
    node = next;
  }
}

static void stream_destroy(PlaybackStream *stream) {
  if (stream->decoder) opus_decoder_destroy(stream->decoder);
  stream->decoder = NULL;
  free_packet_list(stream->packets);
  stream->packets = NULL;
  fifo_free(&stream->pending);
  memset(stream, 0, sizeof(*stream));
}

static bool stream_init(PlaybackStream *stream, uint32_t ssrc, uint16_t first_sequence, uint32_t first_timestamp, int channels) {
  int err = OPUS_OK;
  memset(stream, 0, sizeof(*stream));
  stream->decoder = opus_decoder_create(SAMPLE_RATE, channels, &err);
  if (!stream->decoder || err != OPUS_OK) return false;
  stream->active = true;
  stream->ssrc = ssrc;
  stream->expected_sequence = first_sequence;
  stream->expected_sequence_set = true;
  stream->expected_timestamp = first_timestamp;
  stream->expected_timestamp_set = true;
  stream->last_receive_ms = monotonic_ms();
  return true;
}

static void stream_reset_decoder_state(PlaybackStream *stream, PlaybackStats *stats, bool flush_pending) {
  if (stream->decoder) opus_decoder_ctl(stream->decoder, OPUS_RESET_STATE);
  if (flush_pending) stream->pending.len = 0;
  if (stats) stats->decoder_resets++;
}

static RtpPacketNode *stream_pop_expected(PlaybackStream *stream) {
  RtpPacketNode **cursor = &stream->packets;
  while (*cursor) {
    RtpPacketNode *node = *cursor;
    if (node->sequence == stream->expected_sequence) {
      *cursor = node->next;
      node->next = NULL;
      stream->buffered_packets--;
      return node;
    }
    cursor = &node->next;
  }
  return NULL;
}

static RtpPacketNode *stream_nearest_future(PlaybackStream *stream) {
  RtpPacketNode *best = NULL;
  uint16_t best_delta = 0xffffu;
  for (RtpPacketNode *node = stream->packets; node; node = node->next) {
    if (seq_is_older(node->sequence, stream->expected_sequence)) continue;
    uint16_t delta = seq_forward_distance(node->sequence, stream->expected_sequence);
    if (!best || delta < best_delta) {
      best = node;
      best_delta = delta;
    }
  }
  return best;
}

static bool stream_insert_packet(PlaybackStream *stream, const ParsedRtp *parsed, PlaybackStats *stats) {
  if (parsed->payload_len > MAX_PAYLOAD_SIZE) return false;
  if (!stream->expected_sequence_set) {
    stream->expected_sequence = parsed->sequence;
    stream->expected_sequence_set = true;
  }
  if (seq_is_older(parsed->sequence, stream->expected_sequence)) {
    stats->late_packets++;
    return false;
  }
  RtpPacketNode **cursor = &stream->packets;
  while (*cursor) {
    RtpPacketNode *node = *cursor;
    if (node->sequence == parsed->sequence) {
      stats->duplicate_packets++;
      return false;
    }
    uint16_t new_delta = seq_forward_distance(parsed->sequence, stream->expected_sequence);
    uint16_t old_delta = seq_forward_distance(node->sequence, stream->expected_sequence);
    if (new_delta < old_delta) break;
    cursor = &node->next;
  }
  RtpPacketNode *node = (RtpPacketNode *)calloc(1, sizeof(*node));
  if (!node) return false;
  node->sequence = parsed->sequence;
  node->timestamp = parsed->timestamp;
  node->ssrc = parsed->ssrc;
  node->payload_type = parsed->payload_type;
  node->payload_len = parsed->payload_len;
  memcpy(node->payload, parsed->payload, parsed->payload_len);
  node->next = *cursor;
  *cursor = node;
  stream->buffered_packets++;
  stream->last_receive_ms = monotonic_ms();
  return true;
}

static int decode_packet_into_pending(PlaybackStream *stream, RtpPacketNode *packet, int channels, PlaybackStats *stats) {
  float decoded[OPUS_MAX_FRAME_SAMPLES * 2];
  int samples = opus_decode_float(stream->decoder, packet->payload, (opus_int32)packet->payload_len, decoded, OPUS_MAX_FRAME_SAMPLES, 0);
  if (samples < 0) {
    stats->decode_errors++;
    stream_reset_decoder_state(stream, stats, true);
    return -1;
  }
  size_t count = (size_t)samples * (size_t)channels;
  // A successful Opus decode is authoritative. Do not classify decoded PCM by
  // peak, RMS, clipping, or zero crossings: full-scale music is valid, codec
  // overshoot can exceed +/-1.0, and interleaved stereo channels are not
  // consecutive points in one waveform. The former heuristic falsely replaced
  // loud music frames with silence, making music bots sound badly chopped.
  if (!fifo_append(&stream->pending, decoded, count)) die("out of memory appending decoded Opus");
  stats->normal_packets++;
  stats->decoded_packets++;
  stats_note_opus_samples(stats, samples);
  return samples;
}

static void stream_advance_after_packet(PlaybackStream *stream, uint32_t packet_timestamp, int samples_per_channel) {
  if (!stream->expected_timestamp_set) {
    stream->expected_timestamp = packet_timestamp;
    stream->expected_timestamp_set = true;
  }
  if (timestamp_is_older(packet_timestamp, stream->expected_timestamp)) {
    stream->expected_timestamp += (uint32_t)samples_per_channel;
  } else {
    stream->expected_timestamp = packet_timestamp + (uint32_t)samples_per_channel;
  }
}

static void stream_advance_after_concealment(PlaybackStream *stream) {
  if (stream->expected_timestamp_set) stream->expected_timestamp += FRAME_SAMPLES;
}

static uint32_t stream_missing_audio_frames_before(const PlaybackStream *stream, const RtpPacketNode *packet) {
  if (!stream->expected_timestamp_set) return 0;
  if (timestamp_is_older(packet->timestamp, stream->expected_timestamp)) return 0;
  return timestamp_forward_distance(packet->timestamp, stream->expected_timestamp) / FRAME_SAMPLES;
}

static bool append_silence_into_pending(PlaybackStream *stream, int channels, PlaybackStats *stats) {
  float silence[FRAME_SAMPLES * 2];
  memset(silence, 0, sizeof(silence));
  size_t count = (size_t)FRAME_SAMPLES * (size_t)channels;
  if (!fifo_append(&stream->pending, silence, count)) die("out of memory appending silence concealment");
  stats->concealed_packets++;
  stats->decoded_packets++;
  return true;
}

typedef enum {
  STREAM_FRAME_NONE = 0,
  STREAM_FRAME_AUDIO = 1,
  STREAM_FRAME_ENDED = 2,
} StreamFrameResult;

static StreamFrameResult stream_next_frame(
    PlaybackStream *stream,
    uint64_t now_ms,
    int channels,
    int idle_timeout_ms,
    int max_plc_packets,
    int max_resync_gap,
    bool use_fec,
    PlaybackStats *stats,
    float *out_frame) {
  (void)use_fec;
  size_t frame_samples = (size_t)FRAME_SAMPLES * (size_t)channels;

  while (stream->pending.len < frame_samples) {
    RtpPacketNode *future = stream_nearest_future(stream);
    if (future) {
      uint16_t sequence_gap = seq_forward_distance(future->sequence, stream->expected_sequence);
      uint32_t missing_audio_frames = stream_missing_audio_frames_before(stream, future);
      if (missing_audio_frames > 0) {
        if (sequence_gap == 0 ||
            (max_plc_packets >= 0 && missing_audio_frames > (uint32_t)max_plc_packets) ||
            (max_resync_gap > 0 && missing_audio_frames > (uint32_t)max_resync_gap)) {
          stream_reset_decoder_state(stream, stats, true);
          stream->expected_sequence = future->sequence;
          stream->expected_timestamp = future->timestamp;
          stream->expected_timestamp_set = true;
          stream->consecutive_plc = 0;
          stats->resync_events++;
          continue;
        }

        // We intentionally fill confirmed loss with silence instead of Opus PLC.
        // Since the decoder has not seen the missing coded frames, reset its
        // predictor state before later real packets. Otherwise stale pre-gap
        // state can make the first 1-3 post-gap frames decode as loud metallic
        // bursts even though the missing audio itself was silenced.
        stream_reset_decoder_state(stream, stats, true);
        stream->consecutive_plc++;
        stats->missing_packets++;
        stats->sequence_gap_events += stream->consecutive_plc == 1 ? 1u : 0u;
        if (stream->consecutive_plc > stats->max_consecutive_missing_packets) {
          stats->max_consecutive_missing_packets = stream->consecutive_plc;
        }
        if (!append_silence_into_pending(stream, channels, stats)) return STREAM_FRAME_ENDED;
        stream_advance_after_concealment(stream);
        continue;
      }

      if (sequence_gap > 0) {
        // RTP sequence numbers can advance for packets that Record correctly
        // filters before handing us Opus audio, especially around DAVE/media
        // transition packets.  If RTP timestamps show no missing audio time,
        // skip those sequence numbers instead of manufacturing PLC audio.
        stream->expected_sequence = future->sequence;
        stream->consecutive_plc = 0;
      }

      RtpPacketNode *packet = stream_pop_expected(stream);
      if (!packet) continue;
      uint32_t packet_timestamp = packet->timestamp;
      int samples = decode_packet_into_pending(stream, packet, channels, stats);
      free(packet);
      stream->expected_sequence = (uint16_t)(stream->expected_sequence + 1u);
      stream->consecutive_plc = 0;
      if (samples >= 0) {
        stream_advance_after_packet(stream, packet_timestamp, samples);
      } else {
        // Treat corrupt packets like a loss event, but fill with silence instead
        // of decoder-state PLC.  PLC/FEC can turn packet loss into robotic
        // artifacts; a tiny dropout is less objectionable and easier to reason
        // about in live Discord playback.
        if (!append_silence_into_pending(stream, channels, stats)) return STREAM_FRAME_ENDED;
        stream_advance_after_concealment(stream);
      }
      continue;
    }

    // Do not manufacture tail audio when the stream simply dries up.  Missing
    // audio is concealed with silence only when a future packet proves there was
    // a real timestamp gap.  Blindly generating decoder-state PLC while waiting
    // for more network packets creates watery/metallic tails during talk-spurt
    // ends or Discord/DAVE transition bursts.
    if (stream->pending.len == 0) {
      if (now_ms >= stream->last_receive_ms + (uint64_t)idle_timeout_ms) {
        return STREAM_FRAME_ENDED;
      }
      return STREAM_FRAME_NONE;
    }
    break;
  }

  if (stream->pending.len == 0) return STREAM_FRAME_NONE;
  if (stream->pending.len < frame_samples) stats->output_underruns++;
  fifo_pop_frame(&stream->pending, out_frame, frame_samples);
  return STREAM_FRAME_AUDIO;
}

typedef enum {
  OUTPUT_NULL,
  OUTPUT_WAV,
  OUTPUT_PULSE,
} OutputKind;

typedef struct {
  OutputKind kind;
  int channels;
  pa_simple *pulse;
  FILE *wav;
  uint64_t wav_samples;
} PcmSink;

static void write_le16(FILE *f, uint16_t v) {
  fputc(v & 0xff, f);
  fputc((v >> 8) & 0xff, f);
}

static void write_le32(FILE *f, uint32_t v) {
  fputc(v & 0xff, f);
  fputc((v >> 8) & 0xff, f);
  fputc((v >> 16) & 0xff, f);
  fputc((v >> 24) & 0xff, f);
}

static void wav_write_header(FILE *f, int channels, uint32_t data_bytes) {
  fseek(f, 0, SEEK_SET);
  fwrite("RIFF", 1, 4, f);
  write_le32(f, 36u + data_bytes);
  fwrite("WAVEfmt ", 1, 8, f);
  write_le32(f, 16);
  write_le16(f, 1);
  write_le16(f, (uint16_t)channels);
  write_le32(f, SAMPLE_RATE);
  write_le32(f, SAMPLE_RATE * (uint32_t)channels * 2u);
  write_le16(f, (uint16_t)(channels * 2));
  write_le16(f, 16);
  fwrite("data", 1, 4, f);
  write_le32(f, data_bytes);
}

static bool sink_open(PcmSink *sink, const char *output, const char *wav_path, int channels) {
  memset(sink, 0, sizeof(*sink));
  sink->channels = channels;
  if (!output || strcmp(output, "pipewire") == 0 || strcmp(output, "pulse") == 0) {
    // PipeWire provides a PulseAudio-compatible server on this setup. Using the
    // Pulse simple API keeps the final playback stream in-process (no pw-cat
    // subprocess) while still landing on PipeWire in normal desktops.
    pa_sample_spec spec = {
      .format = PA_SAMPLE_S16LE,
      .rate = SAMPLE_RATE,
      .channels = (uint8_t)channels,
    };
    pa_buffer_attr attr;
    memset(&attr, 0xff, sizeof(attr));
    attr.tlength = (uint32_t)(SAMPLE_RATE * channels * 2 * 80 / 1000); // ~80 ms target buffer.
    attr.prebuf = (uint32_t)-1;
    attr.minreq = (uint32_t)(SAMPLE_RATE * channels * 2 * 20 / 1000);
    int error = 0;
    sink->pulse = pa_simple_new(NULL, "discord-voice-engine", PA_STREAM_PLAYBACK, NULL,
                                "Discord voice playback", &spec, NULL, &attr, &error);
    if (!sink->pulse) {
      fprintf(stderr, "discord-voice-engine: Pulse/PipeWire output unavailable: %s\n", pa_strerror(error));
      return false;
    }
    sink->kind = OUTPUT_PULSE;
    return true;
  }
  if (strcmp(output, "null") == 0) {
    sink->kind = OUTPUT_NULL;
    return true;
  }
  if (strcmp(output, "wav") == 0) {
    if (!wav_path) {
      fprintf(stderr, "discord-voice-engine: --output wav requires --output-wav\n");
      return false;
    }
    sink->wav = fopen(wav_path, "wb+");
    if (!sink->wav) {
      perror("open output wav");
      return false;
    }
    wav_write_header(sink->wav, channels, 0);
    sink->kind = OUTPUT_WAV;
    return true;
  }
  fprintf(stderr, "discord-voice-engine: unsupported --output %s\n", output);
  return false;
}

static bool sink_write_frame(PcmSink *sink, const float *frame, size_t sample_count) {
  if (sink->kind == OUTPUT_NULL) return true;
  int16_t *pcm = (int16_t *)malloc(sample_count * sizeof(int16_t));
  if (!pcm) return false;
  for (size_t i = 0; i < sample_count; i++) pcm[i] = float_to_i16(frame[i]);
  bool ok = true;
  if (sink->kind == OUTPUT_PULSE) {
    int error = 0;
    if (pa_simple_write(sink->pulse, pcm, sample_count * sizeof(int16_t), &error) < 0) {
      fprintf(stderr, "discord-voice-engine: Pulse/PipeWire write failed: %s\n", pa_strerror(error));
      ok = false;
    }
  } else if (sink->kind == OUTPUT_WAV) {
    if (fwrite(pcm, sizeof(int16_t), sample_count, sink->wav) != sample_count) ok = false;
    sink->wav_samples += sample_count;
  }
  free(pcm);
  return ok;
}

static void sink_close(PcmSink *sink) {
  if (sink->kind == OUTPUT_PULSE && sink->pulse) {
    int error = 0;
    pa_simple_drain(sink->pulse, &error);
    pa_simple_free(sink->pulse);
    sink->pulse = NULL;
  }
  if (sink->kind == OUTPUT_WAV && sink->wav) {
    uint64_t bytes64 = sink->wav_samples * sizeof(int16_t);
    uint32_t bytes = bytes64 > 0xffffffffu ? 0xffffffffu : (uint32_t)bytes64;
    wav_write_header(sink->wav, sink->channels, bytes);
    fclose(sink->wav);
    sink->wav = NULL;
  }
}

typedef struct {
  char rtp_addr[256];
  int channels;
  int payload_type;
  int jitter_ms;
  int idle_timeout_ms;
  int max_plc_packets;
  int max_resync_gap;
  float gain_db;
  const char *output;
  const char *output_wav;
  const char *ready_file;
  const char *stats_json;
  int duration_ms;
  bool use_fec;
} PlaybackOptions;

static void options_init(PlaybackOptions *options) {
  memset(options, 0, sizeof(*options));
  snprintf(options->rtp_addr, sizeof(options->rtp_addr), "127.0.0.1:0");
  options->channels = DEFAULT_CHANNELS;
  options->payload_type = DEFAULT_PAYLOAD_TYPE;
  options->jitter_ms = DEFAULT_JITTER_MS;
  options->idle_timeout_ms = DEFAULT_IDLE_TIMEOUT_MS;
  options->max_plc_packets = DEFAULT_MAX_PLC_PACKETS;
  options->max_resync_gap = DEFAULT_MAX_RESYNC_GAP;
  options->gain_db = 0.0f;
  options->output = "pipewire";
  options->duration_ms = 0;
}

static void usage(FILE *f) {
  fprintf(f,
    "usage: discord-voice-engine play-rtp --rtp 127.0.0.1:PORT [options]\n"
    "       discord-voice-engine --self-test\n"
    "\n"
    "options:\n"
    "  --channels N              output channels, 1 or 2 (default 2)\n"
    "  --payload-type N          RTP payload type (default 120)\n"
    "  --jitter-ms N             initial jitter buffer delay (default 240)\n"
    "  --idle-timeout-ms N       end idle stream after timeout (default 350)\n"
    "  --max-plc-packets N       max PLC frames after stream dries up (default 10)\n"
    "  --max-resync-gap N        resync instead of PLC for huge sequence jumps (default 120)\n"
    "  --gain-db DB              initial global playback gain in dB (default 0)\n"
    "  --output pipewire|pulse|null|wav\n"
    "  --output-wav PATH         WAV output path when --output wav\n"
    "  --duration-ms N           stop after N milliseconds\n"
    "  --fec                     try in-band Opus FEC before PLC for isolated gaps\n"
    "  --stats-json PATH         write playback stats JSON on exit\n"
    "  --ready-file PATH         write a file after UDP bind/output init\n"
    "\n"
    "stdin controls (one command per line, read nonblocking):\n"
    "  user-volume SSRC PERCENT set an SSRC's volume; percent is clamped to 0..200\n"
    "  gain-db DB               set global playback gain in dB\n");
}

static bool parse_int_arg(const char *value, int *out) {
  char *end = NULL;
  long parsed = strtol(value, &end, 10);
  if (!value[0] || (end && *end)) return false;
  if (parsed < 0 || parsed > 100000000) return false;
  *out = (int)parsed;
  return true;
}

static bool parse_u32_arg(const char *value, uint32_t *out) {
  if (!value[0] || value[0] == '-' || value[0] == '+') return false;
  errno = 0;
  char *end = NULL;
  unsigned long long parsed = strtoull(value, &end, 10);
  if (errno != 0 || end == value || (end && *end) || parsed > UINT32_MAX) return false;
  *out = (uint32_t)parsed;
  return true;
}

static bool parse_float_arg(const char *value, float *out) {
  errno = 0;
  char *end = NULL;
  float parsed = strtof(value, &end);
  if (errno != 0 || end == value || (end && *end) || !isfinite(parsed)) return false;
  *out = parsed;
  return true;
}

static float gain_db_to_multiplier(float gain_db) {
  double multiplier = pow(10.0, (double)gain_db / 20.0);
  if (!isfinite(multiplier) || multiplier > FLT_MAX) return FLT_MAX;
  return (float)multiplier;
}

static void playback_controls_init(PlaybackControls *controls, float gain_db) {
  memset(controls, 0, sizeof(*controls));
  controls->gain_db = gain_db;
  controls->gain_multiplier = gain_db_to_multiplier(gain_db);
}

static void playback_controls_destroy(PlaybackControls *controls) {
  UserVolumeControl *entry = controls->user_volumes;
  while (entry) {
    UserVolumeControl *next = entry->next;
    free(entry);
    entry = next;
  }
  controls->user_volumes = NULL;
}

static bool playback_controls_set_user_volume(PlaybackControls *controls, uint32_t ssrc, float percent) {
  UserVolumeControl **cursor = &controls->user_volumes;
  while (*cursor && (*cursor)->ssrc != ssrc) cursor = &(*cursor)->next;

  if (percent == 100.0f) {
    if (*cursor) {
      UserVolumeControl *removed = *cursor;
      *cursor = removed->next;
      free(removed);
    }
    return true;
  }

  if (*cursor) {
    (*cursor)->percent = percent;
    return true;
  }

  UserVolumeControl *entry = (UserVolumeControl *)calloc(1, sizeof(*entry));
  if (!entry) return false;
  entry->ssrc = ssrc;
  entry->percent = percent;
  entry->next = controls->user_volumes;
  controls->user_volumes = entry;
  return true;
}

static float playback_controls_user_multiplier(const PlaybackControls *controls, uint32_t ssrc) {
  for (const UserVolumeControl *entry = controls->user_volumes; entry; entry = entry->next) {
    if (entry->ssrc == ssrc) return entry->percent / 100.0f;
  }
  return 1.0f;
}

static void playback_controls_set_gain_db(PlaybackControls *controls, float gain_db) {
  controls->gain_db = gain_db;
  controls->gain_multiplier = gain_db_to_multiplier(gain_db);
}

static bool parse_playback_control_line(const char *line, PlaybackControlCommand *out) {
  char command[32];
  char first[64];
  char second[64];
  char extra[2];
  memset(out, 0, sizeof(*out));
  int fields = sscanf(line, " %31s %63s %63s %1s", command, first, second, extra);

  if (fields == 3 && strcmp(command, "user-volume") == 0) {
    float percent = 0.0f;
    if (!parse_u32_arg(first, &out->ssrc) || !parse_float_arg(second, &percent)) return false;
    if (percent <= 0.0f) percent = 0.0f;
    if (percent > 200.0f) percent = 200.0f;
    out->kind = PLAYBACK_CONTROL_USER_VOLUME;
    out->value = percent;
    return true;
  }

  if (fields == 2 && strcmp(command, "gain-db") == 0) {
    if (!parse_float_arg(first, &out->value)) return false;
    out->kind = PLAYBACK_CONTROL_GAIN_DB;
    return true;
  }

  return false;
}

static void apply_playback_control_line(const char *line, PlaybackControls *controls) {
  PlaybackControlCommand command;
  if (!parse_playback_control_line(line, &command)) return;

  if (command.kind == PLAYBACK_CONTROL_USER_VOLUME) {
    if (!playback_controls_set_user_volume(controls, command.ssrc, command.value)) {
      fprintf(stderr, "discord-voice-engine play-rtp: unable to store user volume for SSRC %u\n", command.ssrc);
      return;
    }
    fprintf(stderr, "discord-voice-engine play-rtp: user volume for SSRC %u set to %.2f%%\n", command.ssrc, command.value);
  } else if (command.kind == PLAYBACK_CONTROL_GAIN_DB) {
    playback_controls_set_gain_db(controls, command.value);
    fprintf(stderr, "discord-voice-engine play-rtp: gain set to %.2f dB\n", command.value);
  }
}

static void control_input_finish_line(ControlInput *input, PlaybackControls *controls) {
  if (!input->discarding_line) {
    input->line[input->line_len] = '\0';
    apply_playback_control_line(input->line, controls);
  }
  input->line_len = 0;
  input->discarding_line = false;
}

static void control_input_consume(ControlInput *input, PlaybackControls *controls, const char *data, size_t len) {
  for (size_t i = 0; i < len; i++) {
    if (data[i] == '\n') {
      control_input_finish_line(input, controls);
    } else if (!input->discarding_line) {
      if (input->line_len + 1u < sizeof(input->line)) {
        input->line[input->line_len++] = data[i];
      } else {
        input->line_len = 0;
        input->discarding_line = true;
      }
    }
  }
}

static void control_input_init(ControlInput *input) {
  memset(input, 0, sizeof(*input));
  int flags = fcntl(STDIN_FILENO, F_GETFL, 0);
  if (flags < 0) {
    input->eof = true;
    return;
  }
  g_stdin_original_flags = flags;
  if ((flags & O_NONBLOCK) == 0) {
    if (fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK) < 0) {
      g_stdin_original_flags = -1;
      input->eof = true;
      return;
    }
    g_stdin_nonblocking_changed = true;
    if (!g_stdin_restore_registered) {
      if (atexit(restore_stdin_flags) == 0) g_stdin_restore_registered = true;
    }
  }
}

static void control_input_drain(ControlInput *input, PlaybackControls *controls) {
  if (input->eof) return;
  char buffer[CONTROL_READ_CAP];
  while (true) {
    ssize_t len = read(STDIN_FILENO, buffer, sizeof(buffer));
    if (len > 0) {
      control_input_consume(input, controls, buffer, (size_t)len);
      continue;
    }
    if (len == 0) {
      if (input->line_len > 0 || input->discarding_line) control_input_finish_line(input, controls);
      input->eof = true;
      return;
    }
    if (errno == EINTR) continue;
    if (errno == EAGAIN || errno == EWOULDBLOCK) return;
    input->eof = true;
    return;
  }
}

static float clamp_pcm_sample(double sample) {
  if (sample > 1.0) return 1.0f;
  if (sample < -1.0) return -1.0f;
  if (!isfinite(sample)) return 0.0f;
  return (float)sample;
}

static void mix_scaled_pcm(float *mix, const float *pcm, size_t count, float multiplier) {
  // Keep the intermediate mix unclipped so later global attenuation and
  // cancellation between participants retain their intended levels.
  for (size_t i = 0; i < count; i++) {
    mix[i] += pcm[i] * multiplier;
  }
}

static void apply_pcm_gain(float *pcm, size_t count, float multiplier) {
  for (size_t i = 0; i < count; i++) {
    pcm[i] = clamp_pcm_sample((double)pcm[i] * (double)multiplier);
  }
}

static bool parse_play_rtp_args(int argc, const char **argv, PlaybackOptions *options) {
  options_init(options);
  int i = 1;
  if (i < argc && strcmp(argv[i], "play-rtp") == 0) i++;
  for (; i < argc; i++) {
    const char *arg = argv[i];
    const char *value = (i + 1 < argc) ? argv[i + 1] : NULL;
    if (strcmp(arg, "--help") == 0 || strcmp(arg, "-h") == 0) {
      usage(stdout);
      exit(0);
    }
#define NEED_VALUE() do { if (!value) { fprintf(stderr, "%s requires a value\n", arg); return false; } } while (0)
    if (strcmp(arg, "--rtp") == 0) { NEED_VALUE(); snprintf(options->rtp_addr, sizeof(options->rtp_addr), "%s", value); i++; }
    else if (strcmp(arg, "--channels") == 0) { NEED_VALUE(); if (!parse_int_arg(value, &options->channels)) return false; i++; }
    else if (strcmp(arg, "--payload-type") == 0) { NEED_VALUE(); if (!parse_int_arg(value, &options->payload_type)) return false; i++; }
    else if (strcmp(arg, "--jitter-ms") == 0) { NEED_VALUE(); if (!parse_int_arg(value, &options->jitter_ms)) return false; i++; }
    else if (strcmp(arg, "--idle-timeout-ms") == 0) { NEED_VALUE(); if (!parse_int_arg(value, &options->idle_timeout_ms)) return false; i++; }
    else if (strcmp(arg, "--max-plc-packets") == 0) { NEED_VALUE(); if (!parse_int_arg(value, &options->max_plc_packets)) return false; i++; }
    else if (strcmp(arg, "--max-resync-gap") == 0) { NEED_VALUE(); if (!parse_int_arg(value, &options->max_resync_gap)) return false; i++; }
    else if (strcmp(arg, "--gain-db") == 0) { NEED_VALUE(); if (!parse_float_arg(value, &options->gain_db)) return false; i++; }
    else if (strcmp(arg, "--output") == 0) { NEED_VALUE(); options->output = value; i++; }
    else if (strcmp(arg, "--output-wav") == 0) { NEED_VALUE(); options->output_wav = value; i++; }
    else if (strcmp(arg, "--ready-file") == 0) { NEED_VALUE(); options->ready_file = value; i++; }
    else if (strcmp(arg, "--stats-json") == 0) { NEED_VALUE(); options->stats_json = value; i++; }
    else if (strcmp(arg, "--duration-ms") == 0) { NEED_VALUE(); if (!parse_int_arg(value, &options->duration_ms)) return false; i++; }
    else if (strcmp(arg, "--fec") == 0) { options->use_fec = true; }
    else {
      fprintf(stderr, "unknown argument: %s\n", arg);
      return false;
    }
#undef NEED_VALUE
  }
  if (options->channels < 1) options->channels = 1;
  if (options->channels > 2) options->channels = 2;
  if (options->payload_type < 0 || options->payload_type > 127) return false;
  return true;
}

static bool split_host_port(const char *addr, char *host, size_t host_cap, char *port, size_t port_cap) {
  const char *colon = strrchr(addr, ':');
  if (!colon || colon == addr || !colon[1]) return false;
  size_t host_len = (size_t)(colon - addr);
  if (host_len >= host_cap || strlen(colon + 1) >= port_cap) return false;
  memcpy(host, addr, host_len);
  host[host_len] = '\0';
  strcpy(port, colon + 1);
  return true;
}

static int bind_udp_socket(const char *addr) {
  char host[128], port[32];
  if (!split_host_port(addr, host, sizeof(host), port, sizeof(port))) {
    fprintf(stderr, "invalid --rtp address: %s\n", addr);
    return -1;
  }
  struct addrinfo hints;
  memset(&hints, 0, sizeof(hints));
  hints.ai_family = AF_INET;
  hints.ai_socktype = SOCK_DGRAM;
  hints.ai_flags = AI_PASSIVE;
  struct addrinfo *res = NULL;
  int gai = getaddrinfo(host, port, &hints, &res);
  if (gai != 0) {
    fprintf(stderr, "getaddrinfo(%s): %s\n", addr, gai_strerror(gai));
    return -1;
  }
  int fd = -1;
  for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
    fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
    if (fd < 0) continue;
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    if (bind(fd, ai->ai_addr, ai->ai_addrlen) == 0) break;
    close(fd);
    fd = -1;
  }
  freeaddrinfo(res);
  if (fd < 0) return -1;
  int flags = fcntl(fd, F_GETFL, 0);
  fcntl(fd, F_SETFL, flags | O_NONBLOCK);
  return fd;
}

static PlaybackStream *find_stream(PlaybackStream streams[MAX_STREAMS], uint32_t ssrc) {
  for (size_t i = 0; i < MAX_STREAMS; i++) {
    if (streams[i].active && streams[i].ssrc == ssrc) return &streams[i];
  }
  return NULL;
}

static PlaybackStream *create_stream(PlaybackStream streams[MAX_STREAMS], uint32_t ssrc, uint16_t first_sequence, uint32_t first_timestamp, int channels) {
  for (size_t i = 0; i < MAX_STREAMS; i++) {
    if (!streams[i].active) {
      if (!stream_init(&streams[i], ssrc, first_sequence, first_timestamp, channels)) return NULL;
      return &streams[i];
    }
  }
  return NULL;
}

static bool ingest_packet(const uint8_t *packet, size_t len, PlaybackOptions *options, PlaybackStream streams[MAX_STREAMS], PlaybackStats *stats) {
  ParsedRtp parsed;
  if (!parse_rtp_packet(packet, len, &parsed)) {
    stats->dropped_wrong_payload_packets++;
    return false;
  }
  stats->received_packets++;
  if (parsed.payload_type != (uint8_t)options->payload_type) {
    stats->dropped_wrong_payload_packets++;
    return false;
  }
  PlaybackStream *stream = find_stream(streams, parsed.ssrc);
  if (!stream) {
    stream = create_stream(streams, parsed.ssrc, parsed.sequence, parsed.timestamp, options->channels);
    if (!stream) {
      stats->dropped_ssrc_packets++;
      return false;
    }
    stats->streams_started++;
  }
  return stream_insert_packet(stream, &parsed, stats);
}

static void drain_socket_until(
    int fd,
    uint64_t deadline_ms,
    PlaybackOptions *options,
    PlaybackStream streams[MAX_STREAMS],
    PlaybackStats *stats,
    ControlInput *control_input,
    PlaybackControls *controls) {
  uint8_t buffer[MAX_PACKET_SIZE];
  while (g_running) {
    control_input_drain(control_input, controls);
    ssize_t len = recv(fd, buffer, sizeof(buffer), 0);
    if (len > 0) {
      ingest_packet(buffer, (size_t)len, options, streams, stats);
      continue;
    }
    if (len < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
      uint64_t now = monotonic_ms();
      if (now >= deadline_ms) return;
      uint64_t wait_ms = deadline_ms - now;
      if (wait_ms > 2) wait_ms = 2;
      fd_set rfds;
      FD_ZERO(&rfds);
      FD_SET(fd, &rfds);
      int max_fd = fd;
      if (!control_input->eof) {
        FD_SET(STDIN_FILENO, &rfds);
        if (STDIN_FILENO > max_fd) max_fd = STDIN_FILENO;
      }
      struct timeval tv;
      tv.tv_sec = (time_t)(wait_ms / 1000u);
      tv.tv_usec = (suseconds_t)((wait_ms % 1000u) * 1000u);
      int ready = select(max_fd + 1, &rfds, NULL, NULL, &tv);
      if (ready > 0 && !control_input->eof && FD_ISSET(STDIN_FILENO, &rfds)) {
        control_input_drain(control_input, controls);
      }
      continue;
    }
    if (len < 0 && errno == EINTR) continue;
    return;
  }
}

static bool any_active_streams(PlaybackStream streams[MAX_STREAMS]) {
  for (size_t i = 0; i < MAX_STREAMS; i++) if (streams[i].active) return true;
  return false;
}

static void write_stats_json(const char *path, const PlaybackStats *stats) {
  if (!path) return;
  FILE *f = fopen(path, "w");
  if (!f) return;
  fprintf(f,
    "{\n"
    "  \"mode\": \"%s\",\n"
    "  \"sample_rate\": %u,\n"
    "  \"channels\": %u,\n"
    "  \"received_packets\": %llu,\n"
    "  \"decoded_packets\": %llu,\n"
    "  \"normal_packets\": %llu,\n"
    "  \"concealed_packets\": %llu,\n"
    "  \"fec_attempts\": %llu,\n"
    "  \"sequence_gap_events\": %llu,\n"
    "  \"missing_packets\": %llu,\n"
    "  \"max_consecutive_missing_packets\": %llu,\n"
    "  \"duplicate_packets\": %llu,\n"
    "  \"out_of_order_packets\": %llu,\n"
    "  \"late_packets\": %llu,\n"
    "  \"dropped_wrong_payload_packets\": %llu,\n"
    "  \"dropped_ssrc_packets\": %llu,\n"
    "  \"decode_errors\": %llu,\n"
    "  \"decoder_resets\": %llu,\n"
    "  \"resync_events\": %llu,\n"
    "  \"streams_started\": %llu,\n"
    "  \"streams_ended\": %llu,\n"
    "  \"output_frames\": %llu,\n"
    "  \"silent_output_frames\": %llu,\n"
    "  \"output_duration_ms\": %llu,\n"
    "  \"output_underruns\": %llu,\n"
    "  \"opus_duration_packets\": {\"2_5ms\": %llu, \"5ms\": %llu, \"10ms\": %llu, \"20ms\": %llu, \"40ms\": %llu, \"60ms\": %llu, \"other\": %llu}\n"
    "}\n",
    stats->mode,
    stats->sample_rate,
    stats->channels,
    (unsigned long long)stats->received_packets,
    (unsigned long long)stats->decoded_packets,
    (unsigned long long)stats->normal_packets,
    (unsigned long long)stats->concealed_packets,
    (unsigned long long)stats->fec_attempts,
    (unsigned long long)stats->sequence_gap_events,
    (unsigned long long)stats->missing_packets,
    (unsigned long long)stats->max_consecutive_missing_packets,
    (unsigned long long)stats->duplicate_packets,
    (unsigned long long)stats->out_of_order_packets,
    (unsigned long long)stats->late_packets,
    (unsigned long long)stats->dropped_wrong_payload_packets,
    (unsigned long long)stats->dropped_ssrc_packets,
    (unsigned long long)stats->decode_errors,
    (unsigned long long)stats->decoder_resets,
    (unsigned long long)stats->resync_events,
    (unsigned long long)stats->streams_started,
    (unsigned long long)stats->streams_ended,
    (unsigned long long)stats->output_frames,
    (unsigned long long)stats->silent_output_frames,
    (unsigned long long)stats->output_duration_ms,
    (unsigned long long)stats->output_underruns,
    (unsigned long long)stats->opus_2_5ms_packets,
    (unsigned long long)stats->opus_5ms_packets,
    (unsigned long long)stats->opus_10ms_packets,
    (unsigned long long)stats->opus_20ms_packets,
    (unsigned long long)stats->opus_40ms_packets,
    (unsigned long long)stats->opus_60ms_packets,
    (unsigned long long)stats->opus_other_duration_packets);
  fclose(f);
}

static int play_rtp(PlaybackOptions *options) {
  PlaybackControls controls;
  playback_controls_init(&controls, options->gain_db);
  ControlInput control_input;
  // Configure stdin before opening the UDP socket. If the caller closed stdin,
  // socket() may reuse fd 0 and it must not be mistaken for the control stream.
  control_input_init(&control_input);

  int fd = bind_udp_socket(options->rtp_addr);
  if (fd < 0) die("failed to bind RTP socket %s: %s", options->rtp_addr, strerror(errno));

  PcmSink sink;
  if (!sink_open(&sink, options->output, options->output_wav, options->channels)) {
    playback_controls_destroy(&controls);
    restore_stdin_flags();
    close(fd);
    return 1;
  }
  if (options->ready_file) {
    FILE *ready = fopen(options->ready_file, "w");
    if (ready) {
      fputs("ready\n", ready);
      fclose(ready);
    }
  }

  PlaybackStream streams[MAX_STREAMS];
  memset(streams, 0, sizeof(streams));
  PlaybackStats stats;
  memset(&stats, 0, sizeof(stats));
  stats.mode = "play-rtp";
  stats.sample_rate = SAMPLE_RATE;
  stats.channels = (uint8_t)options->channels;

  uint64_t started_ms = monotonic_ms();
  uint64_t stop_ms = options->duration_ms > 0 ? started_ms + (uint64_t)options->duration_ms : 0;
  uint64_t last_stats_write_ms = started_ms;
  uint64_t next_tick_ms = 0;
  float *mix = (float *)calloc((size_t)FRAME_SAMPLES * (size_t)options->channels, sizeof(float));
  float *frame = (float *)calloc((size_t)FRAME_SAMPLES * (size_t)options->channels, sizeof(float));
  if (!mix || !frame) die("out of memory allocating mix buffers");

  while (g_running) {
    control_input_drain(&control_input, &controls);
    uint64_t now = monotonic_ms();
    if (stop_ms && now >= stop_ms) break;
    if (options->stats_json && now >= last_stats_write_ms + 1000u) {
      stats.output_duration_ms = (stats.output_frames * 1000u) / SAMPLE_RATE;
      write_stats_json(options->stats_json, &stats);
      last_stats_write_ms = now;
    }

    if (!any_active_streams(streams) && !next_tick_ms) {
      uint8_t packet[MAX_PACKET_SIZE];
      fd_set rfds;
      FD_ZERO(&rfds);
      FD_SET(fd, &rfds);
      int max_fd = fd;
      if (!control_input.eof) {
        FD_SET(STDIN_FILENO, &rfds);
        if (STDIN_FILENO > max_fd) max_fd = STDIN_FILENO;
      }
      struct timeval tv;
      tv.tv_sec = 0;
      tv.tv_usec = 5000;
      int ready = select(max_fd + 1, &rfds, NULL, NULL, &tv);
      if (ready > 0 && !control_input.eof && FD_ISSET(STDIN_FILENO, &rfds)) {
        control_input_drain(&control_input, &controls);
      }
      if (ready > 0 && FD_ISSET(fd, &rfds)) {
        ssize_t len = recv(fd, packet, sizeof(packet), 0);
        if (len > 0) {
          ingest_packet(packet, (size_t)len, options, streams, &stats);
          next_tick_ms = monotonic_ms() + (uint64_t)options->jitter_ms;
        }
      }
      continue;
    }

    if (!next_tick_ms) next_tick_ms = monotonic_ms() + (uint64_t)options->jitter_ms;
    drain_socket_until(fd, next_tick_ms, options, streams, &stats, &control_input, &controls);
    sleep_until_ms(next_tick_ms);
    now = monotonic_ms();

    size_t frame_samples = (size_t)FRAME_SAMPLES * (size_t)options->channels;
    memset(mix, 0, frame_samples * sizeof(float));
    size_t active = 0;
    for (size_t i = 0; i < MAX_STREAMS; i++) {
      if (!streams[i].active) continue;
      memset(frame, 0, frame_samples * sizeof(float));
      StreamFrameResult result = stream_next_frame(&streams[i], now, options->channels,
                                                   options->idle_timeout_ms,
                                                   options->max_plc_packets,
                                                   options->max_resync_gap,
                                                   options->use_fec,
                                                   &stats, frame);
      if (result == STREAM_FRAME_AUDIO) {
        active++;
        float user_multiplier = playback_controls_user_multiplier(&controls, streams[i].ssrc);
        mix_scaled_pcm(mix, frame, frame_samples, user_multiplier);
      } else if (result == STREAM_FRAME_ENDED) {
        stream_destroy(&streams[i]);
        stats.streams_ended++;
      }
    }
    // Once playout has started, keep feeding the Pulse/PipeWire stream every
    // tick even when no participant currently has decoded audio.  Stopping
    // writes during Discord talk-spurt gaps lets the server-side sink input
    // underflow and then resume, which can manifest as random loud pops/bangs.
    // Silence here is local output keepalive, not Opus PLC/audio synthesis.
    if (active > 0 || next_tick_ms) {
      apply_pcm_gain(mix, frame_samples, controls.gain_multiplier);
      if (!sink_write_frame(&sink, mix, frame_samples)) break;
      stats.output_frames += FRAME_SAMPLES;
      if (active == 0) stats.silent_output_frames += FRAME_SAMPLES;
    }
    next_tick_ms += FRAME_MS;
    if (next_tick_ms + 1000 < monotonic_ms()) next_tick_ms = monotonic_ms();
  }

  stats.output_duration_ms = (stats.output_frames * 1000u) / SAMPLE_RATE;
  write_stats_json(options->stats_json, &stats);
  fprintf(stderr,
          "discord-voice-engine: received %llu packet(s), decoded %llu, concealed %llu, missing %llu, late %llu, decoder_resets %llu, errors %llu\n",
          (unsigned long long)stats.received_packets,
          (unsigned long long)stats.normal_packets,
          (unsigned long long)stats.concealed_packets,
          (unsigned long long)stats.missing_packets,
          (unsigned long long)stats.late_packets,
          (unsigned long long)stats.decoder_resets,
          (unsigned long long)stats.decode_errors);

  for (size_t i = 0; i < MAX_STREAMS; i++) stream_destroy(&streams[i]);
  free(mix);
  free(frame);
  playback_controls_destroy(&controls);
  restore_stdin_flags();
  sink_close(&sink);
  close(fd);
  return 0;
}

static void build_rtp_packet(uint8_t *out, size_t *out_len, uint8_t payload_type, uint16_t seq, uint32_t ts, uint32_t ssrc, const uint8_t *payload, size_t payload_len) {
  out[0] = 0x80;
  out[1] = payload_type & 0x7f;
  out[2] = (uint8_t)(seq >> 8);
  out[3] = (uint8_t)(seq & 0xff);
  out[4] = (uint8_t)(ts >> 24);
  out[5] = (uint8_t)(ts >> 16);
  out[6] = (uint8_t)(ts >> 8);
  out[7] = (uint8_t)(ts & 0xff);
  out[8] = (uint8_t)(ssrc >> 24);
  out[9] = (uint8_t)(ssrc >> 16);
  out[10] = (uint8_t)(ssrc >> 8);
  out[11] = (uint8_t)(ssrc & 0xff);
  memcpy(out + RTP_HEADER_LEN, payload, payload_len);
  *out_len = RTP_HEADER_LEN + payload_len;
}

static void test_assert(bool condition, const char *message) {
  if (!condition) die("self-test failed: %s", message);
}

static void encode_test_tone_packet(
    int channels,
    int frames_20ms,
    float amplitude,
    float frequency,
    bool invert_right,
    uint8_t *payload,
    opus_int32 *payload_len) {
  int err = OPUS_OK;
  OpusEncoder *encoder = opus_encoder_create(SAMPLE_RATE, channels, OPUS_APPLICATION_AUDIO, &err);
  if (!encoder || err != OPUS_OK) die("self-test: failed to create Opus encoder");
  opus_encoder_ctl(encoder, OPUS_SET_BITRATE(160000));
  int samples_per_channel = FRAME_SAMPLES * frames_20ms;
  float *pcm = (float *)calloc((size_t)samples_per_channel * (size_t)channels, sizeof(float));
  if (!pcm) die("self-test: out of memory");
  for (int n = 0; n < samples_per_channel; n++) {
    float t = (float)n / (float)SAMPLE_RATE;
    float sample = sinf(t * frequency * 6.28318530718f) * amplitude;
    for (int ch = 0; ch < channels; ch++) {
      pcm[(size_t)n * (size_t)channels + (size_t)ch] = invert_right && ch == 1 ? -sample : sample;
    }
  }
  *payload_len = opus_encode_float(encoder, pcm, samples_per_channel, payload, MAX_PAYLOAD_SIZE);
  if (*payload_len <= 0) die("self-test: failed to encode Opus packet");
  free(pcm);
  opus_encoder_destroy(encoder);
}

static void encode_tone_packet(int channels, int frames_20ms, uint8_t *payload, opus_int32 *payload_len) {
  encode_test_tone_packet(channels, frames_20ms, 0.20f, 330.0f, false, payload, payload_len);
}

static void run_self_tests(void) {
  uint8_t rtp[MAX_PACKET_SIZE];
  size_t rtp_len = 0;
  uint8_t payload[] = {1, 2, 3, 4};
  build_rtp_packet(rtp, &rtp_len, DEFAULT_PAYLOAD_TYPE, 0x1234, 0x10203040u, 0x55667788u, payload, sizeof(payload));
  ParsedRtp parsed;
  test_assert(parse_rtp_packet(rtp, rtp_len, &parsed), "parse basic RTP");
  test_assert(parsed.sequence == 0x1234, "RTP sequence");
  test_assert(parsed.timestamp == 0x10203040u, "RTP timestamp");
  test_assert(parsed.ssrc == 0x55667788u, "RTP SSRC");
  test_assert(parsed.payload_len == sizeof(payload), "RTP payload length");

  test_assert(float_to_i16(NAN) == 0, "non-finite PCM converts to silence safely");

  PlaybackControlCommand control_command;
  test_assert(parse_playback_control_line("user-volume 1432778632 75", &control_command), "parse user-volume control");
  test_assert(control_command.kind == PLAYBACK_CONTROL_USER_VOLUME, "user-volume control kind");
  test_assert(control_command.ssrc == 1432778632u, "user-volume SSRC");
  test_assert(control_command.value == 75.0f, "user-volume percent");
  test_assert(parse_playback_control_line("user-volume 4294967295 -10", &control_command), "parse minimum-clamped user-volume");
  test_assert(control_command.ssrc == UINT32_MAX, "maximum SSRC accepted");
  test_assert(control_command.value == 0.0f, "user-volume clamps below zero");
  test_assert(parse_playback_control_line("user-volume 1 250.5", &control_command), "parse maximum-clamped user-volume");
  test_assert(control_command.value == 200.0f, "user-volume clamps above 200");
  test_assert(!parse_playback_control_line("user-volume 4294967296 100", &control_command), "reject out-of-range SSRC");
  test_assert(!parse_playback_control_line("user-volume 1 NaN", &control_command), "reject non-finite user-volume");
  test_assert(!parse_playback_control_line("user-volume 1 50 trailing", &control_command), "reject extra user-volume fields");
  test_assert(parse_playback_control_line("gain-db -6.25", &control_command), "parse gain-db control");
  test_assert(control_command.kind == PLAYBACK_CONTROL_GAIN_DB, "gain-db control kind");
  test_assert(control_command.value == -6.25f, "gain-db value");
  test_assert(!parse_playback_control_line("gain-db inf", &control_command), "reject non-finite gain-db");

  PlaybackOptions parsed_options;
  const char *gain_argv[] = {"discord-voice-engine", "play-rtp", "--rtp", "127.0.0.1:1234", "--gain-db", "3.5"};
  test_assert(parse_play_rtp_args((int)(sizeof(gain_argv) / sizeof(gain_argv[0])), gain_argv, &parsed_options), "parse initial --gain-db");
  test_assert(parsed_options.gain_db == 3.5f, "initial --gain-db value");

  PlaybackControls controls;
  playback_controls_init(&controls, 0.0f);
  test_assert(playback_controls_user_multiplier(&controls, 77) == 1.0f, "unconfigured SSRC defaults to 100 percent");
  test_assert(playback_controls_set_user_volume(&controls, 77, 25.0f), "store SSRC volume");
  test_assert(playback_controls_user_multiplier(&controls, 77) == 0.25f, "stored SSRC volume multiplier");
  test_assert(playback_controls_user_multiplier(&controls, 78) == 1.0f, "other SSRC keeps default volume");

  float source_pcm[] = {0.4f, -0.4f, 0.75f};
  float mixed_pcm[] = {0.0f, 0.0f, 0.0f};
  mix_scaled_pcm(mixed_pcm, source_pcm, sizeof(source_pcm) / sizeof(source_pcm[0]),
                 playback_controls_user_multiplier(&controls, 77));
  test_assert(fabsf(mixed_pcm[0] - 0.1f) < 0.0001f, "per-SSRC PCM scales before mixing");
  test_assert(fabsf(mixed_pcm[1] + 0.1f) < 0.0001f, "negative per-SSRC PCM scales before mixing");
  playback_controls_set_gain_db(&controls, 6.0206f);
  test_assert(fabsf(controls.gain_multiplier - 2.0f) < 0.0001f, "dB gain converts to linear multiplier");
  apply_pcm_gain(mixed_pcm, sizeof(mixed_pcm) / sizeof(mixed_pcm[0]), controls.gain_multiplier);
  test_assert(fabsf(mixed_pcm[0] - 0.2f) < 0.0001f, "global gain scales mixed PCM");
  float clipping_mix[] = {0.0f};
  float clipping_source[] = {0.75f};
  mix_scaled_pcm(clipping_mix, clipping_source, 1, 2.0f);
  test_assert(fabsf(clipping_mix[0] - 1.5f) < 0.0001f, "per-user boost remains unclipped before global gain");
  apply_pcm_gain(clipping_mix, 1, 0.5f);
  test_assert(fabsf(clipping_mix[0] - 0.75f) < 0.0001f, "global attenuation preserves boosted source level");
  float cancellation_mix[] = {0.0f};
  float positive_source[] = {0.75f};
  float negative_source[] = {-0.75f};
  mix_scaled_pcm(cancellation_mix, positive_source, 1, 2.0f);
  mix_scaled_pcm(cancellation_mix, negative_source, 1, 2.0f);
  apply_pcm_gain(cancellation_mix, 1, 1.0f);
  test_assert(fabsf(cancellation_mix[0]) < 0.0001f, "unclipped sources can cancel before output");

  int stdin_flags_before = fcntl(STDIN_FILENO, F_GETFL, 0);
  if (stdin_flags_before >= 0) {
    ControlInput live_input;
    control_input_init(&live_input);
    int stdin_flags_during = fcntl(STDIN_FILENO, F_GETFL, 0);
    test_assert(stdin_flags_during >= 0 && (stdin_flags_during & O_NONBLOCK) != 0, "stdin control enables nonblocking reads");
    restore_stdin_flags();
    test_assert(fcntl(STDIN_FILENO, F_GETFL, 0) == stdin_flags_before, "stdin flags are restored after playback");
  }

  ControlInput test_input;
  memset(&test_input, 0, sizeof(test_input));
  const char *partial_control = "user-volume 88 2";
  control_input_consume(&test_input, &controls, partial_control, strlen(partial_control));
  test_assert(playback_controls_user_multiplier(&controls, 88) == 1.0f, "partial stdin control waits for newline");
  const char *completed_controls = "50\ngain-db -3\n";
  control_input_consume(&test_input, &controls, completed_controls, strlen(completed_controls));
  test_assert(playback_controls_user_multiplier(&controls, 88) == 2.0f, "stdin control clamps and updates SSRC volume");
  test_assert(controls.gain_db == -3.0f, "stdin control updates runtime gain");
  test_assert(playback_controls_set_user_volume(&controls, 77, 100.0f), "reset SSRC volume to default");
  test_assert(playback_controls_user_multiplier(&controls, 77) == 1.0f, "100 percent removes SSRC override");
  playback_controls_destroy(&controls);

  int channels = 2;
  PlaybackStats stats;
  PlaybackStream stream;
  uint8_t opus_payload[MAX_PAYLOAD_SIZE];
  opus_int32 opus_len = 0;
  encode_tone_packet(channels, 3, opus_payload, &opus_len);
  build_rtp_packet(rtp, &rtp_len, DEFAULT_PAYLOAD_TYPE, 77, 0, 99, opus_payload, (size_t)opus_len);
  test_assert(parse_rtp_packet(rtp, rtp_len, &parsed), "parse encoded RTP");
  memset(&stats, 0, sizeof(stats));
  test_assert(stream_init(&stream, 99, 77, 0, channels), "init stream");
  test_assert(stream_insert_packet(&stream, &parsed, &stats), "insert 60ms packet");
  float frame[FRAME_SAMPLES * 2];
  for (int i = 0; i < 3; i++) {
    StreamFrameResult result = stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame);
    test_assert(result == STREAM_FRAME_AUDIO, "60ms packet yields three frames");
    bool nonzero = false;
    for (size_t s = 0; s < sizeof(frame) / sizeof(frame[0]); s++) if (fabsf(frame[s]) > 0.0001f) { nonzero = true; break; }
    test_assert(nonzero, "decoded frame has energy");
  }
  test_assert(stats.normal_packets == 1, "60ms packet decoded once");
  test_assert(stats.concealed_packets == 0, "60ms packet no PLC");
  test_assert(stats.opus_60ms_packets == 1, "60ms histogram");
  test_assert(stream.pending.len == 0, "60ms pending drained");
  stream_destroy(&stream);

  // Loud anti-phase stereo is valid audio and is common in mastered music. A
  // previous output-statistics heuristic treated interleaved left/right samples
  // as one waveform, classified this as corruption, and muted the whole frame.
  uint8_t loud_opus[MAX_PAYLOAD_SIZE];
  opus_int32 loud_opus_len = 0;
  encode_test_tone_packet(channels, 1, 0.99f, 440.0f, true, loud_opus, &loud_opus_len);
  build_rtp_packet(rtp, &rtp_len, DEFAULT_PAYLOAD_TYPE, 88, 0, 100, loud_opus, (size_t)loud_opus_len);
  test_assert(parse_rtp_packet(rtp, rtp_len, &parsed), "parse loud stereo RTP");
  memset(&stats, 0, sizeof(stats));
  test_assert(stream_init(&stream, 100, 88, 0, channels), "init loud stereo stream");
  test_assert(stream_insert_packet(&stream, &parsed, &stats), "insert loud stereo packet");
  test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_AUDIO, "loud stereo packet yields audio");
  double loud_sum_sq = 0.0;
  for (size_t s = 0; s < sizeof(frame) / sizeof(frame[0]); s++) loud_sum_sq += (double)frame[s] * (double)frame[s];
  double loud_rms = sqrt(loud_sum_sq / (double)(sizeof(frame) / sizeof(frame[0])));
  test_assert(loud_rms > 0.20, "valid loud stereo is not replaced with silence");
  test_assert(stats.decoder_resets == 0, "valid loud stereo does not reset decoder");
  stream_destroy(&stream);

  memset(&stats, 0, sizeof(stats));
  test_assert(stream_init(&stream, 42, 10, 0, channels), "init reorder stream");
  uint8_t opus20a[MAX_PAYLOAD_SIZE], opus20b[MAX_PAYLOAD_SIZE], opus20c[MAX_PAYLOAD_SIZE];
  opus_int32 len20a, len20b, len20c;
  encode_tone_packet(channels, 1, opus20a, &len20a);
  encode_tone_packet(channels, 1, opus20b, &len20b);
  encode_tone_packet(channels, 1, opus20c, &len20c);
  uint8_t pkt10[MAX_PACKET_SIZE], pkt11[MAX_PACKET_SIZE], pkt12[MAX_PACKET_SIZE];
  size_t pkt10_len, pkt11_len, pkt12_len;
  build_rtp_packet(pkt11, &pkt11_len, DEFAULT_PAYLOAD_TYPE, 11, 960, 42, opus20b, (size_t)len20b);
  build_rtp_packet(pkt10, &pkt10_len, DEFAULT_PAYLOAD_TYPE, 10, 0, 42, opus20a, (size_t)len20a);
  build_rtp_packet(pkt12, &pkt12_len, DEFAULT_PAYLOAD_TYPE, 12, 1920, 42, opus20c, (size_t)len20c);
  parse_rtp_packet(pkt11, pkt11_len, &parsed); stream_insert_packet(&stream, &parsed, &stats);
  parse_rtp_packet(pkt10, pkt10_len, &parsed); stream_insert_packet(&stream, &parsed, &stats);
  parse_rtp_packet(pkt12, pkt12_len, &parsed); stream_insert_packet(&stream, &parsed, &stats);
  for (int i = 0; i < 3; i++) {
    test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_AUDIO, "reordered packet output");
  }
  test_assert(stats.normal_packets == 3, "reordered packets decoded");
  test_assert(stats.missing_packets == 0, "reordered packets no loss");
  stream_destroy(&stream);

  memset(&stats, 0, sizeof(stats));
  test_assert(stream_init(&stream, 43, 1, 0, channels), "init loss stream");
  build_rtp_packet(pkt10, &pkt10_len, DEFAULT_PAYLOAD_TYPE, 1, 0, 43, opus20a, (size_t)len20a);
  build_rtp_packet(pkt12, &pkt12_len, DEFAULT_PAYLOAD_TYPE, 3, 1920, 43, opus20c, (size_t)len20c);
  parse_rtp_packet(pkt10, pkt10_len, &parsed); stream_insert_packet(&stream, &parsed, &stats);
  parse_rtp_packet(pkt12, pkt12_len, &parsed); stream_insert_packet(&stream, &parsed, &stats);
  test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_AUDIO, "loss first packet");
  test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_AUDIO, "loss silence packet");
  bool silent = true;
  for (size_t s = 0; s < sizeof(frame) / sizeof(frame[0]); s++) if (fabsf(frame[s]) > 0.000001f) { silent = false; break; }
  test_assert(silent, "loss concealment is silence");
  test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_AUDIO, "loss future packet");
  test_assert(stats.normal_packets == 2, "loss normal decode count");
  test_assert(stats.concealed_packets == 1, "loss silence concealment count");
  test_assert(stats.missing_packets == 1, "loss missing count");
  test_assert(stats.decoder_resets == 1, "loss resets stale Opus predictor state");
  stream_destroy(&stream);

  memset(&stats, 0, sizeof(stats));
  test_assert(stream_init(&stream, 44, 1, 0, channels), "init sequence-only gap stream");
  build_rtp_packet(pkt10, &pkt10_len, DEFAULT_PAYLOAD_TYPE, 1, 0, 44, opus20a, (size_t)len20a);
  build_rtp_packet(pkt12, &pkt12_len, DEFAULT_PAYLOAD_TYPE, 3, 960, 44, opus20c, (size_t)len20c);
  parse_rtp_packet(pkt10, pkt10_len, &parsed); stream_insert_packet(&stream, &parsed, &stats);
  parse_rtp_packet(pkt12, pkt12_len, &parsed); stream_insert_packet(&stream, &parsed, &stats);
  test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_AUDIO, "sequence-only gap first packet");
  test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_AUDIO, "sequence-only gap future packet");
  test_assert(stats.normal_packets == 2, "sequence-only gap decodes both packets");
  test_assert(stats.concealed_packets == 0, "sequence-only gap no PLC");
  test_assert(stats.missing_packets == 0, "sequence-only gap no missing audio");
  stream_destroy(&stream);

  memset(&stats, 0, sizeof(stats));
  test_assert(stream_init(&stream, 45, 1, 0, channels), "init dry stream");
  build_rtp_packet(pkt10, &pkt10_len, DEFAULT_PAYLOAD_TYPE, 1, 0, 45, opus20a, (size_t)len20a);
  parse_rtp_packet(pkt10, pkt10_len, &parsed); stream_insert_packet(&stream, &parsed, &stats);
  test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_AUDIO, "dry stream first packet");
  test_assert(stream_next_frame(&stream, monotonic_ms(), channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_NONE, "dry stream waits without tail PLC");
  test_assert(stats.normal_packets == 1, "dry stream decoded one packet");
  test_assert(stats.concealed_packets == 0, "dry stream no tail PLC");
  test_assert(stats.missing_packets == 0, "dry stream no false missing audio");
  test_assert(stream_next_frame(&stream, stream.last_receive_ms + 1001u, channels, 1000, 10, DEFAULT_MAX_RESYNC_GAP, false, &stats, frame) == STREAM_FRAME_ENDED, "dry stream ends after idle timeout");
  stream_destroy(&stream);

  fprintf(stderr, "discord-voice-engine self-test: ok\n");
}

int discord_voice_engine_c_playback_self_test(void) {
  run_self_tests();
  return 0;
}

int discord_voice_engine_c_play_rtp_main(int argc, const char **argv) {
  signal(SIGINT, on_signal);
  signal(SIGTERM, on_signal);

  if (argc >= 2 && strcmp(argv[1], "--self-test") == 0) {
    return discord_voice_engine_c_playback_self_test();
  }
  if (argc < 2 || strcmp(argv[1], "--help") == 0 || strcmp(argv[1], "-h") == 0) {
    usage(argc < 2 ? stderr : stdout);
    return argc < 2 ? 1 : 0;
  }

  PlaybackOptions options;
  if (!parse_play_rtp_args(argc, argv, &options)) {
    usage(stderr);
    return 1;
  }
  return play_rtp(&options);
}
