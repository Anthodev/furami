# Rebuilding with a modified libfuse

This directory accompanies the runtime as **qualification material**, not a certification of distribution compliance. It contains exact type2-runtime, libfuse 3.15.0 and squashfuse 0.5.2 source archives, the upstream-carried `mount.c.diff`, pinned recipe and dependency locks, and `relink.sh`. Every file is SHA256-listed in the runtime manifest's `relink_files`. `licenses/` alongside it contains LGPL-2.1, the root FUSE notice, per-file runtime notices and notices for other linked code.

An interface-compatible edited FUSE source archive must preserve the original single top-level source directory and not already contain `mount.c.diff`. The script applies that patch with `--fuzz=0` after extraction, recompiles static libfuse/squashfuse and links the runtime in a fresh digest-pinned, network-disabled Alpine container. Changed source SHA256 is recorded distinctly from the original FUSE SHA256. For example:

```sh
mkdir -p /absolute/edit/fuse
# Extract the archived source, edit lib/fuse_opt.c (or another compatible
# library source), then repack its top-level fuse-3.15.0 directory.
tar -xf fuse-3.15.0.tar.xz -C /absolute/edit/fuse
${EDITOR:-vi} /absolute/edit/fuse/fuse-3.15.0/lib/fuse_opt.c
tar -cJf /absolute/edit/modified-fuse.tar.xz -C /absolute/edit/fuse fuse-3.15.0
./relink.sh /absolute/new-runtime /absolute/edit/modified-fuse.tar.xz /absolute/verified-cache
```

`/absolute/verified-cache` must contain SHA256-locked APKs, signed indexes and linked Alpine source inputs recorded in `runtime.lock.json` and `alpine-source.lock.json`, or allow HTTPS retrieval of those exact bytes. Missing or changed inputs fail the build; the pinned Alpine mirror does not promise archival availability. The exact complete verified cache exists as **local qualification evidence**, not inside the AppImage. Normal `build.py` builds only locked sources; `relink.sh` passes explicit `--modified-libfuse` opt-in and records its changed SHA256. Check rebuilt `manifest.json` for `modified_libfuse`, artifact SHA256, `linked_objects`, `linked_archives` and notices; the new ELF must still have x86_64 type-2 magic and no PT_INTERP or DT_NEEDED.

This route uses LGPL-2.1 section 6's source/work-for-relink approach, **not** the shared-library mechanism. The source archives, exact patch and build scripts enable recipients to modify libfuse and rebuild the executable, provided matching dependency/toolchain inputs remain obtainable. Redistribution terms must permit library modification and reverse engineering for debugging such modifications; retain the prominent LGPL notice and license. FUR-005 must arrange durable recipient access to matching complete sources and tools (including linked Alpine source/patch closure and verified APK/toolchain inputs) alongside any distributed AppImage, plus review the actual release terms. URLs, this local cache and an unfulfilled source offer alone do not satisfy that delivery requirement. Do not publish a runtime/AppImage on the strength of this qualification kit alone.
