#!/usr/bin/env python3
"""Build one frozen x86_64 AppImage and emit independently audited release assets."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tomllib

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]


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
                  'version': version, 'source_caches': {key: str(value) for key, value in caches.items()}}
    sources_recipe.dump(out / 'provenance.json', provenance)
    subprocess.run([sys.executable, str(HERE / 'stage.py'), '--media-output', str(media), '--app-build', str(app),
                    '--runtime-build', str(runtime), '--media-source-cache', str(caches['media']), '--output', str(stage),
                    '--tool-cache', str(caches['tool']), '--version', version], check=True)
    image = stage / f'Furami-{version}-x86_64.AppImage'
    audited = out / 'audit'
    audited_result = audit_recipe.audit(image, provenance, audited)
    sources = sources_recipe.assemble(provenance, audited_result, out / 'source-assets', caches, args.acquire_sources)
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
    # All terms below derive from observed files/provenance. No unknown license
    # is assigned MIT, and no source-only inventory claims complete clearance.
    unknown = sorted(identifier for identifier, record in audited_result['component_registry'].items()
                     if not record['terms_resolved'])
    reasons.extend(f'component license terms unresolved: {name}' for name in unknown)
    ready = {'schema': 1, 'ready': not reasons, 'artifact_build_complete': True,
             'image_sha256': stage_recipe.digest(release / image.name), 'technical_audit_sha256': stage_recipe.digest(release / 'audit.json'),
             'sources_index_sha256': stage_recipe.digest(release / 'sources-index.json'),
             'reasons': sorted(set(reasons)), 'source_acquisition_requested': args.acquire_sources,
             'scope': 'source/license completeness gate, not legal opinion or GUI portability certification'}
    sources_recipe.dump(release / 'release-ready.json', ready)
    sums = ''.join(f'{stage_recipe.digest(path)}  {path.name}\n' for path in sorted(release.iterdir()) if path.is_file())
    (release / 'SHA256SUMS').write_text(sums)
    print(json.dumps({'release': str(release), 'image': str(release / image.name), 'release_ready': ready['ready'], 'reason_count': len(ready['reasons'])}))
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
    args = parser.parse_args()
    try:
        build(args)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit('AppImage build failed: ' + str(error))
