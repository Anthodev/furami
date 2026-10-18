/* Real libavdevice packet assertions over the private synthetic syscall adapter. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <time.h>
#include <libavdevice/avdevice.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/time.h>
#include <libavutil/mem.h>
#include "v4l2_fixture.h"

#define CHECK(test) do { if (!(test)) { fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #test); exit(1); } } while (0)
static int64_t deadline;
static int cancel_drain;
static int allocation_case, allocation_limit_armed;
static int (*snapshot)(struct fixture_stats *);
static int (*produce)(unsigned);
static int interrupt(void *opaque)
{
    (void)opaque;
    if (allocation_case && !allocation_limit_armed) {
        struct fixture_stats state;
        snapshot(&state);
        /* Arm only at the post-drain check: six real dequeues and five
         * obsolete candidates requeued. Earlier avformat allocations remain
         * unrestricted; only packet construction sees the process-local limit.
         * Pinned 8.1.2: buff_data fits in AVBufferRef, AVBuffer is larger. */
        if (state.successful_dequeues == 6 && state.requeues == 5) {
            av_max_alloc(allocation_case == 1 ? 0 : sizeof(AVBufferRef));
            allocation_limit_armed = 1;
        }
    }
    if (cancel_drain) {
        struct fixture_stats state;
        snapshot(&state);
        if (state.successful_dequeues)
            return 1;
    }
    return deadline && av_gettime_relative() >= deadline;
}
static int packet_id(const AVPacket *packet, int jpeg)
{
    CHECK(packet->size > (jpeg ? 6 : 0));
    return packet->data[jpeg ? 6 : 0];
}
static void check_packet(const AVPacket *packet, int id, int jpeg, int size)
{
    CHECK(packet_id(packet, jpeg) == id);
    CHECK(packet->pts == INT64_C(1000000) * id);
    CHECK(size < 0 || packet->size == size);
    if (jpeg) {
        const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_MJPEG);
        CHECK(codec);
        AVCodecContext *decoder = avcodec_alloc_context3(codec);
        AVFrame *frame = av_frame_alloc();
        CHECK(decoder && frame);
        CHECK(avcodec_open2(decoder, codec, NULL) == 0);
        CHECK(avcodec_send_packet(decoder, packet) == 0);
        CHECK(avcodec_receive_frame(decoder, frame) == 0);
        CHECK(frame->width == 16 && frame->height == 16);
        av_frame_free(&frame);
        avcodec_free_context(&decoder);
    }
}
struct close_job {
    AVFormatContext **context;
    pthread_mutex_t mutex;
    pthread_cond_t condition;
    int started, done;
};

static void *release_packet_thread(void *opaque)
{
    AVPacket *packet = opaque;
    av_packet_free(&packet);
    return NULL;
}

static void *close_context_thread(void *opaque)
{
    struct close_job *job = opaque;
    pthread_mutex_lock(&job->mutex);
    job->started = 1;
    pthread_cond_signal(&job->condition);
    pthread_mutex_unlock(&job->mutex);
    avformat_close_input(job->context);
    pthread_mutex_lock(&job->mutex);
    job->done = 1;
    pthread_cond_signal(&job->condition);
    pthread_mutex_unlock(&job->mutex);
    return NULL;
}

int main(int argc, char **argv)
{
    CHECK(argc == 4);
    int enabled = !strcmp(argv[1], "on"), explicit_off = !strcmp(argv[1], "zero");
    const char *scenario = argv[2], *format = argv[3];
    int jpeg = !strcmp(format, "mjpeg");
    int two = !strcmp(scenario, "two"), lifetime = !strcmp(scenario, "lifetime");
    int held_duplicate = !strcmp(scenario, "held_duplicate");
    snapshot = dlsym(RTLD_DEFAULT, "furami_fixture_snapshot");
    produce = dlsym(RTLD_DEFAULT, "furami_fixture_produce");
    CHECK(snapshot && produce);
    Dl_info library;
    /* A non-PIE executable's &function can identify its own PLT stub.
     * Look past the executable for the actual loaded shared-library symbol. */
    void *configuration_symbol = dlsym(RTLD_NEXT, "avdevice_configuration");
    CHECK(configuration_symbol && dladdr(configuration_symbol, &library));
    printf("actual libavdevice: %s\n", library.dli_fname);
    const char *prefix = getenv("FURAMI_MEDIA_PREFIX");
    CHECK(prefix);
    size_t prefix_length = strlen(prefix);
    CHECK(!strncmp(library.dli_fname, prefix, prefix_length) && library.dli_fname[prefix_length] == '/');
    avdevice_register_all();
    const AVInputFormat *input = av_find_input_format("video4linux2");
    CHECK(input);
    AVFormatContext *context = avformat_alloc_context();
    CHECK(context);
    context->flags |= AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    if (!strcmp(scenario, "nonblock")) context->flags |= AVFMT_FLAG_NONBLOCK;
    context->interrupt_callback.callback = interrupt;
    AVDictionary *options = NULL;
    av_dict_set(&options, "input_format", format, 0);
    av_dict_set(&options, "video_size", "16x16", 0);
    av_dict_set(&options, "framerate", "60/1", 0);
    if (enabled || explicit_off) av_dict_set(&options, "furami_latest", enabled ? "1" : "0", 0);
    int result = avformat_open_input(&context, getenv("FURAMI_V4L2_FIXTURE_PATH"), input, &options);
    /* On failure avformat_open_input can preserve the original option dict. */
    if (result >= 0)
        CHECK(!av_dict_get(options, "furami_latest", NULL, 0));
    av_dict_free(&options);
    if (!strcmp(scenario, "req-error") || !strcmp(scenario, "query-error") ||
        !strcmp(scenario, "map-error") || !strcmp(scenario, "start-q-error") ||
        !strcmp(scenario, "stream-error")) {
        CHECK(result == AVERROR(!strcmp(scenario, "map-error") ? ENOMEM : EIO));
        CHECK(!context);
        struct fixture_stats state; snapshot(&state);
        CHECK(state.opens == 1 && state.closes == 1 && !state.mappings && !state.violations);
        CHECK(!state.streamoffs);
        printf("PASS partial-init cleanup %s\n", scenario);
        return 0;
    }
    if (!strcmp(scenario, "unsupported") || !strcmp(scenario, "multiplanar")) {
        CHECK(result == AVERROR(EINVAL));
        CHECK(!context);
        struct fixture_stats state; snapshot(&state);
        CHECK(state.opens == 1 && state.closes == 1 && !state.mappings && !state.violations);
        printf("PASS rejected %s %s\n", scenario, format);
        return 0;
    }
    CHECK(result == 0);
    struct fixture_stats initial; snapshot(&initial);
    CHECK(initial.nonblock == (enabled || !strcmp(scenario, "nonblock")));
    CHECK(context->nb_streams == 1);
    CHECK(context->streams[0]->codecpar->width == 16 && context->streams[0]->codecpar->height == 16);
    AVPacket *packet = av_packet_alloc(), *second = av_packet_alloc();
    CHECK(packet && second);
    if (!strcmp(scenario, "cancel") || !strcmp(scenario, "pollinterrupt")) deadline = av_gettime_relative() + 150000;
    cancel_drain = !strcmp(scenario, "draincancel");
    allocation_case = !strcmp(scenario, "descriptor-alloc-error") ? 1 :
                      !strcmp(scenario, "buffer-create-error") ? 2 : 0;
    int64_t started = av_gettime_relative();
    result = av_read_frame(context, packet);
    if (allocation_case)
        av_max_alloc(INT_MAX);
    int64_t elapsed = av_gettime_relative() - started;
    struct fixture_stats state; snapshot(&state);
    if (allocation_case) {
        CHECK(allocation_limit_armed && result == AVERROR(ENOMEM));
        CHECK(state.successful_dequeues == 6 && state.requeues == 6 && !state.held);
        CHECK(!packet->buf);
        CHECK(produce(1) == 1 && av_read_frame(context, second) == 0);
        check_packet(second, 7, jpeg, -1);
    } else if (!strcmp(scenario, "cancel") || !strcmp(scenario, "pollinterrupt")) {
        CHECK(result == AVERROR_EXIT && elapsed < 500000 && state.polls > 0);
    } else if (!strcmp(scenario, "draincancel")) {
        CHECK(result == AVERROR_EXIT && state.successful_dequeues == 1 && !state.held);
    } else if (!strcmp(scenario, "nonblock")) {
        CHECK(result == AVERROR(EAGAIN) && state.polls == 0 && elapsed < 100000);
    } else if (!strcmp(scenario, "firsterror") || !strcmp(scenario, "pollhup")) {
        CHECK(result == AVERROR(ENODEV));
    } else if (!strcmp(scenario, "pollnval")) {
        CHECK(result == AVERROR(EBADF));
    } else if (!strcmp(scenario, "dqerror") || !strcmp(scenario, "qerror") || !strcmp(scenario, "pollerror")) {
        CHECK(result == AVERROR(EIO));
    } else if (!strcmp(scenario, "badindex") || !strcmp(scenario, "duplicate")) {
        CHECK(result == AVERROR(EINVAL));
    } else if (!strcmp(scenario, "oversize")) {
        CHECK(result == AVERROR_INVALIDDATA);
    } else {
        CHECK(result == 0);
        int expected = !enabled ? 1 : two ? 2 : !strcmp(scenario, "eintr") ? 5 :
                       !strcmp(scenario, "single") || !strcmp(scenario, "delayed") ? 1 : 6;
        if (!strcmp(scenario, "short") || !strcmp(scenario, "corrupt")) {
            CHECK(packet->size == 0 && packet->pts == INT64_C(6000000));
        } else {
            int size = jpeg ? -1 : !strcmp(format, "nv12") ? 384 : !strcmp(format, "yuyv422") ? 512 : 768;
            check_packet(packet, expected, jpeg, size);
        }
        if (enabled && (!strcmp(scenario, "continuous") || two)) CHECK(state.dequeues == (two ? 2 : 6));
        if (enabled && !strcmp(scenario, "eintr")) CHECK(state.dequeues == 6 && state.successful_dequeues == 5);
        if (!strcmp(scenario, "close-race")) {
            int (*gate)(enum fixture_gate_action) = dlsym(RTLD_DEFAULT, "furami_fixture_qbuf_gate");
            CHECK(gate);
            CHECK(produce(1) == 1 && av_read_frame(context, second) == 0);
            check_packet(second, 7, jpeg, -1);
            CHECK(gate(FIXTURE_GATE_ARM) == 0);
            pthread_t releasing, closing;
            CHECK(pthread_create(&releasing, NULL, release_packet_thread, packet) == 0);
            packet = NULL;
            CHECK(gate(FIXTURE_GATE_WAIT) == 0);
            struct close_job job = {
                .context = &context,
                .mutex = PTHREAD_MUTEX_INITIALIZER,
                .condition = PTHREAD_COND_INITIALIZER,
            };
            CHECK(pthread_create(&closing, NULL, close_context_thread, &job) == 0);
            pthread_mutex_lock(&job.mutex);
            while (!job.started)
                pthread_cond_wait(&job.condition, &job.mutex);
            struct timespec until;
            clock_gettime(CLOCK_REALTIME, &until);
            until.tv_nsec += 100000000;
            if (until.tv_nsec >= 1000000000) { until.tv_sec++; until.tv_nsec -= 1000000000; }
            int wait_result = 0;
            while (!job.done && !wait_result)
                wait_result = pthread_cond_timedwait(&job.condition, &job.mutex, &until);
            CHECK(wait_result == ETIMEDOUT && !job.done);
            pthread_mutex_unlock(&job.mutex);
            snapshot(&state);
            CHECK(state.closes == 0 && state.streamoffs == 0 && !state.violations);
            CHECK(gate(FIXTURE_GATE_RELEASE) == 0);
            CHECK(pthread_join(releasing, NULL) == 0 && pthread_join(closing, NULL) == 0);
            pthread_cond_destroy(&job.condition);
            pthread_mutex_destroy(&job.mutex);
            snapshot(&state);
            CHECK(state.closes == 1 && state.streamoffs == 1 && !state.violations);
            CHECK(state.mappings == initial.mappings);
            unsigned requeues_at_close = state.requeues;
            check_packet(second, 7, jpeg, -1);
            av_packet_free(&second);
            snapshot(&state);
            CHECK(!state.mappings && !state.violations && state.requeues == requeues_at_close);
            puts("PASS close-race QBUF transaction serializes retirement; late packet cleanup");
            return 0;
        }
        if (!strcmp(scenario, "close-held") || !strcmp(scenario, "close-error") ||
            !strcmp(scenario, "streamoff-error")) {
            AVPacket *clone = av_packet_clone(packet);
            CHECK(clone);
            unsigned requeues_before_close = state.requeues;
            avformat_close_input(&context);
            snapshot(&state);
            /* Fail safely on the faulty provider before touching unmapped data
             * or invoking its dangling release callback. */
            CHECK(state.mappings == initial.mappings && state.mappings > 0);
            CHECK(state.closes == 1 && state.streamoffs == 1 && !state.violations);
            check_packet(packet, expected, jpeg, -1);
            check_packet(clone, expected, jpeg, -1);
            av_packet_free(&packet);
            snapshot(&state);
            CHECK(state.mappings == initial.mappings && state.requeues == requeues_before_close);
            check_packet(clone, expected, jpeg, -1);
            av_packet_free(&clone);
            av_packet_free(&second);
            snapshot(&state);
            CHECK(!state.mappings && state.requeues == requeues_before_close && !state.violations);
            CHECK(state.closes == 1 && state.streamoffs == 1);
            puts("PASS close-held payload/PTS/clone/final-reference cleanup/no late QBUF");
            return 0;
        }
        if (lifetime || held_duplicate) {
            unsigned char *saved = malloc(packet->size);
            CHECK(saved); memcpy(saved, packet->data, packet->size);
            CHECK(produce(1) == 1);
            result = av_read_frame(context, second);
            if (held_duplicate) CHECK(result == AVERROR(EINVAL));
            else { CHECK(result == 0); check_packet(second, enabled ? 7 : 2, jpeg, -1); }
            CHECK(!memcmp(saved, packet->data, packet->size));
            free(saved);
            snapshot(&state);
            CHECK(state.held == (held_duplicate ? 1 : 2));
        }
    }
    av_packet_free(&second);
    av_packet_free(&packet);
    snapshot(&state);
    CHECK(!state.violations && state.held == 0);
    avformat_close_input(&context);
    snapshot(&state);
    CHECK(!state.mappings && state.opens == 1 && state.closes == 1 && state.streamoffs == 1);
    printf("PASS %s %s %s dq=%u q=%u poll=%u elapsed_us=%"PRId64" libavdevice=%s\n",
           argv[1], scenario, format, state.dequeues, state.requeues, state.polls, elapsed, avdevice_configuration());
    return 0;
}
