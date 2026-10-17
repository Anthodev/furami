import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tarfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import publish_candidate as publisher


POLICY = {'schema': 1, 'alpine_bootstrap': 'private', 'public_offline_bootstrap_complete': False}
SUPPORT_SPEC = importlib.util.spec_from_file_location('furami_public_material_fixtures',
    publisher.REPO / 'packaging/appimage/test_sources.py')
support = importlib.util.module_from_spec(SUPPORT_SPEC)
SUPPORT_SPEC.loader.exec_module(support)
ORIGINAL_REPO = publisher.REPO


def source_members():
    root = publisher.REPO / 'public-fixture'
    return {path.relative_to(root).as_posix(): path.read_bytes()
            for path in root.rglob('*') if path.is_file()}


def write_materials(root, members):
    archive = root / 'source-materials-001.tar'
    with tarfile.open(archive, 'w') as tar:
        for name, content in members.items():
            entry = tarfile.TarInfo(name)
            entry.size = len(content)
            tar.addfile(entry, io.BytesIO(content))
    index = {'schema': 1, 'complete': True, 'missing': [], 'material_policy': dict(POLICY),
             'files': [{'path': name, 'size': len(content), 'sha256': hashlib.sha256(content).hexdigest()}
                       for name, content in members.items()],
             'archives': [{'path': archive.name, 'sha256': publisher.digest(archive)}]}
    (root / 'sources-index.json').write_text(json.dumps(index))
    return index


def fixture(root, variance=False, *, version='0.1.0-rc.1'):
    root.mkdir()
    image_name = f'Furami-v{version}-Linux-x64.AppImage'
    for name, content in {image_name: b'image', 'audit.json': b'{}',
                          'NOTICE.txt': b'notices', 'RELINKING.md': b'relink instructions'}.items():
        (root / name).write_bytes(content)
    (root / 'manifest.json').write_text(json.dumps(
        {'schema': 1, 'image': {'path': image_name, 'sha256': hashlib.sha256(b'image').hexdigest()}}))
    (root / 'components.json').write_text(json.dumps({'schema': 1, 'files': {}, 'components': {}}))
    write_materials(root, source_members())
    hashes = {p.name: publisher.digest(p) for p in root.iterdir()}
    ready = {'ready': True, 'image_sha256': hashes[f'Furami-v{version}-Linux-x64.AppImage'],
             'technical_audit_sha256': hashes['audit.json'], 'sources_index_sha256': hashes['sources-index.json'],
             'material_policy': dict(POLICY)}
    (root / 'release-ready.json').write_text(json.dumps(ready))
    criteria = dict.fromkeys(('image_bytes', 'payload_files_modes_symlinks', 'source_runtime_bytes',
                             'source_assets_content', 'source_archive_bytes_except_proven_metadata', 'generated_CPP_headers'), True)
    if variance:
        criteria.update(image_bytes=False, image_known_metadata_only=True)
    sources = {'source-materials-001.tar': hashes['source-materials-001.tar']}
    receipt = {'status': 'documented_metadata_variance' if variance else 'identical',
               'byte_identity': not variance, 'criteria': criteria,
               'first_image_sha256': '0' * 64 if variance else ready['image_sha256'],
               'second_image_sha256': ready['image_sha256'],
               'raw_source_archive_sha256': [sources, sources],
               'metadata_variance': image_variance() if variance else {}}
    (root / 'reproducibility.json').write_text(json.dumps(receipt))
    sums(root)


def image_variance():
    return {'image:.digest_md5': {'offset': 100, 'length': 16, 'first_hex': '0' * 32,
                                  'second_hex': '1' * 32, 'cause': 'pinned digest metadata write'}}


def sums(root):
    (root / 'SHA256SUMS').write_text(''.join(f'{publisher.digest(p)}  {p.name}\n' for p in sorted(root.iterdir()) if p.name != 'SHA256SUMS'))


def public_asset_names(version):
    return {f'Furami-v{version}-Linux-x64.AppImage',
            f'Furami-v{version}-Linux-x64-materials.tar', 'manifest.json', 'SHA256SUMS'}


def retained_material_names():
    return {'SHA256SUMS', 'sources-index.json', 'release-ready.json', 'reproducibility.json',
            'audit.json', 'components.json', 'NOTICE.txt', 'RELINKING.md'}


def read_public_sums(directory):
    return {name: value for value, name in
            (line.split('  ', 1) for line in (directory / 'SHA256SUMS').read_text().splitlines())}


def inspect_materials_tar(bundle):
    seen = {}
    with tarfile.open(bundle, 'r:') as archive:
        for member in archive:
            assert member.isfile() and not member.issym() and not member.islnk(), member.name
            assert member.name == Path(member.name).name, member.name
            assert member.name not in seen, member.name
            assert (member.uid, member.gid, member.uname, member.gname) == (0, 0, '', ''), member.name
            assert member.mode == 0o644, member.mode
            assert member.mtime == 0, member.mtime
            assert member.pax_headers == {}, member.pax_headers
            with archive.extractfile(member) as stream:
                seen[member.name] = stream.read()
        support.sources.verify_tar_footer(archive, bundle.name)
    return seen


class CandidateTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        repo = root / 'recipe'
        shutil.copytree(ORIGINAL_REPO / 'packaging', repo / 'packaging',
                        ignore=shutil.ignore_patterns('__pycache__'))
        shutil.copytree(ORIGINAL_REPO / 'scripts', repo / 'scripts',
                        ignore=shutil.ignore_patterns('__pycache__'))
        (repo / 'Cargo.toml').write_text('[package]\nname="furami"\nversion="0.1.0-rc.1"\n')
        (repo / 'Cargo.lock').write_text('[[package]]\nname="furami"\nversion="0.1.0-rc.1"\n')
        _, _, members = support.runtime_source_fixture(repo, root / 'private-cache')
        for name, data in members.items():
            path = repo / 'public-fixture' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        shutil.rmtree(root / 'private-cache')
        patcher = patch.object(publisher, 'REPO', repo)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_both_successful_receipt_statuses(self):
        for variance in (False, True):
            with self.subTest(variance=variance), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'candidate'
                fixture(root, variance)
                self.assertEqual(publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=True), sorted(root.iterdir()))

    def test_policy_derives_channel_from_exact_push_tag(self):
        self.assertEqual(publisher.release_channel('v0.1.0', '0.1.0', 'push'), 'stable')
        self.assertEqual(publisher.release_channel('v0.1.0-rc.1', '0.1.0-rc.1', 'push'), 'prerelease')
        for tag, version, event in [
                ('v0.1.0-rc.01', '0.1.0-rc.01', 'push'),
                ('v0.1.0-rc.0', '0.1.0-rc.0', 'push'),
                ('v01.1.0', '01.1.0', 'push'), ('v0.1.0-beta.1', '0.1.0-beta.1', 'push'),
                ('v0.1.0+build', '0.1.0+build', 'push'),
                ('v0.1.0-rc.1\n', '0.1.0-rc.1', 'push'),
                ('v0.1.0', '0.1.0', 'workflow_dispatch'),
                ('v0.1.0-rc.1', '0.1.0-rc.1', 'workflow_dispatch'),
                ('v0.1.0-rc.2', '0.1.0-rc.1', 'push'), ('v0.1.0', '0.1.0-rc.1', 'push')]:
            with self.subTest(tag=tag, event=event), self.assertRaises(publisher.CandidatePublicationError):
                publisher.release_channel(tag, version, event)

    def configure_version(self, version, *, lock_version=None, duplicate=False):
        (publisher.REPO / 'Cargo.toml').write_text(f'[package]\nname="furami"\nversion="{version}"\n')
        entry = f'[[package]]\nname="furami"\nversion="{lock_version or version}"\n'
        (publisher.REPO / 'Cargo.lock').write_text(entry * (2 if duplicate else 1))

    def fake_gh(self, root):
        binary = root / 'bin'
        binary.mkdir()
        capture = root / 'gh-calls.jsonl'
        assets = root / 'gh-assets'
        assets.mkdir()
        temporary = root / 'tmp'
        temporary.mkdir()
        gh = binary / 'gh'
        gh.write_text(
            f'#!{sys.executable}\n'
            'import json, os, pathlib, shutil, sys\n'
            'args = sys.argv[1:]\n'
            'notes_hex = pathlib.Path(args[args.index("--notes-file") + 1]).read_bytes().hex()\n'
            'tail = args[args.index("--verify-tag") + 1:]\n'
            'target_dir = pathlib.Path(os.environ["FAKE_GH_ASSETS"])\n'
            'for value in (entry for entry in tail if not entry.startswith("--")):\n'
            '    source = pathlib.Path(value)\n'
            '    target = target_dir / source.name\n'
            '    if target.exists():\n'
            '        raise SystemExit("duplicate upload basename: " + source.name)\n'
            '    shutil.copyfile(source, target)\n'
            'with open(os.environ["FAKE_GH_CAPTURE"], "a") as stream:\n'
            '    stream.write(json.dumps({"argv": args, "notes_hex": notes_hex}) + "\\n")\n'
            'sys.exit(int(os.environ.get("FAKE_GH_EXIT", "0")))\n')
        gh.chmod(0o755)
        env = {key: value for key, value in os.environ.items() if key not in ('GH_TOKEN', 'GITHUB_TOKEN')}
        env.update(PATH=str(binary), FAKE_GH_CAPTURE=str(capture), FAKE_GH_ASSETS=str(assets),
                   TMPDIR=str(temporary))
        return env, capture

    def run_cli(self, root, notes, env, *, tag, event='push', extra=()):
        return subprocess.run(
            [sys.executable, str(publisher.REPO / 'scripts/publish_candidate.py'),
             '--event-name', event, '--tag', tag, '--repository', 'owner/repo',
             '--notes', str(notes), '--candidate', str(root), *extra],
            cwd=publisher.REPO, env=env, capture_output=True, text=True)

    def test_publication_cli_refuses_invalid_assets_and_policy_before_gh(self):
        mutations = ('manual', 'wrong-tag', 'malformed-tag', 'lock', 'duplicate-lock',
                    'notes-missing', 'notes-empty', 'old-only', 'mixed-names', 'no-v', 'empty-image',
                     'image', 'extra', 'symlink', 'directory', 'false', 'integer',
                     'readiness-hash', 'receipt-image', 'receipt-source', 'missing-receipt',
                     'receipt-status', 'receipt-criteria', 'checksum-path', 'checksum-duplicate',
                     'checksum-coverage', 'public-source')
        for version in ('0.1.0', '0.1.0-rc.1'):
            for mutation in mutations:
                with self.subTest(version=version, mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                    parent = Path(temporary)
                    self.configure_version(version)
                    root = parent / 'candidate'
                    fixture(root, version=version)
                    notes = parent / 'notes'
                    notes.write_text('Valid notes\n')
                    env, capture = self.fake_gh(parent)
                    image = root / f'Furami-v{version}-Linux-x64.AppImage'
                    tag, event = 'v' + version, 'push'
                    if mutation == 'manual':
                        event = 'workflow_dispatch'
                    elif mutation == 'wrong-tag':
                        tag = 'v9.9.9'
                    elif mutation == 'malformed-tag':
                        tag += '+unsupported'
                    elif mutation == 'lock':
                        self.configure_version(version, lock_version='9.9.9')
                    elif mutation == 'duplicate-lock':
                        self.configure_version(version, duplicate=True)
                    elif mutation == 'notes-missing':
                        notes.unlink()
                    elif mutation == 'notes-empty':
                        notes.write_text(' \n')
                    elif mutation in ('old-only', 'mixed-names'):
                        old = root / f'Furami-{version}-x86_64.AppImage'
                        if mutation == 'old-only':
                            image.rename(old)
                        else:
                            shutil.copy2(image, old)
                        sums(root)
                    elif mutation == 'no-v':
                        image.rename(root / f'Furami-{version}-Linux-x64.AppImage')
                        sums(root)
                    elif mutation == 'empty-image':
                        image.write_bytes(b'')
                        sums(root)
                    elif mutation == 'image':
                        image.write_bytes(b'changed')
                    elif mutation == 'extra':
                        (root / 'uncovered').write_bytes(b'x')
                    elif mutation == 'symlink':
                        (root / 'link').symlink_to('audit.json')
                    elif mutation == 'directory':
                        (root / 'directory').mkdir()
                    elif mutation == 'missing-receipt':
                        (root / 'reproducibility.json').unlink()
                        sums(root)
                    elif mutation.startswith('checksum'):
                        path = root / 'SHA256SUMS'
                        lines = path.read_text().splitlines(keepends=True)
                        if mutation == 'checksum-coverage':
                            path.write_text(''.join(lines[1:]))
                        else:
                            with path.open('a') as stream:
                                stream.write('0' * 64 + '  ../escape\n' if mutation == 'checksum-path' else lines[0])
                    elif mutation == 'public-source':
                        members = source_members()
                        members['runtime-inputs/private.apk'] = b'private APK'
                        index = write_materials(root, members)
                        ready_path = root / 'release-ready.json'
                        ready = json.loads(ready_path.read_text())
                        ready['sources_index_sha256'] = publisher.digest(root / 'sources-index.json')
                        ready_path.write_text(json.dumps(ready))
                        receipt_path = root / 'reproducibility.json'
                        receipt = json.loads(receipt_path.read_text())
                        receipt['raw_source_archive_sha256'] = [
                            {entry['path']: entry['sha256'] for entry in index['archives']}] * 2
                        receipt_path.write_text(json.dumps(receipt))
                        sums(root)
                    else:
                        name = 'reproducibility.json' if mutation.startswith('receipt') else 'release-ready.json'
                        value = json.loads((root / name).read_text())
                        if mutation in ('false', 'integer'):
                            value['ready'] = False if mutation == 'false' else 1
                        elif mutation == 'readiness-hash':
                            value['technical_audit_sha256'] = '0' * 64
                        elif mutation == 'receipt-image':
                            value['second_image_sha256'] = '0' * 64
                        elif mutation == 'receipt-source':
                            value['raw_source_archive_sha256'][1]['source-materials-001.tar'] = '0' * 64
                        elif mutation == 'receipt-status':
                            value['status'] = 'mismatch'
                        else:
                            value['criteria']['generated_CPP_headers'] = False
                        (root / name).write_text(json.dumps(value))
                        sums(root)
                    result = self.run_cli(root, notes, env, tag=tag, event=event)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertIn('candidate refused:', result.stderr)
                    if mutation in ('old-only', 'mixed-names', 'no-v'):
                        self.assertIn('unexpected public candidate asset', result.stderr)
                    self.assertFalse(capture.exists(), result.stderr)

    def test_public_scope_refuses_private_bytes_and_missing_actual_linked_sources(self):
        for mutation in ('apk', 'renamed-apk', 'unknown-apk', 'base-image', 'base-layer', 'flat-apk',
                         'raw-config', 'raw-layer', 'missing-index', 'missing-source', 'undeclared-extra', 'wrong-hash'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'candidate'
                fixture(root)
                (root / 'reproducibility.json').unlink()
                members = source_members()
                if mutation in ('apk', 'renamed-apk'):
                    members['runtime-inputs/' + ('tool.apk' if mutation == 'apk' else 'innocent.txt')] = b'private APK'
                elif mutation in ('unknown-apk', 'base-image', 'base-layer'):
                    blob = io.BytesIO()
                    with tarfile.open(fileobj=blob, mode='w:gz' if mutation == 'unknown-apk' else 'w') as tar:
                        directory = tarfile.TarInfo('./')
                        directory.type = tarfile.DIRTYPE
                        tar.addfile(directory)
                        marker = '.PKGINFO' if mutation == 'unknown-apk' else (
                            'lib/apk/db/installed' if mutation == 'base-layer' else 'oci-layout')
                        entry = tarfile.TarInfo(marker)
                        data = b'private bootstrap metadata'
                        entry.size = len(data)
                        tar.addfile(entry, io.BytesIO(data))
                    members['runtime-inputs/innocent.data'] = blob.getvalue()
                elif mutation in ('raw-config', 'raw-layer'):
                    members['runtime-inputs/innocent.json'] = (
                        b'private base config' if mutation == 'raw-config' else b'private base layer')
                elif mutation == 'missing-source':
                    members.pop(next(name for name in members if name.startswith('runtime-inputs/alpine-sources/zlib/')
                                     and name.endswith('.tar.gz')))
                elif mutation == 'missing-index':
                    members.pop('runtime-inputs/indexes/main/x86_64/APKINDEX.tar.gz')
                index = write_materials(root, members)
                if mutation == 'flat-apk':
                    (root / 'NOTICE.txt').write_bytes(b'private APK')
                if mutation == 'undeclared-extra':
                    index['files'].pop()
                elif mutation == 'wrong-hash':
                    index['files'][-1]['sha256'] = '0' * 64
                (root / 'sources-index.json').write_text(json.dumps(index))
                ready = json.loads((root / 'release-ready.json').read_text())
                ready['sources_index_sha256'] = publisher.digest(root / 'sources-index.json')
                (root / 'release-ready.json').write_text(json.dumps(ready))
                sums(root)
                with self.assertRaises(publisher.CandidatePublicationError):
                    publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=False)

    def test_canonical_locks_cannot_be_replaced_by_self_consistent_weaker_materials(self):
        for mutation in ('removed-linked', 'empty-linked', 'changed-binary', 'policy-lock'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'candidate'
                fixture(root)
                (root / 'reproducibility.json').unlink()
                members = source_members()
                prefix = 'furami/packaging/appimage-runtime/'
                alpine = json.loads(members[prefix + 'alpine-source.lock.json'])
                runtime = json.loads(members[prefix + 'runtime.lock.json'])
                if mutation in ('removed-linked', 'empty-linked'):
                    removed = ['zlib'] if mutation == 'removed-linked' else list(alpine['linked_sources'])
                    for name in removed:
                        alpine['linked_sources'].pop(name)
                        members = {path: data for path, data in members.items()
                                   if not path.startswith('runtime-inputs/alpine-sources/' + name + '/')}
                    members[prefix + 'alpine-source.lock.json'] = json.dumps(alpine).encode()
                    runtime['linked_source_closure']['sha256'] = hashlib.sha256(
                        members[prefix + 'alpine-source.lock.json']).hexdigest()
                    members[prefix + 'runtime.lock.json'] = json.dumps(runtime).encode()
                elif mutation == 'changed-binary':
                    runtime['apk_packages']['zlib-static']['version'] = '1.3.2-r0'
                    runtime['apk_packages']['zlib-static']['archive'] = 'zlib-static-1.3.2-r0.apk'
                    members[prefix + 'runtime.lock.json'] = json.dumps(runtime).encode()
                else:
                    policy = 'furami/packaging/appimage/sources.lock.json'
                    value = json.loads(members[policy])
                    value['alpine_private_bootstrap']['layers_sha256'] = []
                    members[policy] = json.dumps(value).encode()
                write_materials(root, members)
                ready = json.loads((root / 'release-ready.json').read_text())
                ready['sources_index_sha256'] = publisher.digest(root / 'sources-index.json')
                (root / 'release-ready.json').write_text(json.dumps(ready))
                sums(root)
                with self.assertRaisesRegex(publisher.CandidatePublicationError, 'canonical'):
                    publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=False)

    def test_canonical_recipe_still_requires_valid_schema_package_set_and_source_pairs(self):
        paths = [publisher.REPO / 'packaging/appimage-runtime' / name
                 for name in ('runtime.lock.json', 'alpine-source.lock.json')]
        originals = {path: path.read_bytes() for path in paths}
        for mutation in ('schema', 'package-set', 'linked-set', 'source-version'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'candidate'
                fixture(root)
                (root / 'reproducibility.json').unlink()
                members = source_members()
                runtime, alpine = (json.loads(originals[path]) for path in paths)
                if mutation == 'schema':
                    runtime['schema'] = True
                elif mutation == 'package-set':
                    runtime['apk_packages'].pop('abuild')
                elif mutation == 'linked-set':
                    alpine['linked_sources'].pop('zlib')
                else:
                    alpine['linked_sources']['zlib']['version'] = '1.3.2-r0'
                alpine_bytes = json.dumps(alpine).encode()
                runtime['linked_source_closure']['sha256'] = hashlib.sha256(alpine_bytes).hexdigest()
                runtime_bytes = json.dumps(runtime).encode()
                for path, data in zip(paths, (runtime_bytes, alpine_bytes)):
                    path.write_bytes(data)
                    members['furami/packaging/appimage-runtime/' + path.name] = data
                try:
                    write_materials(root, members)
                    ready = json.loads((root / 'release-ready.json').read_text())
                    ready['sources_index_sha256'] = publisher.digest(root / 'sources-index.json')
                    (root / 'release-ready.json').write_text(json.dumps(ready))
                    sums(root)
                    with self.assertRaises(publisher.CandidatePublicationError):
                        publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=False)
                finally:
                    for path, data in originals.items():
                        path.write_bytes(data)

    def test_actual_archive_trailing_payload_refuses_but_zero_padding_is_allowed(self):
        for mutation in ('outer-apk', 'outer-tar', 'nested-apk', 'nested-gzip', 'nested-xz', 'nested-bz2', 'zero-padding'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'candidate'
                fixture(root)
                (root / 'reproducibility.json').unlink()
                members = source_members()
                benign = io.BytesIO()
                with tarfile.open(fileobj=benign, mode='w') as tar:
                    member = tarfile.TarInfo('ordinary-source.txt')
                    member.size = 1
                    tar.addfile(member, io.BytesIO(b'x'))
                if mutation.startswith('nested'):
                    payload = benign.getvalue() + b'private APK'
                    if mutation == 'nested-gzip':
                        payload = support.gzip.compress(benign.getvalue(), mtime=0) + support.gzip.compress(b'private APK', mtime=0)
                    elif mutation == 'nested-xz':
                        payload = support.sources.lzma.compress(benign.getvalue()) + b'private APK'
                    elif mutation == 'nested-bz2':
                        payload = support.sources.bz2.compress(benign.getvalue()) + b'private APK'
                    members['runtime-inputs/ordinary-source.tar'] = payload
                index = write_materials(root, members)
                archive = root / 'source-materials-001.tar'
                if mutation.startswith('outer') or mutation == 'zero-padding':
                    with archive.open('ab') as stream:
                        stream.write(b'\0' * 512 if mutation == 'zero-padding' else (
                            benign.getvalue() if mutation == 'outer-tar' else b'private APK'))
                    index['archives'][0]['sha256'] = publisher.digest(archive)
                    (root / 'sources-index.json').write_text(json.dumps(index))
                ready = json.loads((root / 'release-ready.json').read_text())
                ready['sources_index_sha256'] = publisher.digest(root / 'sources-index.json')
                (root / 'release-ready.json').write_text(json.dumps(ready))
                sums(root)
                if mutation == 'zero-padding':
                    publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=False)
                else:
                    with self.assertRaisesRegex(publisher.CandidatePublicationError, 'trailing/concatenated'):
                        publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=False)

    def test_nonzero_first_tar_terminator_is_rejected_even_after_reader_consumes_it(self):
        def replace_end_marker(data):
            with tarfile.open(fileobj=io.BytesIO(data), mode='r:') as archive:
                list(archive)
                offset = archive.offset
            self.assertEqual(data[offset:offset + 512], b'\0' * 512)
            return data[:offset] + b'private APK'.ljust(512, b'\0') + data[offset + 512:]

        for container in ('outer', 'nested', 'gzip', 'xz', 'bz2'):
            with self.subTest(container=container), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'candidate'
                fixture(root)
                (root / 'reproducibility.json').unlink()
                members = source_members()
                if container != 'outer':
                    buffer = io.BytesIO()
                    with tarfile.open(fileobj=buffer, mode='w') as archive:
                        member = tarfile.TarInfo('ordinary-source.txt')
                        member.size = 1
                        archive.addfile(member, io.BytesIO(b'x'))
                    payload = replace_end_marker(buffer.getvalue())
                    if container == 'gzip':
                        payload = support.gzip.compress(payload, mtime=0)
                    elif container == 'xz':
                        payload = support.sources.lzma.compress(payload)
                    elif container == 'bz2':
                        payload = support.sources.bz2.compress(payload)
                    members['runtime-inputs/ordinary-source.tar'] = payload
                index = write_materials(root, members)
                archive = root / 'source-materials-001.tar'
                if container == 'outer':
                    archive.write_bytes(replace_end_marker(archive.read_bytes()))
                    index['archives'][0]['sha256'] = publisher.digest(archive)
                    (root / 'sources-index.json').write_text(json.dumps(index))
                ready = json.loads((root / 'release-ready.json').read_text())
                ready['sources_index_sha256'] = publisher.digest(root / 'sources-index.json')
                (root / 'release-ready.json').write_text(json.dumps(ready))
                sums(root)
                result = subprocess.run(
                    [sys.executable, str(publisher.REPO / 'scripts/publish_candidate.py'),
                     '--check-only', '--event-name', 'push', '--tag', 'v0.1.0-rc.1', '--candidate', str(root)],
                    env={**os.environ, 'PATH': ''}, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn('trailing/concatenated', result.stderr)

    def test_missing_or_malformed_scope_policy_refuses_even_with_ready_true(self):
        for sidecar in ('sources-index.json', 'release-ready.json'):
            for policy in (None, {}, {**POLICY, 'public_offline_bootstrap_complete': True},
                           {**POLICY, 'schema': True}):
                with self.subTest(sidecar=sidecar, policy=policy), tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary) / 'candidate'
                    fixture(root)
                    (root / 'reproducibility.json').unlink()
                    path = root / sidecar
                    value = json.loads(path.read_text())
                    value['material_policy'] = policy
                    path.write_text(json.dumps(value))
                    if sidecar == 'sources-index.json':
                        ready = json.loads((root / 'release-ready.json').read_text())
                        ready['sources_index_sha256'] = publisher.digest(path)
                        (root / 'release-ready.json').write_text(json.dumps(ready))
                    sums(root)
                    with self.assertRaisesRegex(publisher.CandidatePublicationError, 'policy'):
                        publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=False)

    def test_check_only_cli_needs_no_tools_and_writes_nothing_for_both_channels(self):
        env = {key: value for key, value in os.environ.items() if key not in ('GH_TOKEN', 'GITHUB_TOKEN')}
        env['PATH'] = ''
        for version in ('0.1.0', '0.1.0-rc.1'):
            for variance in (False, True):
                with self.subTest(version=version, variance=variance):
                    self.configure_version(version)
                    root = publisher.REPO / ('candidate-' + version + '-' + str(variance))
                    fixture(root, variance, version=version)
                    def snapshot():
                        return {str(p.relative_to(publisher.REPO)):
                                (p.read_bytes() if p.is_file() else None, p.stat().st_mtime_ns)
                                for p in publisher.REPO.rglob('*')}
                    before = snapshot()
                    result = self.run_cli(root, publisher.REPO / 'absent-notes', env, tag='v' + version,
                                          extra=('--check-only', '--require-reproducibility'))
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(json.loads(result.stdout),
                                     {'verified_assets': sorted(p.name for p in root.iterdir()), 'version': version})
                    self.assertEqual(before, snapshot())
                    self.assertFalse(list(publisher.REPO.rglob('__pycache__')))

    def test_check_only_cli_refuses_contradictory_metadata_variance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'candidate'
            fixture(root, variance=True)
            receipt_path = root / 'reproducibility.json'
            valid = json.loads(receipt_path.read_text())
            for mutation in ('mismatch', 'identical-variance', 'image-equality', 'missing-first',
                             'invalid-source', 'unstructured', 'wrong-category'):
                invalid = json.loads(json.dumps(valid))
                if mutation == 'mismatch':
                    invalid['status'] = 'mismatch'
                elif mutation == 'identical-variance':
                    invalid['first_image_sha256'] = invalid['second_image_sha256']
                    invalid['criteria']['image_bytes'] = True
                elif mutation == 'image-equality':
                    invalid['criteria']['image_bytes'] = True
                elif mutation == 'missing-first':
                    invalid.pop('first_image_sha256')
                elif mutation == 'invalid-source':
                    invalid['raw_source_archive_sha256'][0] = {'source-materials-001.tar': 'invalid'}
                elif mutation == 'unstructured':
                    invalid['metadata_variance'] = 'some variance'
                else:
                    invalid['metadata_variance'] = {'unrelated': {'cause': 'not image evidence'}}
                receipt_path.write_text(json.dumps(invalid))
                sums(root)
                result = self.run_cli(root, Path(temporary) / 'absent-notes',
                                      {**os.environ, 'PATH': ''}, tag='v0.1.0-rc.1',
                                      extra=('--check-only', '--require-reproducibility'))
                self.assertNotEqual(result.returncode, 0, mutation)

    def test_single_build_requires_exact_indexed_sources_and_mandatory_sidecars(self):
        for mutation in ('missing-part', 'replaced-part', 'missing-notice', 'missing-relink', 'extra-part', 'invalid-index-path'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'candidate'
                fixture(root)
                (root / 'reproducibility.json').unlink()
                if mutation == 'missing-part':
                    (root / 'source-materials-001.tar').unlink()
                elif mutation == 'replaced-part':
                    (root / 'source-materials-001.tar').write_bytes(b'replacement')
                elif mutation == 'missing-notice':
                    (root / 'NOTICE.txt').unlink()
                elif mutation == 'missing-relink':
                    (root / 'RELINKING.md').unlink()
                elif mutation == 'extra-part':
                    (root / 'source-materials-002.tar').write_bytes(b'extra')
                else:
                    index = root / 'sources-index.json'
                    index.write_text(json.dumps({'archives': [{'path': '../source-materials-001.tar', 'sha256': '0' * 64}]}))
                    ready = json.loads((root / 'release-ready.json').read_text())
                    ready['sources_index_sha256'] = publisher.digest(index)
                    (root / 'release-ready.json').write_text(json.dumps(ready))
                sums(root)
                with self.assertRaises(publisher.CandidatePublicationError):
                    publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=False)

    def test_checksum_paths_duplicates_and_missing_coverage_are_refused(self):
        for line in ('0' * 64 + '  ../escape\n', '0' * 64 + '  /absolute\n', '0' * 64 + '  audit.json\n'):
            with self.subTest(line=line), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'candidate'
                fixture(root)
                with (root / 'SHA256SUMS').open('a') as stream:
                    stream.write(line)
                with self.assertRaises(publisher.CandidatePublicationError):
                    publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=True)

    def test_publication_cli_uses_tagged_notes_channel_and_exact_public_assets(self):
        for version in ('0.1.0', '0.1.0-rc.1'):
            for variance in (False, True):
                with self.subTest(version=version, variance=variance), tempfile.TemporaryDirectory() as temporary:
                    parent = Path(temporary)
                    self.configure_version(version)
                    root = parent / 'candidate'
                    fixture(root, variance, version=version)
                    git_root = parent / 'tagged'
                    git_root.mkdir()
                    git_env = {**os.environ, 'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.invalid',
                               'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.invalid'}
                    def git(*args):
                        return subprocess.run(['git', *args], cwd=git_root, env=git_env,
                                              check=True, capture_output=True, text=True).stdout.strip()
                    git('init', '-q')
                    changelog = git_root / 'CHANGELOG.md'
                    changelog.write_text('## [0.1.0]\nTagged stable notes\n\n## [0.1.0-rc.1]\nTagged RC notes\n')
                    git('add', 'CHANGELOG.md')
                    git('-c', 'commit.gpgsign=false', 'commit', '-qm', 'release fixture')
                    commit = git('rev-parse', 'HEAD')
                    tag = 'v' + version
                    git('-c', 'tag.gpgsign=false', 'tag',
                        *(['-a', '-m', 'release fixture'] if variance else []), tag)
                    changelog.write_text('Uncommitted working tree notes must not be published\n')
                    notes = parent / 'notes'
                    extracted = subprocess.run(
                        [sys.executable, str(publisher.REPO / 'scripts/release_notes.py'),
                         '--tag', tag, '--commit', commit, '--output', str(notes)],
                        cwd=git_root, env=git_env, capture_output=True, text=True)
                    self.assertEqual(extracted.returncode, 0, extracted.stderr)
                    expected_notes = 'Tagged RC notes\n' if '-rc.' in version else 'Tagged stable notes\n'
                    self.assertEqual(notes.read_text(), expected_notes)
                    env, capture = self.fake_gh(parent)
                    before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()}
                    result = self.run_cli(root, notes, env, tag=tag)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    calls = [json.loads(line) for line in capture.read_text().splitlines()]
                    self.assertEqual(len(calls), 1, result.stdout)
                    argv, notes_hex = calls[0]['argv'], calls[0]['notes_hex']
                    self.assertEqual(notes_hex, expected_notes.encode().hex())
                    self.assertEqual(argv[:2], ['release', 'create'])
                    self.assertEqual(argv[2], tag)
                    for option, value in (('--repo', 'owner/repo'), ('--title', tag),
                                          ('--notes-file', str(notes.resolve()))):
                        with self.subTest(option=option):
                            self.assertEqual(argv.count(option), 1)
                            self.assertEqual(argv[argv.index(option) + 1], value)
                    self.assertIn('--verify-tag', argv)
                    if '-rc.' in version:
                        self.assertIn('--prerelease', argv)
                        self.assertIn('--latest=false', argv)
                        self.assertNotIn('--latest=true', argv)
                    else:
                        self.assertIn('--latest=true', argv)
                        self.assertNotIn('--prerelease', argv)
                    image = f'Furami-v{version}-Linux-x64.AppImage'
                    bundle = f'Furami-v{version}-Linux-x64-materials.tar'
                    expected = public_asset_names(version)
                    uploaded = Path(env['FAKE_GH_ASSETS'])
                    self.assertEqual({p.name for p in uploaded.iterdir()}, expected)
                    sums_map = read_public_sums(uploaded)
                    self.assertEqual(set(sums_map), expected - {'SHA256SUMS'})
                    for name, value in sums_map.items():
                        self.assertEqual(publisher.digest(uploaded / name), value, name)
                    self.assertEqual((uploaded / image).read_bytes(), (root / image).read_bytes())
                    self.assertEqual((uploaded / 'manifest.json').read_bytes(), (root / 'manifest.json').read_bytes())
                    seen = inspect_materials_tar(uploaded / bundle)
                    self.assertEqual(set(seen), retained_material_names())
                    self.assertFalse(any(name.startswith('source-materials-') for name in seen))
                    self.assertFalse({image, 'manifest.json'} & set(seen))
                    self.assertNotEqual(set(seen), {p.name for p in root.iterdir()})
                    for name, data in seen.items():
                        self.assertEqual(data, (root / name).read_bytes(), name)
                    self.assertIn(b'source-materials-001.tar', seen['SHA256SUMS'])
                    self.assertIn(b'source-materials-001.tar', seen['sources-index.json'])
                    self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()})
                    temporary = Path(env['TMPDIR'])
                    generated = {Path(value) for value in argv if temporary in Path(value).parents}
                    self.assertEqual(len(generated), 2)
                    self.assertFalse(any(path.exists() for path in generated))
                    self.assertEqual(list(temporary.iterdir()), [])
                    self.assertTrue((root / image).is_file())
                    self.assertTrue((root / 'manifest.json').is_file())
                    verified = self.run_cli(root, parent / 'absent-notes', {**env, 'PATH': ''}, tag=tag,
                                            extra=('--check-only', '--require-reproducibility'))
                    self.assertEqual(verified.returncode, 0, verified.stderr)

    def test_failed_existing_release_is_not_retried_replaced_or_edited(self):
        for version in ('0.1.0', '0.1.0-rc.1'):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as temporary:
                parent = Path(temporary)
                self.configure_version(version)
                root = parent / 'candidate'
                fixture(root, version=version)
                notes = parent / 'notes'
                notes.write_text('Release notes\n')
                env, capture = self.fake_gh(parent)
                env['FAKE_GH_EXIT'] = '7'
                before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()}
                result = self.run_cli(root, notes, env, tag='v' + version)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('returned non-zero exit status 7', result.stderr)
                calls = [json.loads(line) for line in capture.read_text().splitlines()]
                self.assertEqual(len(calls), 1)
                argv = calls[0]['argv']
                self.assertEqual(argv[:3], ['release', 'create', 'v' + version])
                self.assertEqual({p.name for p in Path(env['FAKE_GH_ASSETS']).iterdir()}, public_asset_names(version))
                temporary = Path(env['TMPDIR'])
                generated = {Path(value) for value in argv if temporary in Path(value).parents}
                self.assertEqual(len(generated), 2)
                self.assertFalse(any(path.exists() for path in generated))
                self.assertTrue((root / f'Furami-v{version}-Linux-x64.AppImage').is_file())
                self.assertTrue((root / 'manifest.json').is_file())
                self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()})
                self.assertEqual(list(temporary.iterdir()), [])

    def test_prepare_public_assets_exports_deterministic_contract_bundle(self):
        for version in ('0.1.0', '0.1.0-rc.1'):
            for variance in (False, True):
                with self.subTest(version=version, variance=variance), tempfile.TemporaryDirectory() as temporary:
                    self.configure_version(version)
                    root = Path(temporary)
                    observations = []
                    for offset, mtime in enumerate((1_000_000_000, 2_000_000_000)):
                        candidate = root / f'candidate-{offset}'
                        fixture(candidate, variance, version=version)
                        for path in candidate.iterdir():
                            os.utime(path, (mtime, mtime))
                        before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in candidate.iterdir()}
                        output = root / f'output-{offset}'
                        output.mkdir()
                        verified = publisher.verify_candidate(candidate, version, require_reproducibility=True)
                        returned = publisher.prepare_public_assets(verified, version, output)
                        image = f'Furami-v{version}-Linux-x64.AppImage'
                        bundle = f'Furami-v{version}-Linux-x64-materials.tar'
                        expected = public_asset_names(version)
                        self.assertEqual(sorted(p.name for p in returned), sorted(expected))
                        self.assertEqual([p.name for p in returned], sorted(p.name for p in returned))
                        self.assertEqual({p.name for p in output.iterdir()}, {bundle, 'SHA256SUMS'})
                        by_name = {p.name: p for p in returned}
                        self.assertEqual(by_name[image], candidate / image)
                        self.assertEqual(by_name['manifest.json'], candidate / 'manifest.json')
                        self.assertEqual(by_name[bundle], output / bundle)
                        self.assertEqual(by_name['SHA256SUMS'], output / 'SHA256SUMS')
                        sums_map = read_public_sums(output)
                        self.assertEqual(set(sums_map), expected - {'SHA256SUMS'})
                        for name, value in sums_map.items():
                            self.assertEqual(publisher.digest(by_name[name]), value, name)
                        seen = inspect_materials_tar(output / bundle)
                        self.assertEqual(set(seen), retained_material_names())
                        self.assertFalse(any(name.startswith('source-materials-') for name in seen))
                        self.assertFalse({image, 'manifest.json'} & set(seen))
                        for name, data in seen.items():
                            self.assertEqual(data, before[name][0], name)
                        self.assertIn(b'source-materials-001.tar', seen['SHA256SUMS'])
                        self.assertIn(b'source-materials-001.tar', seen['sources-index.json'])
                        self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mtime_ns)
                                                  for p in candidate.iterdir()})
                        observations.append((publisher.digest(output / bundle),
                                             (output / 'SHA256SUMS').read_bytes(), seen))
                    self.assertEqual(observations[0], observations[1])

    def test_prepare_public_assets_refuses_missing_export_prerequisites(self):
        for mutation in ('image', 'manifest', 'components'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                candidate = root / 'candidate'
                fixture(candidate)
                output = root / 'output'
                output.mkdir()
                assets = publisher.verify_candidate(candidate, '0.1.0-rc.1', require_reproducibility=True)
                name = {'image': 'Furami-v0.1.0-rc.1-Linux-x64.AppImage',
                        'manifest': 'manifest.json', 'components': 'components.json'}[mutation]
                reduced = [path for path in assets if path.name != name]
                with self.assertRaisesRegex(publisher.CandidatePublicationError, 'public export requires'):
                    publisher.prepare_public_assets(reduced, '0.1.0-rc.1', output)
                self.assertEqual(list(output.iterdir()), [])

    def test_prepare_public_assets_refuses_invalid_or_nonempty_output_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / 'candidate'
            fixture(candidate)
            assets = publisher.verify_candidate(candidate, '0.1.0-rc.1', require_reproducibility=True)
            nonempty = root / 'nonempty'
            nonempty.mkdir()
            (nonempty / 'keep').write_bytes(b'x')
            symlink = root / 'symlink'
            symlink.symlink_to(nonempty)
            regular = root / 'regular'
            regular.write_bytes(b'x')
            missing = root / 'missing'
            for output in (nonempty, symlink, regular, missing):
                with self.subTest(output=output.name):
                    with self.assertRaises(publisher.CandidatePublicationError):
                        publisher.prepare_public_assets(assets, '0.1.0-rc.1', output)
            self.assertEqual(list(nonempty.iterdir()), [nonempty / 'keep'])
            self.assertEqual(regular.read_bytes(), b'x')
            self.assertFalse(missing.exists())

    def test_public_materials_size_limit_is_strict(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            candidate = root / 'candidate'
            fixture(candidate)
            assets = publisher.verify_candidate(candidate, '0.1.0-rc.1', require_reproducibility=True)
            probe = root / 'probe'
            probe.mkdir()
            returned = publisher.prepare_public_assets(assets, '0.1.0-rc.1', probe)
            bundle = next(path for path in returned if path.name.endswith('.tar'))
            size = bundle.stat().st_size
            self.assertGreater(size, 0)
            for limit in (1, size):
                with self.subTest(limit=limit), tempfile.TemporaryDirectory() as directory:
                    output = Path(directory) / 'output'
                    output.mkdir()
                    with patch.object(publisher, 'MAX_RELEASE_ASSET_BYTES', limit):
                        with self.assertRaisesRegex(publisher.CandidatePublicationError, 'must be under 2 GiB'):
                            publisher.prepare_public_assets(assets, '0.1.0-rc.1', output)
                    self.assertFalse((output / 'SHA256SUMS').exists())
            allowed = root / 'allowed'
            allowed.mkdir()
            with patch.object(publisher, 'MAX_RELEASE_ASSET_BYTES', size + 1):
                exported = publisher.prepare_public_assets(assets, '0.1.0-rc.1', allowed)
            self.assertEqual(sorted(p.name for p in exported), sorted(public_asset_names('0.1.0-rc.1')))

    def test_publication_cli_refuses_oversized_public_materials_before_gh(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            self.configure_version('0.1.0-rc.1')
            root = parent / 'candidate'
            fixture(root)
            notes = parent / 'notes'
            notes.write_text('Release notes\n')
            env, capture = self.fake_gh(parent)
            probe = parent / 'probe'
            probe.mkdir()
            exported = publisher.prepare_public_assets(
                publisher.verify_candidate(root, '0.1.0-rc.1', require_reproducibility=True),
                '0.1.0-rc.1', probe)
            bundle = next(path for path in exported if path.name.endswith('.tar'))
            limit = bundle.stat().st_size
            before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()}
            with patch.object(publisher, 'MAX_RELEASE_ASSET_BYTES', limit), \
                    patch.dict(os.environ, env, clear=True):
                with self.assertRaisesRegex(publisher.CandidatePublicationError, 'must be under 2 GiB'):
                    publisher.publish_candidate(tag='v0.1.0-rc.1', version='0.1.0-rc.1', event_name='push',
                                                repository='owner/repo', notes=notes, candidate=root)
            self.assertFalse(capture.exists())
            self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()})
            self.assertEqual(list(Path(env['TMPDIR']).iterdir()), [])

    def test_publication_cli_refuses_missing_manifest_or_components_before_gh(self):
        for version in ('0.1.0', '0.1.0-rc.1'):
            for missing in ('manifest.json', 'components.json'):
                with self.subTest(version=version, missing=missing), tempfile.TemporaryDirectory() as temporary:
                    parent = Path(temporary)
                    self.configure_version(version)
                    root = parent / 'candidate'
                    fixture(root, version=version)
                    (root / missing).unlink()
                    sums(root)
                    notes = parent / 'notes'
                    notes.write_text('Release notes\n')
                    env, capture = self.fake_gh(parent)
                    before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()}
                    result = self.run_cli(root, notes, env, tag='v' + version)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertIn('candidate refused:', result.stderr)
                    self.assertFalse(capture.exists(), result.stderr)
                    self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mtime_ns)
                                              for p in root.iterdir()})
