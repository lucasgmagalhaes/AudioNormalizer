/*
 * avbridge: minimal C bridge over libavformat / libavcodec / libswresample.
 *
 * Exposes three things to Rust:
 *   - probe:   basic media info for the first audio track
 *   - decoder: first audio track decoded to interleaved float PCM
 *   - remuxer: copies every other stream of the source into a new file and
 *              encodes PCM we hand it as the replacement for that audio track
 *
 * Every function taking `err` writes a human readable message into it
 * (AVB_ERR_LEN bytes) on failure. Paths are UTF-8.
 */
#ifndef AVBRIDGE_H
#define AVBRIDGE_H

#include <stdint.h>

#define AVB_ERR_LEN 512

typedef struct AvbMediaInfo {
    double duration;   /* seconds, 0 when unknown */
    int has_video;     /* 1 when a real (non cover-art) video stream exists */
    int sample_rate;   /* PCM rate produced by the decoder */
    int channels;      /* PCM channel count produced by the decoder */
    int64_t bit_rate;  /* source audio bit rate, 0 when unknown */
    char codec[32];    /* source audio codec name */
} AvbMediaInfo;

void avb_init(void);

int avb_probe(const char *path, AvbMediaInfo *info, char *err);

/* The first audio track's compressed packets kept in memory, so it can be
 * decoded again without reading the (possibly huge) file. Read-only once
 * taken from a decoder; safe to share between decoders. */
typedef struct AvbAudioCache AvbAudioCache;

void avb_cache_free(AvbAudioCache *cache);
int64_t avb_cache_bytes(const AvbAudioCache *cache);

typedef struct AvbDecoder AvbDecoder;

/* With `record_limit` > 0 the packets read are also recorded, up to that
 * many bytes; see avb_decoder_take_cache. */
AvbDecoder *avb_decoder_open(const char *path, int64_t record_limit, char *err);
/* Decodes from a cache instead of the file. The cache must outlive it. */
AvbDecoder *avb_decoder_open_cache(const AvbAudioCache *cache, char *err);
/* After the whole track was read: hands over the recording (caller frees),
 * or NULL when not recording, not finished, or over the limit. */
AvbAudioCache *avb_decoder_take_cache(AvbDecoder *dec);
/* Reads up to `max_frames` interleaved frames. Returns the number of frames
 * read, 0 at end of stream, negative on error. */
int avb_decoder_read(AvbDecoder *dec, float *out, int max_frames, char *err);
void avb_decoder_close(AvbDecoder *dec);

typedef struct AvbRemuxer AvbRemuxer;

/* `encoder_options` is an optional "key=value:key=value" list of private
 * encoder options (e.g. "aac_coder=fast"); NULL or "" for defaults. */
AvbRemuxer *avb_remuxer_open(const char *input, const char *output,
                             const char *encoder_options, char *err);
/* Name of the audio encoder in use, e.g. "aac" or "libopus". */
const char *avb_remuxer_encoder(const AvbRemuxer *mux);
/* `samples` must match the decoder's format: interleaved float, same rate
 * and channel count as reported by avb_probe. */
int avb_remuxer_write(AvbRemuxer *mux, const float *samples, int frames, char *err);
/* Format of the monitor PCM (the encoded audio decoded back), interleaved
 * float. Returns -1 when no monitor decoder is available. */
int avb_remuxer_monitor_format(const AvbRemuxer *mux, int *sample_rate, int *channels);
/* Drains up to `max_frames` monitor frames produced by previous writes (or
 * by finish). Encoder priming is already removed. Returns frames read. */
int avb_remuxer_read_monitor(AvbRemuxer *mux, float *out, int max_frames);
/* Flushes the encoder, copies the remaining packets, writes the trailer. */
int avb_remuxer_finish(AvbRemuxer *mux, char *err);
void avb_remuxer_close(AvbRemuxer *mux);

#endif
