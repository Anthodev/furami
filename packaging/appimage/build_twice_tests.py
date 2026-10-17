import importlib.util
import hashlib
import io
import tarfile
import json
import sys
import shutil
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('furami_twice_tests', Path(__file__).with_name('build-twice.py'))
comparison = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(comparison)
support = comparison.builder.module(Path(__file__).with_name('test_sources.py'), 'furami_compare_source_fixtures')


class ComparisonTests(unittest.TestCase):
    def trees(self, root):
        for label in ('first', 'second'):
            tree = root / label
            (tree / 'release').mkdir(parents=True)
            (tree / 'runtime').mkdir()
            (tree / 'app').mkdir()
            (tree / 'release/candidate.AppImage').write_bytes(b'exact-image')
            (tree / 'release/manifest.json').write_text(json.dumps({'payload': {'.': {'type': 'directory', 'mode': '0o755'}, 'app': {'sha256': 'a'}}}))
            (tree / 'app/manifest.json').write_text(json.dumps({'builds': {'generated_cpp_first': {'header.h': 'a'}}}))
            for name in ('runtime-x86_64', 'link.map', 'apk-installed.txt'):
                (tree / 'runtime' / name).write_bytes(b'constant')
            (tree / 'runtime/manifest.json').write_text('{}')
            self.source_tar(tree / 'release/source-materials-001.tar', b'original')

    def source_tar(self, path, content):
        with tarfile.open(path, 'w', format=tarfile.USTAR_FORMAT) as tar:
            entry = tarfile.TarInfo('source/file')
            entry.size = len(content)
            entry.mode = 0o644
            tar.addfile(entry, io.BytesIO(content))

    def test_real_output_trees_detect_each_material_mismatch(self):
        for mutation in ('none', 'payload', 'root-mode', 'runtime', 'generated', 'source'):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                self.trees(root)
                second = root / 'second'
                if mutation in ('payload', 'root-mode'):
                    path = second / 'release/manifest.json'
                    value = json.loads(path.read_text())
                    if mutation == 'root-mode':
                        value['payload']['.']['mode'] = '0o700'
                    else:
                        value['payload']['app']['sha256'] = 'b'
                    path.write_text(json.dumps(value))
                elif mutation == 'runtime':
                    (second / 'runtime/runtime-x86_64').write_bytes(b'changed')
                elif mutation == 'generated':
                    (second / 'app/manifest.json').write_text(json.dumps({'builds': {'generated_cpp_first': {'header.h': 'b'}}}))
                elif mutation == 'source':
                    self.source_tar(second / 'release/source-materials-001.tar', b'changed!')
                verdict = comparison.compare_outputs(root)
                self.assertEqual(verdict['status'], 'identical' if mutation == 'none' else 'mismatch')
                self.assertEqual(verdict['byte_identity'], mutation == 'none')
                self.assertFalse((root / 'release').exists())

    def test_real_runtime_metadata_variance_is_documented_not_identical(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.trees(root)
            canonical = root / 'recipe'
            shutil.copytree(comparison.HERE.parent, canonical / 'packaging',
                            ignore=shutil.ignore_patterns('__pycache__'))
            _, _, public_members = support.runtime_source_fixture(canonical, root / 'private-inputs')
            shutil.rmtree(root / 'private-inputs')
            for label, token in (('first', 'abcdef'), ('second', '123456')):
                runtime = root / label / 'runtime'
                (runtime / 'link.map').write_text('LOAD /tmp/runtime-' + token + '.o\n')
                digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
                manifest = {'files_sha256': {'link.map': digest(runtime / 'link.map')},
                            'artifact': {'sha256': digest(runtime / 'runtime-x86_64')}}
                (runtime / 'manifest.json').write_text(json.dumps(manifest))
                archive = root / label / 'release/source-materials-001.tar'
                with tarfile.open(archive, 'a', format=tarfile.USTAR_FORMAT) as tar:
                    for name in ('link.map', 'manifest.json'):
                        content = (runtime / name).read_bytes()
                        entry = tarfile.TarInfo('runtime-evidence/' + name)
                        entry.size = len(content)
                        entry.mode = 0o644
                        tar.addfile(entry, io.BytesIO(content))
                    for name, content in public_members.items():
                        entry = tarfile.TarInfo(name)
                        entry.size = len(content)
                        entry.mode = 0o644
                        tar.addfile(entry, io.BytesIO(content))
            verdict = comparison.compare_outputs(root)
            self.assertEqual(verdict['status'], 'documented_metadata_variance')
            self.assertIs(verdict['byte_identity'], False)
            self.assertTrue(comparison.accepted_criteria(verdict['criteria']))
            self.assertTrue(verdict['metadata_variance'])
            # Feed the real producer receipt to the recipient guard: only
            # runtime source provenance differs, while image bytes agree.
            release = root / 'second/release'
            image = release / 'Furami-v0.1.0-rc.1-Linux-x64.AppImage'
            (release / 'candidate.AppImage').rename(image)
            (release / 'audit.json').write_text('{}')
            (release / 'NOTICE.txt').write_text('fixture notices\n')
            (release / 'RELINKING.md').write_text('fixture relinking instructions\n')
            policy = dict(comparison.sources_recipe.PUBLIC_MATERIAL_POLICY)
            inventory = comparison.sources_recipe.source_inventory(release)
            (release / 'sources-index.json').write_text(json.dumps({
                'complete': True, 'missing': [], 'material_policy': policy,
                'files': [{'path': name, 'sha256': row['sha256'], 'size': row['size']}
                          for name, row in inventory.items()],
                'archives': [{'path': name, 'sha256': value}
                             for name, value in verdict['raw_source_archive_sha256'][1].items()]}))
            (release / 'release-ready.json').write_text(json.dumps({
                'ready': True, 'material_policy': policy, 'image_sha256': digest(image),
                'technical_audit_sha256': digest(release / 'audit.json'),
                'sources_index_sha256': digest(release / 'sources-index.json')}))
            (release / 'reproducibility.json').write_text(json.dumps(verdict))
            (release / 'SHA256SUMS').write_text(''.join(
                f'{digest(path)}  {path.name}\n' for path in sorted(release.iterdir())
                if path.name != 'SHA256SUMS'))
            scripts = Path(__file__).resolve().parents[2] / 'scripts'
            with patch.object(sys, 'path', [str(scripts), *sys.path]):
                publisher = comparison.builder.module(scripts / 'publish_candidate.py',
                                                       'furami_source_variance_publication_test')
            self.assertIs(verdict['criteria']['image_bytes'], True)
            self.assertNotEqual(*verdict['raw_source_archive_sha256'])
            with patch.object(publisher, 'REPO', canonical):
                assets = publisher.verify_candidate(release, '0.1.0-rc.1', require_reproducibility=True)
            self.assertEqual(assets, sorted(release.iterdir()))

    def criteria(self):
        return dict.fromkeys(('image_bytes', 'payload_files_modes_symlinks', 'source_runtime_bytes',
                              'source_assets_content', 'source_archive_bytes_except_proven_metadata', 'generated_CPP_headers'), True)

    def test_exact_predicate_accepts_only_proven_variance(self):
        criteria = self.criteria()
        self.assertTrue(comparison.accepted_criteria(criteria))
        criteria.update(image_bytes=False, image_known_metadata_only=True)
        self.assertTrue(comparison.accepted_criteria(criteria))
        for key in self.criteria():
            invalid = dict(criteria)
            invalid.pop(key)
            self.assertFalse(comparison.accepted_criteria(invalid))
            invalid[key] = 1
            self.assertFalse(comparison.accepted_criteria(invalid))
        criteria['image_known_metadata_only'] = 1
        self.assertFalse(comparison.accepted_criteria(criteria))

    def test_byte_range_exception_does_not_ignore_other_bytes_or_size(self):
        with tempfile.TemporaryDirectory() as temporary:
            a, b = Path(temporary) / 'a', Path(temporary) / 'b'
            a.write_bytes(b'prefixAAAApostfix')
            b.write_bytes(b'prefixBBBBpostfix')
            self.assertTrue(comparison.equal_except_metadata(a, b, [(6, 4)]))
            b.write_bytes(b'PrefixBBBBpostfix')
            self.assertFalse(comparison.equal_except_metadata(a, b, [(6, 4)]))
            b.write_bytes(b'prefixBBBBpostfix!')
            self.assertFalse(comparison.equal_except_metadata(a, b, [(6, 4)]))

    def test_mismatch_preserves_receipt_without_delivery(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'out'
            args = type('Args', (), {'output': root, 'source_cache': Path(temporary) / 'cache',
                                    'runtime_source_cache': Path(temporary) / 'private-runtime',
                                    'jobs': 2, 'version': '0.1.0-rc.1', 'acquire_sources': True})()
            criteria = self.criteria()
            criteria['payload_files_modes_symlinks'] = False
            verdict = {'status': 'mismatch', 'criteria': criteria}
            with patch.object(comparison.subprocess, 'run') as runs, patch.object(comparison, 'compare_outputs', return_value=verdict):
                with self.assertRaisesRegex(ValueError, 'independent build mismatch'):
                    comparison.compare(args)
            self.assertEqual(json.loads((root / 'reproducibility.json').read_text()), verdict)
            self.assertFalse((root / 'release').exists())
            self.assertEqual(len(runs.call_args_list), 2)
            for invocation in runs.call_args_list:
                command = invocation.args[0]
                self.assertEqual(command[command.index('--runtime-source-cache') + 1], str(args.runtime_source_cache))
