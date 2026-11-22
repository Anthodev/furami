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

`/absolute/verified-cache` must supply the exact SHA256-locked APKs and required sources/notices in `runtime.lock.json`, or permit HTTPS retrieval of those bytes. The base is pinned by its platform manifest digest. Missing/changed inputs fail; Alpine's CDN does not promise historical APK availability. The producer checks signatures with the base's keys and compares package metadata to the lock, then installs only the explicit selected archives with all repositories disabled. No upstream index is required. The selected versions overlay the base by name, leaving 150 unique package names, including the exact `musl` version required by `musl-dev`. Normal `build.py` also verifies all linked Alpine source inputs; modified-libfuse mode checks their lock associations to the unchanged APKs but does not fetch those unrelated source archives. Check the rebuilt manifest's modified source, linked objects/archives, notices and ELF guards.

The complete Alpine binary bootstrap (selected APKs plus unchanged base OCI closure) is retained **privately**, outside candidate/upload roots. It is not part of public `source-materials` archives and its exclusion is not a legal clearance of those tools. The complete candidate's material set contains corresponding runtime, application and frozen media sources, the complete five-component linked Alpine source/patch closure, recipes, locks, notices and provenance. A release uploads only the AppImage, `manifest.json`, a `-materials.tar` archive of the retained notices, licenses and receipts, and `SHA256SUMS`: the `source-materials-*.tar` archives are not among those downloads, so the reduced public files are not a complete corresponding-source offer. These materials do **not** claim a complete public offline bootstrap; exact build tools/base must separately remain obtainable. The private inventory check in `README.md` concerns private completeness, not public release readiness.

This uses LGPL-2.1 section 6's source/work-for-relink approach, **not** the shared-library mechanism. Matching source archives, patches and build scripts permit an interface-compatible modified libfuse to be rebuilt when matching build inputs remain obtainable. Distribution terms must permit library modification and reverse engineering for debugging those changes; retain the prominent LGPL notice and license. The candidate's existing source/license guard evaluates the **complete internal candidate**; public source/notice/relink completeness and actual release terms are not established by that internal readiness, nor by the reduced public downloads. Private tool retention, URLs and an unfulfilled source offer alone do not establish compliance, nor does a successful private restore promise a public offline bootstrap.
