#!/usr/bin/env python3
"""Build one frozen x86_64 AppImage and emit independently audited release assets."""
import argparse
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tomllib

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
MAX_LOG_BYTES = 8 * 1024 * 1024


def module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def checked_path(path, label, new=False):
    if not path.is_absolute() or path.is_symlink():
        raise ValueError(f'{label} must be absolute, not a symlink')
    value = path.resolve()
    if value == Path('/') or value.is_relative_to(REPO) or ':' in str(value):
        raise ValueError(f'{label} must be outside checkout and mount-safe')
    if new and value.exists():
        raise ValueError(f'{label} must not exist: {value}')
    return value


def private_cache_outside(cache, upload_roots):
    cache = cache.resolve()
    for root in upload_roots:
        root = root.resolve()
        if cache.is_relative_to(root) or root.is_relative_to(cache):
            raise ValueError('private runtime cache must be disjoint from candidate/evidence output roots')


def collect_evidence(out, destination, commit):
    """Copy allowlisted receipts and bounded app logs, never provider/cache trees."""
    out, destination = out.resolve(), destination.absolute()
    if destination.is_symlink() or (destination.exists() and any(destination.iterdir())):
        raise ValueError('evidence upload root must be empty and not a symlink')
    paths = ('provenance.json', 'app/build-inputs.json', 'app/first/evidence/container-command.json',
             'reproducibility.json', 'release/release-ready.json', 'audit/audit.json')
    log_paths = ('app/first/build.log',)
    logs = []
    selected = []
    for prefix in ('', 'first', 'second'):
        for name in (*paths, *log_paths):
            source = out / prefix / name
            if not source.exists() and not source.is_symlink():
                continue
            if (not source.is_file() or not source.resolve().is_relative_to(out)
                    or any(path.is_symlink() for path in (source, *source.parents)
                           if path.is_relative_to(out))):
                raise ValueError('evidence input must be regular and confined without symlinks: ' + name)
            if name in paths and not isinstance(json.loads(source.read_text()), dict):
                raise ValueError('evidence receipt must contain a JSON object: ' + name)
            if name == 'provenance.json':
                receipt = json.loads(source.read_text())
                cache = receipt.get('source_caches', {}).get('runtime')
                if not isinstance(cache, str) or not Path(cache).is_absolute():
                    raise ValueError('runtime private cache provenance missing')
                private_cache_outside(Path(cache), (out, destination))
            selected.append((source, destination / prefix / name, name in log_paths))
    destination.mkdir(parents=True, exist_ok=True)
    for source, target, is_log in selected:
        target.parent.mkdir(parents=True, exist_ok=True)
        if not is_log:
            shutil.copy2(source, target)
            continue
        original_digest = hashlib.sha256()
        original_bytes = 0
        with source.open('rb') as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b''):
                original_digest.update(chunk)
                original_bytes += len(chunk)
            stream.seek(max(0, original_bytes - MAX_LOG_BYTES))
            tail = stream.read(MAX_LOG_BYTES)
        target.write_bytes(tail)
        logs.append({'path': target.relative_to(destination).as_posix(),
                     'original_bytes': original_bytes, 'stored_bytes': len(tail),
                     'truncated_bytes': original_bytes - len(tail),
                     'original_sha256': original_digest.hexdigest(),
                     'sha256': hashlib.sha256(tail).hexdigest()})
    sources_recipe = module(HERE / 'sources.py', 'furami_evidence_sources')
    recipes = {str(path.relative_to(REPO)): sources_recipe.sha(path)
               for path in (REPO / 'packaging').rglob('*')
               if path.is_file() and not path.is_symlink()
               and path.suffix in ('.py', '.json', '.sh', '.Containerfile')}
    sources_recipe.dump(destination / 'executed-inputs.json',
                        {'commit': commit, 'recipes': recipes, 'logs': logs})


def build(args):
    out = checked_path(args.output, 'output', new=True)
    version = tomllib.loads((REPO / 'Cargo.toml').read_text())['package']['version']
    if args.version and args.version != version:
        raise ValueError(f'version {args.version} differs from Cargo version {version}')
    if not 1 <= args.jobs <= 4:
        raise ValueError('jobs must be 1..4')
    for tool in ('podman', 'readelf'):
        if not shutil.which(tool):
            raise ValueError(f'required build/audit tool unavailable: {tool}')
    os.environ['FURAMI_BUILD_JOBS'] = str(args.jobs)
    cache = checked_path(args.source_cache or Path.home() / '.cache/furami/appimage-inputs', 'source cache')
    caches = {
        'app': cache,
        'media': checked_path(args.media_source_cache or cache / 'media', 'media source cache'),
        'runtime': checked_path(args.runtime_source_cache or Path.home() / '.cache/furami/appimage-runtime', 'runtime source cache'),
        'compliance': checked_path(args.compliance_cache or cache / 'compliance', 'compliance source cache'),
        'tool': checked_path(args.tool_cache or Path.home() / '.cache/furami-qualification', 'tool cache'),
    }
    if any(value == out or value.is_relative_to(out) for value in caches.values()):
        raise ValueError('source caches must be outside output')
    private_cache_outside(caches['runtime'], (out,))
    app_recipe = module(HERE / 'build-app.py', 'furami_app_builder')
    stage_recipe = module(HERE / 'stage.py', 'furami_stager')
    audit_recipe = module(HERE / 'audit.py', 'furami_final_auditor')
    sources_recipe = module(HERE / 'sources.py', 'furami_sources')
    stack = json.loads((REPO / 'packaging/media/stack.lock.json').read_text())
    media = checked_path(args.media_output, 'media provider') if args.media_output else out / 'media'
    runtime = checked_path(args.runtime_build, 'runtime provider') if args.runtime_build else out / 'runtime'
    # Reject stale providers before spending resources on app compilation.
    if args.media_output:
        app_recipe.verify_media(media, stack)
    if args.runtime_build:
        receipt = json.loads((runtime / 'manifest.json').read_text())
        if receipt.get('lock_sha256') != stage_recipe.digest(REPO / 'packaging/appimage-runtime/runtime.lock.json'):
            raise ValueError('runtime provider lock mismatch')
        if receipt.get('modified_libfuse', {}).get('enabled'):
            raise ValueError('modified-libfuse demo is not canonical runtime provider')
        for relative, expected in receipt['files_sha256'].items():
            path = runtime / relative
            if Path(relative).is_absolute() or '..' in Path(relative).parts or path.is_symlink() or not path.is_file() or stage_recipe.digest(path) != expected:
                raise ValueError(f'runtime provider artifact mismatch: {relative}')
    out.mkdir(parents=True)
    if not args.media_output:
        subprocess.run([sys.executable, str(REPO / 'packaging/media/build.py'), str(media), '--source-cache', str(caches['media'])], check=True)
    if not args.runtime_build:
        subprocess.run([sys.executable, str(REPO / 'packaging/appimage-runtime/build.py'), str(runtime), '--source-cache', str(caches['runtime'])], check=True)
    # Core source archives required for actual notices, reverified on every read.
    for name in ('mpv', 'libplacebo', 'ffmpeg'):
        entry = stack['sources'][name]
        sources_recipe.obtain(entry, caches['media'], True)
    app = out / 'app'
    subprocess.run([sys.executable, str(HERE / 'build-app.py'), '--media-output', str(media), '--output', str(app), '--source-cache', str(caches['app'])], check=True)
    stage = out / 'stage'
    provenance = {'schema': 1, 'media': str(media), 'app': str(app), 'runtime': str(runtime), 'stage': str(stage),
                  'version': version, 'source_caches': {key: str(value) for key, value in caches.items()},
                  'recipe_sha256': {str(path.relative_to(REPO)): stage_recipe.digest(path)
                                    for path in sorted(HERE.iterdir())
                                    if path.is_file() and path.suffix in ('.py', '.json', '.sh')}}
    sources_recipe.dump(out / 'provenance.json', provenance)
    subprocess.run([sys.executable, str(HERE / 'stage.py'), '--media-output', str(media), '--app-build', str(app),
                    '--runtime-build', str(runtime), '--media-source-cache', str(caches['media']), '--output', str(stage),
                    '--tool-cache', str(caches['tool']), '--version', version], check=True)
    image = stage / f'Furami-v{version}-Linux-x64.AppImage'
    audited = out / 'audit'
    audited_result = audit_recipe.audit(image, provenance, audited)
    return finalize(out, provenance, audited_result, image, caches, args.acquire_sources)


def finalize(out, provenance, audited_result, image, caches, acquire_sources):
    """One readiness/finalization path for cold builds and source-only assembly."""
    stage_recipe = module(HERE / 'stage.py', 'furami_finalization_stage')
    sources_recipe = module(HERE / 'sources.py', 'furami_finalization_sources')
    audited = out / 'audit'
    sources = sources_recipe.assemble(provenance, audited_result, out / 'source-assets', caches, acquire_sources)
    release = out / 'release'
    release.mkdir()
    shutil.move(image, release / image.name)
    for name in ('manifest.json', 'audit.json', 'components.json', 'NOTICE.txt'):
        shutil.copy2(audited / name, release / name)
    for path in sorted((out / 'source-assets').iterdir()):
        if path.is_file():
            shutil.move(path, release / path.name)
    shutil.copy2(HERE / 'RELINKING.md', release / 'RELINKING.md')
    reasons = list(sources['missing'])
    try:
        sources_recipe.verify_public_materials(release, sources)
    except (ValueError, KeyError, OSError) as error:
        reasons.append('public distributed materials refused: ' + str(error))
    # All terms below derive from observed files/provenance. No unknown license
    # is assigned MIT, and no source-only inventory claims complete clearance.
    unknown = sorted(identifier for identifier, record in audited_result['component_registry'].items()
                     if not record['terms_resolved'])
    reasons.extend(f'component license terms unresolved: {name}' for name in unknown)
    ready = {'schema': 1, 'ready': not reasons, 'artifact_build_complete': True,
             'image_sha256': stage_recipe.digest(release / image.name), 'technical_audit_sha256': stage_recipe.digest(release / 'audit.json'),
             'sources_index_sha256': stage_recipe.digest(release / 'sources-index.json'),
             'material_policy': sources['material_policy'],
             'reasons': sorted(set(reasons)), 'source_acquisition_requested': acquire_sources,
             'scope': 'source/license completeness gate, not legal opinion or GUI portability certification'}
    sources_recipe.dump(release / 'release-ready.json', ready)
    sums = ''.join(f'{stage_recipe.digest(path)}  {path.name}\n' for path in sorted(release.iterdir()) if path.is_file())
    (release / 'SHA256SUMS').write_text(sums)
    print(json.dumps({'release': str(release), 'image': str(release / image.name), 'release_ready': ready['ready'], 'reason_count': len(ready['reasons'])}))
    return release


def reassembly_pair_paths(parent, output):
    """Validate immutable pair/cache boundaries before creating any output."""
    parent = checked_path(parent, 'cold pair')
    out = checked_path(output, 'source reassembly output', new=True)
    private_cache_outside(parent, (out,))
    for label in ('first', 'second'):
        side = parent / label
        if side.is_symlink() or not side.is_dir():
            raise ValueError('cold pair side must be a regular directory')
        provenance = json.loads((side / 'provenance.json').read_text())
        for key in ('app', 'media', 'runtime', 'stage'):
            if Path(provenance[key]).resolve() != side / key:
                raise ValueError('cold provider is not owned by its parent side: ' + key)
        for value in provenance['source_caches'].values():
            cache = checked_path(Path(value), 'retained source cache')
            private_cache_outside(cache, (parent, out))
    return parent, out


def reassembly_receipt_sha256(side):
    source = module(HERE / 'sources.py', 'furami_reassembly_receipt')
    receipt_path = side / 'source-reassembly.json'
    receipt = json.loads(receipt_path.read_text())
    local = receipt.get('local_assembly_receipt', {})
    if receipt.get('mode') != 'source-reassembly' or local.get('path') != 'source-assets-local-assembly.json':
        raise ValueError('staged assembly lineage malformed')
    path = side / local['path']
    if path.is_symlink() or not path.is_file() or source.sha(path) != local.get('sha256'):
        raise ValueError('local raw-to-derived assembly receipt changed')
    return source.sha(receipt_path)


def reassemble(parent, output, acquire_sources=False):
    """Source-only assembly: preserve the historical cold-build provenance."""
    parent = checked_path(parent, 'cold parent')
    out = checked_path(output, 'source reassembly output', new=True)
    protected = parent.parent if parent.name in ('first', 'second') else parent
    private_cache_outside(protected, (out,))
    source = module(HERE / 'sources.py', 'furami_reassembly_sources')
    app_recipe = module(HERE / 'build-app.py', 'furami_reassembly_app')
    provenance = json.loads((parent / 'provenance.json').read_text())
    app, media, runtime = (Path(provenance[key]) for key in ('app', 'media', 'runtime'))
    for key in ('app', 'media', 'runtime', 'stage'):
        if Path(provenance[key]).resolve() != parent / key:
            raise ValueError('cold provider is not owned by its parent side: ' + key)
    caches = {key: checked_path(Path(value), 'retained source cache')
              for key, value in provenance['source_caches'].items()}
    for cache in caches.values():
        private_cache_outside(cache, (protected, out))
    app_manifest = json.loads((app / 'manifest.json').read_text())
    for relative, expected in app_manifest['inputs'].items():
        if Path(relative).is_absolute() or '..' in Path(relative).parts:
            raise ValueError('unsafe original application input')
        for path in (app / 'source-snapshot' / relative, REPO / relative):
            if path.is_symlink() or not path.is_file() or source.sha(path) != expected:
                raise ValueError('application input changed since cold build: ' + relative)
    if app_recipe.input_hashes() != app_manifest['inputs']:
        raise ValueError('complete application input set changed since cold build')
    app_recipe.verify_media(media, json.loads((REPO / 'packaging/media/stack.lock.json').read_text()))
    runtime_manifest = json.loads((runtime / 'manifest.json').read_text())
    if runtime_manifest.get('lock_sha256') != source.sha(REPO / 'packaging/appimage-runtime/runtime.lock.json'):
        raise ValueError('retained runtime lock mismatch')
    for relative, expected in runtime_manifest['files_sha256'].items():
        path = runtime / relative
        if (Path(relative).is_absolute() or '..' in Path(relative).parts
                or path.is_symlink() or not path.is_file() or source.sha(path) != expected):
            raise ValueError('retained runtime artifact mismatch: ' + relative)
    old_release = parent / 'release'
    version = provenance['version']
    if (not isinstance(version, str) or not version or '/' in version or '\\' in version
            or any(ord(char) < 32 or ord(char) == 127 for char in version)):
        raise ValueError('retained version must be a safe filename component')
    images = list(old_release.glob('*.AppImage'))
    if len(images) != 1:
        raise ValueError('retained release must contain exactly one immediate AppImage')
    original_image = images[0]
    if (original_image.is_symlink() or not original_image.is_file()
            or not original_image.resolve().is_relative_to(old_release.resolve())
            or original_image.stat().st_size == 0):
        raise ValueError('retained AppImage must be regular, nonempty and confined')
    ready = json.loads((old_release / 'release-ready.json').read_text())
    audit = json.loads((parent / 'audit/audit.json').read_text())
    if (source.sha(original_image) != ready['image_sha256']
            or source.sha(parent / 'audit/audit.json') != ready['technical_audit_sha256']
            or source.sha(old_release / 'sources-index.json') != ready['sources_index_sha256']):
        raise ValueError('stale original image/audit/source bindings')
    old_inventory = source.source_inventory(old_release)
    old_index = json.loads((old_release / 'sources-index.json').read_text())
    if {row['path']: row['sha256'] for row in old_index['archives']} != {
            path.name: source.sha(path) for path in old_release.glob('source-materials-*.tar')}:
        raise ValueError('retained original source archive changed')
    for name in source.LOADER_REPORTS:
        raw = media / name
        if (raw.is_symlink() or not raw.is_file()
                or old_inventory.get('media-evidence/' + name, {}).get('sha256') != source.sha(raw)):
            raise ValueError('retained raw loader evidence changed: ' + name)
    for name, row in old_inventory.items():
        if name.startswith('media-evidence/'):
            path = media / name.removeprefix('media-evidence/')
        elif name.startswith('runtime-evidence/'):
            path = runtime / name.removeprefix('runtime-evidence/')
        else:
            continue
        if path.is_symlink() or not path.is_file() or source.sha(path) != row['sha256']:
            raise ValueError('retained provider evidence changed: ' + name)
    stage = Path(provenance['stage'])
    stage_materials = {'ubuntu-evidence/ubuntu-sources.json': stage / 'ubuntu-sources.json'}
    for path in (stage / 'ubuntu-source-evidence').iterdir():
        if path.is_symlink() or not path.is_file():
            raise ValueError('retained Ubuntu evidence must be regular')
        stage_materials['ubuntu-evidence/' + path.name] = path
    expected_stage = {name for name in old_inventory if name.startswith('ubuntu-evidence/')}
    if set(stage_materials) != expected_stage:
        raise ValueError('retained Ubuntu evidence set changed')
    for name, path in stage_materials.items():
        if path.is_symlink() or not path.is_file() or source.sha(path) != old_inventory[name]['sha256']:
            raise ValueError('retained Ubuntu evidence changed: ' + name)
    static = audit['static_libplacebo']
    for path, expected in (
            (media / 'work/placebo-build/build.ninja', static['build_ninja_sha256']),
            (media / 'prefix/lib/pkgconfig/libplacebo.pc', static['pkg_config_sha256'])):
        if path.is_symlink() or not path.is_file() or source.sha(path) != expected:
            raise ValueError('retained static link evidence changed')
    for relative, expected in provenance['recipe_sha256'].items():
        if Path(relative).is_absolute() or '..' in Path(relative).parts:
            raise ValueError('unsafe original recipe path')
        row = old_inventory.get('furami/' + relative)
        if not row or row['sha256'] != expected:
            raise ValueError('original controlling recipe unavailable: ' + relative)
    out.mkdir(parents=True)
    originals = out / 'original-recipes'
    originals.mkdir()
    import tarfile
    for relative in provenance['recipe_sha256']:
        row = old_inventory['furami/' + relative]
        target = originals / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(old_release / row['archive']) as archive:
            with archive.extractfile('furami/' + relative) as stream, target.open('wb') as destination:
                shutil.copyfileobj(stream, destination)
    image = out / f'Furami-v{version}-Linux-x64.AppImage'
    shutil.copy2(original_image, image)
    audit_recipe = module(HERE / 'audit.py', 'furami_reassembly_audit')
    audit = audit_recipe.audit(image, provenance, out / 'audit')
    shutil.copytree(runtime, out / 'runtime', symlinks=True)
    (out / 'app').mkdir()
    shutil.copy2(app / 'manifest.json', out / 'app/manifest.json')
    shutil.copy2(parent / 'provenance.json', out / 'provenance.json')
    private_cache_outside(caches['runtime'], (out,))
    release = finalize(out, {**provenance, 'original_recipes': str(originals)},
                       audit, image, caches, acquire_sources)
    source.dump(out / 'source-reassembly.json', {
        'schema': 1, 'mode': 'source-reassembly', 'parent_cold_build': str(parent),
        'parent_provenance_sha256': source.sha(parent / 'provenance.json'),
        'original_recipe_sha256': provenance['recipe_sha256'],
        'original_image_sha256': source.sha(original_image),
        'original_audit_sha256': source.sha(parent / 'audit/audit.json'),
        'original_readiness_sha256': source.sha(parent / 'release/release-ready.json'),
        'original_provider_manifest_sha256': {key: source.sha(Path(provenance[key]) / 'manifest.json')
                                             for key in ('app', 'media', 'runtime')},
        'original_source_archives_sha256': {path.name: source.sha(path) for path in old_release.glob('source-materials-*.tar')},
        'assembly_recipe_sha256': {
            str(path.relative_to(REPO)): source.sha(path)
            for folder in ('appimage', 'appimage-runtime', 'media')
            for path in sorted((REPO / 'packaging' / folder).iterdir())
            if path.is_file() and not path.is_symlink() and path.suffix in ('.py', '.json', '.sh')},
        'local_assembly_receipt': {
            'path': 'source-assets-local-assembly.json',
            'sha256': source.sha(out / 'source-assets-local-assembly.json')},
        'result_sha256': {path.name: source.sha(path) for path in release.iterdir() if path.is_file()},
        'scope': 'original cold binaries plus current source assembly; not updated full cold recipe execution'})
    return release


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--version')
    parser.add_argument('--media-output', type=Path)
    parser.add_argument('--runtime-build', type=Path)
    parser.add_argument('--source-cache', type=Path)
    parser.add_argument('--media-source-cache', type=Path)
    parser.add_argument('--runtime-source-cache', type=Path)
    parser.add_argument('--compliance-cache', type=Path)
    parser.add_argument('--tool-cache', type=Path)
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--acquire-sources', action='store_true', help='fetch complete checked source/build inputs; omit for resource-bounded local smoke')
    parser.add_argument('--reassemble-from', type=Path)
    args = parser.parse_args()
    try:
        if args.reassemble_from:
            reassemble(args.reassemble_from, args.output, args.acquire_sources)
        else:
            build(args)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit('AppImage build failed: ' + str(error))
