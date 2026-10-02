#!/usr/bin/env python3
"""Audit exact AppImage bytes, independently extracted payload and provenance."""
import argparse
import hashlib
import importlib.util
import json
import os
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


def qml_backing_evidence(media, mapping, documents):
    """Join installed QML backing-target exports to checked vendor SPDX files.

    Built-in modules have no separate plugin. Module terms establish provenance,
    not a file-level license for generated qmldir/typeinfo output.
    """
    result = {}
    by_document = {item['path'].removeprefix('prefix/'): item for item in documents}
    for path in sorted((media / 'prefix/lib/cmake').rglob('*Targets.cmake')):
        text = path.read_text()
        for block in re.findall(r'set_target_properties\((.*?)\n\)', text, re.S):
            properties = dict(re.findall(r'^\s+(_qt_[A-Za-z0-9_]+)\s+"([^"]*)"\s*$', block, re.M))
            if properties.get('_qt_qml_module_is_backing_target') != 'TRUE':
                continue
            qmldir = properties.get('_qt_qml_module_installed_qmldir_path')
            document = by_document.get(properties.get('_qt_sbom_spdx_v2_document_json_relative_path'))
            if not qmldir or not document:
                continue
            package = next((item for item in document['packages']
                            if item['SPDXID'] == properties.get('_qt_sbom_spdx_id')), None)
            if not package or package.get('versionInfo') != properties.get('_qt_sbom_package_version'):
                continue
            contains = [row for row in document['relationships']
                        if row['spdxElementId'] == package['SPDXID'] and row['relationshipType'] == 'CONTAINS']
            file_ids = {row['relatedSpdxElement'] for row in contains}
            backends = [item for item in mapping.values()
                        if item['evidence'] == document['path'] and item['file_spdx_id'] in file_ids]
            original = media / 'prefix' / qmldir
            if not backends or not original.is_file():
                continue
            declarations = [line.split('#', 1)[0].split() for line in original.read_text().splitlines()]
            uri = properties.get('_qt_qml_module_uri')
            if ['module', uri] not in declarations:
                continue
            generated = {Path(qmldir).name}
            generated.update(row[1] for row in declarations if len(row) == 2 and row[0] == 'typeinfo')
            result[str(Path(qmldir).parent)] = {
                'component_id': backends[0]['component_id'],
                'module_license_expression': package.get('licenseConcluded'),
                'evidence': {'SPDX_document': document['path'], 'package': package,
                             'Qt_vendor_version': package['versionInfo'], 'relationships': contains,
                             'backend_files': backends, 'qml_module_uri': uri,
                             'CMake_export': {'path': path.relative_to(media).as_posix(), 'sha256': stage.digest(path)},
                             'qmldir': {'path': 'prefix/' + qmldir, 'sha256': stage.digest(original)}},
                'generated_files': generated}
    return result


def qt_evidence(media, owners, root):
    documents = []
    mapping = {}
    for path in sorted((media / 'prefix/sbom').glob('*.spdx.json')):
        document = read_json(path)
        documents.append({'path': path.relative_to(media).as_posix(), 'sha256': stage.digest(path),
                          'name': document['name'], 'packages': document.get('packages', []),
                          'relationships': document.get('relationships', []),
                          'extracted_license_texts': document.get('hasExtractedLicensingInfos', [])})
        for file in document.get('files', []):
            relative = file['fileName'].removeprefix('./')
            original = media / 'prefix' / relative
            if not original.is_file():
                continue
            for checksum in file.get('checksums', []):
                algorithm = checksum['algorithm'].lower()
                if algorithm not in ('sha1', 'sha256'):
                    continue
                with original.open('rb') as stream:
                    actual = hashlib.file_digest(stream, algorithm).hexdigest()
                if actual != checksum['checksumValue']:
                    raise ValueError(f'Qt vendor SBOM checksum mismatch: {relative}')
            mapping[relative] = {'component_id': 'qt:' + path.name.split('-6.11.2')[0],
                                 'file_spdx_id': file['SPDXID'], 'license_expression': file.get('licenseConcluded'),
                                 'evidence': path.relative_to(media).as_posix()}
    if len(documents) != 4:
        raise ValueError('four frozen Qt vendor SBOM documents required')
    qml_backends = qml_backing_evidence(media, mapping, documents)
    for relative, owner in owners.items():
        if owner['owner'] != 'media':
            continue
        source = owner['source']
        original = media / 'prefix' / source
        if original.is_symlink():
            canonical = original.resolve()
            if not canonical.is_relative_to((media / 'prefix').resolve()):
                raise ValueError(f'Qt/media source symlink escaped prefix: {source}')
            source = canonical.relative_to(media / 'prefix').as_posix()
        if source in mapping:
            owner['components'] = [mapping[source]]
        elif source.startswith('qml/'):
            module = qml_backends.get(str(Path(source).parent))
            if module and Path(source).name in module['generated_files']:
                evidence = {**module['evidence'], 'generated_metadata': {
                    'path': 'prefix/' + source, 'sha256': stage.digest(original),
                    'terms_basis': 'vendor module provenance; generated-file terms not asserted by binary SBOM'}}
                if source.endswith('.qmltypes'):
                    generator = re.search(r'auto-generated by ([A-Za-z0-9_-]+)\.', original.read_text())
                    if generator:
                        evidence['generator'] = {'name': generator[1], 'packages': [
                            {'SPDX_document': document['path'], 'package': package}
                            for document in documents for package in document['packages']
                            if package.get('name') == generator[1]]}
                owner['components'] = [{'component_id': module['component_id'], 'license_expression': None,
                                         'module_license_expression': module['module_license_expression'],
                                         'evidence': evidence, 'role': 'generated-QML-metadata'}]
                continue
            directory = str(Path(source).parent)
            candidates = [entry for name, entry in mapping.items() if str(Path(name).parent) == directory]
            while not candidates and directory != "qml":
                directory = str(Path(directory).parent)
                candidates = [entry for name, entry in mapping.items() if str(Path(name).parent) == directory]
            if not candidates:
                raise ValueError(f'QML module lacks vendor plugin attribution: {source}')
            owner['components'] = [{'component_id': candidates[0]['component_id'],
                                     'module_license_expression': candidates[0]['license_expression'],
                                     'license_expression': candidates[0]['license_expression'],
                                     'evidence': candidates[0]['evidence'],
                                     'scope': 'containing Qt QML module; source SPDX retains file-level terms'}]
        elif Path(source).name.startswith(('libicu',)):
            icu = read_json(HERE / 'sources.lock.json')['icu_notice']
            notice = root / 'usr/share/licenses/icu/LICENSE'
            if stage.digest(notice) != icu['sha256']:
                raise ValueError('actual ICU notice does not match checked source receipt')
            owner['components'] = [{'component_id': 'icu:73.2', 'license_expression': icu['license_ref'],
                                     'evidence': {'url': icu['url'], 'sha256': icu['sha256'],
                                                  'custom_license_texts': {icu['license_ref']: notice.read_text()},
                                                  'Qt_vendor_version': '73.2', 'source_required': False}}]
        elif source.startswith(('sbom/', 'config_')):
            owner['components'] = [{'component_id': 'qt-license-evidence',
                                     'license_expression': None,
                                     'role': 'vendor-license-evidence',
                                     'evidence': source}]
        else:
            name = Path(source).name
            component = 'mpv' if name.startswith('libmpv') else 'libplacebo' if name.startswith('libplacebo') else 'ffmpeg' if name.startswith(('libav', 'libsw')) else None
            if component:
                owner['components'] = [{'component_id': component,
                                        'license_expression': {'mpv': 'GPL-2.0-or-later', 'ffmpeg': 'GPL-2.0-or-later', 'libplacebo': 'LGPL-2.1-or-later'}[component],
                                        'evidence': 'frozen checked source and build configuration; packaging/media/LICENSE-AUDIT.md'}]
            else:
                raise ValueError(f'media owner lacks component mapping: {relative} <- {source}')
    return documents


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
    for relative in ('AppRun', 'usr/bin/furami'):
        executable = root / relative
        if not executable.is_file() or executable.stat().st_mode & 0o111 != 0o111:
            raise ValueError(f'AppImage launch entry is not executable by recipients: {relative}')
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
    missing_cpp = {name for name in requirements if name.startswith(('GLIBCXX_', 'CXXABI_'))} - provided
    if missing_cpp:
        raise ValueError(f'bundled C++ providers lack symbols: {sorted(missing_cpp)}')
    glibc = [name for name in requirements if re.fullmatch(r'GLIBC_[0-9.]+', name)]
    if any(tuple(map(int, name.split('_')[1].split('.'))) > (2, 39) for name in glibc):
        raise ValueError('shipped ELF requires glibc newer than frozen Ubuntu 24.04 baseline 2.39')
    owners = receipt['component_sources']
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
    qt = qt_evidence(providers['media'], owners, root)
    static_warnings = []
    static = inventory.collect_static_components(str(root), str(providers['media'] / 'work/placebo-build/build.ninja'),
        str(providers['media'] / 'prefix/lib/pkgconfig/libplacebo.pc'), static_warnings)
    if static['status'] != 'link-inputs-matched-objects-unverified':
        raise ValueError('libplacebo static link-input ownership incomplete: ' + '; '.join(static_warnings))
    scanner = providers['media'] / 'prefix/libexec/qmlimportscanner'
    scanned = json.loads(stage.run([scanner, '-rootPath', providers['app'] / 'source-snapshot/qml', '-importPath', root / 'usr/qml'],
        env={**os.environ, 'LD_LIBRARY_PATH': str(providers['media'] / 'prefix/lib')}))
    for item in scanned:
        if item.get('type') != 'module' or item.get('name') == 'dev.antho.furami':
            continue
        location = Path(item.get('path', ''))
        if not location.is_absolute() or not location.resolve().is_relative_to((root / 'usr/qml').resolve()) or not location.is_dir():
            raise ValueError(f'extracted QML import unresolved: {item}')
    if not (root / 'usr/lib/libmpv.so').is_file():
        raise ValueError('canonical dlopen libmpv root missing')
    notices = [item for item in files if item['path'].startswith(('usr/share/licenses/', 'usr/share/doc/ubuntu/', 'usr/share/furami/runtime-compliance/licenses/'))]
    attribution = load('components')
    registry, runtime_components, external_notices = attribution.registry(providers, owners, qt, static, stage.digest)
    header['components'] = [registry[identifier] for identifier in runtime_components]
    manifest = {'schema': 1, 'image': {'path': image.name, 'sha256': stage.digest(image)}, 'outer_runtime': header,
                'payload': actual, 'owners': owners, 'provider_manifest_sha256': ids}
    result = {'schema': 1, 'image_sha256': stage.digest(image), 'technical_pass': True, 'elf': elf,
              'dependencies': dependencies, 'host_ABI_allowlist': sorted(stage.HOST_LIBS),
              'symbol_requirements': sorted(requirements), 'glibc_baseline': '2.39',
              'qml': {'scanner_imports': scanned, 'staging_closure': receipt['qml']},
              'dlopen_roots': ['usr/lib/libmpv.so'], 'Qt_vendor_SBOM': qt,
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
    dump(output / 'components.json', {'schema': 1, 'files': owners, 'components': registry, 'Qt_vendor_SBOM': qt,
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
        for document in qt:
            for item in document['extracted_license_texts']:
                notice.write('\n===== Qt ' + item['licenseId'] + ' =====\n')
                notice.write(item.get('extractedText', '') + '\n')
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
