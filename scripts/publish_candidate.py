#!/usr/bin/env python3
"""Verify a flat source-cleared candidate; publish exact stable or RC pushes."""
import sys
sys.dont_write_bytecode = True

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import tomllib
import tarfile

from release_notes import TAG_RE

REPO = Path(__file__).resolve().parents[1]

MAX_RELEASE_ASSET_BYTES = 2_147_483_648


class CandidatePublicationError(ValueError):
    pass


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def read_json(path):
    try:
        value = json.loads(path.read_text())
        if not isinstance(value, dict):
            raise ValueError('expected object')
        return value
    except (OSError, ValueError) as error:
        raise CandidatePublicationError(f'{path.name}: {error}') from error


def verify_candidate(candidate: Path, version: str, *, require_reproducibility: bool) -> list[Path]:
    candidate = candidate.absolute()
    if candidate.is_symlink() or not candidate.is_dir():
        raise CandidatePublicationError('candidate must be a real directory')
    assets = sorted(candidate.iterdir())
    if not assets or any(not stat.S_ISREG(p.lstat().st_mode) for p in assets):
        raise CandidatePublicationError('candidate assets must be flat regular files, not symlinks')
    names = {p.name for p in assets}
    image_name = f'Furami-v{version}-Linux-x64.AppImage'
    allowed = {image_name, 'SHA256SUMS', 'release-ready.json', 'audit.json', 'manifest.json',
               'components.json', 'sources-index.json', 'NOTICE.txt', 'RELINKING.md', 'reproducibility.json'}
    if any(name not in allowed and not re.fullmatch(r'source-materials-[0-9]{3}\.tar', name)
           for name in names):
        raise CandidatePublicationError('unexpected public candidate asset')
    required = {image_name, 'SHA256SUMS', 'release-ready.json', 'audit.json',
                'sources-index.json', 'NOTICE.txt', 'RELINKING.md'}
    if not required <= names or (candidate / image_name).stat().st_size == 0:
        raise CandidatePublicationError('required candidate assets missing or image empty')
    checksums = {}
    for line in (candidate / 'SHA256SUMS').read_text().splitlines():
        match = re.fullmatch(r'([0-9a-f]{64})  ([^/\\\x00-\x1f\x7f]+)', line)
        if not match or match[2] in ('.', '..', 'SHA256SUMS') or match[2] in checksums:
            raise CandidatePublicationError('unsafe or duplicate SHA256SUMS entry')
        checksums[match[2]] = match[1]
    if set(checksums) != names - {'SHA256SUMS'}:
        raise CandidatePublicationError('SHA256SUMS does not cover exactly all upload assets')
    for name, expected in checksums.items():
        if digest(candidate / name) != expected:
            raise CandidatePublicationError(f'candidate SHA256 mismatch: {name}')
    ready = read_json(candidate / 'release-ready.json')
    if ready.get('ready') is not True:
        raise CandidatePublicationError('source/license clearance refused: ' + json.dumps(ready.get('reasons', [])))
    for field, name in (('image_sha256', image_name), ('technical_audit_sha256', 'audit.json'),
                        ('sources_index_sha256', 'sources-index.json')):
        if ready.get(field) != checksums[name]:
            raise CandidatePublicationError(f'release readiness hash mismatch: {field}')
    actual_sources = {name: value for name, value in checksums.items()
                      if re.fullmatch(r'source-materials-.*\.tar', name)}
    source_index = read_json(candidate / 'sources-index.json')
    archives = source_index.get('archives')
    indexed_sources = {}
    if not isinstance(archives, list) or not archives:
        raise CandidatePublicationError('source index archives required')
    for entry in archives:
        if (not isinstance(entry, dict) or not isinstance(entry.get('path'), str)
                or not re.fullmatch(r'source-materials-[^/\\\x00-\x1f\x7f]+\.tar', entry['path'])
                or entry['path'] in indexed_sources
                or not isinstance(entry.get('sha256'), str)
                or not re.fullmatch(r'[0-9a-f]{64}', entry['sha256'])):
            raise CandidatePublicationError('invalid or duplicate source index archive')
        indexed_sources[entry['path']] = entry['sha256']
    if indexed_sources != actual_sources:
        raise CandidatePublicationError('source index archive set or hash mismatch')
    spec = importlib.util.spec_from_file_location('furami_candidate_sources', REPO / 'packaging/appimage/sources.py')
    material = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(material)
    try:
        material.verify_public_materials(candidate, source_index)
        policy = ready.get('material_policy')
        if (policy != source_index['material_policy'] or not isinstance(policy, dict)
                or type(policy.get('schema')) is not int
                or policy.get('public_offline_bootstrap_complete') is not False):
            raise ValueError('release readiness public material policy missing or malformed')
    except (OSError, ValueError, KeyError, TypeError, IndexError, tarfile.TarError) as error:
        raise CandidatePublicationError('public distributed materials refused: ' + str(error)) from error
    if require_reproducibility or 'reproducibility.json' in names:
        if 'reproducibility.json' not in names:
            raise CandidatePublicationError('reproducibility receipt required')
        receipt = read_json(candidate / 'reproducibility.json')
        spec = importlib.util.spec_from_file_location('furami_candidate_comparator', REPO / 'packaging/appimage/build-twice.py')
        comparator = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(comparator)
        if not comparator.accepted_criteria(receipt.get('criteria')):
            raise CandidatePublicationError('reproducibility criteria refused')
        if receipt.get('second_image_sha256') != checksums[image_name]:
            raise CandidatePublicationError('reproducibility image mismatch')
        sources = receipt.get('raw_source_archive_sha256')
        image_hashes = [receipt.get(key) for key in ('first_image_sha256', 'second_image_sha256')]
        if any(not isinstance(value, str) or not re.fullmatch(r'[0-9a-f]{64}', value) for value in image_hashes):
            raise CandidatePublicationError('invalid raw image hashes')
        image_equal = image_hashes[0] == image_hashes[1]
        if receipt['criteria']['image_bytes'] is not image_equal:
            raise CandidatePublicationError('image equality criterion contradicts raw hashes')
        if not actual_sources or not isinstance(sources, list) or len(sources) != 2 or sources[1] != actual_sources:
            raise CandidatePublicationError('reproducibility source archive mismatch')
        if any(not isinstance(rows, dict) or set(rows) != set(actual_sources)
               or any(not isinstance(value, str) or not re.fullmatch(r'[0-9a-f]{64}', value)
                      for value in rows.values()) for rows in sources):
            raise CandidatePublicationError('invalid raw source archive hashes')
        status, identity = receipt.get('status'), receipt.get('byte_identity')
        if status == 'identical':
            if (identity is not True or receipt['criteria']['image_bytes'] is not True
                    or receipt.get('first_image_sha256') != checksums[image_name] or sources[0] != sources[1]):
                raise CandidatePublicationError('inconsistent identical reproducibility receipt')
        elif status == 'documented_metadata_variance':
            variance = receipt.get('metadata_variance')
            if (identity is not False or (image_equal and sources[0] == sources[1])
                    or not isinstance(variance, dict) or not variance
                    or any(not isinstance(value, dict) or not value.get('cause') for value in variance.values())):
                raise CandidatePublicationError('metadata variance evidence missing or contradictory')
            if not image_equal:
                evidence = variance.get('image:.digest_md5', {})
                if (receipt['criteria'].get('image_known_metadata_only') is not True
                        or type(evidence.get('offset')) is not int or evidence['offset'] < 0
                        or evidence.get('length') != 16
                        or any(not isinstance(evidence.get(key), str)
                               or not re.fullmatch(r'[0-9a-f]{32}', evidence[key])
                               for key in ('first_hex', 'second_hex'))
                        or evidence['first_hex'] == evidence['second_hex']):
                    raise CandidatePublicationError('image metadata variance proof required')
            if sources[0] != sources[1]:
                if any(not isinstance(variance.get(key), dict) or not variance[key].get('cause')
                       for key in ('runtime-evidence/link.map', 'runtime-evidence/manifest.json')):
                    raise CandidatePublicationError('source metadata variance proof required')
        else:
            raise CandidatePublicationError('reproducibility status refused')
    return assets


def prepare_public_assets(verified_assets: list[Path], version: str, output: Path) -> list[Path]:
    image_name = f'Furami-v{version}-Linux-x64.AppImage'
    by_name = {path.name: path for path in verified_assets}
    if len(by_name) != len(verified_assets):
        raise CandidatePublicationError('verified assets must have distinct basenames')
    for name in (image_name, 'manifest.json', 'components.json'):
        if name not in by_name:
            raise CandidatePublicationError(f'public export requires {name}')
    if output.is_symlink() or not output.is_dir():
        raise CandidatePublicationError('public asset output must be a real directory')
    if any(output.iterdir()):
        raise CandidatePublicationError('public asset output directory must be empty')
    internal = {}
    for line in by_name['SHA256SUMS'].read_text().splitlines():
        match = re.fullmatch(r'([0-9a-f]{64})  ([^/\\\x00-\x1f\x7f]+)', line)
        if not match or match[2] in ('.', '..', 'SHA256SUMS') or match[2] in internal:
            raise CandidatePublicationError('unsafe or duplicate internal SHA256SUMS entry')
        internal[match[2]] = match[1]
    if set(internal) != set(by_name) - {'SHA256SUMS'}:
        raise CandidatePublicationError('internal SHA256SUMS does not cover exactly the verified assets')
    members = sorted(
        (path for path in verified_assets
         if path.name not in {image_name, 'manifest.json'}
         and re.fullmatch(r'source-materials-[0-9]{3}\.tar', path.name) is None),
        key=lambda path: path.name)
    archive_path = output / f'Furami-v{version}-Linux-x64-materials.tar'
    try:
        with tarfile.open(archive_path, 'w', format=tarfile.PAX_FORMAT) as archive:
            for path in members:
                info = tarfile.TarInfo(path.name)
                info.type = tarfile.REGTYPE
                info.mode = 0o644
                info.uid = info.gid = 0
                info.uname = info.gname = ''
                info.mtime = 0
                info.size = path.stat().st_size
                with path.open('rb') as stream:
                    archive.addfile(info, stream)
    except tarfile.TarError as error:
        raise CandidatePublicationError('public materials archive refused: ' + str(error)) from error
    size = archive_path.stat().st_size
    if size >= MAX_RELEASE_ASSET_BYTES:
        raise CandidatePublicationError(f'public materials archive must be under 2 GiB: {size} bytes')
    public = {image_name: internal[image_name], 'manifest.json': internal['manifest.json'],
              archive_path.name: digest(archive_path)}
    sums_path = output / 'SHA256SUMS'
    sums_path.write_text(''.join(f'{public[name]}  {name}\n' for name in sorted(public)))
    return sorted([by_name[image_name], by_name['manifest.json'], archive_path, sums_path],
                  key=lambda path: path.name)


def release_channel(tag: str, version: str, event_name: str) -> str:
    match = TAG_RE.fullmatch(tag)
    if event_name != 'push' or match is None or tag != 'v' + version:
        raise CandidatePublicationError('publication requires a push of the exact Cargo release tag')
    return 'prerelease' if match[4] is not None else 'stable'


def publish_candidate(*, tag: str, version: str, event_name: str, repository: str,
                      require_reproducibility: bool, notes: Path, candidate: Path) -> None:
    channel = release_channel(tag, version, event_name)
    assets = verify_candidate(candidate, version, require_reproducibility=require_reproducibility)
    if not notes.is_file() or not notes.read_text().strip():
        raise CandidatePublicationError('nonempty release notes required')
    channel_flags = ['--prerelease', '--latest=false'] if channel == 'prerelease' else ['--latest=true']
    with tempfile.TemporaryDirectory(prefix='furami-release-') as directory:
        public_assets = prepare_public_assets(assets, version, Path(directory))
        subprocess.run(['gh', 'release', 'create', tag, '--repo', repository, '--title', tag,
                        '--notes-file', str(notes.resolve()), '--verify-tag', *channel_flags,
                        *(str(path) for path in public_assets)], check=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate', required=True, type=Path)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--event-name', required=True)
    parser.add_argument('--repository', default='')
    parser.add_argument('--notes', type=Path)
    parser.add_argument('--check-only', action='store_true')
    parser.add_argument('--require-reproducibility', action='store_true')
    args = parser.parse_args(argv)
    try:
        version = tomllib.loads((REPO / 'Cargo.toml').read_text())['package']['version']
        packages = tomllib.loads((REPO / 'Cargo.lock').read_text())['package']
        app = [entry for entry in packages if entry['name'] == 'furami']
        if len(app) != 1 or app[0]['version'] != version:
            raise CandidatePublicationError('Cargo lock version mismatch')
        release_channel(args.tag, version, args.event_name)
        if args.check_only:
            assets = verify_candidate(args.candidate, version, require_reproducibility=args.require_reproducibility)
            print(json.dumps({'verified_assets': [p.name for p in assets], 'version': version}))
        else:
            if not args.repository or args.notes is None:
                raise CandidatePublicationError('--repository and --notes required for publication')
            publish_candidate(tag=args.tag, version=version, event_name=args.event_name,
                              repository=args.repository, notes=args.notes, candidate=args.candidate,
                              require_reproducibility=args.require_reproducibility)
        return 0
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print('candidate refused: ' + str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
