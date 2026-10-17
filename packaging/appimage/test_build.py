"""Candidate and failure-receipt uploads never absorb private runtime inputs."""
import importlib.util
import contextlib
import hashlib
import subprocess
import sys
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace
import io
import tarfile

SPEC = importlib.util.spec_from_file_location('furami_build_upload_tests', Path(__file__).with_name('build.py'))
builder = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(builder)


class ReassemblyIsolationTests(unittest.TestCase):
    def test_overlapping_or_existing_output_refuses_without_running_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            parent = root / 'cold'
            parent.mkdir()
            marker = parent / 'raw'
            marker.write_bytes(b'untouched')
            with patch.object(builder.subprocess, 'run', side_effect=AssertionError('must not execute')):
                for output in (parent, parent / 'new'):
                    with self.subTest(output=output), self.assertRaises(ValueError):
                        builder.reassemble(parent, output)
            self.assertEqual(marker.read_bytes(), b'untouched')
            self.assertFalse((parent / 'new').exists())

    def test_changed_snapshot_refuses_before_output_or_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            parent = root / 'cold'
            app = parent / 'app'
            (app / 'source-snapshot').mkdir(parents=True)
            (app / 'manifest.json').write_text(json.dumps({'inputs': {'Cargo.toml': '0' * 64}}))
            (app / 'source-snapshot/Cargo.toml').write_bytes(b'changed')
            (parent / 'provenance.json').write_text(json.dumps({
                'app': str(app), 'media': str(parent / 'media'), 'runtime': str(parent / 'runtime'),
                'stage': str(parent / 'stage'), 'source_caches': {}}))
            original = (parent / 'provenance.json').read_bytes()
            with patch.object(builder.subprocess, 'run', side_effect=AssertionError('must not execute')):
                with self.assertRaisesRegex(ValueError, 'application input changed'):
                    builder.reassemble(parent, root / 'fresh')
            self.assertFalse((root / 'fresh').exists())
            self.assertEqual(original, (parent / 'provenance.json').read_bytes())

    def test_pair_comparison_cannot_create_output_inside_cold_root(self):
        comparison = builder.module(builder.HERE / 'build-twice.py', 'test_pair_comparison')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            parent = root / 'cold'
            parent.mkdir()
            args = SimpleNamespace(output=parent / 'new', reassemble_from=parent,
                                   source_cache=root / 'cache', runtime_source_cache=root / 'runtime-cache')
            with self.assertRaisesRegex(ValueError, 'disjoint'):
                comparison.compare(args)
            self.assertEqual(list(parent.iterdir()), [])

    def test_pair_overlap_and_cache_under_parent_refuse_before_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            parent = root / 'cold'
            parent.mkdir()
            with self.assertRaises(ValueError):
                builder.reassembly_pair_paths(parent, parent / 'new')
            self.assertFalse((parent / 'new').exists())
            for label in ('first', 'second'):
                side = parent / label
                side.mkdir()
                provenance = {key: str(side / key) for key in ('app', 'media', 'runtime', 'stage')}
                provenance['source_caches'] = {'compliance': str(parent / 'cache')}
                (side / 'provenance.json').write_text(json.dumps(provenance))
            with self.assertRaisesRegex(ValueError, 'disjoint'):
                builder.reassembly_pair_paths(parent, root / 'fresh')
            self.assertFalse((root / 'fresh').exists())

    def test_added_current_application_inputs_refuse_before_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            parent = root / 'cold'
            (parent / 'app').mkdir(parents=True)
            provenance = {key: str(parent / key) for key in ('app', 'media', 'runtime', 'stage')}
            provenance['source_caches'] = {}
            (parent / 'provenance.json').write_text(json.dumps(provenance))
            (parent / 'app/manifest.json').write_text('{"inputs": {}}')
            with self.assertRaisesRegex(ValueError, 'complete application input set changed'):
                builder.reassemble(parent, root / 'fresh')
            self.assertFalse((root / 'fresh').exists())

    def test_local_raw_derived_receipt_cannot_change_under_staged_hash(self):
        source = builder.module(builder.HERE / 'sources.py', 'test_staged_local_source')
        with tempfile.TemporaryDirectory() as directory:
            side = Path(directory)
            local = side / 'source-assets-local-assembly.json'
            local.write_text('{"loader_reports": ["raw and derived hashes"]}')
            receipt = side / 'source-reassembly.json'
            receipt.write_text(json.dumps({
                'mode': 'source-reassembly', 'local_assembly_receipt': {
                    'path': local.name, 'sha256': source.sha(local)}}))
            self.assertEqual(builder.reassembly_receipt_sha256(side), source.sha(receipt))
            local.write_text('{"loader_reports": []}')
            with self.assertRaisesRegex(ValueError, 'raw-to-derived'):
                builder.reassembly_receipt_sha256(side)
            local.unlink()
            with self.assertRaisesRegex(ValueError, 'raw-to-derived'):
                builder.reassembly_receipt_sha256(side)

    def test_changed_retained_ubuntu_or_static_evidence_refuses_before_output(self):
        source = builder.module(builder.HERE / 'sources.py', 'test_retained_evidence_source')
        for changed in ('ubuntu-sources.json', 'source-notice', 'build.ninja'):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                parent = root / 'cold'
                for folder in ('app', 'media', 'runtime', 'stage/ubuntu-source-evidence', 'release', 'audit'):
                    (parent / folder).mkdir(parents=True, exist_ok=True)
                provenance = {key: str(parent / key) for key in ('app', 'media', 'runtime', 'stage')}
                provenance.update(source_caches={}, version='fixture')
                (parent / 'provenance.json').write_text(json.dumps(provenance))
                (parent / 'app/manifest.json').write_text('{"inputs": {}}')
                (parent / 'runtime/manifest.json').write_text(json.dumps({
                    'lock_sha256': source.sha(builder.REPO / 'packaging/appimage-runtime/runtime.lock.json'),
                    'files_sha256': {}}))
                image = parent / 'release/Furami-vfixture-Linux-x64.AppImage'
                image.write_bytes(b'image')
                stage_json = parent / 'stage/ubuntu-sources.json'
                stage_json.write_text('{"packages": []}')
                notice = parent / 'stage/ubuntu-source-evidence/source-notice'
                notice.write_bytes(b'notice')
                ninja = parent / 'media/work/placebo-build/build.ninja'
                ninja.parent.mkdir(parents=True)
                ninja.write_bytes(b'ninja')
                pc = parent / 'media/prefix/lib/pkgconfig/libplacebo.pc'
                pc.parent.mkdir(parents=True)
                pc.write_bytes(b'pkgconfig')
                audit = parent / 'audit/audit.json'
                audit.write_text(json.dumps({'static_libplacebo': {
                    'build_ninja_sha256': source.sha(ninja), 'pkg_config_sha256': source.sha(pc)}}))
                materials = {'ubuntu-evidence/ubuntu-sources.json': stage_json.read_bytes(),
                             'ubuntu-evidence/source-notice': notice.read_bytes()}
                for name in source.LOADER_REPORTS:
                    data = b'\tlinux-vdso.so.1 (0x1234)\n'
                    (parent / 'media' / name).write_bytes(data)
                    materials['media-evidence/' + name] = data
                archive = parent / 'release/source-materials-001.tar'
                with tarfile.open(archive, 'w') as stream:
                    for name, data in materials.items():
                        info = tarfile.TarInfo(name)
                        info.size = len(data)
                        stream.addfile(info, io.BytesIO(data))
                index = parent / 'release/sources-index.json'
                index.write_text(json.dumps({'archives': [{'path': archive.name, 'sha256': source.sha(archive)}]}))
                (parent / 'release/release-ready.json').write_text(json.dumps({
                    'image_sha256': source.sha(image), 'technical_audit_sha256': source.sha(audit),
                    'sources_index_sha256': source.sha(index)}))
                {'ubuntu-sources.json': stage_json, 'source-notice': notice, 'build.ninja': ninja}[changed].write_bytes(b'changed')
                real_module = builder.module
                def load(path, name):
                    if Path(path).name == 'build-app.py':
                        return SimpleNamespace(input_hashes=lambda: {}, verify_media=lambda *args: None)
                    return real_module(path, name)
                with patch.object(builder, 'module', side_effect=load), \
                        patch.object(builder.subprocess, 'run', side_effect=AssertionError('must not execute')):
                    with self.assertRaisesRegex(ValueError, 'retained (Ubuntu|static link) evidence changed'):
                        builder.reassemble(parent, root / 'fresh')
                self.assertFalse((root / 'fresh').exists())


class HistoricalImageTests(unittest.TestCase):
    def fixture(self, root):
        source = builder.module(builder.HERE / 'sources.py', 'historical_sources')
        parent = root / 'cold'
        for folder in ('app', 'media', 'runtime', 'stage/ubuntu-source-evidence', 'release', 'audit'):
            (parent / folder).mkdir(parents=True, exist_ok=True)
        provenance = {key: str(parent / key) for key in ('app', 'media', 'runtime', 'stage')}
        provenance.update(version='0.1.0-rc.1', recipe_sha256={},
                          source_caches={'runtime': str(root / 'private')})
        source.dump(parent / 'provenance.json', provenance)
        source.dump(parent / 'app/manifest.json', {'inputs': {}})
        source.dump(parent / 'media/manifest.json', {})
        source.dump(parent / 'runtime/manifest.json', {
            'lock_sha256': source.sha(builder.REPO / 'packaging/appimage-runtime/runtime.lock.json'),
            'files_sha256': {}})
        image = parent / 'release/arbitrary-archived-name.AppImage'
        image.write_bytes(b'original image')
        materials = {}
        for name in source.LOADER_REPORTS:
            (parent / 'media' / name).write_bytes(b'raw loader evidence')
            materials['media-evidence/' + name] = b'raw loader evidence'
        stage = parent / 'stage/ubuntu-sources.json'
        stage.write_text('{"packages": []}')
        materials['ubuntu-evidence/ubuntu-sources.json'] = stage.read_bytes()
        static = {}
        for relative, key in (('work/placebo-build/build.ninja', 'build_ninja_sha256'),
                              ('prefix/lib/pkgconfig/libplacebo.pc', 'pkg_config_sha256')):
            path = parent / 'media' / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'static evidence')
            static[key] = source.sha(path)
        source.dump(parent / 'audit/audit.json', {'static_libplacebo': static})
        archive = parent / 'release/source-materials-001.tar'
        with tarfile.open(archive, 'w') as stream:
            for name, data in materials.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                stream.addfile(info, io.BytesIO(data))
        source.dump(parent / 'release/sources-index.json', {
            'archives': [{'path': archive.name, 'sha256': source.sha(archive)}]})
        source.dump(parent / 'release/release-ready.json', {
            'image_sha256': source.sha(image),
            'technical_audit_sha256': source.sha(parent / 'audit/audit.json'),
            'sources_index_sha256': source.sha(parent / 'release/sources-index.json')})
        return parent, image, source

    def test_invalid_historical_images_refuse_before_output(self):
        for mutation in ('missing', 'multiple', 'symlink', 'empty', 'changed', 'directory', 'version'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                parent, image, source = self.fixture(root)
                expected = 'exactly one'
                if mutation == 'missing':
                    image.unlink()
                elif mutation == 'multiple':
                    (image.parent / 'second.AppImage').symlink_to(image)
                elif mutation == 'symlink':
                    target = root / 'external'
                    target.write_bytes(image.read_bytes())
                    image.unlink()
                    image.symlink_to(target)
                    expected = 'regular, nonempty'
                elif mutation == 'directory':
                    image.unlink()
                    image.mkdir()
                    expected = 'regular, nonempty'
                elif mutation == 'empty':
                    image.write_bytes(b'')
                    expected = 'regular, nonempty'
                elif mutation == 'changed':
                    image.write_bytes(b'changed')
                    expected = 'stale original image'
                else:
                    provenance = json.loads((parent / 'provenance.json').read_text())
                    provenance['version'] = '../../escape'
                    source.dump(parent / 'provenance.json', provenance)
                    expected = 'safe filename component'
                real_module = builder.module
                def load(path, name):
                    if Path(path).name == 'build-app.py':
                        return SimpleNamespace(input_hashes=lambda: {}, verify_media=lambda *args: None)
                    return real_module(path, name)
                with patch.object(builder, 'module', side_effect=load), \
                        patch.object(builder.subprocess, 'run', side_effect=AssertionError('no commands')):
                    with self.assertRaisesRegex(ValueError, expected):
                        builder.reassemble(parent, root / 'fresh')
                self.assertFalse((root / 'fresh').exists())

    def test_historical_image_is_copied_to_fresh_audit_then_finalized_once(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            parent, original, source = self.fixture(root)
            before = {path.relative_to(parent): (path.read_bytes(), path.stat().st_mtime_ns)
                      for path in parent.rglob('*') if path.is_file()}
            out = root / 'fresh'
            canonical = 'Furami-v0.1.0-rc.1-Linux-x64.AppImage'
            def audit(image, provenance, destination):
                self.assertEqual(image, out / canonical)
                self.assertEqual(image.read_bytes(), original.read_bytes())
                destination.mkdir()
                source.dump(destination / 'manifest.json', {'image': {'path': image.name}})
                for name in ('audit.json', 'components.json'):
                    source.dump(destination / name, {})
                (destination / 'NOTICE.txt').write_text('fixture notice')
                return {'component_registry': {}}
            def assemble(provenance, audited, destination, caches, acquire):
                destination.mkdir()
                index = {'missing': [], 'material_policy': source.PUBLIC_MATERIAL_POLICY}
                source.dump(destination / 'sources-index.json', index)
                source.dump(out / 'source-assets-local-assembly.json', {})
                return index
            real_module = builder.module
            def load(path, name):
                if Path(path).name == 'build-app.py':
                    return SimpleNamespace(input_hashes=lambda: {}, verify_media=lambda *args: None)
                if Path(path).name == 'audit.py':
                    return SimpleNamespace(audit=audit)
                if name == 'furami_finalization_sources':
                    return SimpleNamespace(assemble=assemble, dump=source.dump,
                                           verify_public_materials=lambda *args: None)
                return real_module(path, name)
            with patch.object(builder, 'module', side_effect=load), \
                    patch.object(builder.subprocess, 'run', side_effect=AssertionError('no commands')):
                release = builder.reassemble(parent, out)
            self.assertEqual([path.name for path in release.glob('*.AppImage')], [canonical])
            self.assertFalse((out / canonical).exists())
            self.assertEqual((release / canonical).read_bytes(), original.read_bytes())
            self.assertEqual(json.loads((release / 'manifest.json').read_text())['image']['path'], canonical)
            ready = json.loads((release / 'release-ready.json').read_text())
            self.assertIs(ready['ready'], True)
            self.assertEqual(ready['image_sha256'], source.sha(release / canonical))
            for line in (release / 'SHA256SUMS').read_text().splitlines():
                checksum, name = line.split('  ')
                self.assertEqual(checksum, source.sha(release / name))
            self.assertEqual(before, {path.relative_to(parent): (path.read_bytes(), path.stat().st_mtime_ns)
                                      for path in parent.rglob('*') if path.is_file()})


class UploadIsolationTests(unittest.TestCase):
    def test_failed_producer_receipt_and_logs_are_collectable(self):
        app = builder.module(Path(__file__).with_name('build-app.py'), 'failed_app_fixture')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            media, out = root / 'media', root / 'out'
            output = out / 'first/app'
            output.mkdir(parents=True)
            for name in ('bin/qmake6', 'libexec/rcc'):
                tool = media / 'prefix' / name
                tool.parent.mkdir(parents=True, exist_ok=True)
                tool.write_bytes(b'fixture Qt tool')
            stack = json.loads(app.STACK_LOCK.read_text())
            commands = []
            real_run = subprocess.run

            def failed_container(command, **kwargs):
                commands.append(command)
                # A real child writes to run_build's real redirected log.
                return real_run([sys.executable, '-c',
                                 'import sys; print("fixture compiler failure"); sys.exit(23)'],
                                **kwargs)

            stderr = io.StringIO()
            with patch.object(app.subprocess, 'run', side_effect=failed_container), \
                    contextlib.redirect_stderr(stderr), \
                    self.assertRaisesRegex(RuntimeError, 'container exited 23'):
                app.run_build(media, output, root / 'sources', stack, 'first')
            receipt = json.loads((output / 'first/evidence/container-command.json').read_text())
            self.assertEqual(receipt, {'schema': 1, 'command': commands[0]})
            self.assertIn('fixture compiler failure', stderr.getvalue())
            (out / 'private.apk').write_bytes(b'PRIVATE BOOTSTRAP')
            (out / 'runtime').mkdir()
            (out / 'runtime/build.log').write_bytes(b'PRIVATE BOOTSTRAP')
            dest = root / 'evidence'
            builder.collect_evidence(out, dest, 'fixture')
            relative = 'first/app/first/build.log'
            self.assertEqual((dest / relative).read_bytes(),
                             (out / relative).read_bytes())
            metadata = json.loads((dest / 'executed-inputs.json').read_text())['logs'][0]
            self.assertEqual(metadata['path'], relative)
            self.assertEqual(metadata['truncated_bytes'], 0)
            self.assertEqual(metadata['sha256'], hashlib.sha256((dest / relative).read_bytes()).hexdigest())
            self.assertFalse(any(b'PRIVATE BOOTSTRAP' in path.read_bytes()
                                 for path in dest.rglob('*') if path.is_file()))

    def test_log_tail_archive_is_bounded_and_hashes_are_truthful(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            out, dest = root / 'out', root / 'evidence'
            log = out / 'app/first/build.log'
            log.parent.mkdir(parents=True)
            content = b'head\n' + b'x' * builder.MAX_LOG_BYTES + b'\nerror\n'
            log.write_bytes(content)
            builder.collect_evidence(out, dest, 'fixture')
            stored = (dest / 'app/first/build.log').read_bytes()
            self.assertEqual(stored, content[-builder.MAX_LOG_BYTES:])
            record = json.loads((dest / 'executed-inputs.json').read_text())['logs'][0]
            self.assertEqual(record['original_bytes'], len(content))
            self.assertEqual(record['stored_bytes'], len(stored))
            self.assertEqual(record['truncated_bytes'], len(content) - len(stored))
            self.assertEqual(record['original_sha256'], hashlib.sha256(content).hexdigest())
            self.assertEqual(record['sha256'], hashlib.sha256(stored).hexdigest())

    def test_log_symlinks_and_json_arrays_refuse_before_copying(self):
        for mutation in ('log-symlink', 'parent-escape', 'internal-parent-symlink', 'array'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                out, dest = root / 'out', root / 'evidence'
                out.mkdir()
                (out / 'reproducibility.json').write_text('[]' if mutation == 'array' else '{}')
                private = root / 'private'
                private.mkdir()
                (private / 'build.log').write_bytes(b'PRIVATE')
                if mutation == 'parent-escape':
                    (out / 'app').mkdir()
                    (out / 'app/first').symlink_to(private, target_is_directory=True)
                elif mutation == 'internal-parent-symlink':
                    (out / 'app').mkdir()
                    (out / 'app/first').symlink_to(out / 'elsewhere', target_is_directory=True)
                    (out / 'elsewhere').mkdir()
                    (out / 'elsewhere/build.log').write_bytes(b'PRIVATE')
                elif mutation == 'log-symlink':
                    (out / 'app/first').mkdir(parents=True)
                    (out / 'app/first/build.log').symlink_to(private / 'build.log')
                with self.assertRaises(ValueError):
                    builder.collect_evidence(out, dest, 'fixture')
                self.assertFalse(dest.exists())

    def test_private_runtime_cache_must_be_disjoint_from_both_upload_roots(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outputs = (root / 'candidate', root / 'evidence')
            builder.private_cache_outside(root / 'private-bootstrap', outputs)
            for cache in (outputs[0], outputs[0] / 'cache', outputs[1] / 'cache', root):
                with self.subTest(cache=cache), self.assertRaisesRegex(ValueError, 'disjoint'):
                    builder.private_cache_outside(cache, outputs)
            (root / 'alias').symlink_to(outputs[1], target_is_directory=True)
            with self.assertRaisesRegex(ValueError, 'disjoint'):
                builder.private_cache_outside(root / 'alias/cache', outputs)

    def test_failed_and_successful_builds_copy_only_allowed_json_receipts(self):
        for success in (False, True):
            with self.subTest(success=success), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                out, dest, private = root / 'build', root / 'evidence', root / 'private-bootstrap'
                private.mkdir()
                (private / 'compiler.apk').write_bytes(b'private APK bytes')
                (private / 'base-image.tar').write_bytes(b'private base image bytes')
                (out / 'first/app/first/evidence').mkdir(parents=True)
                (out / 'first/provenance.json').write_text(json.dumps({'source_caches': {'runtime': str(private)}}))
                (out / 'first/app/first/evidence/container-command.json').write_text('{"command": ["podman"]}')
                # Deliberate excluded files in the build tree too: collection is
                # an allowlist, not recursive JSON/archive discovery.
                (out / 'first/compiler.apk').write_bytes(b'private APK bytes')
                (out / 'first/base-image.tar').write_bytes(b'private base image bytes')
                (out / 'first/unlisted.json').write_text('{"do_not_upload": true}')
                if success:
                    (out / 'release').mkdir()
                    (out / 'release/release-ready.json').write_text('{"ready": true}')
                builder.collect_evidence(out, dest, 'fixture-commit')
                actual = {path.relative_to(dest).as_posix() for path in dest.rglob('*') if path.is_file()}
                expected = {'first/provenance.json', 'first/app/first/evidence/container-command.json', 'executed-inputs.json'}
                if success:
                    expected.add('release/release-ready.json')
                self.assertEqual(actual, expected)
                self.assertFalse(any(path.read_bytes() in (b'private APK bytes', b'private base image bytes')
                                     for path in dest.rglob('*') if path.is_file()))

    def test_cache_under_evidence_and_receipt_symlink_fail_before_copying(self):
        for mutation in ('cache', 'symlink', 'parent-symlink', 'binary-receipt'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                out, dest = root / 'build', root / 'evidence'
                private = dest / 'private' if mutation == 'cache' else root / 'private'
                private.mkdir(parents=True)
                out.mkdir()
                (out / 'provenance.json').write_text(json.dumps({'source_caches': {'runtime': str(private)}}))
                if mutation == 'symlink':
                    (private / 'receipt.json').write_text('{}')
                    (out / 'reproducibility.json').symlink_to(private / 'receipt.json')
                elif mutation == 'parent-symlink':
                    (private / 'build-inputs.json').write_text('{}')
                    (out / 'app').symlink_to(private, target_is_directory=True)
                elif mutation == 'binary-receipt':
                    (out / 'reproducibility.json').write_bytes(b'private APK bytes')
                with self.assertRaises(ValueError):
                    builder.collect_evidence(out, dest, 'fixture-commit')
                self.assertFalse((dest / 'provenance.json').exists())


if __name__ == '__main__':
    unittest.main()
