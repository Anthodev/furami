#pragma once

#include <cstdint>

namespace furami::capture {

struct V4l2Constants;
struct RawCapabilities;
struct RawFormat;
struct RawSize;
struct RawInterval;

V4l2Constants v4l2_constants() noexcept;
RawCapabilities query_cap(std::int32_t fd) noexcept;
RawFormat enum_format(std::int32_t fd, std::uint32_t buffer_type,
                      std::uint32_t index) noexcept;
RawSize enum_size(std::int32_t fd, std::uint32_t fourcc,
                  std::uint32_t index) noexcept;
RawInterval enum_interval(std::int32_t fd, std::uint32_t fourcc,
                          std::uint32_t width, std::uint32_t height,
                          std::uint32_t index) noexcept;

} // namespace furami::capture
