#!/usr/bin/env python3
"""Audit exact AppImage bytes, independently extracted payload and provenance."""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location('furami_' + name, HERE / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


stage = load('stage')
inventory = load('inventory')

# Launch entries every extracted payload must carry executable: AppRun, the
# application, and the media provider's own renderer program seeded by stage.py.
REQUIRED_EXECUTABLES = ('AppRun', 'usr/bin/furami', 'usr/bin/mpv')


def dump(path, value):
    Path(path).write_text(json.dumps(value, sort_keys=True, indent=2) + '\n')


def read_json(path):
    return json.loads(Path(path).read_text())


def versions(path):
    text = stage.run(['readelf', '--version-info', '--', path])
    needed = text.split('Version needs section', 1)[1] if 'Version needs section' in text else ''
    defined = text.split('Version definition section', 1)[1].split('Version needs section', 1)[0] if 'Version definition section' in text else ''
    pattern = r'Name: ((?:GLIBC|GLIBCXX|CXXABI)_[A-Za-z0-9_.]+)'
    return {'required': sorted(set(re.findall(pattern, needed))),
            'provided': sorted(set(re.findall(pattern, defined)))}


def media_evidence(media, owners):
    prefix = (media / 'prefix').resolve()
    terms = {'mpv': 'GPL-2.0-or-later', 'ffmpeg': 'GPL-2.0-or-later',
             'libplacebo': 'LGPL-2.1-or-later'}
    basis = 'frozen checked source and build configuration; packaging/media/LICENSE-AUDIT.md'
    ffmpeg = None
    for relative, owner in owners.items():
        if owner['owner'] != 'media':
            continue
        source = owner['source']
        original = prefix / source
        canonical = original.resolve()
        if not canonical.is_relative_to(prefix):
            raise ValueError(f'media source escaped prefix: {source}')
        component = None
        if (source, relative) in stage.MEDIA_PROGRAMS:
            component = 'mpv'
        elif relative == 'usr/' + source and canonical.parent == prefix / 'lib':
            name = canonical.name
            component = ('mpv' if name.startswith('libmpv.so')
                         else 'libplacebo' if name.startswith('libplacebo.so')
                         else 'ffmpeg' if re.fullmatch(r'lib(?:av(?:codec|device|filter|format|util)|sw(?:resample|scale))\.so(?:\..*)?', name)
                         else None)
        if component is None:
            raise ValueError(f'media owner lacks component mapping: {relative} <- {source}')
        if component == 'ffmpeg' and ffmpeg is None:
            manifest = read_json(media / 'manifest.json')
            stage.source_patches.verify_provider(media, manifest, manifest['sources'])
            ffmpeg = {'basis': basis, 'upstream_source': manifest['sources'][component],
                      'source_patch_receipt': manifest.get('source_patches')}
        owner['components'] = [{'component_id': component, 'license_expression': terms[component],
                                'evidence': ffmpeg if component == 'ffmpeg' else basis}]


def verify_executables(root, relatives):
    for relative in relatives:
        executable = root / relative
        if not executable.is_file() or executable.stat().st_mode & 0o111 != 0o111:
            raise ValueError(f'AppImage launch entry is not executable by recipients: {relative}')


def verify_payload_owners(root, actual, owners):
    for relative, owner in owners.items():
        if owner['owner'].startswith('ubuntu:') and actual.get(relative, {}).get('type') not in ('file', 'symlink'):
            raise ValueError(f'Ubuntu owner path absent from final image: {relative}')
    for relative, metadata in actual.items():
        if metadata['type'] == 'directory':
            continue
        owner = owners.get(relative)
        if not owner and metadata['type'] == 'symlink':
            resolved = (root / relative).resolve().relative_to(root).as_posix()
            owner = owners.get(resolved)
        if not owner:
            raise ValueError(f'final image file owner missing: {relative}')
        if metadata['type'] == 'file' and owner.get('shipped_sha256') != metadata['sha256']:
            raise ValueError(f'final image owner hash mismatch: {relative}')


def verify_crate_notices(app_root, root, actual, owners):
    app = read_json(app_root / 'manifest.json')
    records, expected_owners, _ = stage.crate_notice_inventory(app_root, app)
    offer = read_json(root / 'usr/share/licenses/furami/source-offer.json')
    if offer.get('rust_crates') != records:
        raise ValueError('final crate source offer differs from verified notice inputs')
    prefix = 'usr/share/licenses/rust/'
    extracted = {path for path, entry in actual.items()
                 if path.startswith(prefix)
                 and not path.startswith(prefix + 'rust-std-1.97.1/')
                 and entry['type'] != 'directory'}
    if extracted != set(expected_owners):
        raise ValueError('final crate notice inventory differs from verified notice inputs')
    if {path for path, owner in owners.items() if owner['owner'] == 'app-crate-source'} != extracted:
        raise ValueError('crate notice ownership exists outside the verified inventory')
    for path, expected in expected_owners.items():
        digest = expected['source_sha256']
        if (actual[path]['type'] != 'file' or actual[path].get('sha256') != digest
                or owners.get(path) != {**expected, 'shipped_sha256': digest}):
            raise ValueError(f'final crate notice bytes or provenance differ: {path}')


def audit(image, provenance, output):
    if not output.is_absolute() or output.exists() or output.is_relative_to(REPO):
        raise ValueError('audit output must be new absolute directory outside checkout')
    output.mkdir(parents=True)
    providers = {key: Path(provenance[key]) for key in ('media', 'app', 'runtime', 'stage')}
    receipt = read_json(providers['stage'] / 'evidence.json')
    ids, media_manifest = stage.check_inputs(providers['media'], providers['app'], providers['runtime'])
    if ids != receipt['inputs_manifest_sha256']:
        raise ValueError('provider manifests differ from staging receipt')
    if image.is_symlink() or stage.digest(image) != receipt['image']['sha256']:
        raise ValueError('final image bytes differ from build receipt')
    header = stage.verify_runtime_header(image, providers['runtime'] / 'runtime-x86_64')
    if header != receipt['runtime_header']:
        raise ValueError('outer runtime header differs from receipt')
    extract = output / 'extracted'
    extract.mkdir()
    stage.run([image, '--appimage-extract'], cwd=extract)
    root = extract / 'squashfs-root'
    actual = stage.snapshot(root)
    if actual != receipt['appdir']['artifacts']:
        raise ValueError('actual image file/mode/symlink manifest differs from staging receipt')
    verify_executables(root, REQUIRED_EXECUTABLES)
    files, symlinks = inventory.walk_tree(str(root))
    if any('error' in item for item in files):
        raise ValueError('unreadable final image file')
    for link in symlinks:
        if link['target_absolute'] or not link['resolved_within_root'] or not (root / link['path']).exists():
            raise ValueError(f'unsafe or dangling final image symlink: {link["path"]}')
    paths = inventory.detect_elf_files(str(root), files)
    elf, errors = inventory.read_elf_metadata(str(root), paths, 'readelf')
    if errors:
        raise ValueError('; '.join(errors))
    inventory.require_qt_free(actual, elf)
    dependency_warnings = []
    dependencies = inventory.resolve_dependency_closure(str(root), elf,
        {item['path']: item['sha256'] for item in files}, symlinks, ['usr/lib'], dependency_warnings)
    for entry in dependencies:
        if entry['resolution']['kind'] == 'missing' and entry['name'] in stage.HOST_LIBS:
            entry['resolution'] = {'kind': 'declared-host-ABI', 'soname': entry['name'], 'loader_verified': False}
            warning = f'missing dependency: {entry["name"]} (required by {entry["requester"]})'
            if warning in dependency_warnings:
                dependency_warnings.remove(warning)
    if dependency_warnings:
        raise ValueError('; '.join(dependency_warnings))
    requirements = set()
    provided = set()
    for item in elf:
        path = root / item['path']
        if item['class'] != 'ELF64' or item['machine'] != 'Advanced Micro Devices X86-64':
            raise ValueError(f'wrong shipped ELF architecture: {item["path"]}')
        if re.search(r'(?:_dri|_icd|vulkan_[A-Za-z0-9]+)\.so', path.name) or 'dri' in path.parts:
            raise ValueError(f'GPU driver/ICD bundled: {item["path"]}')
        for tag in ('rpath', 'runpath'):
            value = item.get(tag)
            if value:
                for token in value.split(':'):
                    if not token or not token.startswith(('$ORIGIN', '${ORIGIN}')):
                        raise ValueError(f'non-relative final ELF {tag}: {item["path"]}: {value}')
                    resolved = (path.parent / token.replace('${ORIGIN}', '.').replace('$ORIGIN', '.')).resolve()
                    if not resolved.is_relative_to(root.resolve()):
                        raise ValueError(f'escaped final ELF {tag}: {item["path"]}: {value}')
        program = stage.run(['readelf', '-l', '--', path])
        interp = re.findall(r'Requesting program interpreter: ([^\]]+)', program)
        if any(value != '/lib64/ld-linux-x86-64.so.2' for value in interp):
            raise ValueError(f'undeclared ELF interpreter: {item["path"]}: {interp}')
        item['interpreter'] = interp
        item['symbol_versions'] = versions(path)
        requirements.update(item['symbol_versions']['required'])
        provided.update(item['symbol_versions']['provided'])
    if 'usr/bin/mpv' not in {item['path'] for item in elf}:
        raise ValueError('seeded media program is not an inspected ELF: usr/bin/mpv')
    missing_cpp = {name for name in requirements if name.startswith(('GLIBCXX_', 'CXXABI_'))} - provided
    if missing_cpp:
        raise ValueError(f'bundled C++ providers lack symbols: {sorted(missing_cpp)}')
    glibc = [name for name in requirements if re.fullmatch(r'GLIBC_[0-9.]+', name)]
    if any(tuple(map(int, name.split('_')[1].split('.'))) > (2, 39) for name in glibc):
        raise ValueError('shipped ELF requires glibc newer than frozen Ubuntu 24.04 baseline 2.39')
    owners = receipt['component_sources']
    verify_payload_owners(root, actual, owners)
    verify_crate_notices(providers['app'], root, actual, owners)
    media_evidence(providers['media'], owners)
    static_warnings = []
    static = inventory.collect_static_components(str(root), str(providers['media'] / 'work/placebo-build/build.ninja'),
        str(providers['media'] / 'prefix/lib/pkgconfig/libplacebo.pc'), static_warnings)
    if static['status'] != 'link-inputs-matched-objects-unverified':
        raise ValueError('libplacebo static link-input ownership incomplete: ' + '; '.join(static_warnings))
    if not (root / 'usr/lib/libmpv.so').is_file():
        raise ValueError('canonical dlopen libmpv root missing')
    notices = [item for item in files if item['path'].startswith(('usr/share/licenses/', 'usr/share/doc/ubuntu/', 'usr/share/furami/runtime-compliance/licenses/'))]
    attribution = load('components')
    registry, runtime_components, external_notices = attribution.registry(
        providers, owners, static, stage.digest, payload_root=root, elf_paths=paths)
    header['components'] = [registry[identifier] for identifier in runtime_components]
    manifest = {'schema': 1, 'image': {'path': image.name, 'sha256': stage.digest(image)}, 'outer_runtime': header,
                'payload': actual, 'owners': owners, 'provider_manifest_sha256': ids}
    result = {'schema': 1, 'image_sha256': stage.digest(image), 'technical_pass': True, 'elf': elf,
              'dependencies': dependencies, 'host_ABI_allowlist': sorted(stage.HOST_LIBS),
              'symbol_requirements': sorted(requirements), 'glibc_baseline': '2.39',
              'dlopen_roots': list(stage.DLOPEN_ROOTS),
              'static_libplacebo': static, 'static_warnings': static_warnings,
              'app_static': read_json(providers['app'] / 'manifest.json')['binary'],
              'runtime_static': read_json(providers['runtime'] / 'manifest.json')['components'],
              'notices': notices,
              'component_files': owners,
              'component_registry': registry,
              'limits': ['Static graph does not prove runtime plugin/dlopen loads or host GPU driver namespace compatibility. Real launch evidence required.',
                         'Static archive input ownership established conservatively; all contributor source/notices required, no retained-function claim.']}
    dump(output / 'manifest.json', manifest)
    dump(output / 'audit.json', result)
    dump(output / 'components.json', {'schema': 1, 'files': owners, 'components': registry,
                                      'libplacebo_static': static, 'runtime': result['runtime_static'], 'application': result['app_static']})
    with (output / 'NOTICE.txt').open('w') as notice:
        notice.write('Furami combined executable uses GPL-compatible terms subject to release-ready.json. Authored Furami source remains MIT.\n\n')
        for item in notices:
            path = root / item['path']
            # Source/relink archives and metadata are not license text.
            if path.suffix in ('.json', '.gz', '.xz', '.tar', '.sh', '.py'):
                continue
            notice.write('\n===== ' + item['path'] + ' =====\n')
            notice.write(path.read_text(errors='strict'))
            notice.write('\n')
        for path in external_notices:
            notice.write('\n===== signed Ubuntu component ' + path.name + ' =====\n')
            notice.write(path.read_text())
            notice.write('\n')
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', type=Path, required=True)
    parser.add_argument('--provenance', type=Path, required=True, help='build provenance JSON')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        audit(args.image.resolve(), read_json(args.provenance), args.output)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit('audit failed: ' + str(error))
