#!/usr/bin/env bash
# Invoked by build-app.py only, inside digest-pinned Ubuntu 24 OCI image.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive TZ=UTC LC_ALL=C.UTF-8
mkdir -p /build/evidence /build/bin /build/rust /build/vendor /build/cargo-home /build/wrappers

# Reuse media recipe's direct requests; prove exact transitive versions and bytes
# against same stack.lock.json closure. No separately floating APT dependencies.
mkdir -p /tmp/noble-ca
printf '%s  %s\n' "$CA_CERT_SHA256" /sources/ca-certificates.deb |
  sha256sum --check --status || { echo 'verified CA archive checksum mismatch' >&2; exit 1; }
dpkg-deb -x /sources/ca-certificates.deb /tmp/noble-ca
find /tmp/noble-ca/usr/share/ca-certificates/mozilla -name '*.crt' -type f -print0 |
  sort -z | xargs -0 cat > /tmp/noble-ca/bundle.pem
[[ -s /tmp/noble-ca/bundle.pem ]]
printf 'Acquire::https::CaInfo "/tmp/noble-ca/bundle.pem";\n' > /etc/apt/apt.conf.d/50-furami-ca
cat > /etc/apt/sources.list.d/ubuntu.sources <<EOF
Types: deb
URIs: https://snapshot.ubuntu.com/ubuntu/${APT_SNAPSHOT}
Suites: noble noble-updates noble-backports noble-security
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
apt-get update -o APT::Update::Error-Mode=any
mapfile -t packages < /out/apt-direct-packages.txt
(( ${#packages[@]} > 0 ))
apt-get install --yes --no-install-recommends --download-only "${packages[@]}"
find /var/cache/apt/archives -maxdepth 1 -name '*.deb' -type f -print0 |
  sort -z | xargs -0 -r sha256sum > /build/evidence/apt-debs.sha256
apt-get install --yes --no-install-recommends "${packages[@]}"
dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\n' | sort > /build/evidence/apt-installed.tsv
printf '%s  %s\n' "$APT_CLOSURE_SHA256" /workspace/packaging/media/build-apt-closure.json | sha256sum --check --status
printf '%s  %s\n' "$APT_INSTALLED_SHA256" /build/evidence/apt-installed.tsv | sha256sum --check --status
printf '%s  %s\n' "$APT_DEBS_SHA256" /build/evidence/apt-debs.sha256 | sha256sum --check --status

# Official component installers supply rustc, cargo, std, and corresponding
# standard-library sources; never use Fedora's cargo/rustc.
for component in rustc cargo rust-std rust-src; do
  archive=$(python3 - "$component" <<'PY'
import json, sys
print(json.load(open('/workspace/packaging/appimage/app.lock.json'))['toolchain'][sys.argv[1]]['archive'])
PY
)
  mkdir -p "/build/toolchain-$component"
  tar -xf "/sources/$archive" --strip-components=1 -C "/build/toolchain-$component"
  "/build/toolchain-$component/install.sh" --prefix=/build/rust --disable-ldconfig >/build/evidence/install-"$component".log
done
export PATH="/build/rust/bin:$PATH"
rustc --version --verbose > /build/evidence/rustc-version.txt
cargo --version --verbose > /build/evidence/cargo-version.txt
[[ "$(rustc --version)" == 'rustc 1.97.1 '* ]]
[[ "$(cargo --version)" == 'cargo 1.97.1 '* ]]
[[ -d /build/rust/lib/rustlib/src/rust/library ]] || {
  echo 'verified Rust standard-library sources missing from installed toolchain' >&2
  exit 1
}

# Extract locked .crate files into Cargo directory source, generating per-file
# checksums from verified archive content. Reject paths/links escaping vendor.
python3 - <<'PY'
import hashlib, json, pathlib, tarfile, tomllib
root = pathlib.Path('/build/vendor')
lock = tomllib.loads(pathlib.Path('/workspace/Cargo.lock').read_text())
for pkg in lock['package']:
    if 'source' not in pkg:
        continue
    assert pkg['source'] == 'registry+https://github.com/rust-lang/crates.io-index'
    name = f"{pkg['name']}-{pkg['version']}"
    archive = pathlib.Path('/sources') / f'{name}.crate'
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == pkg['checksum'], name
    directory = root / name
    directory.mkdir()
    files = {}
    with tarfile.open(archive) as tar:
        for member in tar:
            parts = pathlib.PurePosixPath(member.name).parts
            assert parts and parts[0] == name and all(p not in ('..', '.') for p in parts), member.name
            if len(parts) == 1:
                assert member.isdir(), member.name
                continue
            relative = pathlib.PurePosixPath(*parts[1:])
            destination = directory.joinpath(*relative.parts)
            if member.isdir():
                destination.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                assert relative.as_posix() != '.cargo-checksum.json', name
                destination.parent.mkdir(parents=True, exist_ok=True)
                content = tar.extractfile(member).read()
                destination.write_bytes(content)
                files[relative.as_posix()] = hashlib.sha256(content).hexdigest()
            else:
                raise ValueError(f'{name}: unexpected archive member {member.name}: {member.type}')
    (directory / '.cargo-checksum.json').write_text(json.dumps({'files': files, 'package': pkg['checksum']}))
PY
cat > /build/cargo-home/config.toml <<'EOF'
[source.crates-io]
replace-with = "verified-vendor"
[source.verified-vendor]
directory = "/build/vendor"
[net]
offline = true
EOF

# Wrappers record actual native compiler and linker commands; final executable
# gets relocatable RUNPATH and independent linker map (not source grep guesses).
cat > /build/wrappers/cxx <<'EOF'
#!/usr/bin/env bash
printf -v line '%q ' "$@"
printf '%s\n' "$line" >> /build/evidence/cxx-commands.txt
exec c++ "$@"
EOF
cat > /build/wrappers/linker <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf -v line '%q ' "$@"
printf '%s\n' "$line" >> /build/evidence/linker-commands.txt
final=0
for argument in "$@"; do
  case "$argument" in
    /build/target/release/deps/furami-*|/build/target/release/furami) final=1 ;;
  esac
done
if (( final )); then
  args=(-fuse-ld=lld '-Wl,-rpath,$ORIGIN/../lib' -Wl,-Map,/build/evidence/furami.link.map "$@")
  printf -v line '%q ' cc "${args[@]}"
  printf '%s\n' "$line" > /build/evidence/furami-link-args.txt
  exec cc "${args[@]}"
fi
exec cc -fuse-ld=lld "$@"
EOF
chmod +x /build/wrappers/cxx /build/wrappers/linker
# QMAKE remains the actual frozen tool. Only rcc gets a read-only build-container
# bind, logging its invocation and executing unchanged original provider bytes.
# No host prefix, runtime library, qmake query, or vendored crate is modified.
export QMAKE=/out/prefix/bin/qmake6 FURAMI_MEDIA_PREFIX=/out/prefix
export PKG_CONFIG_LIBDIR=/out/prefix/lib/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig
unset PKG_CONFIG_PATH PKG_CONFIG_SYSROOT_DIR
export LD_LIBRARY_PATH=/out/prefix/lib
[[ "$("$QMAKE" -query QT_VERSION)" == '6.11.2' ]]
[[ "$("$QMAKE" -query QT_INSTALL_PREFIX)" == '/out/prefix' ]]
[[ "$(pkg-config --modversion Qt6Core)" == '6.11.2' ]]
[[ "$(pkg-config --variable=prefix Qt6Core)" == '/out/prefix' ]]
[[ "$(pkg-config --modversion mpv)" == '2.5.0' ]]
[[ "$(pkg-config --variable=prefix mpv)" == '/out/prefix' ]]
pkg-config --exists x11 xcb xcb-shape
pkg-config --cflags --libs Qt6Core > /build/evidence/qt-pkg-config.txt
if grep -Eq '/home/qt/work/install|/usr/(local/)?(lib|include).*[Qq]t6' /build/evidence/qt-pkg-config.txt; then
  echo 'Qt build flags escaped the frozen prefix' >&2
  exit 1
fi
export CARGO_HOME=/build/cargo-home CARGO_TARGET_DIR=/build/target CARGO_NET_OFFLINE=true
export CARGO_BUILD_JOBS="${FURAMI_BUILD_JOBS:-2}"
[[ "$CARGO_BUILD_JOBS" =~ ^[1-4]$ ]] || { echo 'build jobs must be 1..4' >&2; exit 1; }
export CXX=/build/wrappers/cxx
export CFLAGS='-O2 -ffile-prefix-map=/workspace=/usr/src/furami -ffile-prefix-map=/build=/usr/src/furami-build'
export CXXFLAGS="$CFLAGS"
export RUSTFLAGS='-C linker=/build/wrappers/linker --remap-path-prefix=/workspace=/usr/src/furami --remap-path-prefix=/build=/usr/src/furami-build'
export SOURCE_DATE_EPOCH

cargo metadata --offline --locked --format-version=1 --filter-platform x86_64-unknown-linux-gnu > /build/evidence/cargo-metadata.json
cargo build --release --offline --frozen --verbose > /build/evidence/cargo-build.log 2>&1 || {
  cat /build/evidence/cargo-build.log
  exit 1
}
cp /build/target/release/furami /build/bin/furami
[[ -s /build/evidence/furami.link.map && -s /build/evidence/furami-link-args.txt ]]
readelf -h /build/bin/furami > /build/evidence/furami.elf-header.txt
readelf -d /build/bin/furami > /build/evidence/furami.dynamic.txt
readelf -Ws /build/bin/furami > /build/evidence/furami.symbols.txt
ldd /build/bin/furami > /build/evidence/furami.ldd.txt
! grep -q 'not found' /build/evidence/furami.ldd.txt
if grep -E '^[[:space:]]*lib(Qt6|mpv)' /build/evidence/furami.ldd.txt |
    grep -Ev '=> /out/prefix/lib/'; then
  echo 'Furami resolved a Qt/libmpv library outside the verified media prefix' >&2
  exit 1
fi
# libmpv is loaded at runtime from FURAMI_MEDIA_PREFIX/lib/libmpv.so.
