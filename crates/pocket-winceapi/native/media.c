/* PocketHLE's small opaque C boundary to statically linked FFmpeg.
 * No FFmpeg structs cross the Rust ABI; every decoder has one owner. */
#include <errno.h>
#include <limits.h>
#include <stdint.h>
#include <string.h>
#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
#include <libavutil/channel_layout.h>
#include <libavutil/error.h>
#include <libavutil/mem.h>
#include <libavutil/mathematics.h>
#include <libswscale/swscale.h>
#include <libswresample/swresample.h>

typedef struct PocketMedia {
    AVFormatContext *format;
    AVCodecContext *codec;
    AVPacket *packet;
    AVFrame *frame;
    struct SwsContext *scale;
    SwrContext *resample;
    uint8_t *buffer;
    unsigned capacity;
    int stream, video, width, height, draining;
    int64_t first_pts, frame_count;
} PocketMedia;

void pocket_media_close(PocketMedia *m) {
    if (!m) return;
    sws_freeContext(m->scale);
    swr_free(&m->resample);
    av_frame_free(&m->frame);
    av_packet_free(&m->packet);
    avcodec_free_context(&m->codec);
    avformat_close_input(&m->format);
    av_free(m->buffer);
    av_free(m);
}

PocketMedia *pocket_media_open(const char *path, int video, int width, int height, int *error) {
    PocketMedia *m = av_mallocz(sizeof(*m));
    const AVCodec *codec = NULL;
    int ret;
    *error = AVERROR(ENOMEM);
    if (!m) return NULL;
    m->video = video;
    m->width = width;
    m->height = height;
    m->first_pts = AV_NOPTS_VALUE;
    if (video && (width < 0 || height < 0 || width > 8192 || height > 8192 || ((width == 0) != (height == 0)))) {
        ret = AVERROR(EINVAL); goto fail;
    }
    ret = avformat_open_input(&m->format, path, NULL, NULL);
    if (ret < 0) goto fail;
    ret = avformat_find_stream_info(m->format, NULL);
    if (ret < 0) goto fail;
    ret = av_find_best_stream(m->format, video ? AVMEDIA_TYPE_VIDEO : AVMEDIA_TYPE_AUDIO, -1, -1, &codec, 0);
    /* A movie without audio is valid. Distinguish it from decode failures. */
    if (ret == AVERROR_STREAM_NOT_FOUND) { ret = 1; goto fail; }
    if (ret < 0) goto fail;
    m->stream = ret;
    m->first_pts = m->format->streams[m->stream]->start_time;
    if (m->first_pts == AV_NOPTS_VALUE) m->first_pts = 0;
    m->codec = avcodec_alloc_context3(codec);
    m->packet = av_packet_alloc();
    m->frame = av_frame_alloc();
    if (!m->codec || !m->packet || !m->frame) { ret = AVERROR(ENOMEM); goto fail; }
    ret = avcodec_parameters_to_context(m->codec, m->format->streams[m->stream]->codecpar);
    if (ret < 0) goto fail;
    ret = avcodec_open2(m->codec, codec, NULL);
    if (ret < 0) goto fail;
    if (video && width == 0) {
        m->width = m->codec->width; m->height = m->codec->height;
        if (m->width <= 0 || m->height <= 0 || m->width > 8192 || m->height > 8192) { ret = AVERROR(EINVAL); goto fail; }
    }
    if (!video) {
        AVChannelLayout stereo = AV_CHANNEL_LAYOUT_STEREO;
        ret = swr_alloc_set_opts2(&m->resample, &stereo, AV_SAMPLE_FMT_S16, 44100,
            &m->codec->ch_layout, m->codec->sample_fmt, m->codec->sample_rate, 0, NULL);
        if (ret < 0) goto fail;
        ret = swr_init(m->resample);
        if (ret < 0) goto fail;
    }
    *error = 0;
    return m;
fail:
    *error = ret;
    pocket_media_close(m);
    return NULL;
}

/* Metadata and seeking retain single-owner decoder semantics. */
void pocket_media_info(PocketMedia *m, double *duration, int *width, int *height, double *interval, int64_t *bitrate) {
    AVStream *stream = m->format->streams[m->stream];
    AVRational rate = av_guess_frame_rate(m->format, stream, NULL);
    *duration = m->format->duration != AV_NOPTS_VALUE ? (double)m->format->duration / AV_TIME_BASE :
        (stream->duration != AV_NOPTS_VALUE ? stream->duration * av_q2d(stream->time_base) : 0);
    *width = m->codec->width; *height = m->codec->height;
    *interval = rate.num > 0 && rate.den > 0 ? av_q2d(av_inv_q(rate)) : 1.0 / 30.0;
    *bitrate = m->format->bit_rate;
}
int pocket_media_seek(PocketMedia *m, double seconds) {
    AVStream *stream = m->format->streams[m->stream];
    int64_t target;
    int ret;
    if (seconds < 0 || seconds > (double)INT64_MAX / AV_TIME_BASE) return AVERROR(EINVAL);
    target = av_rescale_q((int64_t)(seconds * AV_TIME_BASE), AV_TIME_BASE_Q, stream->time_base) + m->first_pts;
    ret = av_seek_frame(m->format, m->stream, target, AVSEEK_FLAG_BACKWARD);
    if (ret < 0) return ret;
    avcodec_flush_buffers(m->codec);
    av_packet_unref(m->packet); av_frame_unref(m->frame);
    m->draining = 0; m->frame_count = (int64_t)(seconds * 30);
    if (m->resample) { swr_close(m->resample); ret = swr_init(m->resample); }
    return ret;
}

static int audio_output(PocketMedia *m, const uint8_t **input, int count, const uint8_t **data, int *length) {
    int capacity = swr_get_out_samples(m->resample, count);
    int samples;
    uint8_t *output[1];
    if (capacity < 0) return capacity;
    if (capacity > INT_MAX / 4) return AVERROR(ENOMEM);
    av_fast_malloc(&m->buffer, &m->capacity, (size_t)(capacity ? capacity : 1) * 4);
    if (!m->buffer) return AVERROR(ENOMEM);
    output[0] = m->buffer;
    samples = swr_convert(m->resample, output, capacity, input, count);
    if (samples < 0) return samples;
    *data = m->buffer;
    *length = samples * 4;
    return samples ? 1 : 0;
}

/* Returns 1 for an owned output buffer, 0 for EOF, or an FFmpeg error.
 * The borrowed output is valid only until the next call/close. */
int pocket_media_next(PocketMedia *m, const uint8_t **data, int *length, double *seconds) {
    int ret;
    *data = NULL; *length = 0; *seconds = 0;
    for (;;) {
        ret = avcodec_receive_frame(m->codec, m->frame);
        if (ret == AVERROR_EOF) {
            if (m->video) return 0;
            return audio_output(m, NULL, 0, data, length);
        }
        if (ret == 0) {
            if (!m->video) {
                ret = audio_output(m, (const uint8_t **)m->frame->extended_data, m->frame->nb_samples, data, length);
                av_frame_unref(m->frame);
                if (ret == 0) continue;
                return ret;
            }
            {
                AVFrame *f = m->frame;
                AVStream *stream = m->format->streams[m->stream];
                int w = m->width, h = m->height, x, y;
                uint8_t *output[4] = { NULL };
                int stride[4] = { m->width * 2, 0, 0, 0 };
                int64_t pts = f->best_effort_timestamp;
                if (f->width <= 0 || f->height <= 0) return AVERROR_INVALIDDATA;
                if ((int64_t)w * f->height > (int64_t)h * f->width)
                    w = (int)((int64_t)h * f->width / f->height);
                else h = (int)((int64_t)w * f->height / f->width);
                if (w < 1) w = 1;
                if (h < 1) h = 1;
                x = (m->width - w) / 2; y = (m->height - h) / 2;
                av_fast_malloc(&m->buffer, &m->capacity, (size_t)m->width * m->height * 2);
                if (!m->buffer) return AVERROR(ENOMEM);
                memset(m->buffer, 0, (size_t)m->width * m->height * 2);
                m->scale = sws_getCachedContext(m->scale, f->width, f->height, (enum AVPixelFormat)f->format,
                    w, h, AV_PIX_FMT_RGB565LE, SWS_BILINEAR, NULL, NULL, NULL);
                if (!m->scale) return AVERROR(ENOMEM);
                output[0] = m->buffer + (y * m->width + x) * 2;
                ret = sws_scale(m->scale, (const uint8_t *const *)f->data, f->linesize, 0, f->height, output, stride);
                if (ret < 0) return ret;
                if (pts != AV_NOPTS_VALUE) {
                    if (m->first_pts == AV_NOPTS_VALUE) m->first_pts = pts;
                    *seconds = (pts - m->first_pts) * av_q2d(stream->time_base);
                } else {
                    AVRational rate = av_guess_frame_rate(m->format, stream, f);
                    *seconds = rate.num > 0 && rate.den > 0 ? m->frame_count / av_q2d(rate) : m->frame_count / 30.0;
                }
                m->frame_count++;
                *data = m->buffer;
                *length = m->width * m->height * 2;
                av_frame_unref(f);
                return 1;
            }
        }
        if (ret != AVERROR(EAGAIN)) return ret;
        if (m->draining) return AVERROR_INVALIDDATA;
        do {
            ret = av_read_frame(m->format, m->packet);
            if (ret < 0) break;
            if (m->packet->stream_index == m->stream) break;
            av_packet_unref(m->packet);
        } while (1);
        if (ret == AVERROR_EOF) {
            m->draining = 1;
            ret = avcodec_send_packet(m->codec, NULL);
        } else if (ret >= 0) {
            ret = avcodec_send_packet(m->codec, m->packet);
            av_packet_unref(m->packet);
        }
        if (ret < 0) return ret;
    }
}

void pocket_media_error(int code, char *text, int capacity) {
    av_strerror(code, text, (size_t)capacity);
}
