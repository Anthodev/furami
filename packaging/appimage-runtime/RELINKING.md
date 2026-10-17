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

`/absolute/verified-cache` must contain the exact SHA256-locked APKs, authenticated indexes and linked Alpine source inputs in `runtime.lock.json` and `alpine-source.lock.json`, or permit HTTPS retrieval of those bytes. Missing/changed inputs fail; Alpine's live CDN does not promise historical availability. The producer authenticates indexes using the original immutable base's keys before accepting metadata, verifies selected APK signatures and apk-generated identities, and passes only selected archives to `provision.sh`. It overlays selected versions by package name on the immutable base; the final closure is 150 packages, not both old and new OpenSSL/zlib versions. Normal `build.py` uses locked sources; `relink.sh` alone explicitly opts into `--modified-libfuse` and records the changed hash. Check the rebuilt manifest's modified source, linked objects/archives, notices and ELF guards.

The complete Alpine binary bootstrap (138 selected APKs plus unchanged base OCI closure) is retained **privately**, outside candidate/upload roots. It is not part of public `source-materials` archives and its exclusion is not a legal clearance of those tools. Public materials contain corresponding runtime/application/Qt sources and the complete five-component linked Alpine source/patch closure, including zlib's `CVE-2026-85091.patch`, recipes, locks, notices and provenance. These materials do **not** claim a complete public offline bootstrap; exact build tools/base must separately remain obtainable. The private inventory check in `README.md` concerns private completeness, not public release readiness.

This uses LGPL-2.1 section 6's source/work-for-relink approach, **not** the shared-library mechanism. Matching source archives, patches and build scripts permit an interface-compatible modified libfuse to be rebuilt when matching build inputs remain obtainable. Distribution terms must permit library modification and reverse engineering for debugging those changes; retain the prominent LGPL notice and license. Public source/notice/relink completeness and actual release terms remain subject to the candidate's existing source/license guard. Private tool retention, URLs and an unfulfilled source offer alone do not establish compliance, nor does a successful private restore promise a public offline bootstrap.
