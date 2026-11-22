"""Ubuntu observation is factual evidence, never release terms clearance."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import stage
import sources


class UbuntuObservationTests(unittest.TestCase):
    def test_observation_records_shipped_elf_sources_and_notice_despite_stale_terms(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            appdir, output, indexes = root / 'AppDir', root / 'output', root / 'indexes'
            for path in (appdir / 'usr/lib', output, indexes):
                path.mkdir(parents=True)
            (indexes / 'snapshot_InRelease').write_bytes(b'APT-verified release fixture')
            index = indexes / 'snapshot_source_Sources.lz4'
            index.write_bytes(b'APT-verified index fixture')
            elf = 'usr/lib/libexample.so.1'
            (appdir / elf).write_bytes(b'\x7fELFexample')
            owners = {elf: {'owner': 'ubuntu:libexample1=2:1.0-1',
                            'ubuntu_path': '/usr/lib/x86_64-linux-gnu/libexample.so.1.0'}}
            app_manifest = {'binary': {'direct_objects': []}}
            source_record = ('Package: example\nVersion: 2:1.0-1\nDirectory: pool/main/e/example\n'
                             'Checksums-Sha256:\n ' + 'a' * 64 + ' 321 example.dsc\n')
            notice_root = root / 'doc'
            for package in ('libexample1', 'glslang-dev', 'spirv-tools'):
                package_doc = notice_root / package
                package_doc.mkdir(parents=True)
                (package_doc / 'copyright').write_text('Files: *\nLicense: Example\n')

            def command(argv):
                if argv[0] == 'dpkg-query':
                    return 'example\t2:1.0-1\t2:1.0-1\n'
                self.assertEqual(argv, ['apt-cache', 'showsrc', 'example'])
                return source_record

            with (patch.object(sources, 'APT_LISTS', indexes),
                  patch.object(sources, 'UBUNTU_DOC', notice_root),
                  patch.object(sources, 'command', side_effect=command),
                  patch.object(sources, 'load_ubuntu_terms', return_value={'packages': {'old@1': {}}})):
                sources.observe_ubuntu(appdir, owners, app_manifest, '20250101T000000Z', output)
                self.assertFalse((output / 'ubuntu-sources.json').exists())
                observed = json.loads((output / 'ubuntu-observation.json').read_text())
                self.assertIs(observed['release_eligible'], False)
                self.assertEqual(observed['indexes_sha256'][index.name], sources.sha(index))
                row = next(row for row in observed['packages'] if row['binary_package'] == 'libexample1')
                self.assertEqual(row['binary_version'], '2:1.0-1')
                self.assertEqual((row['source_package'], row['source_version']), ('example', '2:1.0-1'))
                self.assertEqual(row['shipped_objects'], [elf])
                self.assertEqual(row['object_owners'][elf], owners[elf])
                self.assertEqual(row['files'], [{'archive': 'example.dsc', 'sha256': 'a' * 64, 'size': 321,
                    'url': 'https://snapshot.ubuntu.com/ubuntu/20250101T000000Z/pool/main/e/example/example.dsc'}])
                copied = output / 'ubuntu-observation-evidence/libexample1.copyright'
                self.assertEqual(copied.read_bytes(), (notice_root / 'libexample1/copyright').read_bytes())
                self.assertEqual(row['notice'], {'path': copied.name, 'sha256': sources.sha(copied)})

                app = root / 'app'
                app.mkdir()
                (app / 'manifest.json').write_text(json.dumps(app_manifest))
                (root / 'evidence.json').write_text(json.dumps({'component_sources': owners,
                    'ubuntu_base_image': {'apt_snapshot': '20250101T000000Z'}}))
                with self.assertRaisesRegex(ValueError, 'Ubuntu terms identity absent'):
                    sources.capture_ubuntu(root, app, output)
                self.assertFalse((output / 'ubuntu-sources.json').exists())

    def test_container_observation_uses_disposable_elf_tree_without_release_tools(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            out = root / 'out'
            out.mkdir()
            closure = root / 'packaging/media/build-apt-closure.json'
            closure.parent.mkdir(parents=True)
            closure.write_text(json.dumps({'installed': [], 'downloaded_debs': []}))
            (out / 'ubuntu-apt.log').touch()
            (out / 'base-image-provenance.json').write_text(json.dumps({
                'image': 'pinned-image', 'probe_sha256': stage.digest(stage.HERE / 'base-image-inventory.sh'),
                'installed_rows_checked': 0, 'downloaded_deb_rows': 0}))
            media = {'container': 'pinned-image', 'apt_snapshot': 'frozen', 'artifacts': {}}
            scratch = []

            def observe(appdir, owners, app_manifest, snapshot, output):
                self.assertTrue(appdir.is_dir())
                self.assertFalse(appdir.is_relative_to(output))
                self.assertEqual(owners, {})
                self.assertEqual(app_manifest, {})
                self.assertEqual(snapshot, 'frozen')
                self.assertEqual(output, out)
                scratch.append(appdir)
                (output / 'ubuntu-observation.json').touch()

            with (patch.object(stage, 'REPO', root),
                  patch.object(stage, 'check_inputs', return_value=({}, media)),
                  patch.object(stage, 'read_manifest', return_value=({}, None)),
                  patch.object(stage, 'stage_elf_roots') as seed,
                  patch.object(stage, 'stage_libraries') as libraries,
                  patch.object(sources, 'observe_ubuntu', side_effect=observe)):
                stage.internal(root / 'media', root / 'app', root / 'runtime',
                               out, '1.0.0', None, capture_ubuntu_only=True)

            self.assertEqual(len(scratch), 1)
            self.assertFalse(scratch[0].exists())
            self.assertEqual(seed.call_args.args[3], scratch[0])
            self.assertEqual(libraries.call_args.args[2], scratch[0])
            self.assertEqual({item.name for item in out.iterdir()},
                             {'ubuntu-apt.log', 'base-image-provenance.json', 'ubuntu-observation.json'})
