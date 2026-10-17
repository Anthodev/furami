# Isolated Fedora 44 GUI-qualification environment for a Furami AppImage.
#
# This image exists only to give one extracted candidate a real Wayland/XWayland
# compositor, a hardware Vulkan/GL stack and a screenshot tool. It is never the
# host, never mounts the host session bus/audio, and never ships the application
# or any media provider. Build it explicitly:
#
#   podman build --pull=never \
#     --build-arg FEDORA_BASE=registry.fedoraproject.org/fedora@sha256:923dd7d4362a78ef7edc1887cb599720e9f59ff8669a38c81062c6e2aaa83204 \
#     --iidfile "$EVIDENCE/fedora44.iid" \
#     -f packaging/appimage/qualification-fedora44.Containerfile packaging/appimage
#
# The base is a digest, never a floating tag. The argument is declared before
# FROM so the reference cannot silently drift.
ARG FEDORA_BASE
FROM ${FEDORA_BASE}

# An ARG declared before FROM is only in scope for FROM; redeclare it inside the
# stage so the provenance label below records the real base digest.
ARG FEDORA_BASE

LABEL org.furami.qualification="fedora44-kwin-wayland-appimage-smoke" \
      org.furami.qualification.base="${FEDORA_BASE}"

# System prerequisites only: KWin Wayland with its virtual backend and XWayland,
# Spectacle for region captures, the reference D-Bus daemon (dbus-run-session),
# the Mesa Vulkan/GL stack, fonts for Qt text, Python+Pillow for the in-session
# harness, and basic process/file inspection tools.
#
# install_weak_deps=False keeps optional pulls (including media providers) out.
# No mpv/ffmpeg/ffprobe program may enter this image: the candidate must run on
# bundled providers. The assertion below fails the build closed if any arrives.
#
# Fedora ships /usr/bin/kwin_wayland with the file capability cap_sys_nice=ep.
# Linux execve refuses a capability-bearing file when any requested capability
# is missing from the caller's bounding set, so the smoke's approved
# --cap-drop=all would fail before KWin starts (EPERM). Removing the attribute
# here keeps every runtime guard intact instead of granting a capability. The
# image is a test fixture, not the shipped Fedora package: KWin simply loses
# its ability to request realtime scheduling. /usr/bin/setcap and
# /usr/bin/getcap come from libcap, already installed as a package dependency.
# This runs in the install layer so no capped kwin_wayland is ever persisted,
# and it fails the build closed if the attribute survives.
RUN dnf --setopt=install_weak_deps=False -y install \
        kwin-wayland \
        xorg-x11-server-Xwayland \
        spectacle \
        dbus-daemon \
        mesa-vulkan-drivers \
        mesa-dri-drivers \
        mesa-libGL \
        mesa-libEGL \
        libglvnd-glx \
        libglvnd-egl \
        vulkan-loader \
        vulkan-tools \
        fontconfig \
        dejavu-sans-fonts \
        python3 \
        python3-pillow \
        procps-ng \
        util-linux \
    && dnf clean all \
    && for program in mpv ffmpeg ffprobe; do \
           if command -v "$program" >/dev/null 2>&1; then \
               echo "qualification image must not ship $program" >&2; exit 1; \
           fi; \
       done \
    && test -x /usr/bin/kwin_wayland \
    && test -x /usr/bin/Xwayland \
    && test -x /usr/bin/spectacle \
    && test -x /usr/bin/dbus-run-session \
    && test -x /usr/bin/vulkaninfo \
    && setcap -r /usr/bin/kwin_wayland \
    && kwin_caps="$(getcap /usr/bin/kwin_wayland)" \
    && test -z "${kwin_caps}"

# Record the exact RPM set (NEVRA, epoch included) for the evidence trail.
RUN rpm -qa --qf '%{NAME}-%{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n' | LC_ALL=C sort \
        > /etc/furami-qualification-rpms.txt

# The compositor and screenshot tool live outside any application loader
# environment; nothing here exports AppImage Qt/LD paths.
ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    KWIN_BACKEND=virtual

# Headless by construction: no display, session bus or service is set up here.
# The smoke creates one private dbus-run-session per invocation.
WORKDIR /root
