# AppImage type-2 runtime recipe

Builds only the x86_64 type-2 runtime. This recipe does **not** bundle Furami's AppDir or run appimagetool.

```sh
python3 packaging/appimage-runtime/build.py /absolute/new-output
python3 packaging/appimage-runtime/build-twice.py /absolute/new-comparison
```

Requires rootless Podman, HTTPS access for initial input acquisition, and space for 229 MB of APK downloads, one temporary APK snapshot per runtime build, and the build itself. `--source-cache /absolute/cache` shares checked inputs; default is `$XDG_CACHE_HOME/furami/appimage-runtime` (or `~/.cache/furami/appimage-runtime`). Outputs must be new absolute paths. A successful build writes `runtime-x86_64`, `manifest.json`, `licenses/`, package/toolchain records, raw link map, and `build.log`. A failed build has no manifest; inspect its build log. The two-build helper creates fresh independent containers and compares runtime bytes, package closure, manifest and link map. Clang creates a random `/tmp/runtime-xxxxxx.o` filename when compiling/linking in one step; it appears in the map, changing its raw SHA256 in the manifest but not the runtime binary. `comparison.json` records raw hashes and offsets, the two temporary filenames and occurrence count, then accepts the run **only** if removing that single filename class makes the complete maps equal and the manifest differs solely in its correctly recorded raw map hash. Any other variance fails.

If a lock update changes a fixed-name runtime source, notice, or `APKBUILD`, use a new input cache (`--source-cache` here, `--runtime-source-cache` in the AppImage builder). The builder rejects mismatching cached bytes rather than replacing retained inputs.

`runtime.lock.json` pins upstream type2-runtime commit `75849dce7cc37e4319b633df1f116ca895c71a12`, FUSE 3.15.0, squashfuse 0.5.2, the FUSE mount patch, the Alpine 3.21 linux/amd64 **platform manifest**, and the original base APK database. It selects **139 APK archives** by exact name and version, whole-file SHA256, Alpine control checksum, aports commit and package license. The selected overlay replaces the base versions of `libcrypto3`, `libssl3`, `zlib` and `musl`; `musl-dev` requires the exact matching `musl` version. The effective installed closure has **150 unique package names**, from the base map followed by the selected overlay. Old versions, duplicate records and unexpected packages fail the installed-state guard.

The producer copies only the selected APKs into a private per-run snapshot with lock-derived `SHA256SUMS`. Both containers mount it read-only and check its SHA256s and signatures with the immutable base's keys. `apk index` reads the signed `.PKGINFO` control metadata; name, version, architecture, license, aports commit and APK control checksum must match the lock/target before build. The `C` field is **not** a whole-file SHA-1 or SHA256. Provisioning repeats the SHA256 and signature checks immediately before installing exactly that archive list with no repositories. The snapshot is removed after the run; `authentication/run-*/` keeps the selected index, log and receipt separately for each invocation. Those files are private proof, not solver inputs. No live `APKINDEX`, cache entry or `apk update` is needed for a build.

`alpine-source.lock.json` distinguishes binary APK SHA256 from corresponding source provenance for linked musl, zstd, zlib, secure mimalloc and GCC startup objects. Before a normal build, the producer checks all five exact-commit aports recipes, their upstream archives and every patch or other input against the locked SHA256 and the APKBUILD SHA512 entries.

Alpine **binary bootstrap APKs and the base-image closure are private inputs**, excluded from public candidate/source assets. Public materials retain the actual runtime, the application source and locked Cargo inputs, the frozen media sources, the complete linked Alpine source closure, recipes, locks, notices and provenance. Excluding binary build tools does not clear their redistribution terms; they are not publicly distributed. Public assets do **not** promise a complete offline Alpine bootstrap.

A retained private generation contains all selected APKs, runtime sources/notices, all five linked source closures, the exact OCI manifest/config/layers, recipe snapshots and APK-authentication/offline-restoration receipts. It must stay outside candidate and evidence-upload roots. Its relative SHA256 inventory is checked separately from public source/license readiness:

```sh
python3 packaging/appimage-runtime/build.py --check-private-inputs \
  --source-cache /absolute/private-generation/sources
```

This integrity/completeness check performs no network access or build and does not re-run signatures or prove public clearance. It requires the retained apk-generated selected index, installed-package output and restored raw manifest, and checks them against the exact selected closure, effective base overlay and immutable manifest. Copy the index, log and APK receipt from one producer run when assembling `private/authentication/`; successful APK and offline-restore receipts must bind the current runtime/source locks, selected-APK hashes, producer/provisioning recipes, logs and derived outputs. Same-base success receipts from another closure are not sufficient. Actual restoration must use isolated image storage, unavailable networking, and the exact digest reference with `--pull=never`. Mount the exact APK snapshot and its lock-derived `SHA256SUMS` at `/selected-apks` read-only and pass the explicit sorted archive paths to `provision.sh`. Loading a converted archive or matching only a config ID is not proof of preserving the upstream manifest. Keep historical receipts/inventories unchanged when emitting new bound proof; do not repurpose the host image store.

The Alpine CDN need not keep historical **APK archives** or guarantee immutable URLs. Missing or changed locked bytes still fail rather than rolling forward. Initial acquisition needs HTTPS and a locally available or pullable immutable base; compilation has networking disabled. Reuse available exact inputs rather than creating a cache just for repository indexes. Private bootstrap retention and public corresponding-source clearance remain distinct.

Build uses upstream runtime Makefile flags and patches bytes `AI\x02` at ELF offsets 8–10 **after** stripping, matching upstream `scripts/build-runtime.sh`. FUSE's mount patch changes fusermount lookup to `FUSERMOUNT_PROG`, which the upstream runtime sets when executing an AppImage. Runtime version marker is replaced with the pinned commit, as upstream CI does. `manifest.json` includes link-map archive membership, observed installed package closure, compiler/linker versions, exact recipes and build flags, artifact hash, license file hashes, and source references. ELF verification rejects a wrong architecture, missing type-2 magic, dynamic interpreter or `DT_NEEDED` library. It cannot prove which dead-stripped functions from a static archive remain in the executable; the archive list is a conservative inclusion record, not a per-function SBOM.

Link audit attributes direct musl `rcrt1.o`, `crti.o`, `crtn.o`, GCC `crtbeginS.o`/`crtendS.o` (GPL-3.0-or-later WITH GCC-exception-3.1) and the compiled runtime object as well as static archive members. Unexpected direct objects or archives fail the manifest gate. Successful output also includes `relink/` with the four verified upstream source/patch archives, pinned recipe/locks and operative modified-libfuse build instructions; `notice_files` and `relink_files` map each shipped notice/source/tool to SHA256.

Bundled notices cover the root runtime MIT license plus its `runtime.c` Alexander Larsson MIT notice, FUSE linked library code (LGPL-2.1-only), squashfuse (BSD-2-Clause), musl (MIT plus COPYRIGHT exceptions), zstd (BSD-3-Clause selection), zlib (Zlib), secure mimalloc (MIT), and the actually linked GCC startup objects (GPL-3.0-or-later with GCC Runtime Library Exception 3.1). Upstream runtime `LICENSE` omits mimalloc; this recipe supplies its notice. Alpine's SPDX package fields cover build-only tooling too; they do **not** imply every installed tool is linked. Static libfuse requires matching source and ability to modify/relink, not an invented source offer. `relink/RELINKING.md` documents a modified-libfuse rebuild route and explicit distribution gate: no AppImage publication until FUR-005 supplies complete recipient-accessible sources/toolchain and reviews license terms. This qualification kit alone is not legal compliance certification.

## Updating the Alpine lock

Only when reviewing a lock update, download fresh `main` and `community` x86_64 `APKINDEX.tar.gz` files into a temporary directory. Authenticate them using the keys from the candidate immutable base, not keys supplied with a download. For example, to use the base and repository currently in the lock:

```sh
(
  set -eu
  lock=packaging/appimage-runtime/runtime.lock.json
  image=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["container"])' "$lock")
  repository=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["repository_base_url"])' "$lock")
  indexes=$(mktemp -d)
  trap 'rm -rf "$indexes"' EXIT
  for repo in main community; do
    mkdir -p "$indexes/$repo/x86_64"
    printf 'file:///indexes/%s\n' "$repo" >> "$indexes/repositories"
    curl --fail --silent --show-error --location \
      "$repository/$repo/x86_64/APKINDEX.tar.gz" -o "$indexes/$repo/x86_64/APKINDEX.tar.gz"
  done
  podman run --rm --pull=missing --platform linux/amd64 --network=none \
    --security-opt label=disable --volume "$indexes:/indexes:ro" "$image" \
    sh -ec 'apk --no-network --repositories-file /indexes/repositories verify "$@";
            apk --no-network --repositories-file /indexes/repositories update' \
    sh /indexes/main/x86_64/APKINDEX.tar.gz /indexes/community/x86_64/APKINDEX.tar.gz
  for repo in main community; do
    tar --ignore-zeros -xOzf "$indexes/$repo/x86_64/APKINDEX.tar.gz" APKINDEX > "$indexes/$repo.txt"
  done
  less "$indexes/main.txt" "$indexes/community.txt"
)
```

Compare each proposed selection with the verified row from the intended repository: `P` (name), `V` (version), `A` (architecture), `C` (control checksum), `L` (license) and `c` (aports commit). Check and record the exact APK SHA256 independently. A valid package signature does not prove that the package appeared in a particular repository; repository membership is reviewed here, not re-queried by builds. A historical APK absent from the current index must not have its provenance inferred from that index or be silently replaced. Update the linked aports recipe/source/patch pins whenever the matched binary inputs change; verify the exact effective installed set with the normal producer. Do not add whole-index hashes, downloads or retained archives to normal build or relink inputs.
