/* LD_PRELOAD syscall boundary for actual libavdevice/libmpv tests.
 * Only FURAMI_V4L2_FIXTURE_PATH is synthetic; all other descriptors pass through.
 * The runner creates that path in a private temporary directory, never /dev.
 */
#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/videodev2.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include "v4l2_fixture.h"
#include "v4l2_fixture_jpeg.h"

#define MAX_BUFFERS 8
#define MAP_LENGTH 4096
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gate_condition = PTHREAD_COND_INITIALIZER;
static int gate_armed, gate_entered, gate_released, last_closed_fd = -1;
static int device_fd = -1, streaming, count = 6, sequence, fifo[MAX_BUFFERS], ready;
static int queued[MAX_BUFFERS], mapped[MAX_BUFFERS], ids[MAX_BUFFERS];
static unsigned char *memory[MAX_BUFFERS];
static uint32_t pixel_format;
static char scenario[64];
static struct fixture_stats stats;

#define REAL(type, name) ((type)dlsym(RTLD_NEXT, name))
static int error(int value) { errno = value; return -1; }
static int is_case(const char *name) { return !strcmp(scenario, name); }
static unsigned payload_size(void)
{
    if (pixel_format == V4L2_PIX_FMT_MJPEG) return sizeof(fixture_jpeg) + 6;
    if (pixel_format == V4L2_PIX_FMT_NV12) return 16 * 16 * 3 / 2;
    if (pixel_format == V4L2_PIX_FMT_YUYV) return 16 * 16 * 2;
    return 16 * 16 * 3;
}
static void complete(unsigned index)
{
    if (!queued[index] || ready == count) { stats.violations++; return; }
    for (int i = 0; i < ready; i++)
        if (fifo[i] == (int)index) { stats.violations++; return; }
    ids[index] = ++sequence;
    if (pixel_format == V4L2_PIX_FMT_MJPEG) {
        const unsigned char comment[] = {0xff, 0xd8, 0xff, 0xfe, 0, 4,
                                        (unsigned char)sequence, (unsigned char)~sequence};
        memcpy(memory[index], comment, sizeof(comment));
        memcpy(memory[index] + sizeof(comment), fixture_jpeg + 2, sizeof(fixture_jpeg) - 2);
    } else {
        memset(memory[index], sequence & 255, payload_size());
    }
    fifo[ready++] = index;
}
static int produce(unsigned amount)
{
    unsigned produced = 0;
    for (int i = 0; i < count && produced < amount; i++) {
        int already_ready = 0;
        for (int j = 0; j < ready; j++) already_ready |= fifo[j] == i;
        if (queued[i] && !already_ready) { complete(i); produced++; }
    }
    return produced;
}
int furami_fixture_produce(unsigned amount)
{
    pthread_mutex_lock(&mutex);
    int result = produce(amount);
    pthread_mutex_unlock(&mutex);
    return result;
}
int furami_fixture_snapshot(struct fixture_stats *result)
{
    pthread_mutex_lock(&mutex);
    *result = stats;
    result->held = 0;
    for (int i = 0; i < count; i++) result->held += streaming && !queued[i];
    pthread_mutex_unlock(&mutex);
    return 0;
}
int furami_fixture_qbuf_gate(enum fixture_gate_action action)
{
    int result = 0;
    pthread_mutex_lock(&mutex);
    if (action == FIXTURE_GATE_ARM) {
        gate_armed = 1;
        gate_entered = gate_released = 0;
    } else if (action == FIXTURE_GATE_WAIT) {
        struct timespec until;
        clock_gettime(CLOCK_REALTIME, &until);
        until.tv_sec += 3;
        while (!gate_entered && !result)
            result = pthread_cond_timedwait(&gate_condition, &mutex, &until);
    } else if (action == FIXTURE_GATE_RELEASE) {
        gate_released = 1;
        pthread_cond_broadcast(&gate_condition);
    } else {
        result = EINVAL;
    }
    pthread_mutex_unlock(&mutex);
    return result;
}
static int synthetic_open(const char *path, int flags)
{
    const char *expected = getenv("FURAMI_V4L2_FIXTURE_PATH");
    if (!expected || strcmp(path, expected)) return -2;
    /* Fail closed if a caller bypasses the runner's private regular-file fixture. */
    struct stat st;
    if (strncmp(path, "/tmp/furami-v4l2-", strlen("/tmp/furami-v4l2-")) || lstat(path, &st) ||
        !S_ISREG(st.st_mode) || st.st_uid != getuid() || (st.st_mode & 077))
        return error(EPERM);
    if (device_fd >= 0) return error(EBUSY);
    const char *value = getenv("FURAMI_V4L2_SCENARIO");
    snprintf(scenario, sizeof(scenario), "%s", value ? value : "backlog");
    count = is_case("two") ? 2 : is_case("lifetime") || is_case("held_duplicate") ? 8 : 6;
    memset(&stats, 0, sizeof(stats));
    memset(queued, 0, sizeof(queued));
    memset(mapped, 0, sizeof(mapped));
    ready = sequence = streaming = 0;
    device_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (device_fd < 0) return -1;
    stats.opens = 1;
    stats.nonblock = !!(flags & O_NONBLOCK);
    return device_fd;
}
int open(const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (flags & O_CREAT) { va_list ap; va_start(ap, flags); mode = va_arg(ap, int); va_end(ap); }
    int result = synthetic_open(path, flags);
    if (result != -2) return result;
    return REAL(int (*)(const char *, int, ...), "open")(path, flags, mode);
}
int open64(const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (flags & O_CREAT) { va_list ap; va_start(ap, flags); mode = va_arg(ap, int); va_end(ap); }
    return open(path, flags, mode);
}
int close(int fd)
{
    pthread_mutex_lock(&mutex);
    int synthetic = fd == device_fd && fd >= 0;
    if (synthetic) {
        if (gate_entered && !gate_released) stats.violations++;
        stats.closes++;
        last_closed_fd = fd;
        device_fd = -1;
    }
    pthread_mutex_unlock(&mutex);
    int result = REAL(int (*)(int), "close")(fd);
    if (synthetic && is_case("close-error")) return error(EIO);
    return result;
}
void *mmap(void *address, size_t length, int prot, int flags, int fd, off_t offset)
{
    if (fd != device_fd || fd < 0)
        return REAL(void *(*)(void *, size_t, int, int, int, off_t), "mmap")(address, length, prot, flags, fd, offset);
    unsigned index = offset / MAP_LENGTH;
    if (offset % MAP_LENGTH || index >= (unsigned)count || length != MAP_LENGTH) {
        errno = EINVAL; return MAP_FAILED;
    }
    if (is_case("map-error") && index == 2) {
        errno = ENOMEM;
        return MAP_FAILED;
    }
    void *result = REAL(void *(*)(void *, size_t, int, int, int, off_t), "mmap")(
        NULL, length, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (result != MAP_FAILED) { memory[index] = result; mapped[index] = 1; stats.mappings++; }
    return result;
}
void *mmap64(void *address, size_t length, int prot, int flags, int fd, off64_t offset)
{
    return mmap(address, length, prot, flags, fd, offset);
}
int munmap(void *address, size_t length)
{
    pthread_mutex_lock(&mutex);
    for (int i = 0; i < count; i++) if (mapped[i] && memory[i] == address) {
        mapped[i] = 0; stats.mappings--; break;
    }
    pthread_mutex_unlock(&mutex);
    return REAL(int (*)(void *, size_t), "munmap")(address, length);
}
static int synthetic_ioctl(unsigned long request, void *argument)
{
    switch (request) {
    case VIDIOC_QUERYCAP: {
        struct v4l2_capability *cap = argument;
        memset(cap, 0, sizeof(*cap));
        strcpy((char *)cap->card, "Furami synthetic V4L2");
        cap->capabilities = V4L2_CAP_STREAMING | (is_case("multiplanar") ?
            V4L2_CAP_VIDEO_CAPTURE_MPLANE : V4L2_CAP_VIDEO_CAPTURE);
        return 0;
    }
    case VIDIOC_G_INPUT: *(int *)argument = 0; return 0;
    case VIDIOC_S_INPUT: return 0;
    case VIDIOC_ENUMINPUT: {
        struct v4l2_input *input = argument;
        strcpy((char *)input->name, "Synthetic input"); input->std = 0; return 0;
    }
    case VIDIOC_G_STD: return error(ENOTTY);
    case VIDIOC_S_FMT: {
        struct v4l2_format *format = argument;
        pixel_format = format->fmt.pix.pixelformat;
        format->fmt.pix.field = V4L2_FIELD_NONE;
        format->fmt.pix.sizeimage = payload_size();
        return 0;
    }
    case VIDIOC_G_PARM:
    case VIDIOC_S_PARM: {
        struct v4l2_streamparm *parameter = argument;
        parameter->parm.capture.capability = V4L2_CAP_TIMEPERFRAME;
        parameter->parm.capture.timeperframe.numerator = 1;
        parameter->parm.capture.timeperframe.denominator = 60;
        return 0;
    }
    case VIDIOC_REQBUFS:
        if (is_case("req-error")) return error(EIO);
        ((struct v4l2_requestbuffers *)argument)->count = count; return 0;
    case VIDIOC_QUERYBUF: {
        struct v4l2_buffer *buffer = argument;
        if (buffer->index >= (unsigned)count) return error(EINVAL);
        if (is_case("query-error") && buffer->index == 2) return error(EIO);
        buffer->length = MAP_LENGTH; buffer->m.offset = buffer->index * MAP_LENGTH; return 0;
    }
    case VIDIOC_QBUF: {
        struct v4l2_buffer *buffer = argument;
        if (buffer->index >= (unsigned)count || queued[buffer->index]) {
            stats.violations++; return error(EINVAL);
        }
        if (!streaming && is_case("start-q-error") && buffer->index == 2) return error(EIO);
        if (streaming && gate_armed) {
            gate_entered = 1;
            pthread_cond_broadcast(&gate_condition);
            while (!gate_released)
                pthread_cond_wait(&gate_condition, &mutex);
            gate_armed = 0;
        }
        if (streaming && is_case("qerror") && stats.requeues++ == 0) return error(EIO);
        if (streaming && !is_case("qerror")) stats.requeues++;
        queued[buffer->index] = 1;
        if (streaming && (is_case("continuous") || is_case("two") || is_case("mpv")))
            complete(buffer->index);
        return 0;
    }
    case VIDIOC_STREAMON:
        if (is_case("stream-error")) return error(EIO);
        streaming = 1;
        if (!is_case("cancel") && !is_case("nonblock") && !is_case("pollerror") &&
            !is_case("pollhup") && !is_case("pollnval") && !is_case("pollinterrupt") && !is_case("delayed"))
            produce(is_case("single") ? 1 : is_case("lifetime") || is_case("held_duplicate") ? 6 : count);
        return 0;
    case VIDIOC_STREAMOFF:
        if (gate_entered && !gate_released) stats.violations++;
        streaming = 0; ready = 0; stats.streamoffs++;
        memset(queued, 0, sizeof(queued));
        return is_case("streamoff-error") ? error(EIO) : 0;
    case VIDIOC_DQBUF: {
        struct v4l2_buffer *buffer = argument;
        unsigned attempt = ++stats.dequeues;
        if (is_case("dqerror") && attempt == 2) return error(EIO);
        if (is_case("firsterror") && attempt == 1) return error(ENODEV);
        if (is_case("eintr") && attempt == 2) return error(EINTR);
        if (is_case("firsteintr") && attempt == 1) return error(EINTR);
        if (is_case("badindex") && attempt == 2) { buffer->index = count; return 0; }
        if (is_case("duplicate") && attempt == 2) { buffer->index = 0; return 0; }
        if (is_case("held_duplicate") && attempt == 8) { buffer->index = 5; return 0; }
        if (!ready) return error(EAGAIN);
        unsigned index = fifo[0];
        memmove(fifo, fifo + 1, --ready * sizeof(fifo[0]));
        if (!queued[index]) { stats.violations++; return error(EINVAL); }
        queued[index] = 0;
        stats.successful_dequeues++;
        buffer->index = index;
        buffer->bytesused = payload_size();
        buffer->flags = 0;
        buffer->timestamp.tv_sec = ids[index]; buffer->timestamp.tv_usec = 0;
        buffer->sequence = ids[index];
        if (ids[index] == count && is_case("oversize")) buffer->bytesused = MAP_LENGTH + 1;
        if (ids[index] == count && is_case("short")) buffer->bytesused--;
        if (ids[index] == count && is_case("corrupt")) buffer->flags = V4L2_BUF_FLAG_ERROR;
        return 0;
    }
    default: return error(ENOTTY);
    }
}
int ioctl(int fd, unsigned long request, ...)
{
    va_list ap; va_start(ap, request); void *argument = va_arg(ap, void *); va_end(ap);
    pthread_mutex_lock(&mutex);
    if (fd >= 0 && fd == last_closed_fd && request == VIDIOC_QBUF) {
        stats.violations++;
        pthread_mutex_unlock(&mutex);
        return error(EBADF);
    }
    if (fd != device_fd || fd < 0) {
        pthread_mutex_unlock(&mutex);
        return REAL(int (*)(int, unsigned long, ...), "ioctl")(fd, request, argument);
    }
    int result = synthetic_ioctl(request, argument);
    pthread_mutex_unlock(&mutex);
    return result;
}
/* glibc marks poll's array write_only for its caller-facing diagnostics.
 * An interposer must read fd/events: export the ABI symbol from a distinct
 * C identifier so the definition does not inherit that caller attribute. */
int furami_fixture_poll(struct pollfd *fds, nfds_t amount, int timeout) __asm__("poll");
int furami_fixture_poll(struct pollfd *fds, nfds_t amount, int timeout)
{
    if (amount != 1 || fds[0].fd != device_fd || device_fd < 0)
        return REAL(int (*)(struct pollfd *, nfds_t, int), "poll")(fds, amount, timeout);
    stats.polls++;
    if (is_case("pollerror")) return error(EIO);
    if (is_case("pollinterrupt")) return error(EINTR);
    if (is_case("pollhup")) { fds[0].revents = POLLHUP; return 1; }
    if (is_case("pollnval")) { fds[0].revents = POLLNVAL; return 1; }
    if (is_case("delayed")) furami_fixture_produce(1);
    if (ready) { fds[0].revents = POLLIN; return 1; }
    if (timeout < 0 || timeout > 100) { stats.violations++; return error(EINVAL); }
    struct timespec delay = {.tv_sec = 0, .tv_nsec = timeout * 1000000L};
    nanosleep(&delay, NULL);
    fds[0].revents = 0;
    return 0;
}
