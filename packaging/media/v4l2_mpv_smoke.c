/* Actual libmpv demux/decode/null-VO integration, never a presentation test. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <mpv/client.h>
#include <time.h>
#include "v4l2_fixture.h"
#define CHECK(test) do { if (!(test)) { fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #test); exit(1); } } while (0)
static void option(mpv_handle *handle, const char *key, const char *value)
{
    int result = mpv_set_option_string(handle, key, value);
    if (result < 0) fprintf(stderr, "%s: %s\n", key, mpv_error_string(result));
    CHECK(result >= 0);
}
int main(void)
{
    Dl_info library;
    void *client_symbol = dlsym(RTLD_NEXT, "mpv_create");
    CHECK(client_symbol && dladdr(client_symbol, &library));
    printf("actual libmpv: %s\n", library.dli_fname);
    const char *prefix = getenv("FURAMI_MEDIA_PREFIX");
    CHECK(prefix);
    size_t prefix_length = strlen(prefix);
    CHECK(!strncmp(library.dli_fname, prefix, prefix_length) && library.dli_fname[prefix_length] == '/');
    mpv_handle *handle = mpv_create();
    CHECK(handle);
    option(handle, "config", "no");
    option(handle, "terminal", "no");
    option(handle, "vo", "null");
    option(handle, "audio", "no");
    option(handle, "idle", "yes");
    option(handle, "cache", "no");
    option(handle, "demuxer-readahead-secs", "0");
    option(handle, "untimed", "yes");
    option(handle, "correct-pts", "no");
    option(handle, "video-latency-hacks", "yes");
    option(handle, "pause", "no");
    option(handle, "demuxer-lavf-format", "v4l2");
    option(handle, "demuxer-lavf-o", "input_format=nv12,video_size=16x16,framerate=60/1,furami_latest=1");
    double fps = 60.0;
    CHECK(mpv_set_option(handle, "container-fps-override", MPV_FORMAT_DOUBLE, &fps) >= 0);
    CHECK(mpv_initialize(handle) >= 0);
    CHECK(mpv_request_log_messages(handle, "warn") >= 0);
    const char *path = getenv("FURAMI_V4L2_FIXTURE_PATH");
    CHECK(path);
    char url[4096];
    int length = snprintf(url, sizeof(url), "av://v4l2:%s", path);
    CHECK(length > 0 && (size_t)length < sizeof(url));
    const char *command[] = {"loadfile", url, NULL};
    CHECK(mpv_command(handle, command) >= 0);
    int loaded = 0, decoded = 0;
    time_t deadline = time(NULL) + 8;
    while (time(NULL) < deadline && !decoded) {
        mpv_event *event = mpv_wait_event(handle, 0.1);
        if (event->event_id == MPV_EVENT_LOG_MESSAGE) {
            mpv_event_log_message *log = event->data;
            fprintf(stderr, "%s: %s", log->prefix, log->text);
            CHECK(!strstr(log->text, "furami_latest"));
        }
        if (event->event_id == MPV_EVENT_END_FILE) {
            mpv_event_end_file *end = event->data;
            fprintf(stderr, "premature end: %s\n", mpv_error_string(end->error));
            CHECK(0);
        }
        loaded |= event->event_id == MPV_EVENT_FILE_LOADED;
        int64_t width = 0, height = 0;
        if (loaded && mpv_get_property(handle, "video-out-params/w", MPV_FORMAT_INT64, &width) >= 0 &&
            mpv_get_property(handle, "video-out-params/h", MPV_FORMAT_INT64, &height) >= 0 &&
            width == 16 && height == 16) decoded = 1;
    }
    CHECK(loaded && decoded);
    const char *stop[] = {"stop", NULL};
    CHECK(mpv_command(handle, stop) >= 0);
    mpv_terminate_destroy(handle);
    int (*snapshot)(struct fixture_stats *) = dlsym(RTLD_DEFAULT, "furami_fixture_snapshot");
    struct fixture_stats state;
    CHECK(snapshot && snapshot(&state) == 0);
    CHECK(state.opens == 1 && state.closes == 1 && state.streamoffs == 1);
    CHECK(state.nonblock && !state.mappings && !state.violations);
    puts("PASS actual libmpv capture/options/decode 16x16/null-VO/terminate-destroy (synthetic kernel adapter)");
    return 0;
}
