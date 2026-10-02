#!/usr/bin/env python3
"""Capture signed Ubuntu source metadata and assemble checked recipient inputs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.parse
import urllib.request
import tempfile

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def dump(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def paragraphs(text):
    for block in text.strip().split('\n\n'):
        fields = {}
        key = None
        for line in block.splitlines():
            if line.startswith((' ', '\t')) and key:
                fields[key] += '\n' + line.strip()
            elif ':' in line:
                key, value = line.split(':', 1)
                fields[key] = value.lstrip(' \t')
        yield fields


def command(argv):
    return subprocess.run(argv, check=True, capture_output=True, text=True).stdout


def capture_ubuntu(stage, app, output):
    """Run only in staging container after authenticated snapshot apt update."""
    receipt = json.loads((stage / 'evidence.json').read_text())
    app_manifest = json.loads((app / 'manifest.json').read_text())
    packages = {item['owner'].split(':', 1)[1].split('=', 1)[0]
                for item in receipt['component_sources'].values()
                if item['owner'].startswith('ubuntu:')}
    packages.update(('glslang-dev', 'spirv-tools'))
    packages.update(item['binary_package']['name'] for item in app_manifest['binary']['direct_objects'])
    # Source identities and checksums come from authenticated apt source lists,
    # not from an unverified descriptor URL or guessed binary package name.
    records = []
    evidence = output / 'ubuntu-source-evidence'
    evidence.mkdir(exist_ok=True)
    apt_lists = Path('/var/lib/apt/lists')
    index_files = {}
    for path in sorted(apt_lists.iterdir()):
        if path.is_file() and ('_InRelease' in path.name or '_source_Sources' in path.name):
            shutil.copy2(path, evidence / path.name)
            index_files[path.name] = sha(path)
    if not index_files or not any('_source_Sources' in name for name in index_files):
        raise ValueError('authenticated Ubuntu source indexes unavailable')
    for package in sorted(packages):
        identity = command(['dpkg-query', '-W', '-f=${source:Package}\t${source:Version}\t${Version}', package]).strip().split('\t')
        source, version, binary_version = identity
        entries = [entry for entry in paragraphs(command(['apt-cache', 'showsrc', source]))
                   if entry.get('Package') == source and entry.get('Version') == version]
        unique = {entry.get('Checksums-Sha256'): entry for entry in entries}
        if len(unique) != 1:
            raise ValueError(f'{package}: exact signed source record unavailable: {source}={version}')
        entry = next(iter(unique.values()))
        directory = entry.get('Directory', '')
        if not directory.startswith('pool/') or '..' in Path(directory).parts:
            raise ValueError(f'{package}: unsafe source pool path')
        files = []
        for line in entry.get('Checksums-Sha256', '').splitlines():
            if not line.strip():
                continue
            digest, size, filename = line.split()
            if not re.fullmatch(r'[0-9a-f]{64}', digest) or Path(filename).name != filename:
                raise ValueError(f'{package}: invalid signed source checksum')
            files.append({'archive': filename, 'sha256': digest, 'size': int(size),
                          'url': f'https://snapshot.ubuntu.com/ubuntu/{receipt["ubuntu_base_image"]["apt_snapshot"]}/{directory}/{filename}'})
        if not files or not any(item['archive'].endswith('.dsc') for item in files):
            raise ValueError(f'{package}: source record lacks descriptor')
        notice = Path('/usr/share/doc') / package / 'copyright'
        if not notice.is_file():
            raise ValueError(f'{package}: source notice unavailable')
        target = evidence / f'{package}.copyright'
        shutil.copy2(notice, target)
        sections = [row for row in paragraphs(notice.read_text()) if 'License' in row]
        root_labels = sorted({row['License'].split('\n', 1)[0] for row in sections if row.get('Files') == '*'})
        static_package = package in ('glslang-dev', 'spirv-tools')
        def upstream_code(row):
            patterns = row.get('Files', '').split()
            return any(pattern == '*' or
                       (not pattern.startswith(('debian/', 'builds/', 'tests/', 'test/', 'examples/', 'doc/')) and
                        (pattern.startswith(('src/', 'lib', 'include/')) or
                         re.search(r'\.(?:c|cc|cpp|h|hpp|inc|rs)(?:$|\*)', pattern)))
                       for pattern in patterns)
        applicable = sorted({row['License'].split('\n', 1)[0] for row in sections if upstream_code(row)})
        copyleft = any(re.search(r"(?:L?GPL|MPL)", label, re.IGNORECASE) for label in applicable)
        copied_gnu_dso = package in ('libstdc++6', 'libgcc-s1')
        exception = package in ('libgcc-13-dev', 'libc6-dev') and package not in {
            item['owner'].split(':', 1)[1].split('=', 1)[0]
            for item in receipt['component_sources'].values() if item['owner'].startswith('ubuntu:')}
        records.append({'binary_package': package, 'binary_version': binary_version,
                        'source_package': source, 'source_version': version,
                        'source_required': copied_gnu_dso or (copyleft and not exception),
                        'source_requirement_basis': 'applicable upstream copyright sections; copied DSOs never exempted by CRT exception',
                        'files': files, 'notice': {'path': target.name, 'sha256': sha(target)},
                        'license_fields': sorted({row['License'] for row in paragraphs(notice.read_text()) if 'License' in row})})
        records[-1]['license_sections'] = sections
        records[-1]['applicable_license_labels'] = ' AND '.join(applicable)
        records[-1]['terms_scope'] = 'candidate upstream code terms; original full file-scope notice retained, exact DSO selection unresolved'
        known_labels = {'mit', 'expat', 'isc', 'bsd-2-clause', 'bsd-3-clause', 'apache-2.0',
                        'ftl', 'zlib', 'lgpl-2.1+', 'lgpl-3+', 'gpl-2+', 'gpl-3+', 'mpl-2.0'}
        # Uniform documented upstream terms can be selected from original
        # notice. Mixed scoped terms need actual mapping, not a label shortcut.
        records[-1]['terms_selection_resolved'] = (
            len(applicable) == 1 and applicable == root_labels and
            applicable[0].lower() in known_labels)
    dump(output / 'ubuntu-sources.json', {'schema': 1, 'authenticated_by': 'apt snapshot Signed-By ubuntu-archive-keyring.gpg',
                                         'indexes_sha256': index_files, 'packages': records})


def obtain(entry, cache, acquire):
    if Path(entry['archive']).name != entry['archive'] or not entry['url'].startswith('https://'):
        raise ValueError('unsafe source archive URL/name')
    path = cache / entry['archive']
    if path.is_symlink():
        raise ValueError(f'input symlink rejected: {path}')
    if path.exists():
        if sha(path) != entry['sha256']:
            raise ValueError(f'input checksum mismatch: {path}')
        return path
    if not acquire:
        return None
    cache.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=cache, prefix=path.name + '.', delete=False) as fresh:
        temporary = Path(fresh.name)
    try:
        with urllib.request.urlopen(entry['url'], timeout=120) as response, temporary.open('wb') as stream:
            shutil.copyfileobj(response, stream, 1024 * 1024)
        if sha(temporary) != entry['sha256']:
            raise ValueError(f'download checksum mismatch: {entry["url"]}')
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)
    return path


def qt_correspondence(archive, sbom):
    """Reconcile source/code/license bytes, not require bit-identical Qt rebuild."""
    text = sbom.read_text()
    records = {}
    for block in text.split("\n\n"):
        filename = re.search(r"^FileName: (.+)$", block, re.MULTILINE)
        checksum = re.search(r"^FileChecksum: SHA1: ([0-9a-f]{40})$", block, re.MULTILINE)
        if not filename or not checksum:
            continue
        relative = filename.group(1).removeprefix("./")
        if relative.startswith(("src/", "LICENSES/", "cmake/", "mkspecs/")) or relative in ("CMakeLists.txt", ".cmake.conf", "configure", "configure.bat"):
            records[relative] = checksum.group(1)
    if not records:
        raise ValueError(f"source SPDX has no build/source/license checksums: {sbom.name}")
    checked = {}
    differences = []
    with tarfile.open(archive) as tar:
        for member in tar:
            parts = Path(member.name).parts
            relative = "/".join(parts[1:])
            if relative not in records:
                continue
            if not member.isfile():
                differences.append(relative + ": not regular source")
                continue
            content = tar.extractfile(member).read()
            digest = hashlib.sha1(content).hexdigest()
            checked[relative] = digest
            if digest != records[relative]:
                differences.append(relative + ": differs from vendor source SPDX")
    differences.extend(relative + ": absent from checked source archive" for relative in records.keys() - checked.keys())
    return {"source_archive_sha256": sha(archive), "vendor_source_SPDX_sha256": sha(sbom),
            "checked_files": len(checked), "differences": sorted(differences)}


def assemble(provenance, audit, output, caches, acquire):
    output.mkdir()
    artifacts = []
    missing = []
    optional_unavailable = []
    qt_receipts = {}
    inputs = {}
    app = Path(provenance['app'])
    runtime = Path(provenance['runtime'])
    stage = Path(provenance['stage'])
    media = Path(provenance['media'])
    app_manifest = json.loads((app / 'manifest.json').read_text())
    media_manifest = json.loads((media / 'manifest.json').read_text())
    runtime_manifest = json.loads((runtime / 'manifest.json').read_text())

    def add(path, target, expected=None, role='corresponding-source', component=None):
        if Path(target).is_absolute() or '..' in Path(target).parts:
            raise ValueError(f'unsafe recipient path: {target}')
        path = Path(path)
        if path.is_symlink() or not path.is_file():
            missing.append(f'missing regular input: {target}')
            return
        digest = sha(path)
        if expected and expected != digest:
            raise ValueError(f'corresponding source hash mismatch: {path}')
        if target in inputs:
            if sha(inputs[target]) != digest:
                raise ValueError(f'conflicting source input: {target}')
            return
        inputs[target] = path
        artifacts.append({'path': target, 'sha256': digest, 'size': path.stat().st_size,
                          'role': role, 'component_id': component})

    def locked(entry, cache, target, component=None, role='corresponding-source', required=True):
        problems = missing if required else optional_unavailable
        try:
            path = obtain(entry, cache, acquire and required)
        except (OSError, ValueError) as error:
            problems.append(f'{component or target}: {error}')
            return None
        if path is None:
            problems.append(f'missing checked input: {target} ({entry["sha256"]})')
        else:
            add(path, target, entry['sha256'], role, component)
        return path

    for relative, digest in app_manifest['inputs'].items():
        add(app / 'source-snapshot' / relative, 'furami/' + relative, digest, component='furami')
    # Full controlling scripts and locks, not developer-specific output paths.
    for folder in ('appimage', 'media', 'appimage-runtime'):
        for path in sorted((REPO / 'packaging' / folder).iterdir()):
            if path.is_file() and not path.is_symlink() and (path.name == 'AppRun' or path.suffix in ('.py', '.sh', '.json', '.md', '.conf', '.svg', '.png', '.desktop')):
                add(path, 'furami/packaging/' + folder + '/' + path.name, role='build-recipe')
    for path in sorted((app / 'first/vendor').rglob('*')):
        if path.is_file():
            add(path, 'furami/vendor/' + path.relative_to(app / 'first/vendor').as_posix(), component='cargo-vendor')
    for entry in app_manifest['source_archives']:
        add(app / 'sources' / entry['archive'], 'app-inputs/' + entry['archive'], entry['sha256'], 'build-input')
    for name, entry in media_manifest['sources'].items():
        if name in ('qt', 'ca_certificates'):
            continue
        locked(entry, caches['media'], 'media-inputs/' + entry['archive'], name)
    for entry in media_manifest['sources']['qt']['archives']:
        locked(entry, caches['media'], 'media-inputs/' + entry['archive'], 'qt-kit:' + entry['component'], 'build-input', required=False)
    qt_lock = json.loads((HERE / 'sources.lock.json').read_text())
    required_qt = {item.get('source_component_id', item['component_id']).removeprefix('qt:')
                   for relative, owner in audit['component_files'].items()
                   if not relative.startswith('usr/share/')
                   for item in owner.get('components', []) if item.get('source_component_id', item['component_id']).startswith('qt:')}
    for module, entry in qt_lock['qt_sources'].items():
        source = locked(entry, caches['compliance'], 'qt-inputs/' + entry['archive'], 'qt:' + module,
                        required=module in required_qt)
        if source and module in required_qt:
            proof = qt_correspondence(source, media / 'prefix/sbom' / f'{module}-6.11.2.source.spdx')
            qt_receipts[module] = proof
            missing.extend(f'Qt {module}: {path}' for path in proof['differences'])
    for path in sorted((media / 'prefix/sbom').iterdir()):
        add(path, 'qt-evidence/sbom/' + path.name, media_manifest['artifacts'][path.relative_to(media / 'prefix').as_posix()]['sha256'], 'license-evidence')
    for path in sorted((media / 'prefix').glob('config_*')):
        if path.is_file():
            add(path, 'qt-evidence/' + path.name, role='build-configuration')
    for path in sorted(media.iterdir()):
        if path.is_file() and path.suffix in ('.json', '.mak', '.h', '.txt', '.tsv'):
            add(path, 'media-evidence/' + path.name, role='build-configuration')
    ubuntu = json.loads((stage / 'ubuntu-sources.json').read_text())
    for package in ubuntu['packages']:
        for entry in package['files']:
            locked(entry, caches['compliance'] / 'ubuntu', 'ubuntu-inputs/' + entry['archive'],
                   'ubuntu:' + package['source_package'], required=package['source_required'])
    for path in sorted((stage / 'ubuntu-source-evidence').iterdir()):
        add(path, 'ubuntu-evidence/' + path.name, role='license-evidence')
    add(stage / 'ubuntu-sources.json', 'ubuntu-evidence/ubuntu-sources.json', role='build-configuration')
    lock = json.loads((REPO / 'packaging/appimage-runtime/runtime.lock.json').read_text())
    for entry in lock['sources'].values():
        local = runtime / 'relink' / entry['archive']
        if local.is_file():
            add(local, 'runtime-inputs/' + entry['archive'], entry['sha256'], component='runtime')
        else:
            locked(entry, caches['runtime'], 'runtime-inputs/' + entry['archive'], 'runtime')
    alpine = json.loads((REPO / 'packaging/appimage-runtime/alpine-source.lock.json').read_text())
    for name, source in alpine['linked_sources'].items():
        entry = {**source['apkbuild'], 'archive': 'APKBUILD'}
        locked(entry, caches['runtime'] / 'alpine-sources' / name, 'runtime-inputs/alpine-sources/' + name + '/APKBUILD', 'runtime:' + name, required=False)
        for entry in [source['upstream_source'], *source['aports_inputs']]:
            locked({**entry, 'archive': entry['file']}, caches['runtime'] / 'alpine-sources' / name,
                   'runtime-inputs/alpine-sources/' + name + '/' + entry['file'], 'runtime:' + name, required=False)
    for repo, entry in lock['apk_indexes'].items():
        locked({**entry, 'archive': 'APKINDEX.tar.gz'}, caches['runtime'] / 'indexes' / repo / 'x86_64',
               'runtime-inputs/indexes/' + repo + '/x86_64/APKINDEX.tar.gz', 'runtime-toolchain', 'build-input')
    for entry in lock['apk_packages'].values():
        locked({**entry, 'url': f'{lock["repository_base_url"]}/{entry["repository"]}/x86_64/{entry["archive"]}'},
               caches['runtime'] / 'apks', 'runtime-inputs/apks/' + entry['archive'], 'runtime-toolchain', 'build-input')
    for path in sorted((runtime / 'licenses').iterdir()):
        add(path, 'runtime-evidence/licenses/' + path.name, role='license-evidence')
    for name in ('manifest.json', 'link.map', 'apk-installed.txt'):
        add(runtime / name, 'runtime-evidence/' + name, role='build-configuration')
    # Archive available bytes even when incomplete, plainly marked material set.
    # Each part is independently extractable; no cache paths needed by recipients.
    epoch = json.loads((REPO / 'packaging/media/stack.lock.json').read_text())['source_date_epoch']
    part = 1
    size = 0
    archive = None
    archives = []
    try:
        for target, path in sorted(inputs.items()):
            if archive is None or size + path.stat().st_size > 512 * 1024 * 1024:
                if archive:
                    archive.close()
                name = f'source-materials-{part:03}.tar'
                archive = tarfile.open(output / name, 'w', format=tarfile.PAX_FORMAT)
                archives.append(name)
                part += 1
                size = 0
            info = archive.gettarinfo(str(path), arcname=target)
            info.uid = info.gid = 0
            info.uname = info.gname = ''
            info.mtime = epoch
            info.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
            with path.open('rb') as stream:
                archive.addfile(info, stream)
            size += path.stat().st_size
    finally:
        if archive:
            archive.close()
    index = {'schema': 1, 'complete': not missing, 'missing': sorted(set(missing)), 'files': artifacts,
             'optional_unavailable': sorted(set(optional_unavailable)), 'qt_correspondence': qt_receipts,
             'archives': [{'path': name, 'sha256': sha(output / name)} for name in archives],
             'scope': 'checked available material set; corresponding source complete only if release-ready.json ready=true'}
    dump(output / 'sources-index.json', index)
    return index


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--capture-ubuntu', action='store_true')
    parser.add_argument('--stage', type=Path, required=True)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        if not args.capture_ubuntu:
            parser.error('recipient assembly runs through build.py')
        capture_ubuntu(args.stage, args.app, args.output)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
