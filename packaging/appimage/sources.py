#!/usr/bin/env python3
"""Capture signed Ubuntu source metadata and assemble checked recipient inputs."""
import argparse
import bz2
from contextlib import ExitStack
import gzip
import hashlib
import json
import os
import importlib.util
import lzma
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.parse
import urllib.request
import tempfile
import tomllib

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
APT_LISTS = Path('/var/lib/apt/lists')
UBUNTU_DOC = Path('/usr/share/doc')
CARGO_NOTICE_LOCK = 'packaging/appimage/sources.lock.json'
CARGO_NOTICE_DIR = 'packaging/appimage/notices/'
CARGO_REGISTRY = 'registry+https://github.com/rust-lang/crates.io-index'
HEX_SHA = re.compile(r'[0-9a-f]{64}\Z')

PUBLIC_MATERIAL_POLICY = {'schema': 1, 'alpine_bootstrap': 'private',
                          'public_offline_bootstrap_complete': False}
PATCH_SPEC = importlib.util.spec_from_file_location("furami_sources_media_patches", REPO / "packaging/media/source-patches.py")
source_patches = importlib.util.module_from_spec(PATCH_SPEC)
PATCH_SPEC.loader.exec_module(source_patches)


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def dump(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def public_runtime_manifest_bytes(manifest):
    """Serialize runtime provenance without the producer's private cache path."""
    if 'linked_source_closure' in manifest:
        manifest = {**manifest, 'linked_source_closure': {
            key: value for key, value in manifest['linked_source_closure'].items()
            if key != 'local_cache_path'}}
    return (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode('utf-8')


def notice_relative(path):
    return (isinstance(path, str) and path and '\\' not in path and '\0' not in path
            and all(part not in ('', '.', '..') for part in path.split('/')))


def notice_file(root, relative):
    if not notice_relative(relative):
        raise ValueError(f'unsafe notice path: {relative!r}')
    path = root / relative
    if any(parent.is_symlink() for parent in (path, *path.parents)) or not path.is_file():
        raise ValueError(f'notice input must be a regular nonsymlink file: {relative}')
    return path


def crate_identity(row):
    return tuple(row.get(key) for key in ('name', 'version', 'source', 'checksum'))


def registry_crate(identity):
    name, version, source, checksum = identity
    return (isinstance(name, str) and re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_-]*', name)
            and isinstance(version, str) and re.fullmatch(r'[0-9][A-Za-z0-9.+-]*', version)
            and source == CARGO_REGISTRY and isinstance(checksum, str) and HEX_SHA.fullmatch(checksum))


def _notice_origin(origin):
    if not isinstance(origin, dict) or not notice_relative(origin.get('path')):
        return False
    kind = origin.get('kind')
    if kind == 'upstream':
        if (origin.keys() != {'kind', 'path', 'revision', 'repository'}
                or not isinstance(origin['revision'], str)
                or not re.fullmatch(r'[0-9a-f]{40}', origin['revision'])):
            return False
        url = origin['repository']
    elif kind == 'distribution':
        if (origin.keys() != {'kind', 'path', 'url', 'archive_sha256', 'descriptor_sha256'}
                or any(not isinstance(origin[key], str) or not HEX_SHA.fullmatch(origin[key])
                       for key in ('archive_sha256', 'descriptor_sha256'))):
            return False
        url = origin['url']
    else:
        return False
    if not isinstance(url, str) or any(character.isspace() for character in url):
        return False
    parsed = urllib.parse.urlsplit(url)
    return (parsed.scheme == 'https' and parsed.hostname and parsed.path
            and parsed.username is None and not parsed.query and not parsed.fragment)


def cargo_notice_lock(root):
    """Read the reviewed crate identities and exact source-snapshot input hashes."""
    cargo = notice_file(root, 'Cargo.lock').read_bytes()
    policy = notice_file(root, CARGO_NOTICE_LOCK).read_bytes()
    locked = set()
    for row in tomllib.loads(cargo.decode()).get('package', []):
        identity = crate_identity(row)
        if row.get('source') is not None:
            if not registry_crate(identity) or identity in locked:
                raise ValueError(f'invalid Cargo.lock registry identity: {identity}')
            locked.add(identity)
    rows = json.loads(policy).get('cargo_notices')
    if not isinstance(rows, list):
        raise ValueError('reviewed Cargo notice list missing from sources.lock.json')
    inputs = {'Cargo.lock': hashlib.sha256(cargo).hexdigest(),
              CARGO_NOTICE_LOCK: hashlib.sha256(policy).hexdigest()}
    supplemental = {}
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError('invalid supplemental Cargo notice row')
        identity = crate_identity(row)
        if not registry_crate(identity) or identity not in locked or identity in supplemental:
            raise ValueError(f'Cargo.lock does not uniquely match supplemental notice: {identity}')
        unresolved = row.get('unresolved')
        notices = row.get('notices')
        if (row.keys() - {'name', 'version', 'source', 'checksum', 'notices', 'unresolved'}
                or ('unresolved' in row and (not isinstance(unresolved, str) or not unresolved.strip()))
                or not isinstance(notices, list) or (not notices and not unresolved)):
            raise ValueError(f'invalid supplemental Cargo notice row: {identity}')
        names = set()
        for notice in notices:
            if not isinstance(notice, dict):
                raise ValueError(f'invalid supplemental Cargo notice: {identity}')
            path, expected = notice.get('file'), notice.get('sha256')
            if (notice.keys() - {'file', 'sha256', 'origin', 'crate_member'}
                    or not notice_relative(path) or not path.startswith(CARGO_NOTICE_DIR)
                    or not path[len(CARGO_NOTICE_DIR):]
                    or not isinstance(expected, str) or not HEX_SHA.fullmatch(expected)
                    or not _notice_origin(notice.get('origin'))
                    or ('crate_member' in notice and not notice_relative(notice['crate_member']))):
                raise ValueError(f'invalid supplemental Cargo notice: {identity}: {path!r}')
            name = Path(path).name
            if name in names or inputs.setdefault(path, expected) != expected:
                raise ValueError(f'supplemental Cargo notice collision: {identity}: {path}')
            names.add(name)
        supplemental[identity] = row
    return locked, supplemental, inputs


def cargo_notice_input_files(root, inputs):
    for relative, expected in inputs.items():
        path = notice_file(root, relative)
        if sha(path) != expected:
            raise ValueError(f'Cargo notice input SHA256 mismatch: {relative}')


def cargo_notice_snapshot(app_root, app_manifest, repository):
    locked, supplemental, expected = cargo_notice_lock(repository)
    recorded = app_manifest.get('inputs', {})
    if (app_manifest.get('cargo_lock_sha256') != expected['Cargo.lock']
            or any(recorded.get(path) != digest for path, digest in expected.items())
            or any(path.startswith(CARGO_NOTICE_DIR) and path not in expected for path in recorded)):
        raise ValueError('application notice snapshot inputs differ from reviewed locks')
    cargo_notice_input_files(app_root / 'source-snapshot', expected)
    return locked, supplemental, expected


def verify_public_cargo_notices(inventory, repository):
    """Use reviewed identities, not the recipient index, to require these bytes."""
    _, supplemental, inputs = cargo_notice_lock(repository)
    required = {'furami/' + path: digest for path, digest in inputs.items()}
    required.update({f'app-inputs/{name}-{version}.crate': checksum
                     for name, version, _, checksum in supplemental})
    for path, expected in required.items():
        if inventory.get(path, {}).get('sha256') != expected:
            raise ValueError(f'public Cargo notice input absent or changed: {path}')
    unexpected = sorted(path for path in inventory
                        if path.startswith('furami/' + CARGO_NOTICE_DIR) and path not in required)
    if unexpected:
        raise ValueError(f'unreviewed public Cargo notice inputs: {unexpected}')


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


def load_ubuntu_terms(path: Path) -> dict:
    lock = json.loads(path.read_text())
    if lock.get('schema') != 1 or not isinstance(lock.get('packages'), dict) or not lock['packages']:
        raise ValueError('invalid Ubuntu terms lock')
    texts = lock.get('custom_license_texts', {})
    for identity, entry in lock['packages'].items():
        expression = entry.get('license_expression')
        objects = entry.get('shipped_objects')
        if (not isinstance(expression, str) or not expression or
                entry.get('source_required') is not True or
                not entry.get('source_mapping_basis') or
                not isinstance(objects, list) or objects != sorted(set(objects)) or
                any(not isinstance(obj, str) or Path(obj).is_absolute() or '..' in Path(obj).parts for obj in objects) or
                not re.fullmatch(r'[0-9a-f]{64}', entry.get('notice', {}).get('sha256', '')) or
                entry.get('notice', {}).get('path') != identity.split('@', 1)[0] + '.copyright' or
                not isinstance(entry.get('source_package'), dict) or
                not all(isinstance(entry['source_package'].get(key), str) and entry['source_package'][key]
                        for key in ('name', 'version')) or
                not isinstance(entry.get('license_exception'), (str, type(None)))):
            raise ValueError(f'invalid Ubuntu terms entry: {identity}')
        for name in re.findall(r'LicenseRef-[A-Za-z0-9.-]+', expression):
            if not isinstance(texts.get(name), str) or not texts[name].strip():
                raise ValueError(f'Ubuntu terms lack custom text: {name}')
    return lock


def _require_ubuntu_package_set(lock, observed):
    if set(observed) != set(lock['packages']) or len(observed) != len(lock['packages']):
        raise ValueError('Ubuntu terms package set differs from observed stage')


def resolve_ubuntu_terms(lock: dict, *, binary_package: str, binary_version: str,
                         source_package: str, source_version: str, notice: Path,
                         shipped_objects: list[str]) -> dict:
    identity = binary_package + '@' + binary_version
    entry = lock['packages'].get(identity)
    if not entry:
        raise ValueError(f'Ubuntu terms identity absent: {identity}')
    if (entry['source_package'] != {'name': source_package, 'version': source_version} or
            notice.is_symlink() or not notice.is_file() or sha(notice) != entry['notice']['sha256'] or
            notice.name != entry['notice']['path'] or
            sorted(shipped_objects) != entry['shipped_objects']):
        raise ValueError(f'Ubuntu terms correspondence mismatch: {identity}')
    refs = re.findall(r'LicenseRef-[A-Za-z0-9.-]+', entry['license_expression'])
    if entry.get('source_required') is not True:
        raise ValueError(f'Ubuntu source requirement missing: {identity}')
    if any(not lock.get('custom_license_texts', {}).get(name) for name in refs):
        raise ValueError(f'Ubuntu terms custom text absent: {identity}')
    selected_texts = {**lock['custom_license_texts'], **entry.get('custom_license_texts', {})}
    return {**entry, 'custom_license_texts': {name: selected_texts[name] for name in refs},
            'custom_license_evidence': {name: lock.get('custom_license_evidence', {}).get(name) for name in refs},
            'expression_basis': entry['source_mapping_basis'],
            'source_requirement_basis': 'all pinned Ubuntu packages require corresponding sources, including permissive and CRT inputs',
            'terms_selection_resolved': True}


def _collect_ubuntu(appdir, owners, app_manifest, apt_snapshot, evidence):
    """Read the frozen dpkg/APT state, independent of release terms selection."""
    package_objects = {}
    for relative, item in owners.items():
        if not item['owner'].startswith('ubuntu:'):
            continue
        package = item['owner'].split(':', 1)[1].split('=', 1)[0]
        package_objects.setdefault(package, []).append((relative, item))
    packages = set(package_objects)
    packages.update(('glslang-dev', 'spirv-tools'))
    packages.update(item['binary_package']['name'] for item in app_manifest['binary']['direct_objects'])
    # Source identities and checksums come from authenticated apt source lists,
    # not from an unverified descriptor URL or guessed binary package name.
    records = []
    evidence.mkdir(exist_ok=True)
    index_files = {}
    for path in sorted(APT_LISTS.iterdir()):
        if path.is_file() and ('_InRelease' in path.name or '_source_Sources' in path.name):
            shutil.copy2(path, evidence / path.name)
            index_files[path.name] = sha(path)
    if not any('_InRelease' in name for name in index_files) or not any(
            '_source_Sources' in name for name in index_files):
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
                          'url': f'https://snapshot.ubuntu.com/ubuntu/{apt_snapshot}/{directory}/{filename}'})
        if not files or not any(item['archive'].endswith('.dsc') for item in files):
            raise ValueError(f'{package}: source record lacks descriptor')
        notice = UBUNTU_DOC / package / 'copyright'
        if not notice.is_file():
            raise ValueError(f'{package}: source notice unavailable')
        target = evidence / f'{package}.copyright'
        shutil.copy2(notice, target)
        sections = [row for row in paragraphs(target.read_text()) if 'License' in row]
        objects = {}
        for relative, item in sorted(package_objects.get(package, ())):
            if item['owner'] != 'ubuntu:' + package + '=' + binary_version:
                raise ValueError(f'Ubuntu staged owner version mismatch: {package}')
            staged = appdir / relative
            if staged.is_symlink() or not staged.is_file():
                continue
            with staged.open('rb') as stream:
                if stream.read(4) == b'\x7fELF':
                    objects[relative] = item
        records.append({'binary_package': package, 'binary_version': binary_version,
                        'source_package': source, 'source_version': version,
                        'shipped_objects': list(objects), 'object_owners': objects,
                        'notice': {'path': target.name, 'sha256': sha(target)},
                        'files': files, 'license_sections': sections,
                        'license_fields': sorted({row['License'] for row in sections})})
    return {'authenticated_by': 'apt snapshot Signed-By ubuntu-archive-keyring.gpg',
            'indexes_sha256': index_files, 'packages': records}


def observe_ubuntu(appdir, owners, app_manifest, apt_snapshot, output):
    """Write facts for manual terms review; never create a release capture."""
    observation = _collect_ubuntu(appdir, owners, app_manifest, apt_snapshot,
                                  output / 'ubuntu-observation-evidence')
    dump(output / 'ubuntu-observation.json', {'schema': 1, 'release_eligible': False,
                                             'apt_snapshot': apt_snapshot, **observation})


def capture_ubuntu(stage, app, output):
    """Run only in staging container after authenticated snapshot apt update."""
    receipt = json.loads((stage / 'evidence.json').read_text())
    app_manifest = json.loads((app / 'manifest.json').read_text())
    terms_path = HERE / 'ubuntu-terms.lock.json'
    terms_lock = load_ubuntu_terms(terms_path)
    evidence = output / 'ubuntu-source-evidence'
    captured = _collect_ubuntu(stage / 'AppDir', receipt['component_sources'], app_manifest,
                               receipt['ubuntu_base_image']['apt_snapshot'], evidence)
    observed = set()
    for record in captured['packages']:
        package, binary_version = record['binary_package'], record['binary_version']
        terms = resolve_ubuntu_terms(terms_lock, binary_package=package, binary_version=binary_version,
                                     source_package=record['source_package'],
                                     source_version=record['source_version'],
                                     notice=evidence / record['notice']['path'],
                                     shipped_objects=record['shipped_objects'])
        observed.add(package + '@' + binary_version)
        del record['object_owners']
        record.update((key, value) for key, value in terms.items() if key != 'source_package')
    _require_ubuntu_package_set(terms_lock, observed)
    dump(output / 'ubuntu-sources.json', {'schema': 1, 'terms_lock_sha256': sha(terms_path), **captured})


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


LOADER_REPORTS = {'mpv.ldd.txt': 'mpv.loader-resolution.txt',
                  'libmpv.so.ldd.txt': 'libmpv.so.loader-resolution.txt'}


def loader_resolution_report(raw):
    """Keep dependency facts in order, removing only terminal process addresses."""
    if not raw or b'\r' in raw or b'\x00' in raw:
        raise ValueError('invalid recorded ldd output')
    result = []
    for line in raw.splitlines():
        if not re.fullmatch(
                rb'\s*(?:[^\s]+ => /[^\s]+|linux-vdso\.so\.[0-9]+|/[^\s]+) \(0x[0-9a-f]+\)', line):
            raise ValueError('unrecognized or unresolved recorded ldd line')
        result.append(re.sub(rb' \(0x[0-9a-f]+\)$', b'', line))
    return (b'Derived loader-resolution report; terminal per-process addresses omitted.\n'
            b'Transform: glibc-terminal-address-v1. Recorded build environment only.\n'
            + b'\n'.join(result) + b'\n')


def verify_tar_footer(archive, name):
    """A tar's end marker may be padded with zeros, never another payload."""
    # TarFile.next() may consume a nonzero invalid header before stopping.
    # Re-read from the member boundary, not from its already-advanced cursor.
    archive.fileobj.seek(archive.offset)
    padding = 0
    while block := archive.fileobj.read(1024 * 1024):
        if any(block):
            raise ValueError('nonzero trailing/concatenated archive bytes: ' + name)
        padding += len(block)
    if padding < 1024 or padding % 512:
        raise ValueError('invalid or missing tar zero footer: ' + name)


def source_inventory(release):
    """Hash actual regular tar members; never extract recipient-controlled paths."""
    result = {}
    for archive in sorted(release.glob('source-materials-*.tar')):
        if archive.is_symlink() or not archive.is_file():
            raise ValueError('source archive must be a regular file')
        with tarfile.open(archive, mode='r:') as tar:
            for member in tar:
                name = member.name
                if (not member.isfile() or name in result or not name
                        or Path(name).is_absolute() or '..' in Path(name).parts
                        or Path(name).as_posix() != name or '\\' in name
                        or any(ord(char) < 32 or ord(char) == 127 for char in name)):
                    raise ValueError('unexpected source archive entry: ' + name)
                with tar.extractfile(member) as stream:
                    checksum = hashlib.file_digest(stream, 'sha256').hexdigest()
                result[name] = {'sha256': checksum, 'size': member.size, 'mode': member.mode,
                                'mtime': member.mtime, 'uid': member.uid, 'gid': member.gid,
                                'uname': member.uname, 'gname': member.gname, 'pax': member.pax_headers,
                                'archive': archive.name, 'data_offset': member.offset_data}
            verify_tar_footer(tar, archive.name)
    return result


def verify_compressed_tail(stream, signature, name):
    """bz2/lzma file wrappers otherwise silently ignore non-format trailers."""
    factory = bz2.BZ2Decompressor if signature.startswith(b'BZh') else lzma.LZMADecompressor
    decoder = factory()
    pending = b''
    padding = 0
    try:
        while True:
            chunk = pending or stream.read(1024 * 1024)
            pending = b''
            if not chunk:
                if decoder is not None and not decoder.eof:
                    raise ValueError('truncated compressed archive: ' + name)
                if padding % 4:
                    raise ValueError('invalid XZ stream padding: ' + name)
                return
            if decoder is None:
                if signature == b'\xfd7zXZ\x00':
                    stripped = chunk.lstrip(b'\0')
                    padding += len(chunk) - len(stripped)
                    chunk = stripped
                    if not chunk:
                        continue
                    if padding % 4:
                        raise ValueError('invalid XZ stream padding: ' + name)
                decoder = factory()
                padding = 0
            decoder.decompress(chunk, max_length=1024 * 1024)
            while not decoder.eof and not decoder.needs_input:
                decoder.decompress(b'', max_length=1024 * 1024)
            if decoder.eof:
                pending = decoder.unused_data
                decoder = None
    except (EOFError, OSError, lzma.LZMAError) as error:
        raise ValueError('nonzero trailing/concatenated compressed archive bytes: ' + name) from error
    finally:
        stream.seek(0)


def reject_private_archive(stream, name):
    """Recognize renamed private APK, index and image archives."""
    signature = stream.read(6)
    stream.seek(0)
    if signature.startswith(b'BZh') or signature == b'\xfd7zXZ\x00':
        verify_compressed_tail(stream, signature, name)
    with ExitStack() as context:
        decoded = stream
        if signature.startswith(b'\x1f\x8b'):
            decoded = context.enter_context(gzip.GzipFile(fileobj=stream))
        elif signature.startswith(b'BZh'):
            decoded = context.enter_context(bz2.BZ2File(stream))
        elif signature == b'\xfd7zXZ\x00':
            decoded = context.enter_context(lzma.LZMAFile(stream))
        try:
            nested = tarfile.open(fileobj=decoded, mode='r:')
        except tarfile.ReadError:
            return  # Not a tar container; ordinary source/text/binary input.
        with nested:
            for member in nested:
                relative = member.name.removeprefix('./')
                if (relative in ('oci-layout', 'manifest.json', 'repositories', '.PKGINFO', 'APKINDEX',
                                 'lib/apk/db/installed')
                        or relative.startswith(('blobs/sha256/', '.SIGN.'))):
                    raise ValueError('private Alpine bootstrap archive in public material: ' + name)
            verify_tar_footer(nested, name)


def verify_public_materials(release, index, *, require_complete=True):
    """Verify public bytes offline, independently of private bootstrap retention."""
    policy = index.get('material_policy')
    if (not isinstance(policy, dict) or policy != PUBLIC_MATERIAL_POLICY
            or type(policy.get('schema')) is not int
            or policy.get('public_offline_bootstrap_complete') is not False):
        raise ValueError('public material policy missing or malformed')
    if require_complete and (index.get('complete') is not True or index.get('missing') != []):
        raise ValueError('public corresponding source set incomplete')
    inventory = source_inventory(release)
    files = index.get('files')
    if not isinstance(files, list) or not files:
        raise ValueError('public source inventory missing')
    expected = {}
    for row in files:
        if (not isinstance(row, dict) or not isinstance(row.get('path'), str)
                or row['path'] in expected or type(row.get('size')) is not int
                or not isinstance(row.get('sha256'), str)
                or not re.fullmatch(r'[0-9a-f]{64}', row['sha256'])):
            raise ValueError('invalid public source inventory')
        expected[row['path']] = {'sha256': row['sha256'], 'size': row['size']}
    if expected != {name: {key: row[key] for key in ('sha256', 'size')}
                    for name, row in inventory.items()}:
        raise ValueError('actual public source inventory differs from index')
    archives = index.get('archives')
    actual_archives = {path.name: sha(path) for path in release.glob('source-materials-*.tar')}
    if (not isinstance(archives, list) or not archives
            or any(not isinstance(row, dict) for row in archives)
            or len({row.get('path') for row in archives}) != len(archives)
            or {row.get('path'): row.get('sha256') for row in archives} != actual_archives):
        raise ValueError('actual public source archive hashes differ from index')

    def read_lock(name, folder='appimage-runtime'):
        relative = 'furami/packaging/' + folder + '/' + name
        row = inventory.get(relative)
        if row is None:
            raise ValueError('public runtime lock missing: ' + name)
        canonical = REPO / 'packaging' / folder / name
        if canonical.is_symlink() or row['sha256'] != sha(canonical):
            raise ValueError('public runtime lock differs from reviewed canonical recipe: ' + name)
        with tarfile.open(release / row['archive']) as archive:
            with archive.extractfile(relative) as stream:
                value = json.load(stream)
        if not isinstance(value, dict):
            raise ValueError('invalid public runtime lock: ' + name)
        return value

    runtime = read_lock('runtime.lock.json')
    alpine = read_lock('alpine-source.lock.json')
    media_lock = read_lock('stack.lock.json', 'media')
    media_patches = source_patches.entries(media_lock['sources'])
    runtime_recipe_path = REPO / 'packaging/appimage-runtime/build.py'
    spec = importlib.util.spec_from_file_location('furami_public_runtime_validation', runtime_recipe_path)
    runtime_recipe = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runtime_recipe)
    if type(runtime.get('schema')) is not int or type(alpine.get('schema')) is not int or alpine['schema'] != 1:
        raise ValueError('invalid public runtime/source lock schema')
    runtime_recipe.verify_lock(runtime)
    for name in runtime_recipe.RELINK_RECIPE_FILES:
        canonical = runtime_recipe_path.with_name(name)
        relative = 'furami/packaging/appimage-runtime/' + name
        if (canonical.is_symlink() or not canonical.is_file()
                or inventory.get(relative, {}).get('sha256') != sha(canonical)):
            raise ValueError('public runtime relink recipe differs from reviewed canonical recipe: ' + name)
    linked_sources = runtime_recipe.alpine_sources(runtime)
    if linked_sources != alpine.get('linked_sources'):
        raise ValueError('public linked source closure differs from reviewed canonical pairs')
    source_policy = read_lock('sources.lock.json', 'appimage')
    verify_public_cargo_notices(inventory, REPO)
    base = source_policy.get('alpine_private_bootstrap')
    if (type(source_policy.get('schema')) is not int or source_policy['schema'] != 1
            or not isinstance(base, dict) or base.get('image') != runtime['container']
            or base.get('manifest_sha256') != runtime['container'].split('@sha256:')[1]
            or not isinstance(base.get('layers_sha256'), list) or not base['layers_sha256']
            or any(not isinstance(value, str) or not re.fullmatch(r'[0-9a-f]{64}', value)
                   for value in [base.get('manifest_sha256'), base.get('config_sha256'), *base['layers_sha256']])):
        raise ValueError('invalid canonical private Alpine base identities')
    forbidden = {entry['sha256'] for entry in runtime['apk_packages'].values()}
    # The exact upstream base manifest is also private; OCI blob/layout paths
    # and nested image-archive signatures are rejected below.
    forbidden.update((base['manifest_sha256'], base['config_sha256'], *base['layers_sha256']))
    required = {'runtime-inputs/' + entry['archive']: entry['sha256']
                for entry in runtime['sources'].values()}
    for name, source in media_lock['sources'].items():
        if name != 'ca_certificates':
            required['media-inputs/' + source['archive']] = source['sha256']
    required.update({'furami/packaging/media/' + row['file']: row['sha256']
                     for row in media_patches})
    for name, source in linked_sources.items():
        prefix = 'runtime-inputs/alpine-sources/' + name + '/'
        required[prefix + 'APKBUILD'] = source['apkbuild']['sha256']
        for entry in (source['upstream_source'], *source['aports_inputs']):
            required[prefix + entry['file']] = entry['sha256']
    with tempfile.TemporaryDirectory(prefix='furami-offline-media-') as temporary:
        def delivered(name):
            row = inventory.get(name)
            if row is None:
                raise ValueError('media recipient material missing: ' + name)
            target = Path(temporary) / hashlib.sha256(name.encode()).hexdigest()
            with tarfile.open(release / row['archive']) as archive:
                with archive.extractfile(name) as stream, target.open('wb') as output:
                    shutil.copyfileobj(stream, output)
            return target
        if media_patches:
            manifest_name = 'media-evidence/manifest.json'
            receipt_name = 'media-evidence/source-patches.json'
            if require_complete or manifest_name in inventory or receipt_name in inventory:
                manifest = json.loads(delivered(manifest_name).read_bytes())
                applied = delivered(receipt_name)
                if (manifest.get('sources') != media_lock['sources']
                        or manifest.get('lock_sha256') != sha(REPO / 'packaging/media/stack.lock.json')
                        or manifest.get('source_patches') != {'path': 'source-patches.json', 'sha256': sha(applied)}
                        or json.loads(applied.read_bytes()) != source_patches.receipt(media_lock['sources'])):
                    raise ValueError('public applied media patches differ from frozen source lock')
            for row in media_patches:
                name = 'furami/packaging/media/' + row['file']
                if name in inventory and inventory[name]['sha256'] != row['sha256']:
                    raise ValueError('public media source patch differs from lock: ' + name)
        for raw_name, report_name in LOADER_REPORTS.items():
            if 'media-evidence/' + raw_name in inventory:
                raise ValueError('raw process-address transcript in public sources')
            if require_complete or 'media-evidence/' + report_name in inventory:
                report = delivered('media-evidence/' + report_name).read_bytes()
                lines = report.splitlines()
                recorded = b'\n'.join(line + b' (0x0)' for line in lines[2:]) + b'\n'
                if loader_resolution_report(recorded) != report:
                    raise ValueError('invalid derived loader-resolution report')
    if require_complete:
        for name, checksum in required.items():
            if inventory.get(name, {}).get('sha256') != checksum:
                raise ValueError('public linked corresponding source absent or changed: ' + name)
    for archive_path in sorted(release.glob('source-materials-*.tar')):
        with tarfile.open(archive_path) as archive:
            for member in archive:
                name, row = member.name, inventory[member.name]
                parts = Path(name).parts
                if (name.lower().endswith('.apk') or row['sha256'] in forbidden
                        or any(part in ('apks', 'blobs', 'oci-layout') for part in parts)
                        or name.startswith('runtime-inputs/base-image/')):
                    raise ValueError('private Alpine bootstrap material in public archive: ' + name)
                with archive.extractfile(member) as stream:
                    reject_private_archive(stream, name)
    for path in release.iterdir():
        if path.is_file() and not path.name.startswith('source-materials-'):
            if sha(path) in forbidden:
                raise ValueError('private Alpine bootstrap bytes in public asset: ' + path.name)
            with path.open('rb') as stream:
                reject_private_archive(stream, path.name)
    return inventory


def assemble(provenance, audit, output, caches, acquire):
    output.mkdir()
    artifacts = []
    missing = []
    optional_unavailable = []
    inputs = {}
    app = Path(provenance['app'])
    runtime = Path(provenance['runtime'])
    stage = Path(provenance['stage'])
    media = Path(provenance['media'])
    app_manifest = json.loads((app / 'manifest.json').read_text())
    _, _, notice_inputs = cargo_notice_snapshot(app, app_manifest, REPO)
    media_manifest = json.loads((media / 'manifest.json').read_text())
    runtime_manifest = json.loads((runtime / 'manifest.json').read_text())
    stack = json.loads((REPO / 'packaging/media/stack.lock.json').read_text())
    if media_manifest.get('sources') != stack['sources']:
        raise ValueError('media source versions differ from stack lock')
    source_patches.verify_provider(media, media_manifest, stack['sources'])

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

    patch_owners = {'packaging/media/' + row['file']: row['source']
                    for row in source_patches.entries(media_manifest['sources'])}
    for relative, digest in app_manifest['inputs'].items():
        if relative.startswith(CARGO_NOTICE_DIR):
            add(app / 'source-snapshot' / relative, 'furami/' + relative,
                notice_inputs[relative], role='license-evidence')
            continue
        add(app / 'source-snapshot' / relative, 'furami/' + relative, digest,
            component=patch_owners.get(relative, 'furami'))
    # Full controlling scripts and locks, not developer-specific output paths.
    for folder in ('appimage', 'media', 'appimage-runtime'):
        for path in sorted((REPO / 'packaging' / folder).iterdir()):
            if path.is_file() and not path.is_symlink() and (path.name == 'AppRun' or path.suffix in ('.py', '.sh', '.json', '.md', '.conf', '.svg', '.png', '.desktop')):
                add(path, 'furami/packaging/' + folder + '/' + path.name, role='build-recipe')
    for relative in notice_inputs:
        if relative != 'Cargo.lock':
            add(REPO / relative, 'furami/' + relative, notice_inputs[relative], role='license-evidence')
    for row in source_patches.entries(media_manifest['sources']):
        add(media / 'source-inputs' / row['file'], 'furami/packaging/media/' + row['file'],
            row['sha256'], component=row['source'])
    if provenance.get('original_recipes'):
        original = Path(provenance['original_recipes'])
        for path in sorted(original.rglob('*')):
            if path.is_file():
                add(path, 'original-cold-recipes/' + path.relative_to(original).as_posix(),
                    role='build-recipe')
    for path in sorted((app / 'first/vendor').rglob('*')):
        if path.is_file():
            add(path, 'furami/vendor/' + path.relative_to(app / 'first/vendor').as_posix(), component='cargo-vendor')
    for entry in app_manifest['source_archives']:
        add(app / 'sources' / entry['archive'], 'app-inputs/' + entry['archive'], entry['sha256'], 'build-input')
    for name, entry in media_manifest['sources'].items():
        if name == 'ca_certificates':
            continue
        locked(entry, caches['media'], 'media-inputs/' + entry['archive'], name)
    derived = tempfile.TemporaryDirectory(prefix='furami-loader-reports-')
    loader_receipts = []
    for raw_name, report_name in LOADER_REPORTS.items():
        raw = media / raw_name
        if raw.is_symlink() or not raw.is_file():
            raise ValueError('missing regular recorded loader evidence: ' + raw_name)
        report = Path(derived.name) / report_name
        report.write_bytes(loader_resolution_report(raw.read_bytes()))
        add(report, 'media-evidence/' + report_name, role='build-configuration')
        loader_receipts.append({'raw_path': str(raw), 'raw_sha256': sha(raw),
                                'report_path': 'media-evidence/' + report_name,
                                'report_sha256': sha(report), 'transform': 'glibc-terminal-address-v1'})
    dump(output.parent / (output.name + '-local-assembly.json'),
         {'mode': 'source-assembly', 'assembler_sha256': sha(Path(__file__)),
          'loader_reports': loader_receipts})
    for path in sorted(media.iterdir()):
        if path.name not in LOADER_REPORTS and path.is_file() and path.suffix in ('.json', '.mak', '.h', '.txt', '.tsv'):
            add(path, 'media-evidence/' + path.name, role='build-configuration')
    ubuntu = json.loads((stage / 'ubuntu-sources.json').read_text())
    terms_path = HERE / 'ubuntu-terms.lock.json'
    terms = load_ubuntu_terms(terms_path)
    _require_ubuntu_package_set(terms, [row['binary_package'] + '@' + row['binary_version']
                                      for row in ubuntu['packages']])
    if ubuntu.get('terms_lock_sha256') != sha(terms_path):
        raise ValueError('Ubuntu recipient terms lock changed')
    for package in ubuntu['packages']:
        resolved = resolve_ubuntu_terms(
            terms, binary_package=package['binary_package'], binary_version=package['binary_version'],
            source_package=package['source_package'], source_version=package['source_version'],
            notice=stage / 'ubuntu-source-evidence' / package['notice']['path'],
            shipped_objects=package['shipped_objects'])
        if package.get('source_required') is not True or not resolved['terms_selection_resolved']:
            raise ValueError('Ubuntu recipient sources are mandatory')
        for entry in package['files']:
            locked(entry, caches['compliance'] / 'ubuntu', 'ubuntu-inputs/' + entry['archive'],
                   'ubuntu:' + package['source_package'], required=True)
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
        locked(entry, caches['runtime'] / 'alpine-sources' / name, 'runtime-inputs/alpine-sources/' + name + '/APKBUILD', 'runtime:' + name)
        for entry in [source['upstream_source'], *source['aports_inputs']]:
            locked({**entry, 'archive': entry['file']}, caches['runtime'] / 'alpine-sources' / name,
                   'runtime-inputs/alpine-sources/' + name + '/' + entry['file'], 'runtime:' + name)
    for path in sorted((runtime / 'licenses').iterdir()):
        add(path, 'runtime-evidence/licenses/' + path.name, role='license-evidence')
    recipient_manifest = Path(derived.name) / 'runtime-manifest.json'
    recipient_manifest.write_bytes(public_runtime_manifest_bytes(runtime_manifest))
    add(recipient_manifest, 'runtime-evidence/manifest.json', role='build-configuration')
    for name in ('link.map', 'apk-installed.txt'):
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
    derived.cleanup()
    index = {'schema': 1, 'complete': not missing, 'missing': sorted(set(missing)), 'files': artifacts,
             'material_policy': dict(PUBLIC_MATERIAL_POLICY),
             'optional_unavailable': sorted(set(optional_unavailable)),
             'archives': [{'path': name, 'sha256': sha(output / name)} for name in archives],
             'scope': 'public corresponding sources and distributed materials; excludes private Alpine binary bootstrap'}
    dump(output / 'sources-index.json', index)
    verify_public_materials(output, index, require_complete=False)
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
