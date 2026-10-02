#include "furami/src/capture/linux/ffi.rs.h"

#include <linux/videodev2.h>
#include <sys/ioctl.h>

#include <algorithm>
#include <cerrno>

namespace furami::capture {

V4l2Constants v4l2_constants() noexcept
{
    V4l2Constants result{};
    result.cap_device_caps = V4L2_CAP_DEVICE_CAPS;
    result.cap_video_capture = V4L2_CAP_VIDEO_CAPTURE;
    result.cap_video_capture_mplane = V4L2_CAP_VIDEO_CAPTURE_MPLANE;
    result.buffer_capture = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    result.buffer_capture_mplane = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    result.size_discrete = V4L2_FRMSIZE_TYPE_DISCRETE;
    result.size_continuous = V4L2_FRMSIZE_TYPE_CONTINUOUS;
    result.size_stepwise = V4L2_FRMSIZE_TYPE_STEPWISE;
    result.interval_discrete = V4L2_FRMIVAL_TYPE_DISCRETE;
    result.interval_continuous = V4L2_FRMIVAL_TYPE_CONTINUOUS;
    result.interval_stepwise = V4L2_FRMIVAL_TYPE_STEPWISE;
    result.einval = EINVAL;
    result.enotty = ENOTTY;
    return result;
}

RawCapabilities query_cap(std::int32_t fd) noexcept
{
    v4l2_capability value{};
    RawCapabilities result{};
    if (::ioctl(fd, VIDIOC_QUERYCAP, &value) == -1) {
        result.error = errno;
        return result;
    }
    std::copy_n(value.driver, result.driver.size(), result.driver.begin());
    std::copy_n(value.card, result.card.size(), result.card.begin());
    std::copy_n(value.bus_info, result.bus_info.size(), result.bus_info.begin());
    result.capabilities = value.capabilities;
    result.device_caps = value.device_caps;
    return result;
}

RawFormat enum_format(std::int32_t fd, std::uint32_t buffer_type,
                      std::uint32_t index) noexcept
{
    v4l2_fmtdesc value{};
    value.type = buffer_type;
    value.index = index;
    RawFormat result{};
    if (::ioctl(fd, VIDIOC_ENUM_FMT, &value) == -1) {
        result.error = errno;
        return result;
    }
    result.index = value.index;
    result.buffer_type = value.type;
    result.fourcc = value.pixelformat;
    result.flags = value.flags;
    std::copy_n(value.description, result.description.size(), result.description.begin());
    return result;
}

RawSize enum_size(std::int32_t fd, std::uint32_t fourcc,
                  std::uint32_t index) noexcept
{
    v4l2_frmsizeenum value{};
    value.pixel_format = fourcc;
    value.index = index;
    RawSize result{};
    if (::ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &value) == -1) {
        result.error = errno;
        return result;
    }
    result.index = value.index;
    result.fourcc = value.pixel_format;
    result.kind = value.type;
    switch (value.type) {
    case V4L2_FRMSIZE_TYPE_DISCRETE:
        result.width = value.discrete.width;
        result.height = value.discrete.height;
        break;
    case V4L2_FRMSIZE_TYPE_CONTINUOUS:
    case V4L2_FRMSIZE_TYPE_STEPWISE:
        result.min_width = value.stepwise.min_width;
        result.max_width = value.stepwise.max_width;
        result.step_width = value.stepwise.step_width;
        result.min_height = value.stepwise.min_height;
        result.max_height = value.stepwise.max_height;
        result.step_height = value.stepwise.step_height;
        break;
    default:
        // Unknown tags stay visible, but their union payload is never read.
        break;
    }
    return result;
}

RawInterval enum_interval(std::int32_t fd, std::uint32_t fourcc,
                          std::uint32_t width, std::uint32_t height,
                          std::uint32_t index) noexcept
{
    v4l2_frmivalenum value{};
    value.pixel_format = fourcc;
    value.width = width;
    value.height = height;
    value.index = index;
    RawInterval result{};
    if (::ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &value) == -1) {
        result.error = errno;
        return result;
    }
    result.index = value.index;
    result.fourcc = value.pixel_format;
    result.width = value.width;
    result.height = value.height;
    result.kind = value.type;
    switch (value.type) {
    case V4L2_FRMIVAL_TYPE_DISCRETE:
        result.numerator = value.discrete.numerator;
        result.denominator = value.discrete.denominator;
        break;
    case V4L2_FRMIVAL_TYPE_CONTINUOUS:
    case V4L2_FRMIVAL_TYPE_STEPWISE:
        result.min_numerator = value.stepwise.min.numerator;
        result.min_denominator = value.stepwise.min.denominator;
        result.max_numerator = value.stepwise.max.numerator;
        result.max_denominator = value.stepwise.max.denominator;
        result.step_numerator = value.stepwise.step.numerator;
        result.step_denominator = value.stepwise.step.denominator;
        break;
    default:
        // Unknown tags stay visible, but their union payload is never read.
        break;
    }
    return result;
}

} // namespace furami::capture
