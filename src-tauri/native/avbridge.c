#include "avbridge.h"

#include <stdio.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/audio_fifo.h>
#include <libavutil/channel_layout.h>
#include <libavutil/error.h>
#include <libavutil/mathematics.h>
#include <libavutil/opt.h>
#include <libavutil/samplefmt.h>
#include <libswresample/swresample.h>

/* ------------------------------------------------------------------------ */
/* helpers                                                                   */
/* ------------------------------------------------------------------------ */

static int fail(char *err, const char *what, int code)
{
    char reason[AV_ERROR_MAX_STRING_SIZE] = {0};
    if (code < 0) {
        av_strerror(code, reason, sizeof(reason));
        snprintf(err, AVB_ERR_LEN, "%s: %s", what, reason);
        return code;
    }
    snprintf(err, AVB_ERR_LEN, "%s", what);
    return AVERROR_UNKNOWN;
}

void avb_init(void)
{
    av_log_set_level(AV_LOG_ERROR);
}

static int open_input(const char *path, AVFormatContext **fmt, char *err)
{
    int ret = avformat_open_input(fmt, path, NULL, NULL);
    if (ret < 0)
        return fail(err, "não foi possível abrir o arquivo", ret);
    ret = avformat_find_stream_info(*fmt, NULL);
    if (ret < 0) {
        avformat_close_input(fmt);
        return fail(err, "não foi possível ler as faixas do arquivo", ret);
    }
    return 0;
}

/* The first audio track is the one we normalize (same as `-map 0:a:0`). */
static int first_audio_stream(const AVFormatContext *fmt)
{
    for (unsigned i = 0; i < fmt->nb_streams; i++)
        if (fmt->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_AUDIO)
            return (int)i;
    return -1;
}

/* Layout the PCM is exchanged in: the source layout, or the default layout
 * for its channel count when the source does not specify one. */
static int pcm_layout(const AVChannelLayout *src, AVChannelLayout *dst)
{
    if (src->order == AV_CHANNEL_ORDER_UNSPEC || src->nb_channels <= 0) {
        av_channel_layout_default(dst, src->nb_channels > 0 ? src->nb_channels : 2);
        return 0;
    }
    return av_channel_layout_copy(dst, src);
}

/* ------------------------------------------------------------------------ */
/* sample conversion into a FIFO                                             */
/* ------------------------------------------------------------------------ */

typedef struct Converter {
    SwrContext *swr;
    AVChannelLayout in_layout;
    int in_fmt;
    int in_rate;
    AVChannelLayout out_layout;
    int out_fmt;
    int out_rate;
    uint8_t **buf;
    int buf_samples;
} Converter;

static void converter_free(Converter *c)
{
    swr_free(&c->swr);
    if (c->buf) {
        av_freep(&c->buf[0]);
        av_freep(&c->buf);
    }
    av_channel_layout_uninit(&c->in_layout);
    av_channel_layout_uninit(&c->out_layout);
}

static int converter_configure(Converter *c, const AVChannelLayout *in_layout,
                               int in_fmt, int in_rate, char *err)
{
    if (c->swr && c->in_fmt == in_fmt && c->in_rate == in_rate &&
        !av_channel_layout_compare(&c->in_layout, in_layout))
        return 0;

    swr_free(&c->swr);
    av_channel_layout_uninit(&c->in_layout);
    int ret = pcm_layout(in_layout, &c->in_layout);
    if (ret < 0)
        return fail(err, "layout de canais inválido", ret);
    c->in_fmt = in_fmt;
    c->in_rate = in_rate;

    ret = swr_alloc_set_opts2(&c->swr, &c->out_layout, c->out_fmt, c->out_rate,
                              &c->in_layout, in_fmt, in_rate, 0, NULL);
    if (ret >= 0)
        ret = swr_init(c->swr);
    if (ret < 0)
        return fail(err, "falha ao configurar a conversão de áudio", ret);
    return 0;
}

/* Converts `nb` input samples (or flushes when data == NULL) into `fifo`. */
static int converter_push(Converter *c, const uint8_t **data, int nb,
                          AVAudioFifo *fifo, char *err)
{
    if (!c->swr)
        return 0;

    int needed = swr_get_out_samples(c->swr, nb);
    if (needed <= 0)
        return 0;
    if (needed > c->buf_samples) {
        if (c->buf) {
            av_freep(&c->buf[0]);
            av_freep(&c->buf);
        }
        int ret = av_samples_alloc_array_and_samples(&c->buf, NULL, c->out_layout.nb_channels,
                                                     needed, c->out_fmt, 0);
        if (ret < 0)
            return fail(err, "memória insuficiente", ret);
        c->buf_samples = needed;
    }

    int converted = swr_convert(c->swr, c->buf, needed, data, nb);
    if (converted < 0)
        return fail(err, "falha ao converter o áudio", converted);
    if (converted > 0 && av_audio_fifo_write(fifo, (void **)c->buf, converted) < converted)
        return fail(err, "memória insuficiente", AVERROR(ENOMEM));
    return 0;
}

/* ------------------------------------------------------------------------ */
/* probe                                                                     */
/* ------------------------------------------------------------------------ */

int avb_probe(const char *path, AvbMediaInfo *info, char *err)
{
    AVFormatContext *fmt = NULL;
    int ret = open_input(path, &fmt, err);
    if (ret < 0)
        return ret;

    memset(info, 0, sizeof(*info));
    int audio = first_audio_stream(fmt);
    if (audio < 0) {
        avformat_close_input(&fmt);
        return fail(err, "o arquivo não possui faixa de áudio", 0);
    }

    for (unsigned i = 0; i < fmt->nb_streams; i++) {
        const AVStream *st = fmt->streams[i];
        if (st->codecpar->codec_type == AVMEDIA_TYPE_VIDEO &&
            !(st->disposition & AV_DISPOSITION_ATTACHED_PIC))
            info->has_video = 1;
    }

    const AVStream *st = fmt->streams[audio];
    const AVCodecParameters *par = st->codecpar;
    if (fmt->duration != AV_NOPTS_VALUE)
        info->duration = fmt->duration / (double)AV_TIME_BASE;
    else if (st->duration != AV_NOPTS_VALUE)
        info->duration = st->duration * av_q2d(st->time_base);

    info->sample_rate = par->sample_rate;
    info->channels = par->ch_layout.nb_channels;
    info->bit_rate = par->bit_rate;
    snprintf(info->codec, sizeof(info->codec), "%s", avcodec_get_name(par->codec_id));

    avformat_close_input(&fmt);
    if (info->sample_rate <= 0 || info->channels <= 0)
        return fail(err, "não foi possível identificar o formato do áudio", 0);
    return 0;
}

/* ------------------------------------------------------------------------ */
/* decoder                                                                   */
/* ------------------------------------------------------------------------ */

struct AvbDecoder {
    AVFormatContext *fmt;
    AVCodecContext *codec;
    AVPacket *pkt;
    AVFrame *frame;
    AVAudioFifo *fifo;
    Converter conv;
    int stream;
    int input_done;
    int finished;
};

void avb_decoder_close(AvbDecoder *d)
{
    if (!d)
        return;
    converter_free(&d->conv);
    av_audio_fifo_free(d->fifo);
    av_frame_free(&d->frame);
    av_packet_free(&d->pkt);
    avcodec_free_context(&d->codec);
    avformat_close_input(&d->fmt);
    av_free(d);
}

AvbDecoder *avb_decoder_open(const char *path, char *err)
{
    AvbDecoder *d = av_mallocz(sizeof(*d));
    if (!d) {
        fail(err, "memória insuficiente", AVERROR(ENOMEM));
        return NULL;
    }
    if (open_input(path, &d->fmt, err) < 0)
        goto error;

    d->stream = first_audio_stream(d->fmt);
    if (d->stream < 0) {
        fail(err, "o arquivo não possui faixa de áudio", 0);
        goto error;
    }
    /* Only demux what we decode. */
    for (unsigned i = 0; i < d->fmt->nb_streams; i++)
        if ((int)i != d->stream)
            d->fmt->streams[i]->discard = AVDISCARD_ALL;

    const AVCodecParameters *par = d->fmt->streams[d->stream]->codecpar;
    const AVCodec *codec = avcodec_find_decoder(par->codec_id);
    if (!codec) {
        fail(err, "codec de áudio não suportado", 0);
        goto error;
    }
    d->codec = avcodec_alloc_context3(codec);
    if (!d->codec) {
        fail(err, "memória insuficiente", AVERROR(ENOMEM));
        goto error;
    }
    int ret = avcodec_parameters_to_context(d->codec, par);
    if (ret >= 0)
        ret = avcodec_open2(d->codec, codec, NULL);
    if (ret < 0) {
        fail(err, "não foi possível abrir o decodificador de áudio", ret);
        goto error;
    }

    /* Output: interleaved float at the source rate / layout. */
    d->conv.out_fmt = AV_SAMPLE_FMT_FLT;
    d->conv.out_rate = par->sample_rate;
    ret = pcm_layout(&par->ch_layout, &d->conv.out_layout);
    if (ret < 0) {
        fail(err, "layout de canais inválido", ret);
        goto error;
    }

    d->pkt = av_packet_alloc();
    d->frame = av_frame_alloc();
    d->fifo = av_audio_fifo_alloc(AV_SAMPLE_FMT_FLT, d->conv.out_layout.nb_channels, 8192);
    if (!d->pkt || !d->frame || !d->fifo) {
        fail(err, "memória insuficiente", AVERROR(ENOMEM));
        goto error;
    }
    return d;

error:
    avb_decoder_close(d);
    return NULL;
}

/* Advances decoding by one step: one decoded frame or one demuxed packet. */
static int decoder_step(AvbDecoder *d, char *err)
{
    int ret = avcodec_receive_frame(d->codec, d->frame);
    if (ret == 0) {
        ret = converter_configure(&d->conv, &d->frame->ch_layout, d->frame->format,
                                  d->frame->sample_rate, err);
        if (ret >= 0)
            ret = converter_push(&d->conv, (const uint8_t **)d->frame->extended_data,
                                 d->frame->nb_samples, d->fifo, err);
        av_frame_unref(d->frame);
        return ret;
    }
    if (ret == AVERROR_EOF) {
        d->finished = 1;
        return converter_push(&d->conv, NULL, 0, d->fifo, err);
    }
    if (ret != AVERROR(EAGAIN) && ret != AVERROR_INVALIDDATA)
        return fail(err, "falha ao decodificar o áudio", ret);

    if (d->input_done)
        return fail(err, "o decodificador parou de responder", 0);

    ret = av_read_frame(d->fmt, d->pkt);
    if (ret == AVERROR_EOF) {
        d->input_done = 1;
        return avcodec_send_packet(d->codec, NULL);
    }
    if (ret < 0)
        return fail(err, "falha ao ler o arquivo", ret);

    if (d->pkt->stream_index == d->stream) {
        ret = avcodec_send_packet(d->codec, d->pkt);
        /* Skip corrupt packets like the ffmpeg CLI does. */
        if (ret < 0 && ret != AVERROR_INVALIDDATA && ret != AVERROR(EAGAIN)) {
            av_packet_unref(d->pkt);
            return fail(err, "falha ao decodificar o áudio", ret);
        }
    }
    av_packet_unref(d->pkt);
    return 0;
}

int avb_decoder_read(AvbDecoder *d, float *out, int max_frames, char *err)
{
    while (!d->finished && av_audio_fifo_size(d->fifo) < max_frames) {
        int ret = decoder_step(d, err);
        if (ret < 0)
            return ret;
    }
    int n = FFMIN(av_audio_fifo_size(d->fifo), max_frames);
    if (n <= 0)
        return 0;
    void *planes[1] = {out};
    return av_audio_fifo_read(d->fifo, planes, n);
}

/* ------------------------------------------------------------------------ */
/* remuxer                                                                   */
/* ------------------------------------------------------------------------ */

struct AvbRemuxer {
    AVFormatContext *in;
    AVFormatContext *out;
    int audio_in;          /* input index of the replaced audio track */
    int *stream_map;       /* input index -> output index, -1 = dropped */
    AVCodecContext *enc;
    AVStream *enc_stream;
    Converter conv;
    AVAudioFifo *fifo;     /* encoder sample format */
    AVFrame *frame;
    AVPacket *copy_pkt;
    AVPacket *enc_pkt;
    int copy_pending;
    int copy_done;
    int header_written;
    int64_t next_pts;      /* encoder time base */
    AVChannelLayout src_layout;
    int src_rate;
    /* Monitor: decodes our own encoded packets so the result can be
     * measured without reading the output file back. */
    AVCodecContext *mon;
    AVFrame *mon_frame;
    Converter mon_conv;
    AVAudioFifo *mon_fifo;  /* interleaved float */
    int64_t mon_start;      /* first real sample, encoder time base */
    int64_t mon_skip;       /* priming samples still to drop */
    int mon_started;
};

void avb_remuxer_close(AvbRemuxer *m)
{
    if (!m)
        return;
    converter_free(&m->mon_conv);
    av_audio_fifo_free(m->mon_fifo);
    av_frame_free(&m->mon_frame);
    avcodec_free_context(&m->mon);
    converter_free(&m->conv);
    av_audio_fifo_free(m->fifo);
    av_frame_free(&m->frame);
    av_packet_free(&m->copy_pkt);
    av_packet_free(&m->enc_pkt);
    avcodec_free_context(&m->enc);
    if (m->out) {
        if (m->out->pb && !(m->out->oformat->flags & AVFMT_NOFILE))
            avio_closep(&m->out->pb);
        avformat_free_context(m->out);
    }
    avformat_close_input(&m->in);
    av_channel_layout_uninit(&m->src_layout);
    av_free(m->stream_map);
    av_free(m);
}

static int container_accepts(const AVOutputFormat *ofmt, enum AVCodecID id)
{
    /* 1 = yes, 0 = no, negative = unknown (let the muxer decide). */
    return avformat_query_codec(ofmt, id, FF_COMPLIANCE_NORMAL) != 0;
}

/* Re-encode with the source's codec family so the container keeps
 * accepting it; fall back to whatever common codec the container takes. */
static const AVCodec *pick_encoder(enum AVCodecID src, const AVOutputFormat *ofmt)
{
    const char *preferred[3] = {NULL, NULL, NULL};
    switch (src) {
    case AV_CODEC_ID_AAC: preferred[0] = "aac"; break;
    case AV_CODEC_ID_MP3: preferred[0] = "libmp3lame"; break;
    case AV_CODEC_ID_OPUS: preferred[0] = "libopus"; preferred[1] = "opus"; break;
    case AV_CODEC_ID_VORBIS: preferred[0] = "libvorbis"; preferred[1] = "vorbis"; break;
    case AV_CODEC_ID_AC3: preferred[0] = "ac3"; break;
    case AV_CODEC_ID_EAC3: preferred[0] = "eac3"; break;
    case AV_CODEC_ID_FLAC: preferred[0] = "flac"; break;
    case AV_CODEC_ID_ALAC: preferred[0] = "alac"; break;
    default: break;
    }
    for (int i = 0; preferred[i]; i++) {
        const AVCodec *c = avcodec_find_encoder_by_name(preferred[i]);
        if (c && container_accepts(ofmt, c->id))
            return c;
    }
    /* PCM variants: encode back to the very same PCM format. */
    if (src >= AV_CODEC_ID_PCM_S16LE && src < AV_CODEC_ID_ADPCM_IMA_QT) {
        const AVCodec *c = avcodec_find_encoder(src);
        if (c && container_accepts(ofmt, c->id))
            return c;
    }
    static const char *fallback[] = {"aac", "libopus", "libvorbis", "libmp3lame", "flac", "pcm_s16le"};
    for (size_t i = 0; i < sizeof(fallback) / sizeof(fallback[0]); i++) {
        const AVCodec *c = avcodec_find_encoder_by_name(fallback[i]);
        if (c && container_accepts(ofmt, c->id))
            return c;
    }
    return NULL;
}

static enum AVSampleFormat pick_sample_fmt(const AVCodec *codec)
{
    const enum AVSampleFormat *fmts = NULL;
    int n = 0;
    if (avcodec_get_supported_config(NULL, codec, AV_CODEC_CONFIG_SAMPLE_FORMAT, 0,
                                     (const void **)&fmts, &n) < 0 || !fmts || n == 0)
        return AV_SAMPLE_FMT_FLTP;
    for (int i = 0; i < n; i++)
        if (fmts[i] == AV_SAMPLE_FMT_FLTP || fmts[i] == AV_SAMPLE_FMT_FLT)
            return fmts[i];
    return fmts[0];
}

static int pick_sample_rate(const AVCodec *codec, int wanted)
{
    const int *rates = NULL;
    int n = 0;
    if (avcodec_get_supported_config(NULL, codec, AV_CODEC_CONFIG_SAMPLE_RATE, 0,
                                     (const void **)&rates, &n) < 0 || !rates || n == 0)
        return wanted;
    int best = rates[0];
    for (int i = 0; i < n; i++) {
        if (rates[i] == wanted)
            return wanted;
        if (FFABS(rates[i] - wanted) < FFABS(best - wanted))
            best = rates[i];
    }
    return best;
}

static int pick_layout(const AVCodec *codec, const AVChannelLayout *wanted, AVChannelLayout *dst)
{
    const AVChannelLayout *layouts = NULL;
    int n = 0;
    if (avcodec_get_supported_config(NULL, codec, AV_CODEC_CONFIG_CHANNEL_LAYOUT, 0,
                                     (const void **)&layouts, &n) < 0 || !layouts || n == 0)
        return av_channel_layout_copy(dst, wanted);

    const AVChannelLayout *best = NULL;
    for (int i = 0; i < n; i++) {
        if (!av_channel_layout_compare(&layouts[i], wanted))
            return av_channel_layout_copy(dst, wanted);
        /* Otherwise the largest layout that does not exceed the source. */
        if (layouts[i].nb_channels <= wanted->nb_channels &&
            (!best || layouts[i].nb_channels > best->nb_channels))
            best = &layouts[i];
    }
    return av_channel_layout_copy(dst, best ? best : &layouts[0]);
}

static int64_t pick_bit_rate(const AVCodec *codec, const AVCodecParameters *src, int channels)
{
    if (codec->id == AV_CODEC_ID_FLAC || codec->id == AV_CODEC_ID_ALAC ||
        (codec->id >= AV_CODEC_ID_PCM_S16LE && codec->id < AV_CODEC_ID_ADPCM_IMA_QT))
        return 0;
    int64_t rate = src->bit_rate > 0 ? src->bit_rate : (channels <= 2 ? 192000 : 384000);
    int64_t max = codec->id == AV_CODEC_ID_MP3 ? 320000 : 640000;
    return av_clip64(rate, 96000, max);
}

static int open_encoder(AvbRemuxer *m, const AVCodecParameters *src, const char *options,
                        char *err)
{
    const AVCodec *codec = pick_encoder(src->codec_id, m->out->oformat);
    if (!codec)
        return fail(err, "nenhum codificador de áudio compatível com este formato", 0);

    m->enc = avcodec_alloc_context3(codec);
    if (!m->enc)
        return fail(err, "memória insuficiente", AVERROR(ENOMEM));

    int ret = pick_layout(codec, &m->src_layout, &m->enc->ch_layout);
    if (ret < 0)
        return fail(err, "layout de canais inválido", ret);
    m->enc->sample_fmt = pick_sample_fmt(codec);
    m->enc->sample_rate = pick_sample_rate(codec, m->src_rate);
    m->enc->bit_rate = pick_bit_rate(codec, src, m->enc->ch_layout.nb_channels);
    m->enc->time_base = (AVRational){1, m->enc->sample_rate};
    if (m->out->oformat->flags & AVFMT_GLOBALHEADER)
        m->enc->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
    if (codec->capabilities & AV_CODEC_CAP_EXPERIMENTAL)
        m->enc->strict_std_compliance = FF_COMPLIANCE_EXPERIMENTAL;

    AVDictionary *opts = NULL;
    if (options && *options) {
        ret = av_dict_parse_string(&opts, options, "=", ":", 0);
        if (ret < 0) {
            av_dict_free(&opts);
            return fail(err, "opções de codificador inválidas", ret);
        }
    }
    ret = avcodec_open2(m->enc, codec, &opts);
    av_dict_free(&opts);
    if (ret < 0)
        return fail(err, "não foi possível abrir o codificador de áudio", ret);
    return 0;
}

const char *avb_remuxer_encoder(const AvbRemuxer *m)
{
    return m && m->enc && m->enc->codec ? m->enc->codec->name : "";
}

static int copy_stream(AvbRemuxer *m, const AVStream *ist, AVStream **out_st, char *err)
{
    AVStream *st = avformat_new_stream(m->out, NULL);
    if (!st)
        return fail(err, "memória insuficiente", AVERROR(ENOMEM));
    int ret = avcodec_parameters_copy(st->codecpar, ist->codecpar);
    if (ret < 0)
        return fail(err, "falha ao copiar faixa", ret);

    /* Keep the source tag (e.g. hvc1) only when the container agrees. */
    const struct AVCodecTag *const *tags = m->out->oformat->codec_tag;
    unsigned int tag = st->codecpar->codec_tag, tmp;
    if (tags && av_codec_get_id(tags, tag) != st->codecpar->codec_id &&
        av_codec_get_tag2(tags, st->codecpar->codec_id, &tmp))
        st->codecpar->codec_tag = 0;

    st->time_base = ist->time_base;
    st->avg_frame_rate = ist->avg_frame_rate;
    st->r_frame_rate = ist->r_frame_rate;
    st->sample_aspect_ratio = ist->sample_aspect_ratio;
    st->disposition = ist->disposition;
    av_dict_copy(&st->metadata, ist->metadata, 0);
    *out_st = st;
    return 0;
}

static int copy_chapters(AvbRemuxer *m, char *err)
{
    for (unsigned i = 0; i < m->in->nb_chapters; i++) {
        const AVChapter *src = m->in->chapters[i];
        AVChapter *ch = av_mallocz(sizeof(*ch));
        if (!ch)
            return fail(err, "memória insuficiente", AVERROR(ENOMEM));
        ch->id = src->id;
        ch->time_base = src->time_base;
        ch->start = src->start;
        ch->end = src->end;
        av_dict_copy(&ch->metadata, src->metadata, 0);
        av_dynarray_add(&m->out->chapters, (int *)&m->out->nb_chapters, ch);
        if (!m->out->chapters)
            return fail(err, "memória insuficiente", AVERROR(ENOMEM));
    }
    return 0;
}

static void open_monitor(AvbRemuxer *m);

AvbRemuxer *avb_remuxer_open(const char *input, const char *output,
                             const char *encoder_options, char *err)
{
    AvbRemuxer *m = av_mallocz(sizeof(*m));
    if (!m) {
        fail(err, "memória insuficiente", AVERROR(ENOMEM));
        return NULL;
    }
    int ret;
    if (open_input(input, &m->in, err) < 0)
        goto error;

    m->audio_in = first_audio_stream(m->in);
    if (m->audio_in < 0) {
        fail(err, "o arquivo não possui faixa de áudio", 0);
        goto error;
    }
    const AVStream *audio_st = m->in->streams[m->audio_in];
    const AVCodecParameters *audio_par = audio_st->codecpar;
    m->src_rate = audio_par->sample_rate;
    if (pcm_layout(&audio_par->ch_layout, &m->src_layout) < 0) {
        fail(err, "layout de canais inválido", 0);
        goto error;
    }

    ret = avformat_alloc_output_context2(&m->out, NULL, NULL, output);
    if (ret < 0 || !m->out) {
        fail(err, "formato de saída não suportado", ret);
        goto error;
    }
    if (open_encoder(m, audio_par, encoder_options, err) < 0)
        goto error;

    m->stream_map = av_malloc_array(m->in->nb_streams, sizeof(int));
    if (!m->stream_map) {
        fail(err, "memória insuficiente", AVERROR(ENOMEM));
        goto error;
    }

    /* Same stream order as the source; the first audio track is swapped
     * for the encoded one, data streams and unsupported codecs are dropped. */
    for (unsigned i = 0; i < m->in->nb_streams; i++) {
        const AVStream *ist = m->in->streams[i];
        enum AVMediaType type = ist->codecpar->codec_type;
        m->stream_map[i] = -1;

        if ((int)i == m->audio_in) {
            AVStream *st = avformat_new_stream(m->out, NULL);
            if (!st) {
                fail(err, "memória insuficiente", AVERROR(ENOMEM));
                goto error;
            }
            ret = avcodec_parameters_from_context(st->codecpar, m->enc);
            if (ret < 0) {
                fail(err, "falha ao configurar a faixa de áudio", ret);
                goto error;
            }
            st->time_base = m->enc->time_base;
            st->disposition = ist->disposition;
            av_dict_copy(&st->metadata, ist->metadata, 0);
            m->enc_stream = st;
            m->stream_map[i] = st->index;
            continue;
        }

        if (type != AVMEDIA_TYPE_VIDEO && type != AVMEDIA_TYPE_AUDIO &&
            type != AVMEDIA_TYPE_SUBTITLE && type != AVMEDIA_TYPE_ATTACHMENT)
            continue;
        if (!container_accepts(m->out->oformat, ist->codecpar->codec_id))
            continue;

        AVStream *st;
        if (copy_stream(m, ist, &st, err) < 0)
            goto error;
        m->stream_map[i] = st->index;
    }

    av_dict_copy(&m->out->metadata, m->in->metadata, 0);
    if (copy_chapters(m, err) < 0)
        goto error;

    if (!(m->out->oformat->flags & AVFMT_NOFILE)) {
        ret = avio_open(&m->out->pb, output, AVIO_FLAG_WRITE);
        if (ret < 0) {
            fail(err, "não foi possível criar o arquivo de saída", ret);
            goto error;
        }
    }

    AVDictionary *opts = NULL;
    const char *name = m->out->oformat->name;
    if (strstr(name, "mp4") || strstr(name, "mov") || strstr(name, "ipod"))
        av_dict_set(&opts, "movflags", "+faststart", 0);
    ret = avformat_write_header(m->out, &opts);
    av_dict_free(&opts);
    if (ret < 0) {
        fail(err, "falha ao iniciar o arquivo de saída", ret);
        goto error;
    }
    m->header_written = 1;

    /* PCM from Rust -> encoder format. */
    m->conv.out_fmt = m->enc->sample_fmt;
    m->conv.out_rate = m->enc->sample_rate;
    if (av_channel_layout_copy(&m->conv.out_layout, &m->enc->ch_layout) < 0 ||
        converter_configure(&m->conv, &m->src_layout, AV_SAMPLE_FMT_FLT, m->src_rate, err) < 0)
        goto error;

    m->fifo = av_audio_fifo_alloc(m->enc->sample_fmt, m->enc->ch_layout.nb_channels, 8192);
    m->frame = av_frame_alloc();
    m->copy_pkt = av_packet_alloc();
    m->enc_pkt = av_packet_alloc();
    if (!m->fifo || !m->frame || !m->copy_pkt || !m->enc_pkt) {
        fail(err, "memória insuficiente", AVERROR(ENOMEM));
        goto error;
    }

    /* Keep the original audio delay relative to the other streams. */
    int64_t start = audio_st->start_time;
    m->next_pts = start == AV_NOPTS_VALUE
                      ? 0
                      : av_rescale_q(start, audio_st->time_base, m->enc->time_base);

    open_monitor(m);
    return m;

error:
    avb_remuxer_close(m);
    return NULL;
}

/* Stream-copies source packets whose timestamp is at or before `until`
 * seconds (everything when `all` is set), so the muxer can interleave
 * without buffering the whole video in memory. */
static int pump_copy(AvbRemuxer *m, double until, int all, char *err)
{
    for (;;) {
        if (!m->copy_pending) {
            if (m->copy_done)
                return 0;
            int ret = av_read_frame(m->in, m->copy_pkt);
            if (ret == AVERROR_EOF) {
                m->copy_done = 1;
                return 0;
            }
            if (ret < 0)
                return fail(err, "falha ao ler o arquivo", ret);
            int idx = m->copy_pkt->stream_index;
            if (idx == m->audio_in || m->stream_map[idx] < 0) {
                av_packet_unref(m->copy_pkt);
                continue;
            }
            m->copy_pending = 1;
        }

        const AVStream *ist = m->in->streams[m->copy_pkt->stream_index];
        int64_t ts = m->copy_pkt->dts != AV_NOPTS_VALUE ? m->copy_pkt->dts : m->copy_pkt->pts;
        if (!all && ts != AV_NOPTS_VALUE && ts * av_q2d(ist->time_base) > until)
            return 0;

        AVStream *ost = m->out->streams[m->stream_map[m->copy_pkt->stream_index]];
        av_packet_rescale_ts(m->copy_pkt, ist->time_base, ost->time_base);
        m->copy_pkt->stream_index = ost->index;
        m->copy_pkt->pos = -1;
        m->copy_pending = 0;
        int ret = av_interleaved_write_frame(m->out, m->copy_pkt);
        if (ret < 0)
            return fail(err, "falha ao gravar o vídeo", ret);
    }
}

/* Best effort: without a matching decoder the caller measures the file. */
static void open_monitor(AvbRemuxer *m)
{
    const AVCodec *codec = avcodec_find_decoder(m->enc->codec_id);
    AVCodecParameters *par = avcodec_parameters_alloc();
    int ok = codec && par && avcodec_parameters_from_context(par, m->enc) >= 0;
    if (ok)
        ok = (m->mon = avcodec_alloc_context3(codec)) != NULL &&
             avcodec_parameters_to_context(m->mon, par) >= 0;
    avcodec_parameters_free(&par);
    if (ok) {
        m->mon->pkt_timebase = m->enc->time_base;
        ok = avcodec_open2(m->mon, codec, NULL) >= 0;
    }
    if (ok) {
        m->mon_conv.out_fmt = AV_SAMPLE_FMT_FLT;
        m->mon_conv.out_rate = m->enc->sample_rate;
        ok = av_channel_layout_copy(&m->mon_conv.out_layout, &m->enc->ch_layout) >= 0 &&
             (m->mon_frame = av_frame_alloc()) != NULL &&
             (m->mon_fifo = av_audio_fifo_alloc(AV_SAMPLE_FMT_FLT, m->enc->ch_layout.nb_channels,
                                                8192)) != NULL;
    }
    if (!ok) {
        avcodec_free_context(&m->mon);
        return;
    }
    m->mon_start = m->next_pts;
}

/* Decodes one encoded packet (NULL flushes) into the monitor FIFO. */
static int monitor_packet(AvbRemuxer *m, const AVPacket *pkt, char *err)
{
    int ret = avcodec_send_packet(m->mon, pkt);
    if (ret < 0 && ret != AVERROR_EOF)
        return fail(err, "falha ao conferir o áudio codificado", ret);
    for (;;) {
        ret = avcodec_receive_frame(m->mon, m->mon_frame);
        if (ret == AVERROR(EAGAIN))
            return 0;
        if (ret == AVERROR_EOF)
            return converter_push(&m->mon_conv, NULL, 0, m->mon_fifo, err);
        if (ret < 0)
            return fail(err, "falha ao conferir o áudio codificado", ret);
        if (!m->mon_started) {
            /* Everything before the first real sample is encoder priming,
             * which players trim; drop it too. */
            int64_t pts = m->mon_frame->pts;
            m->mon_skip = pts != AV_NOPTS_VALUE ? FFMAX(0, m->mon_start - pts) : m->enc->initial_padding;
            m->mon_started = 1;
        }
        ret = converter_configure(&m->mon_conv, &m->mon_frame->ch_layout, m->mon_frame->format,
                                  m->mon_frame->sample_rate, err);
        if (ret >= 0)
            ret = converter_push(&m->mon_conv, (const uint8_t **)m->mon_frame->extended_data,
                                 m->mon_frame->nb_samples, m->mon_fifo, err);
        av_frame_unref(m->mon_frame);
        if (ret < 0)
            return ret;
    }
}

int avb_remuxer_monitor_format(const AvbRemuxer *m, int *sample_rate, int *channels)
{
    if (!m->mon)
        return -1;
    *sample_rate = m->enc->sample_rate;
    *channels = m->enc->ch_layout.nb_channels;
    return 0;
}

int avb_remuxer_read_monitor(AvbRemuxer *m, float *out, int max_frames)
{
    if (!m->mon)
        return 0;
    while (m->mon_skip > 0 && av_audio_fifo_size(m->mon_fifo) > 0) {
        int n = (int)FFMIN(m->mon_skip, av_audio_fifo_size(m->mon_fifo));
        av_audio_fifo_drain(m->mon_fifo, n);
        m->mon_skip -= n;
    }
    int n = FFMIN(av_audio_fifo_size(m->mon_fifo), max_frames);
    if (n <= 0)
        return 0;
    void *planes[1] = {out};
    return av_audio_fifo_read(m->mon_fifo, planes, n);
}

static int drain_encoder(AvbRemuxer *m, char *err)
{
    for (;;) {
        int ret = avcodec_receive_packet(m->enc, m->enc_pkt);
        if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF)
            return 0;
        if (ret < 0)
            return fail(err, "falha ao codificar o áudio", ret);

        if (m->mon && (ret = monitor_packet(m, m->enc_pkt, err)) < 0) {
            av_packet_unref(m->enc_pkt);
            return ret;
        }
        av_packet_rescale_ts(m->enc_pkt, m->enc->time_base, m->enc_stream->time_base);
        m->enc_pkt->stream_index = m->enc_stream->index;
        double audio_time = m->enc_pkt->pts != AV_NOPTS_VALUE
                                ? m->enc_pkt->pts * av_q2d(m->enc_stream->time_base)
                                : 0.0;
        ret = pump_copy(m, audio_time, 0, err);
        if (ret < 0) {
            av_packet_unref(m->enc_pkt);
            return ret;
        }
        ret = av_interleaved_write_frame(m->out, m->enc_pkt);
        if (ret < 0)
            return fail(err, "falha ao gravar o áudio", ret);
    }
}

static int encode_fifo(AvbRemuxer *m, int flush, char *err)
{
    int variable = m->enc->codec->capabilities & AV_CODEC_CAP_VARIABLE_FRAME_SIZE;
    int small_last = m->enc->codec->capabilities & AV_CODEC_CAP_SMALL_LAST_FRAME;
    int frame_size = (m->enc->frame_size > 0 && !variable) ? m->enc->frame_size : 1024;
    int channels = m->enc->ch_layout.nb_channels;

    for (;;) {
        int available = av_audio_fifo_size(m->fifo);
        if (available <= 0 || (available < frame_size && !flush))
            return 0;

        int n = FFMIN(available, frame_size);
        int padded = (n < frame_size && !variable && !small_last) ? frame_size : n;

        m->frame->format = m->enc->sample_fmt;
        m->frame->sample_rate = m->enc->sample_rate;
        m->frame->nb_samples = padded;
        int ret = av_channel_layout_copy(&m->frame->ch_layout, &m->enc->ch_layout);
        if (ret >= 0)
            ret = av_frame_get_buffer(m->frame, 0);
        if (ret < 0)
            return fail(err, "memória insuficiente", ret);

        if (av_audio_fifo_read(m->fifo, (void **)m->frame->extended_data, n) < n) {
            av_frame_unref(m->frame);
            return fail(err, "falha ao ler amostras", 0);
        }
        if (padded > n)
            av_samples_set_silence(m->frame->extended_data, n, padded - n, channels,
                                   m->enc->sample_fmt);

        m->frame->pts = m->next_pts;
        m->next_pts += padded;
        ret = avcodec_send_frame(m->enc, m->frame);
        av_frame_unref(m->frame);
        if (ret < 0)
            return fail(err, "falha ao codificar o áudio", ret);
        ret = drain_encoder(m, err);
        if (ret < 0)
            return ret;
    }
}

int avb_remuxer_write(AvbRemuxer *m, const float *samples, int frames, char *err)
{
    if (frames <= 0)
        return 0;
    const uint8_t *data[1] = {(const uint8_t *)samples};
    int ret = converter_push(&m->conv, data, frames, m->fifo, err);
    if (ret < 0)
        return ret;
    return encode_fifo(m, 0, err);
}

int avb_remuxer_finish(AvbRemuxer *m, char *err)
{
    int ret = converter_push(&m->conv, NULL, 0, m->fifo, err);
    if (ret >= 0)
        ret = encode_fifo(m, 1, err);
    if (ret < 0)
        return ret;

    ret = avcodec_send_frame(m->enc, NULL);
    if (ret < 0)
        return fail(err, "falha ao finalizar o codificador", ret);
    ret = drain_encoder(m, err);
    if (ret >= 0 && m->mon)
        ret = monitor_packet(m, NULL, err);
    if (ret >= 0)
        ret = pump_copy(m, 0.0, 1, err);
    if (ret < 0)
        return ret;

    ret = av_write_trailer(m->out);
    if (ret < 0)
        return fail(err, "falha ao finalizar o arquivo", ret);
    if (!(m->out->oformat->flags & AVFMT_NOFILE)) {
        ret = avio_closep(&m->out->pb);
        if (ret < 0)
            return fail(err, "falha ao fechar o arquivo", ret);
    }
    return 0;
}
