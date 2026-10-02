/*
 * avbridge_ref: reference loudness through libavfilter, used only by tests.
 *
 * Runs FFmpeg's own `ebur128` filter (an independent BS.1770 / EBU R128
 * implementation) in-process, so tests can compare our measurement with it
 * without spawning an ffmpeg process. Built as a separate static library so
 * the application itself never links against libavfilter.
 */
#include "avbridge.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <libavfilter/avfilter.h>
#include <libavfilter/buffersink.h>
#include <libavfilter/buffersrc.h>
#include <libavutil/channel_layout.h>
#include <libavutil/dict.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/mem.h>

#define REF_BLOCK_FRAMES 8192

typedef struct AvbRefLoudness {
    double integrated;   /* LUFS */
    double range;        /* LU */
    double true_peak;    /* linear amplitude, highest channel */
} AvbRefLoudness;

static int ref_fail(char *err, const char *what, int code)
{
    char detail[128] = "";
    if (code < 0)
        av_strerror(code, detail, sizeof(detail));
    snprintf(err, AVB_ERR_LEN, "%s%s%s", what, detail[0] ? ": " : "", detail);
    return code < 0 ? code : -1;
}

static double meta_number(const AVFrame *frame, const char *key, int *found)
{
    const AVDictionaryEntry *entry = av_dict_get(frame->metadata, key, NULL, 0);
    if (!entry) {
        *found = 0;
        return 0.0;
    }
    *found = 1;
    return atof(entry->value);
}

/* Reads the metadata the ebur128 filter attaches to each frame. Integrated
 * loudness and loudness range are running values, so the last frame wins;
 * true peaks are running per-channel maxima, so the highest value wins. */
static void collect(AVFrame *frame, int channels, AvbRefLoudness *out, int *have_peak)
{
    int found;
    double v = meta_number(frame, "lavfi.r128.I", &found);
    if (found)
        out->integrated = v;
    v = meta_number(frame, "lavfi.r128.LRA", &found);
    if (found)
        out->range = v;
    for (int ch = 0; ch < channels; ch++) {
        char key[40];
        snprintf(key, sizeof(key), "lavfi.r128.true_peaks_ch%d", ch);
        v = meta_number(frame, key, &found);
        if (found && (!*have_peak || v > out->true_peak)) {
            out->true_peak = v;
            *have_peak = 1;
        }
    }
}

static int drain(AVFilterContext *sink, int channels, AvbRefLoudness *out, int *have_peak)
{
    AVFrame *frame = av_frame_alloc();
    if (!frame)
        return AVERROR(ENOMEM);
    int ret;
    while ((ret = av_buffersink_get_frame(sink, frame)) >= 0) {
        collect(frame, channels, out, have_peak);
        av_frame_unref(frame);
    }
    av_frame_free(&frame);
    return (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) ? 0 : ret;
}

int avb_ref_loudness(const char *path, AvbRefLoudness *out, char *err)
{
    AvbMediaInfo info;
    if (avb_probe(path, 0, &info, err) < 0)
        return -1;

    AvbDecoder *decoder = avb_decoder_open(path, 0, 0, err);
    if (!decoder)
        return -1;

    AVChannelLayout layout = {0};
    av_channel_layout_default(&layout, info.channels);
    char layout_name[64];
    av_channel_layout_describe(&layout, layout_name, sizeof(layout_name));

    AVFilterGraph *graph = avfilter_graph_alloc();
    AVFilterContext *src = NULL, *meter = NULL, *sink = NULL;
    float *pcm = av_malloc_array((size_t)REF_BLOCK_FRAMES * info.channels, sizeof(float));
    int ret = 0, have_peak = 0;
    memset(out, 0, sizeof(*out));
    if (!graph || !pcm) {
        ret = ref_fail(err, "memória insuficiente", AVERROR(ENOMEM));
        goto done;
    }

    char args[256];
    snprintf(args, sizeof(args), "time_base=1/%d:sample_rate=%d:sample_fmt=flt:channel_layout=%s",
             info.sample_rate, info.sample_rate, layout_name);
    ret = avfilter_graph_create_filter(&src, avfilter_get_by_name("abuffer"), "in", args, NULL, graph);
    if (ret >= 0)
        ret = avfilter_graph_create_filter(&meter, avfilter_get_by_name("ebur128"), "meter",
                                           "peak=true:metadata=1", NULL, graph);
    if (ret >= 0)
        ret = avfilter_graph_create_filter(&sink, avfilter_get_by_name("abuffersink"), "out", NULL, NULL, graph);
    if (ret >= 0)
        ret = avfilter_link(src, 0, meter, 0);
    if (ret >= 0)
        ret = avfilter_link(meter, 0, sink, 0);
    if (ret >= 0)
        ret = avfilter_graph_config(graph, NULL);
    if (ret < 0) {
        ret = ref_fail(err, "não foi possível montar o filtro ebur128", ret);
        goto done;
    }

    int64_t pts = 0;
    for (;;) {
        int frames = avb_decoder_read(decoder, pcm, REF_BLOCK_FRAMES, err);
        if (frames < 0) {
            ret = -1;
            goto done;
        }
        if (frames == 0)
            break;
        AVFrame *frame = av_frame_alloc();
        if (!frame) {
            ret = ref_fail(err, "memória insuficiente", AVERROR(ENOMEM));
            goto done;
        }
        frame->nb_samples = frames;
        frame->format = AV_SAMPLE_FMT_FLT;
        frame->sample_rate = info.sample_rate;
        frame->pts = pts;
        pts += frames;
        av_channel_layout_copy(&frame->ch_layout, &layout);
        ret = av_frame_get_buffer(frame, 0);
        if (ret >= 0) {
            memcpy(frame->data[0], pcm, (size_t)frames * info.channels * sizeof(float));
            ret = av_buffersrc_add_frame(src, frame);
        }
        av_frame_free(&frame);
        if (ret >= 0)
            ret = drain(sink, info.channels, out, &have_peak);
        if (ret < 0) {
            ret = ref_fail(err, "falha ao medir com o filtro ebur128", ret);
            goto done;
        }
    }
    ret = av_buffersrc_add_frame(src, NULL);
    if (ret >= 0)
        ret = drain(sink, info.channels, out, &have_peak);
    if (ret < 0)
        ret = ref_fail(err, "falha ao encerrar o filtro ebur128", ret);

done:
    av_freep(&pcm);
    avfilter_graph_free(&graph);
    av_channel_layout_uninit(&layout);
    avb_decoder_close(decoder);
    return ret < 0 ? ret : 0;
}
