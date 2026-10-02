#!/usr/bin/env bash
# Execute only in media lock's immutable Ubuntu 24.04 container, before stage.py.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive TZ=UTC LC_ALL=C.UTF-8
mkdir -p /tmp/furami-ca
printf '%s  %s\n' "$CA_SHA256" /bootstrap-ca.deb | sha256sum --check --status
dpkg-deb -x /bootstrap-ca.deb /tmp/furami-ca
find /tmp/furami-ca/usr/share/ca-certificates/mozilla -name '*.crt' -type f -print0 |
  sort -z | xargs -0 cat > /tmp/furami-ca/bundle.pem
[[ -s /tmp/furami-ca/bundle.pem ]]
printf 'Acquire::https::CaInfo "/tmp/furami-ca/bundle.pem";\n' > /etc/apt/apt.conf.d/50-furami-ca
cat > /etc/apt/sources.list.d/ubuntu.sources <<EOF
Types: deb deb-src
URIs: https://snapshot.ubuntu.com/ubuntu/${APT_SNAPSHOT}
Suites: noble noble-updates noble-backports noble-security
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
apt-get update -o APT::Update::Error-Mode=any
mapfile -t packages < /media/apt-direct-packages.txt
[[ ${#packages[@]} -gt 0 ]]
apt-get install --yes --no-install-recommends --download-only "${packages[@]}"
find /var/cache/apt/archives -maxdepth 1 -name '*.deb' -type f -print0 |
  sort -z | xargs -0 -r sha256sum > /tmp/stage-apt-debs.sha256
printf '%s  %s\n' "$APT_DEBS_SHA256" /tmp/stage-apt-debs.sha256 | sha256sum --check --status
cmp /tmp/stage-apt-debs.sha256 /media/apt-debs.sha256
apt-get install --yes --no-install-recommends "${packages[@]}"
dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\n' | sort > /tmp/stage-apt-installed.tsv
printf '%s  %s\n' "$APT_INSTALLED_SHA256" /tmp/stage-apt-installed.tsv | sha256sum --check --status
cmp /tmp/stage-apt-installed.tsv /media/apt-installed.tsv
# Recreate only authenticated package/source metadata, never stage another image.
if [[ ${1:-} == --capture-ubuntu-only ]]; then
  exec python3 /workspace/packaging/appimage/sources.py --capture-ubuntu \
    --stage /out --app /app --output /out
fi
python3 /workspace/packaging/appimage/tool-closure.py --phase preflight
# The media 408/317 closure above is finalized before adding three
# packaging-only packages; never relabel these as media/AppDir dependencies.
apt-get install --yes --no-install-recommends --download-only file
python3 /workspace/packaging/appimage/tool-closure.py --phase downloaded
apt-get install --yes --no-install-recommends file
python3 /workspace/packaging/appimage/tool-closure.py \
  --phase installed --output /out/stage-tool-closure.json
python3 /workspace/packaging/appimage/stage.py \
  --media-output /media --app-build /app --runtime-build /runtime \
  --output /out --version "$FURAMI_VERSION" --media-source-cache /media-sources --in-container
exec python3 /workspace/packaging/appimage/sources.py --capture-ubuntu \
  --stage /out --app /app --output /out
