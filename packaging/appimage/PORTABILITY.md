# Linux AppImage portability

Furami ships one x86_64 AppImage, built from the pinned Ubuntu 24.04 environment. The AppImage carries Furami, Qt/QML, and its frozen media stack; it deliberately uses the host Vulkan loader, GPU driver, ICD, and graphics stack. A Fedora build host does not create a Fedora-specific image.

## Host requirements

- **glibc:** Ubuntu 24.04's `libc6` is glibc 2.39 and sets the intended build baseline. This does not by itself prove a 2.39 minimum: the release audit must inspect every shipped ELF—including the separately pinned Qt kit—for required `GLIBC_*` symbols and interpreter. Do not claim older-glibc support unless that complete audit and runtime qualification pass. [Ubuntu `libc6`](https://packages.ubuntu.com/noble/libc6); [AppImage build guidance](https://docs.appimage.org/reference/best-practices.html).
- **Wayland/X11:** On Wayland, Furami requires a running XWayland server, a usable `DISPLAY`, Qt's `xcb` QPA plugin, and X11 SHAPE 1.1. This is not a native Wayland-QPA application. The AppImage supplies its Qt/XCB client-side libraries, not the compositor's X server or XWayland service. [Qt Linux requirements](https://doc.qt.io/qt-6.11/linux-requirements.html); [X.Org SHAPE 1.1 protocol](https://www.x.org/releases/X11R7.7-RC1/doc/xextproto/shape.pdf).
- **Vulkan/GPU:** A host Vulkan loader and a working ICD for the host GPU are required. GPU and driver must expose capabilities required by the frozen media configuration. Furami does not bundle Mesa, Vulkan ICD manifests, GPU drivers, or a software-rendering fallback. [Khronos Vulkan driver discovery](https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderDriverInterface.md).
- **FUSE:** The pinned type-2 runtime statically links FUSE and does not require host `libfuse2`; mounted launch still needs normal host FUSE setup, including an accessible `/dev/fuse` and usable `fusermount`/`fusermount3` helper. Without FUSE, extract with `./Furami.AppImage --appimage-extract`, then launch `squashfs-root/AppRun`. Extraction does not prove mounted execution. [Pinned runtime](https://github.com/AppImage/type2-runtime/tree/75849dce7cc37e4319b633df1f116ca895c71a12); [AppImage extraction](https://docs.appimage.org/user-guide/run-appimages.html).
- **Qt/QML isolation:** Qt libraries, plugins, and QML imports come from the AppImage. Its launcher sets bundle-local paths; a system Qt installation is not a prerequisite. Avoid overriding them with system Qt/QML paths. [Qt Linux deployment](https://doc.qt.io/qt-6.11/linux-deployment.html); [QML import paths](https://doc.qt.io/qt-6.11/qtqml-syntax-imports.html).

- **Fonts:** The pinned ELF closure supplies Fontconfig, while configuration and font files remain host desktop data: `/etc/fonts`, `/usr/share/fonts`, and normal per-user font/config roots. The launcher preserves XDG state and deliberate `FONTCONFIG_*` settings. This host font lookup is separate from Qt/QML plugin paths and adds no separate font bundle, engine, or ELF dependency. [Fontconfig configuration](https://fontconfig.pages.freedesktop.org/fontconfig/fontconfig-user.html); [Ubuntu font files](https://packages.ubuntu.com/noble/all/fonts-dejavu-core/filelist).
- **Launch and desktop integration:** Make the downloaded AppImage executable before direct launch. File managers use different permission labels (for example, GNOME Files and KDE Dolphin); menu integration is optional and desktop-specific, not guaranteed by the AppImage itself. Verify the published checksum before running. [AppImage quickstart](https://docs.appimage.org/introduction/quickstart.html); [desktop integration](https://docs.appimage.org/reference/desktop-integration.html).

The graphics boundary also carries an ABI risk: a newer host driver can require `GLIBCXX_*` or `CXXABI_*` symbols absent from the bundled C++ runtime. An image-wide ELF audit alone cannot prove that later `dlopen` of the host driver will succeed. Runtime verification records the loaded loader, ICD and driver libraries; rolling distributions remain untested without that evidence. Do not fix such a collision by copying the workstation's libraries into the portable image. [GCC C++ ABI version history](https://gcc.gnu.org/onlinedocs/libstdc++/manual/abi.html).

The following package identifiers are confirmed in the linked distribution repositories as of 2026-10-02. They identify examples, not an exhaustive install recipe; select the Vulkan ICD matching the actual GPU. `fuse3` supplies the `fusermount3` helper on these examples, but is unnecessary for the extraction path.

| Target | XWayland | Vulkan loader | Mesa ICD example | FUSE helper package |
|---|---|---|---|---|
| Ubuntu 24.04 | [`xwayland`](https://packages.ubuntu.com/noble/amd64/xwayland) | [`libvulkan1`](https://packages.ubuntu.com/noble/amd64/libvulkan1) | [`mesa-vulkan-drivers`](https://packages.ubuntu.com/noble/amd64/mesa-vulkan-drivers) | [`fuse3`](https://packages.ubuntu.com/noble/amd64/fuse3) |
| Fedora 44 | [`xorg-x11-server-Xwayland`](https://packages.fedoraproject.org/pkgs/xorg-x11-server-Xwayland/xorg-x11-server-Xwayland/fedora-44.html) | [`vulkan-loader`](https://packages.fedoraproject.org/pkgs/vulkan-loader/vulkan-loader/fedora-44.html) | [`mesa-vulkan-drivers`](https://packages.fedoraproject.org/pkgs/mesa/mesa-vulkan-drivers/fedora-44.html) | [`fuse3`](https://packages.fedoraproject.org/pkgs/fuse3/fuse3/fedora-44.html) |
| Arch Linux | [`xorg-xwayland`](https://archlinux.org/packages/extra/x86_64/xorg-xwayland/) | [`vulkan-icd-loader`](https://archlinux.org/packages/extra/x86_64/vulkan-icd-loader/) | [`vulkan-radeon`](https://archlinux.org/packages/extra/x86_64/vulkan-radeon/) (AMD) | [`fuse3`](https://archlinux.org/packages/extra/x86_64/fuse3/) |

## Compatibility status

"Expected" describes a research-based target profile, not runtime evidence. Only the Fedora profile below has been exercised with this AppImage; Ubuntu and Arch remain **UNTESTED**.

| Host profile | Repository glibc package/version checked 2026-10-02 | Assessment | AppImage evidence |
|---|---|---|---|
| Ubuntu 24.04 / GNOME / Wayland | `libc6` 2.39 | Expected reference baseline; still needs whole-image ABI audit and runtime test. | **UNTESTED** |
| Arch Linux rolling / Hyprland / Wayland | `glibc` 2.44 | Expected if actual ELF ABI, XWayland/SHAPE, and host Vulkan ICD requirements are met. | **UNTESTED** |
| Fedora 44 / KDE Plasma 6.7.5 / Wayland, XWayland 24.1.13, AMD RX 7900 XTX / Mesa 26.2.3 RADV | `glibc` 2.43 | Mounted terminal launch, temporary desktop-entry launch, extracted launch in a path containing spaces, embedded animated proof video and ordinary closure passed. | **TESTED**, 2026-10-02 |

Repository package versions can change. These rows do not cover other releases, compositors, GPUs, or native-X11 sessions.

The Fedora artifact is `Furami-0.1.0-x86_64.AppImage`, SHA-256 `ed825e4a0d03eefdbbb47b583c443535879de262b77a15b9a577940d4171eb6b`. Runtime logs selected the RX 7900 XTX through host `/usr/lib64/libvulkan_radeon.so`; process maps place Qt/QML, libmpv, FFmpeg and libplacebo under the mount or extracted tree. Other host ICD libraries were enumerated, not selected as a software-rendering fallback.

The desktop-entry path used `gio launch` with temporary metadata and no persistent menu or MIME changes. Dolphin's current AppImage association opens Ark, so automatic double-click execution was not proven. System mpv/FFmpeg remained installed. Extraction passed on a FUSE-capable host, not on a separate FUSE-less host. Missing, empty and unreachable `DISPLAY` returned controlled exit 1 with XWayland guidance. These observations do not establish release source/license clearance or CI reproducibility.