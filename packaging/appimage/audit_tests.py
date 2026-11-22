#!/usr/bin/env python3
"""Behavioral tests for payload attribution, required entries and program closure."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('appimage_audit', Path(__file__).with_name('audit.py'))
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class QtFreePayloadTests(unittest.TestCase):
    def test_native_x11_and_media_payload_is_admitted(self):
        payload = {'usr/bin/furami': {}, 'usr/bin/mpv': {}, 'usr/lib/libmpv.so.2': {},
                   'usr/lib/libxcb.so.1': {}, 'usr/share/icons/hicolor/scalable/apps/dev.antho.furami.svg': {}}
        elf = [{'path': 'usr/bin/furami', 'needed': ['libxcb.so.1']},
               {'path': 'usr/lib/libmpv.so.2', 'needed': [], 'soname': 'libmpv.so.2'}]
        audit.inventory.require_qt_free(payload, elf)

    def test_legacy_frontend_payload_is_refused_even_without_an_elf_edge(self):
        for relative in ('usr/lib/libQt6Core.so.6', 'usr/lib/qt6/plugins/platforms/libqxcb.so',
                         'usr/bin/qt.conf', 'usr/share/qml/Example/qmldir',
                         'usr/share/Example.qmltypes', 'usr/share/Example.qml'):
            with self.subTest(relative=relative), self.assertRaisesRegex(ValueError, 'Qt/QML artifact'):
                audit.inventory.require_qt_free({relative: {}}, [])

    def test_renamed_files_do_not_hide_qt_elf_dependencies_or_sonames(self):
        relative = 'usr/lib/librenamed.so'
        for metadata in ({'needed': ['libQt6Core.so.6']},
                         {'needed': [], 'soname': 'libQt6Core.so.6'}):
            with self.subTest(metadata=metadata), self.assertRaisesRegex(ValueError, 'Qt ELF dependency or SONAME'):
                audit.inventory.require_qt_free({relative: {}}, [{'path': relative, **metadata}])


class MediaProgramAttributionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.media = Path(temporary.name)
        self.prefix = self.media / 'prefix'
        self.prefix.mkdir()
        self.file('bin/mpv', b'\x7fELF provider mpv program')
        self.file('lib/libmpv.so.2', b'checked provider shared library')

    def file(self, relative, content):
        path = self.prefix / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return path

    def test_seeded_provider_program_shares_checked_mpv_component(self):
        owners = {'usr/bin/mpv': {'owner': 'media', 'source': 'bin/mpv'},
                  'usr/lib/libmpv.so.2': {'owner': 'media', 'source': 'lib/libmpv.so.2'}}
        audit.media_evidence(self.media, owners)
        for owner in owners.values():
            component = owner['components'][0]
            self.assertEqual(component['component_id'], 'mpv')
            self.assertEqual(component['license_expression'], 'GPL-2.0-or-later')

    def test_ffmpeg_attribution_retains_pristine_identity_and_applied_patch_metadata(self):
        lock_path = audit.REPO / 'packaging/media/stack.lock.json'
        stack = json.loads(lock_path.read_text())
        helper = audit.stage.source_patches
        helper.snapshot(self.media / 'source-inputs', lock_path.read_bytes(),
                        helper.inputs(lock_path.parent, stack['sources']))
        applied = self.media / 'source-patches.json'
        applied.write_text(json.dumps(helper.receipt(stack['sources'])))
        manifest = {'sources': stack['sources'], 'lock_sha256': audit.stage.digest(lock_path),
                    'source_patches': {'path': applied.name, 'sha256': audit.stage.digest(applied)}}
        (self.media / 'manifest.json').write_text(json.dumps(manifest))
        self.file('lib/libavdevice.so.62', b'checked patched library')
        owners = {'usr/lib/libavdevice.so.62': {'owner': 'media', 'source': 'lib/libavdevice.so.62'}}
        audit.media_evidence(self.media, owners)
        evidence = owners['usr/lib/libavdevice.so.62']['components'][0]['evidence']
        self.assertEqual(evidence['upstream_source'], stack['sources']['ffmpeg'])
        self.assertEqual(evidence['source_patch_receipt'], manifest['source_patches'])

    def test_unrelated_or_inexact_provider_programs_remain_refused(self):
        for source in ('bin/ffmpeg', 'bin/ffprobe', 'bin/mpv-tool', 'mpv', 'usr/bin/mpv'):
            owners = {'usr/bin/mpv': {'owner': 'media', 'source': source}}
            with self.assertRaisesRegex(ValueError, 'media owner lacks component mapping'):
                audit.media_evidence(self.media, owners)

    def test_required_executables_include_staged_provider_program(self):
        root = self.media / 'AppDir'
        (root / 'usr/bin').mkdir(parents=True)
        (root / 'AppRun').write_bytes(b'#!/bin/sh\n')
        (root / 'usr/bin/furami').write_bytes(b'\x7fELF application')
        for relative in ('AppRun', 'usr/bin/furami'):
            (root / relative).chmod(0o755)
        with self.assertRaisesRegex(ValueError, 'not executable by recipients: usr/bin/mpv'):
            audit.verify_executables(root, audit.REQUIRED_EXECUTABLES)
        program = root / 'usr/bin/mpv'
        program.write_bytes(b'\x7fELF provider mpv program')
        with self.assertRaisesRegex(ValueError, 'not executable by recipients: usr/bin/mpv'):
            audit.verify_executables(root, audit.REQUIRED_EXECUTABLES)
        program.chmod(0o755)
        audit.verify_executables(root, audit.REQUIRED_EXECUTABLES)


class UbuntuComponentAuditTests(unittest.TestCase):
    PACKAGE = 'libexample1@2:1.0-1'
    LABEL = 'ubuntu:libexample1=2:1.0-1'
    LIBRARY = 'usr/lib/libexample.so.1'
    HELPER = 'usr/lib/example/libhelper.so.1'

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        work = Path(temporary.name)
        self.providers = {name: work / name for name in ('app', 'runtime', 'stage')}
        for directory in self.providers.values():
            directory.mkdir()
        self.root = work / 'extracted'
        self.root.mkdir()
        self.components = audit.load('components')
        self.terms = work / 'ubuntu-terms.lock.json'
        terms_patch = patch.object(self.components, 'UBUNTU_TERMS', self.terms)
        terms_patch.start()
        self.addCleanup(terms_patch.stop)
        self.static = {'components': [{'package': 'glslang-dev', 'version': '3'},
                                     {'package': 'spirv-tools', 'version': '4'}]}
        crt = {'name': 'libc6-dev', 'version': '5'}
        app = {'crates': [], 'binary': {'rust_standard_library': {'archives': []},
               'direct_objects': [{'binary_package': crt, 'source_package': 'glibc',
                                   'license_expression': 'LGPL-2.1-or-later', 'notice': 'crt notice'}]}}
        audit.dump(self.providers['app'] / 'manifest.json', app)
        audit.dump(self.providers['runtime'] / 'manifest.json', {'components': []})
        evidence = self.providers['stage'] / 'ubuntu-source-evidence'
        evidence.mkdir()
        pinned, rows = {}, []
        for identity, paths in ((self.PACKAGE, [self.HELPER, self.LIBRARY]),
                                ('glslang-dev@3', []), ('spirv-tools@4', []), ('libc6-dev@5', [])):
            package, _, version = identity.partition('@')
            notice = evidence / f'{package}.copyright'
            notice.write_text('Copyright and terms\n')
            terms = {'notice': {'path': notice.name, 'sha256': audit.stage.digest(notice)},
                     'license_expression': 'MIT', 'license_exception': None, 'source_required': True,
                     'shipped_objects': paths, 'source_mapping_basis': 'fixture source terms'}
            pinned[identity] = {**terms, 'source_package': {'name': package, 'version': version}}
            rows.append({**terms, 'binary_package': package, 'binary_version': version,
                         'source_package': package, 'source_version': version, 'terms_selection_resolved': True,
                         'custom_license_texts': {}, 'expression_basis': 'fixture source terms',
                         'license_sections': [], 'files': [{'archive': f'{package}.dsc'}]})
        audit.dump(self.terms, {'schema': 1, 'packages': pinned, 'custom_license_texts': {}})
        self.capture = {'packages': rows, 'terms_lock_sha256': audit.stage.digest(self.terms)}
        self.owners = {}
        self.add_file(self.LIBRARY, b'\x7fELF shared object')
        self.add_file(self.HELPER, b'\x7fELF private helper')
        self.add_file('usr/share/doc/ubuntu/libexample1/copyright', b'Copyright and terms\n')
        alias = 'usr/lib/libexample.so'
        (self.root / alias).symlink_to('libexample.so.1')
        self.owners[alias] = {'owner': self.LABEL}

    def add_file(self, relative, content, label=LABEL, source=None):
        file = self.root / relative
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_bytes(content)
        self.owners[relative] = {'owner': label, 'source': source or relative,
                                 'shipped_sha256': audit.stage.digest(file)}

    def registry(self):
        audit.dump(self.providers['stage'] / 'ubuntu-sources.json', self.capture)
        audit.verify_payload_owners(self.root, audit.stage.snapshot(self.root), self.owners)
        return self.attributed()

    def attributed(self):
        audit.dump(self.providers['stage'] / 'ubuntu-sources.json', self.capture)
        files, _ = audit.inventory.walk_tree(str(self.root))
        elf_paths = audit.inventory.detect_elf_files(str(self.root), files)
        registry, _, _ = self.components.registry(
            self.providers, self.owners, self.static, audit.stage.digest,
            payload_root=self.root, elf_paths=elf_paths)
        return registry

    def test_absent_extracted_copyright_is_refused_with_a_staged_owner(self):
        relative = 'usr/share/doc/ubuntu/libexample1/copyright'
        (self.root / relative).unlink()
        with self.assertRaisesRegex(ValueError, 'extracted copyright is not a regular file: ' + relative):
            self.attributed()

    def test_replaced_extracted_copyright_is_refused_with_a_consistent_staging_owner(self):
        relative = 'usr/share/doc/ubuntu/libexample1/copyright'
        (self.root / relative).write_bytes(b'Replacement terms\n')
        self.owners[relative]['shipped_sha256'] = audit.stage.digest(self.root / relative)
        with self.assertRaisesRegex(ValueError, 'extracted copyright bytes differ: ' + relative):
            self.registry()

    def test_symlinked_extracted_copyright_is_refused(self):
        relative = 'usr/share/doc/ubuntu/libexample1/copyright'
        path = self.root / relative
        path.unlink()
        (path.parent / 'copyright-target').write_bytes(b'Copyright and terms\n')
        path.symlink_to('copyright-target')
        with self.assertRaisesRegex(ValueError, 'extracted copyright is not a regular file: ' + relative):
            self.attributed()

    def test_missing_extracted_copyright_without_a_staged_owner_is_refused(self):
        relative = 'usr/share/doc/ubuntu/libexample1/copyright'
        (self.root / relative).unlink()
        del self.owners[relative]
        with self.assertRaisesRegex(ValueError, 'extracted copyright has no owner: ' + relative):
            self.registry()

    def test_regular_elfs_and_static_sources_match_exact_receipts(self):
        registry = self.registry()
        for identity in (self.PACKAGE, 'glslang-dev@3', 'spirv-tools@4', 'libc6-dev@5'):
            self.assertTrue(registry['ubuntu:' + identity]['terms_resolved'])
        for owner in self.owners.values():
            self.assertEqual(owner['components'][0]['component_id'], 'ubuntu:' + self.PACKAGE)

    def test_static_link_terms_stay_attached_to_libplacebo_elf(self):
        path = 'usr/lib/libplacebo.so.360'
        self.add_file(path, b'\x7fELF libplacebo', 'media')
        self.owners[path]['components'] = [{'component_id': 'libplacebo', 'license_expression': 'LGPL-2.1-or-later',
                                            'evidence': 'provider licence'}]
        self.registry()
        identifiers = {row['component_id'] for row in self.owners[path]['components']}
        self.assertEqual(identifiers, {'libplacebo', 'ubuntu:glslang-dev@3', 'ubuntu:spirv-tools@4'})

    def test_terms_cannot_accept_extra_regular_elf(self):
        self.add_file('usr/lib/libextra.so.1', b'\x7fELF unlisted object')
        with self.assertRaisesRegex(ValueError, 'Ubuntu component ELF paths mismatch.*libextra'):
            self.registry()

    def test_terms_cannot_accept_missing_regular_elf(self):
        (self.root / self.HELPER).unlink()
        del self.owners[self.HELPER]
        with self.assertRaisesRegex(ValueError, 'Ubuntu component ELF paths mismatch.*libhelper'):
            self.registry()

    def test_ubuntu_owner_receipt_cannot_keep_an_absent_path(self):
        (self.root / self.HELPER).unlink()
        with self.assertRaisesRegex(ValueError, 'Ubuntu owner path absent from final image.*libhelper'):
            self.registry()

    def test_owner_version_including_debian_epoch_must_match(self):
        self.owners[self.LIBRARY]['owner'] = 'ubuntu:libexample1=1.0-1'
        with self.assertRaisesRegex(ValueError, 'Ubuntu component observed identity set mismatch.*libexample1=1.0-1'):
            self.registry()

    def test_source_only_receipt_requires_static_contributor_evidence(self):
        self.static['components'].pop()
        with self.assertRaisesRegex(ValueError, 'Ubuntu component observed identity set mismatch.*spirv-tools=4'):
            self.registry()

    def test_regular_non_elf_cannot_satisfy_a_pinned_object_path(self):
        self.add_file(self.HELPER, b'Not an ELF object')
        with self.assertRaisesRegex(ValueError, 'Ubuntu component ELF paths mismatch.*libhelper'):
            self.registry()

    def test_ubuntu_elf_owner_evidence_cannot_disagree_with_its_label(self):
        self.owners[self.LIBRARY].update(package='libexample1', version='1.0-1')
        with self.assertRaisesRegex(ValueError, 'Ubuntu owner package/version evidence disagrees.*libexample'):
            self.registry()

    def test_static_contributor_version_must_match_the_terms_lock(self):
        self.static['components'][0]['version'] = '2'
        with self.assertRaisesRegex(ValueError, 'Ubuntu component observed identity set mismatch.*glslang-dev=2'):
            self.registry()

    def test_stale_source_receipt_cannot_override_terms(self):
        self.capture['packages'][0]['shipped_objects'] = [self.LIBRARY]
        with self.assertRaisesRegex(ValueError, 'Ubuntu component pinned terms correspondence mismatch'):
            self.registry()

    def test_empty_source_receipt_cannot_bypass_the_terms_lock(self):
        self.capture['packages'].clear()
        with self.assertRaisesRegex(ValueError, 'Ubuntu component terms lock/set mismatch'):
            self.registry()

    def test_source_patch_is_ffmpeg_gpl_material_never_authored_mit(self):
        path = 'usr/share/licenses/media/ffmpeg/patches/latest.patch'
        self.add_file(path, b'patch', 'checked-source-patch', 'ffmpeg:patches/latest.patch')
        self.registry()
        component = self.owners[path]['components'][0]
        self.assertEqual(component['component_id'], 'ffmpeg')
        self.assertEqual(component['license_expression'], 'GPL-2.0-or-later')


class CargoNoticeAuditTests(unittest.TestCase):
    def test_consistent_stage_receipt_cannot_replace_a_pinned_crate_notice(self):
        path = 'usr/share/licenses/rust/example-1.0.0/LICENSE'
        std = 'usr/share/licenses/rust/rust-std-1.97.1/LICENSE-MIT'
        records = [{'name': 'example', 'notices': [path]}]
        digest = hashlib.sha256(b'original notice').hexdigest()
        expected = {'owner': 'app-crate-source', 'source_sha256': digest}
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            offer = root / 'usr/share/licenses/furami/source-offer.json'
            offer.parent.mkdir(parents=True)
            offer.write_text(json.dumps({'rust_crates': records}))
            (root / 'manifest.json').write_text('{}')
            actual = {path: {'type': 'file', 'sha256': digest}, std: {'type': 'file'}}
            owners = {path: {**expected, 'shipped_sha256': digest}}
            with patch.object(audit.stage, 'crate_notice_inventory',
                              return_value=(records, {path: expected}, {})):
                audit.verify_crate_notices(root, root, actual, owners)
                owners['usr/share/licenses/furami/LICENSE'] = {'owner': 'app-crate-source'}
                with self.assertRaisesRegex(ValueError, 'crate notice ownership exists outside the verified inventory'):
                    audit.verify_crate_notices(root, root, actual, owners)
                del owners['usr/share/licenses/furami/LICENSE']
                replaced = hashlib.sha256(b'replacement notice').hexdigest()
                actual[path]['sha256'] = replaced
                owners[path].update(source_sha256=replaced, shipped_sha256=replaced)
                with self.assertRaisesRegex(ValueError, 'notice bytes or provenance differ'):
                    audit.verify_crate_notices(root, root, actual, owners)


class MediaProgramClosureTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name) / 'payload'
        self.root.mkdir()

    def file(self, relative, data=b'ELF contents'):
        target = self.root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        return target

    def metadata(self, path, needed=(), runpath=None):
        return {'path': path, 'needed': list(needed), 'runpath': runpath, 'rpath': None,
                'class': 'ELF64', 'machine': 'Advanced Micro Devices X86-64'}

    def hashes(self, *targets):
        return {relative: hashlib.sha256((self.root / relative).read_bytes()).hexdigest()
                for relative in targets}

    def test_seeded_program_missing_unsearched_and_escaped_paths_are_reported(self):
        self.file('usr/bin/mpv')
        self.file('usr/private/libstaged.so.9')
        entries = [self.metadata('usr/bin/mpv', needed=['libabsent.so.9', 'libstaged.so.9'],
                                 runpath='$ORIGIN/../lib'),
                   self.metadata('usr/private/libstaged.so.9')]
        warnings = []
        dependencies = audit.inventory.resolve_dependency_closure(
            str(self.root), entries, self.hashes('usr/bin/mpv', 'usr/private/libstaged.so.9'),
            [], ['../outside', 'usr/missing-lib'], warnings)
        self.assertEqual({entry['resolution']['kind'] for entry in dependencies}, {'missing'})
        self.assertIn('missing dependency: libabsent.so.9 (required by usr/bin/mpv)', warnings)
        self.assertTrue(any('not in loader search paths' in warning for warning in warnings))
        self.assertTrue(any('escapes staged root' in warning for warning in warnings))
        self.assertTrue(any('library search path absent' in warning for warning in warnings))


if __name__ == '__main__':
    unittest.main()
