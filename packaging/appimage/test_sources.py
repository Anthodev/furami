"""Deb822 source metadata must retain signed checksum fields and notices."""
import importlib.util
from pathlib import Path
import unittest
import copy
import json
import tempfile
import gzip
import shutil
import hashlib
import io
import tarfile
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("furami_sources", Path(__file__).with_name("sources.py"))
sources = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sources)


class DerivedLoaderTests(unittest.TestCase):
    def test_only_terminal_addresses_change_and_raw_is_immutable(self):
        raw = b'\tlinux-vdso.so.1 (0xabcd)\n\tlib0x123.so => /prefix/lib0x123.so (0x1234)\n\t/lib64/ld-linux-x86-64.so.2 (0x5678)\n'
        original = bytes(raw)
        report = sources.loader_resolution_report(raw)
        self.assertEqual(report, sources.loader_resolution_report(
            raw.replace(b'(0xabcd)', b'(0x1111)').replace(b'(0x1234)', b'(0x9999)')))
        self.assertEqual(raw, original)
        self.assertIn(b'lib0x123.so', report)
        self.assertNotEqual(report, sources.loader_resolution_report(raw.replace(b'/prefix/', b'/other/')))
        self.assertNotEqual(report, sources.loader_resolution_report(b'\n'.join(reversed(raw.splitlines()))))

    def test_unresolved_unknown_empty_and_crlf_refuse(self):
        for raw in (b'', b'libfoo => not found\n', b'warning: odd output\n',
                    b'/loader (0xABCD)\n', b'/loader (0x123)\r\n', b'\n'):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                sources.loader_resolution_report(raw)

    def test_canonical_supplemental_dossier_is_identity_and_path_bound(self):
        entry = copy.deepcopy(json.loads((Path(__file__).with_name('sources.lock.json')).read_text())['qt_sources']['qtbase'])
        with tempfile.TemporaryDirectory() as directory:
            sbom = Path(directory) / 'vendor.spdx'
            sbom.write_bytes(b'vendor fixture')
            entry['supplemental_metadata']['spdx_sha256'] = sources.sha(sbom)
            self.assertIsNotNone(sources.supplemental_dossier(entry, sbom))
            for mutation in ('commit', 'archive', 'spdx', 'extra', 'missing', 'evidence', 'target', 'duplicate-cache', 'url'):
                changed = copy.deepcopy(entry)
                dossier = changed['supplemental_metadata']
                first = next(iter(dossier['files']))
                if mutation == 'commit':
                    dossier['commit'] = '0' * 40
                elif mutation == 'archive':
                    dossier['archive_sha256'] = '0' * 64
                elif mutation == 'spdx':
                    dossier['spdx_sha256'] = '0' * 64
                elif mutation == 'extra':
                    dossier['files']['src/unrelated/.gitignore'] = dossier['files'][first]
                elif mutation == 'missing':
                    del dossier['files'][first]
                elif mutation == 'evidence':
                    dossier['license_evidence'].clear()
                elif mutation == 'target':
                    dossier['files'][first]['recipient'] = '../unsafe'
                elif mutation == 'duplicate-cache':
                    keys = list(dossier['files'])
                    dossier['files'][keys[1]]['archive'] = dossier['files'][first]['archive']
                else:
                    dossier['files'][first]['url'] = dossier['files'][first]['url'].replace(dossier['commit'], 'dev')
                with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                    sources.supplemental_dossier(changed, sbom)

    def test_supplement_cannot_shadow_archive_member(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sbom, archive, supplied = root / 'source.spdx', root / 'source.tar', root / 'supplement'
            supplied.write_bytes(b'expected')
            row = {'sha1': hashlib.sha1(b'expected').hexdigest(),
                   'sha256': sources.sha(supplied), 'recipient': 'overlay/src/android/jar/.gitignore'}
            sbom.write_text('FileName: ./src/android/jar/.gitignore\nFileChecksum: SHA1: ' + row['sha1'] + '\n')
            with tarfile.open(archive, 'w') as tar:
                info = tarfile.TarInfo('qtbase/src/android/jar/.gitignore')
                info.size = len(b'wrong')
                tar.addfile(info, io.BytesIO(b'wrong'))
            proof = sources.qt_correspondence(archive, sbom, supplements={'src/android/jar/.gitignore': (supplied, row)})
            self.assertTrue(proof['differences'])
            self.assertEqual(proof['supplemental_checked_files'], 0)
            with tarfile.open(archive, 'w'):
                pass
            self.assertEqual(sources.qt_correspondence(
                archive, sbom, supplements={'src/android/jar/.gitignore': (supplied, row)})['differences'], [])
            supplied.write_bytes(b'tampered')
            with self.assertRaises(ValueError):
                sources.qt_correspondence(archive, sbom, supplements={'src/android/jar/.gitignore': (supplied, row)})


class SourceMetadataTests(unittest.TestCase):
    def test_empty_checksum_headers_keep_exact_descriptor_and_separate_tables(self):
        # Frozen noble-updates glslang source record uses empty field headers.
        text = """Package: glslang
Version: 15.1.0-2~ubuntu0.24.04.2
Directory: pool/universe/g/glslang
Files:
 34652a97c488f6381f39c54606979dea 2274 glslang_15.1.0-2~ubuntu0.24.04.2.dsc
Checksums-Sha1:
 6128e0890a94027ec68d5ff86f66f269304f37a6 2274 glslang_15.1.0-2~ubuntu0.24.04.2.dsc
Checksums-Sha256:
 94bc1ae3d867a8893948d486aae763ae9de8567437fa77d444a38a89be89312c 2274 glslang_15.1.0-2~ubuntu0.24.04.2.dsc
Homepage: https://github.com/KhronosGroup/glslang
"""
        record, = sources.paragraphs(text)
        digest, size, descriptor = record["Checksums-Sha256"].split()
        self.assertEqual((digest, int(size), descriptor), (
            "94bc1ae3d867a8893948d486aae763ae9de8567437fa77d444a38a89be89312c",
            2274, "glslang_15.1.0-2~ubuntu0.24.04.2.dsc"))
        self.assertEqual(record["Directory"], "pool/universe/g/glslang")
        self.assertEqual(record["Files"].split()[0], "34652a97c488f6381f39c54606979dea")
        self.assertEqual(record["Checksums-Sha1"].split()[0], "6128e0890a94027ec68d5ff86f66f269304f37a6")
        self.assertEqual(record["Homepage"], "https://github.com/KhronosGroup/glslang")

    def test_optional_field_whitespace_and_notice_continuations(self):
        text = """Package:source-name
Version:\t1.0-1
Checksums-Sha256: \n\tchecksum 42 source-name_1.0-1.dsc

Files: *
License: Expat
 Permission is hereby granted: preserve this inline colon.
"""
        source, notice = sources.paragraphs(text)
        self.assertEqual(source["Package"], "source-name")
        self.assertEqual(source["Version"], "1.0-1")
        self.assertEqual(source["Checksums-Sha256"].split(), ["checksum", "42", "source-name_1.0-1.dsc"])
        self.assertEqual(notice["Files"], "*")
        self.assertEqual(notice["License"], "Expat\nPermission is hereby granted: preserve this inline colon.")


class UbuntuTermsTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.notice = self.root / 'libexample.copyright'
        self.notice.write_text('License (library): LGPLv2.1+\n original grant\n')
        self.identity = 'libexample@1.0-1'
        self.entry = {'source_package': {'name': 'example', 'version': '1.0-1'},
                      'notice': {'path': self.notice.name, 'sha256': sources.sha(self.notice)},
                      'license_expression': 'LGPL-2.1-or-later AND LicenseRef-example',
                      'license_exception': None, 'source_required': True,
                      'source_mapping_basis': 'reviewed library scope',
                      'shipped_objects': ['usr/lib/libexample.so.1']}
        self.lock = {'schema': 1, 'packages': {self.identity: self.entry},
                     'custom_license_texts': {'LicenseRef-example': 'exact original grant'}}

    def resolve(self, **changes):
        arguments = {'binary_package': 'libexample', 'binary_version': '1.0-1',
                     'source_package': 'example', 'source_version': '1.0-1',
                     'notice': self.notice, 'shipped_objects': ['usr/lib/libexample.so.1']}
        arguments.update(changes)
        return sources.resolve_ubuntu_terms(self.lock, **arguments)

    def test_valid_terms_require_sources_and_preserve_custom_grant(self):
        result = self.resolve()
        self.assertTrue(result['terms_selection_resolved'])
        self.assertIs(result['source_required'], True)
        self.assertEqual(result['custom_license_texts'], {'LicenseRef-example': 'exact original grant'})
        self.assertEqual(result['license_expression'], self.entry['license_expression'])

    def test_identity_versions_and_shipped_objects_fail_closed(self):
        for changes in ({'binary_version': '2.0'}, {'source_version': '2.0'},
                        {'source_package': 'different'}, {'shipped_objects': []},
                        {'shipped_objects': ['usr/lib/extra.so']}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.resolve(**changes)

    def test_missing_extra_or_duplicate_package_set_is_rejected(self):
        sources._require_ubuntu_package_set(self.lock, [self.identity])
        for identities in ([], [self.identity, 'unexpected@1'], [self.identity, self.identity]):
            with self.subTest(identities=identities), self.assertRaisesRegex(ValueError, 'package set differs'):
                sources._require_ubuntu_package_set(self.lock, identities)

    def test_changed_notice_and_missing_custom_text_fail_closed(self):
        self.notice.write_text('changed notice')
        with self.assertRaisesRegex(ValueError, 'correspondence mismatch'):
            self.resolve()
        self.notice.write_text('License (library): LGPLv2.1+\n original grant\n')
        self.lock['custom_license_texts'].clear()
        with self.assertRaisesRegex(ValueError, 'custom text absent'):
            self.resolve()

    def test_loader_rejects_missing_text_and_non_boolean_source_requirement(self):
        path = self.root / 'terms.json'
        for change in ('text', 'required'):
            lock = copy.deepcopy(self.lock)
            if change == 'text':
                lock['custom_license_texts'].clear()
            else:
                lock['packages'][self.identity]['source_required'] = 1
            path.write_text(json.dumps(lock))
            with self.subTest(change=change), self.assertRaises(ValueError):
                sources.load_ubuntu_terms(path)

    def test_normative_lock_covers_all_pinned_packages_including_library_and_crt_sources(self):
        lock = sources.load_ubuntu_terms(Path(__file__).with_name('ubuntu-terms.lock.json'))
        self.assertEqual(len(lock['packages']), 76)
        self.assertEqual(len({(row['source_package']['name'], row['source_package']['version'])
                              for row in lock['packages'].values()}), 60)
        for package in ('libgcrypt20', 'libasound2t64', 'libkeyutils1', 'libc6-dev', 'libgcc-13-dev'):
            record, = [row for identity, row in lock['packages'].items() if identity.startswith(package + '@')]
            self.assertIs(record['source_required'], True)
        for package in ('libc6-dev', 'libgcc-13-dev'):
            record, = [row for identity, row in lock['packages'].items() if identity.startswith(package + '@')]
            self.assertTrue(record['license_exception'])


def runtime_source_fixture(repo, cache):
    """Real lock schemas/package identities, tiny deterministic checked inputs."""
    original = sources.REPO / 'packaging/appimage-runtime'
    target = repo / 'packaging/appimage-runtime'
    target.mkdir(parents=True, exist_ok=True)
    shutil.copy2(original / 'build.py', target / 'build.py')
    runtime = json.loads((original / 'runtime.lock.json').read_text())
    alpine = json.loads((original / 'alpine-source.lock.json').read_text())
    members = {}

    def put(relative, content):
        path = cache / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return hashlib.sha256(content).hexdigest()

    for entry in runtime['sources'].values():
        data = ('runtime-source:' + entry['archive']).encode()
        entry['sha256'] = put(entry['archive'], data)
        members['runtime-inputs/' + entry['archive']] = data
    for name, source in alpine['linked_sources'].items():
        items = [source['upstream_source'], *source['aports_inputs']]
        for item in items:
            data = ('linked-source:' + name + '/' + item['file']).encode()
            item['sha256'] = put('alpine-sources/' + name + '/' + item['file'], data)
            item['sha512'] = hashlib.sha512(data).hexdigest()
            members['runtime-inputs/alpine-sources/' + name + '/' + item['file']] = data
        recipe = ('sha512sums="\n' + ''.join(
            item['sha512'] + '  ' + item['file'] + '\n' for item in items) + '"\n').encode()
        source['apkbuild']['sha256'] = put('alpine-sources/' + name + '/APKBUILD', recipe)
        members['runtime-inputs/alpine-sources/' + name + '/APKBUILD'] = recipe
    # Real APKINDEX format: concatenated gzip streams for signature and index.
    for name, entry in runtime['apk_indexes'].items():
        chunks = []
        for member_name in ('.SIGN.RSA.test-key.rsa.pub', 'APKINDEX'):
            buffer = io.BytesIO()
            with tarfile.open(fileobj=buffer, mode='w') as tar:
                member = tarfile.TarInfo(member_name)
                member.size = 1
                tar.addfile(member, io.BytesIO(b'x'))
            chunks.append(gzip.compress(buffer.getvalue(), mtime=0))
        data = b''.join(chunks)
        entry['sha256'] = put('indexes/' + name + '/x86_64/APKINDEX.tar.gz', data)
        members['runtime-inputs/indexes/' + name + '/x86_64/APKINDEX.tar.gz'] = data
    runtime['apk_packages']['abuild']['sha256'] = put(
        'apks/' + runtime['apk_packages']['abuild']['archive'], b'private APK')
    (target / 'alpine-source.lock.json').write_text(json.dumps(alpine))
    runtime['linked_source_closure']['sha256'] = sources.sha(target / 'alpine-source.lock.json')
    (target / 'runtime.lock.json').write_text(json.dumps(runtime))
    for path in (target / 'runtime.lock.json', target / 'alpine-source.lock.json'):
        members['furami/packaging/appimage-runtime/' + path.name] = path.read_bytes()
    policy = json.loads((sources.HERE / 'sources.lock.json').read_text())
    policy['qt_sources'] = {}
    policy['required_qt_modules'] = []
    policy['alpine_private_bootstrap'] = {
        'image': runtime['container'], 'manifest_sha256': runtime['container'].split('@sha256:')[1],
        'config_sha256': hashlib.sha256(b'private base config').hexdigest(),
        'layers_sha256': [hashlib.sha256(b'private base layer').hexdigest()]}
    policy_path = repo / 'packaging/appimage/sources.lock.json'
    policy_path.parent.mkdir(parents=True, exist_ok=True)
    policy_path.write_text(json.dumps(policy))
    members['furami/packaging/appimage/sources.lock.json'] = policy_path.read_bytes()
    for report_name in sources.LOADER_REPORTS.values():
        members['media-evidence/' + report_name] = sources.loader_resolution_report(
            b'\tlinux-vdso.so.1 (0x1234)\n\tlibmpv.so => /prefix/lib/libmpv.so (0x5678)\n')
    return runtime, alpine, members


class PublicMaterialTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repo = self.root / 'repo'
        self.cache = self.root / 'private-bootstrap'
        self.provenance = {name: str(self.root / name) for name in ('app', 'media', 'runtime', 'stage')}
        for relative in ('app/first/vendor', 'app/source-snapshot', 'media/prefix/sbom',
                         'runtime/licenses', 'runtime/relink', 'stage/ubuntu-source-evidence'):
            (self.root / relative).mkdir(parents=True)
        for folder in ('appimage', 'media', 'appimage-runtime'):
            (self.repo / 'packaging' / folder).mkdir(parents=True)
        self.recipe = self.repo / 'packaging/appimage'
        runtime, linked, self.members = runtime_source_fixture(self.repo, self.cache)
        self.apk = runtime['apk_packages']['abuild']
        self.zlib_archive = linked['linked_sources']['zlib']['upstream_source']['file']
        patcher = patch.object(sources, 'REPO', self.repo)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.write_json(self.repo / 'packaging/media/stack.lock.json', {'source_date_epoch': 1})
        self.write_json(self.root / 'app/manifest.json', {'inputs': {}, 'source_archives': []})
        self.write_json(self.root / 'media/manifest.json', {'sources': {'qt': {'archives': []}}, 'artifacts': {}})
        for name in sources.LOADER_REPORTS:
            (self.root / 'media' / name).write_bytes(
                b'\tlinux-vdso.so.1 (0x1234)\n\tlibmpv.so => /prefix/lib/libmpv.so (0x5678)\n')
        # The assembly hashes ubuntu-terms.lock.json even for an empty fixture.
        (self.recipe / 'ubuntu-terms.lock.json').write_bytes((self.recipe / 'sources.lock.json').read_bytes())
        self.write_json(self.root / 'runtime/manifest.json', {})
        for name in ('link.map', 'apk-installed.txt'):
            (self.root / 'runtime' / name).write_text('runtime proof\n')
        self.write_json(self.root / 'stage/ubuntu-sources.json',
                        {'packages': [], 'terms_lock_sha256': sources.sha(self.recipe / 'ubuntu-terms.lock.json')})

    def write_json(self, path, value):
        path.write_text(json.dumps(value))

    def assemble(self):
        output = self.root / 'public'
        with patch.object(sources, 'REPO', self.repo), patch.object(sources, 'HERE', self.recipe), \
                patch.object(sources, 'load_ubuntu_terms', return_value={'packages': {}}):
            index = sources.assemble(self.provenance, {'component_files': {}}, output,
                                     dict.fromkeys(('runtime', 'media', 'compliance'), self.cache), False)
        return output, index

    def test_actual_archives_exclude_private_apks_but_keep_linked_sources(self):
        output, index = self.assemble()
        self.assertIs(index['complete'], True)
        inventory = sources.source_inventory(output)
        self.assertIn('runtime-inputs/alpine-sources/zlib/' + self.zlib_archive, inventory)
        self.assertFalse(any(name.endswith('.apk') for name in inventory))
        self.assertNotIn(self.apk['sha256'], {row['sha256'] for row in inventory.values()})
        self.assertIs(index['material_policy']['public_offline_bootstrap_complete'], False)
        # Recipient verification must not access this private cache at all.
        import shutil
        shutil.rmtree(self.cache)
        sources.verify_public_materials(output, index)

    def test_actual_missing_linked_source_blocks_completeness(self):
        (self.cache / 'alpine-sources/zlib' / self.zlib_archive).unlink()
        output, index = self.assemble()
        self.assertIs(index['complete'], False)
        self.assertTrue(any(self.zlib_archive in reason for reason in index['missing']))
        with self.assertRaisesRegex(ValueError, 'incomplete'):
            sources.verify_public_materials(output, index)

    def test_injected_apk_is_rejected_even_renamed_and_fully_indexed(self):
        for name in ('runtime-inputs/apks/tool.apk', 'runtime-inputs/innocent.txt'):
            with self.subTest(name=name):
                output, index = self.assemble()
                data = (self.cache / 'apks' / self.apk['archive']).read_bytes()
                with tarfile.open(output / 'source-materials-001.tar', 'a') as archive:
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
                index['files'].append({'path': name, 'sha256': hashlib.sha256(data).hexdigest(),
                                       'size': len(data), 'role': 'build-input', 'component_id': None})
                index['archives'][0]['sha256'] = sources.sha(output / 'source-materials-001.tar')
                with self.assertRaisesRegex(ValueError, 'private Alpine'):
                    sources.verify_public_materials(output, index)
                import shutil
                shutil.rmtree(output)

    def rewrite_materials(self, output, index, members):
        for path in output.glob('source-materials-*.tar'):
            path.unlink()
        archive_path = output / 'source-materials-001.tar'
        with tarfile.open(archive_path, 'w') as archive:
            for name, data in sorted(members.items()):
                member = tarfile.TarInfo(name)
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
        index['files'] = [{'path': name, 'sha256': hashlib.sha256(data).hexdigest(), 'size': len(data)}
                          for name, data in members.items()]
        index['archives'] = [{'path': archive_path.name, 'sha256': sources.sha(archive_path)}]

    def test_removing_all_media_evidence_and_rehashing_still_refuses(self):
        output, index = self.assemble()
        members = {}
        for archive_path in output.glob('source-materials-*.tar'):
            with tarfile.open(archive_path) as archive:
                members.update({member.name: archive.extractfile(member).read() for member in archive
                                if not member.name.startswith('media-evidence/')})
        self.rewrite_materials(output, index, members)
        with self.assertRaisesRegex(ValueError, 'recipient material missing'):
            sources.verify_public_materials(output, index)

    def configure_qt_fixture(self):
        policy_path = self.recipe / 'sources.lock.json'
        policy = json.loads(policy_path.read_text())
        entry = copy.deepcopy(json.loads((Path(__file__).with_name('sources.lock.json')).read_text())['qt_sources']['qtbase'])
        archive = self.cache / entry['archive']
        with tarfile.open(archive, 'w') as tar:
            member = tarfile.TarInfo('qtbase/src/ordinary.cpp')
            member.size = 4
            tar.addfile(member, io.BytesIO(b'code'))
        sbom = self.root / 'media/prefix/sbom/qtbase-6.11.2.source.spdx'
        dossier = entry['supplemental_metadata']
        sbom.write_text('\n\n'.join(
            ['FileName: ./src/ordinary.cpp\nFileChecksum: SHA1: ' + hashlib.sha1(b'code').hexdigest()]
            + ['FileName: ./' + name + '\nFileChecksum: SHA1: ' + row['sha1']
               for name, row in dossier['files'].items()]))
        entry['sha256'] = sources.sha(archive)
        dossier['archive_sha256'] = entry['sha256']
        dossier['spdx_sha256'] = sources.sha(sbom)
        policy['qt_sources'] = {'qtbase': entry}
        policy['required_qt_modules'] = ['qtbase']
        policy_path.write_text(json.dumps(policy))
        manifest = self.root / 'media/manifest.json'
        value = json.loads(manifest.read_text())
        value['artifacts']['sbom/' + sbom.name] = {'sha256': sources.sha(sbom)}
        manifest.write_text(json.dumps(value))
        return entry

    def test_cached_qt_archive_without_supplements_remains_incomplete(self):
        self.configure_qt_fixture()
        output = self.root / 'public'
        audit = {'component_files': {'usr/lib/libQt6Core.so': {
            'components': [{'component_id': 'qt:qtbase'}]}}}
        with patch.object(sources, 'REPO', self.repo), patch.object(sources, 'HERE', self.recipe), \
                patch.object(sources, 'load_ubuntu_terms', return_value={'packages': {}}):
            index = sources.assemble(self.provenance, audit, output,
                                     dict.fromkeys(('runtime', 'media', 'compliance'), self.cache), False)
        self.assertFalse(index['complete'])
        self.assertTrue(index['missing'])
        sources.verify_public_materials(output, index, require_complete=False)
        with self.assertRaisesRegex(ValueError, 'incomplete'):
            sources.verify_public_materials(output, index)

    def test_removing_qt_receipt_spdx_overlay_and_rehashing_still_refuses(self):
        self.configure_qt_fixture()
        output, index = self.assemble()
        members = {}
        for archive_path in output.glob('source-materials-*.tar'):
            with tarfile.open(archive_path) as archive:
                members.update({member.name: archive.extractfile(member).read() for member in archive
                                if not member.name.startswith(('qt-inputs/', 'qt-evidence/'))})
        index['qt_correspondence'] = {}
        index['complete'], index['missing'] = True, []
        self.rewrite_materials(output, index, members)
        with self.assertRaisesRegex(ValueError, 'required Qt correspondence'):
            sources.verify_public_materials(output, index)

    def test_policy_and_actual_membership_cannot_be_self_declared(self):
        output, index = self.assemble()
        for policy in (None, {}, {**sources.PUBLIC_MATERIAL_POLICY, 'public_offline_bootstrap_complete': True},
                       {**sources.PUBLIC_MATERIAL_POLICY, 'schema': True}):
            changed = {**index, 'material_policy': policy}
            with self.subTest(policy=policy), self.assertRaisesRegex(ValueError, 'policy'):
                sources.verify_public_materials(output, changed)
        index['files'] = [row for row in index['files'] if not row['path'].endswith(self.zlib_archive)]
        with self.assertRaisesRegex(ValueError, 'inventory'):
            sources.verify_public_materials(output, index)


if __name__ == "__main__":
    unittest.main()
