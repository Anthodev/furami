#!/usr/bin/env python3
"""CI-only independent cold builds; only proven metadata variance is accepted."""
import argparse
import importlib.util
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('furami_single_image', HERE / 'build.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)
sources_recipe = builder.module(HERE / 'sources.py', 'furami_compare_sources')




def equal_except_metadata(left, right, ranges):
    if left.stat().st_size != right.stat().st_size:
        return False
    with left.open('rb') as a, right.open('rb') as b:
        offset = 0
        for start, length in sorted(ranges):
            while offset < start:
                size = min(1024 * 1024, start - offset)
                if a.read(size) != b.read(size):
                    return False
                offset += size
            a.seek(length, 1)
            b.seek(length, 1)
            offset = start + length
        while True:
            first, second = a.read(1024 * 1024), b.read(1024 * 1024)
            if first != second:
                return False
            if not first:
                return True


def accepted_criteria(criteria: dict) -> bool:
    required = ('image_bytes', 'payload_files_modes_symlinks', 'source_runtime_bytes',
                'source_assets_content', 'source_archive_bytes_except_proven_metadata',
                'generated_CPP_headers')
    if not isinstance(criteria, dict) or any(type(criteria.get(key)) is not bool for key in required):
        return False
    if 'image_known_metadata_only' in criteria and type(criteria['image_known_metadata_only']) is not bool:
        return False
    return (all(criteria[key] is True for key in required[1:])
            and (criteria['image_bytes'] is True or criteria.get('image_known_metadata_only') is True))


def compare(args):
    out = builder.checked_path(args.output, 'comparison output', new=True)
    # Only checked download inputs shared, never media/runtime/app binaries.
    cache = builder.checked_path(args.source_cache or Path.home() / '.cache/furami/appimage-inputs', 'source cache')
    runtime_cache = builder.checked_path(args.runtime_source_cache or Path.home() / '.cache/furami/appimage-runtime', 'runtime source cache')
    builder.private_cache_outside(runtime_cache, (out,))
    if getattr(args, 'reassemble_from', None):
        args.reassemble_from, out = builder.reassembly_pair_paths(args.reassemble_from, out)
        for selected_cache in (cache, runtime_cache):
            builder.private_cache_outside(selected_cache, (args.reassemble_from, out))
    out.mkdir(parents=True)
    for label in ('first', 'second'):
        if getattr(args, 'reassemble_from', None):
            builder.reassemble(args.reassemble_from / label, out / label, args.acquire_sources)
        else:
            command = [sys.executable, str(HERE / 'build.py'), '--output', str(out / label),
                       '--source-cache', str(cache), '--jobs', str(args.jobs)]
            command += ['--runtime-source-cache', str(runtime_cache)]
            if args.version:
                command += ['--version', args.version]
            if args.acquire_sources:
                command += ['--acquire-sources']
            subprocess.run(command, check=True)
    verdict = compare_outputs(out)
    if getattr(args, 'reassemble_from', None):
        verdict['evidence_origin'] = {
            'mode': 'source-reassembly',
            'scope': 'historical independent cold binaries plus new independent source assemblies; updated full cold release recipe not executed',
            'parent': str(args.reassemble_from),
            'assembly_receipt_sha256': {label: builder.reassembly_receipt_sha256(out / label)
                                       for label in ('first', 'second')}}
    (out / 'reproducibility.json').write_text(json.dumps(verdict, sort_keys=True, indent=2) + '\n')
    if not accepted_criteria(verdict['criteria']):
        raise ValueError('independent build mismatch; see ' + str(out / 'reproducibility.json'))
    shutil.copytree(out / 'second/release', out / 'release')
    shutil.copy2(out / 'reproducibility.json', out / 'release/reproducibility.json')
    sums = ''
    for path in sorted((out / 'release').iterdir()):
        if path.name != 'SHA256SUMS':
            with path.open('rb') as stream:
                sums += f'{hashlib.file_digest(stream, "sha256").hexdigest()}  {path.name}\n'
    (out / 'release/SHA256SUMS').write_text(sums)
    print(json.dumps(verdict))


def compare_outputs(out: Path) -> dict:
    audit_recipe = builder.module(HERE / 'audit.py', 'furami_compare_audit')
    first, second = out / 'first/release', out / 'second/release'
    left = next(first.glob('*.AppImage'))
    right = next(second.glob('*.AppImage'))
    digest = audit_recipe.stage.digest
    a, b = json.loads((first / 'manifest.json').read_text()), json.loads((second / 'manifest.json').read_text())
    raw_source_hashes = [{p.name: digest(p) for p in root.glob('source-materials-*.tar')}
                        for root in (first, second)]
    source_a, source_b = sources_recipe.source_inventory(first), sources_recipe.source_inventory(second)
    metadata_variance = {}
    permitted_source_ranges = {}
    runtime_comparator = builder.module(HERE.parent / 'appimage-runtime/build-twice.py', 'furami_runtime_comparator')
    differences = runtime_comparator.compare(out / 'first/runtime', out / 'second/runtime')
    material, accepted = runtime_comparator.classify_variance(out / 'first/runtime', out / 'second/runtime', differences)
    if accepted and not material:
        for relative, provider_name in (('runtime-evidence/link.map', 'link.map'), ('runtime-evidence/manifest.json', 'manifest.json')):
            for index, rows in enumerate((source_a, source_b)):
                provider = out / ('first' if index == 0 else 'second') / 'runtime' / provider_name
                if relative not in rows or rows[relative]['sha256'] != digest(provider):
                    raise ValueError('source evidence is not exact runtime provider material: ' + relative)
                rows[relative]['sha256'] = '<proven runtime metadata variance>'
                if index == 0:
                    row = rows[relative]
                    permitted_source_ranges.setdefault(row['archive'], []).append((row['data_offset'], row['size']))
            metadata_variance[relative] = accepted[provider_name]
    criteria = {
        'image_bytes': digest(left) == digest(right),
        'payload_files_modes_symlinks': a['payload'] == b['payload'],
        'source_runtime_bytes': not material and digest(out / 'first/runtime/runtime-x86_64') == digest(out / 'second/runtime/runtime-x86_64'),
        'source_assets_content': source_a == source_b,
        'source_archive_bytes_except_proven_metadata': set(raw_source_hashes[0]) == set(raw_source_hashes[1]) and all(
            equal_except_metadata(first / name, second / name, permitted_source_ranges.get(name, []))
            for name in raw_source_hashes[0]),
    }
    # appimagetool writes one known ELF metadata field, never instructions.
    # Preserve raw hashes and identify exact byte range. This is not byte identity.
    if not criteria['image_bytes'] and criteria['source_runtime_bytes']:
        first_bytes, second_bytes = left.read_bytes(), right.read_bytes()
        offset, _ = audit_recipe.stage.runtime_md5_field((out / 'first/runtime/runtime-x86_64').read_bytes())
        if (len(first_bytes) == len(second_bytes) and first_bytes[:offset] == second_bytes[:offset]
                and first_bytes[offset + 16:] == second_bytes[offset + 16:]):
            criteria['image_known_metadata_only'] = True
            metadata_variance['image:.digest_md5'] = {
                'offset': offset, 'length': 16, 'first_hex': first_bytes[offset:offset + 16].hex(),
                'second_hex': second_bytes[offset:offset + 16].hex(),
                'cause': 'pinned appimagetool documented metadata write; digest semantics unverified'}
        else:
            criteria['image_known_metadata_only'] = False
    generated = []
    for label in ('first', 'second'):
        app = json.loads((out / label / 'app/manifest.json').read_text())
        generated.append(app['builds']['generated_cpp_first'])
    criteria['generated_CPP_headers'] = generated[0] == generated[1]
    passed = accepted_criteria(criteria)
    identical = passed and criteria['image_bytes'] and raw_source_hashes[0] == raw_source_hashes[1]
    verdict = {'schema': 1, 'status': 'identical' if identical else 'documented_metadata_variance' if passed else 'mismatch',
               'byte_identity': identical, 'criteria': criteria, 'metadata_variance': metadata_variance,
               'raw_source_archive_sha256': raw_source_hashes,
               'first_image_sha256': digest(left), 'second_image_sha256': digest(right),
               'candidate': 'second/release',
               'variance_policy': 'only exact inherited runtime metadata proof and pinned digest field; instructions/payload never normalized'}
    return verdict


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--version')
    parser.add_argument('--source-cache', type=Path)
    parser.add_argument('--runtime-source-cache', type=Path)
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--acquire-sources', action='store_true')
    parser.add_argument('--reassemble-from', type=Path)
    args = parser.parse_args()
    try:
        compare(args)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit('reproducibility failed: ' + str(error))
