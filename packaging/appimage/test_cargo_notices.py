"""Offline tests for exact crate and supplemental notice identities."""
import hashlib
import io
import json
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import sources
import stage


REGISTRY = 'registry+https://github.com/rust-lang/crates.io-index'
LOCK = 'packaging/appimage/sources.lock.json'


def digest(data):
    return hashlib.sha256(data).hexdigest()


class CargoNoticeFixture:
    def __init__(self, root):
        self.root = root
        self.repo = root / 'repo'
        self.app = root / 'app'
        self.snapshot = self.app / 'source-snapshot'
        self.appdir = root / 'AppDir'
        self.appdir.mkdir()
        self.crates = []
        self.policy = {'schema': 1, 'cargo_notices': []}

    def crate(self, name, members, *, role='shipped-code-candidate', declared=None):
        slug = name + '-1.0.0'
        path = self.app / 'sources' / (slug + '.crate')
        path.parent.mkdir(exist_ok=True, parents=True)
        with tarfile.open(path, 'w:gz') as archive:
            for relative, data in members.items():
                member = tarfile.TarInfo(slug + '/' + relative)
                if isinstance(data, bytes):
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
                else:
                    member.type = tarfile.SYMTYPE
                    member.linkname = data
                    archive.addfile(member)
        row = {'name': name, 'version': '1.0.0', 'source': REGISTRY,
               'checksum': sources.sha(path), 'license_expression': 'MIT',
               'license_file': declared, 'role': role, 'vendor_path': 'first/vendor/' + slug}
        self.crates.append(row)
        return row

    def supplement(self, crate, name, data, *, member=None, unresolved=None):
        file = 'packaging/appimage/notices/example/' + name
        path = self.repo / file
        path.parent.mkdir(exist_ok=True, parents=True)
        path.write_bytes(data)
        row = {key: crate[key] for key in ('name', 'version', 'source', 'checksum')}
        notice = {'file': file, 'sha256': digest(data), 'origin': {
            'kind': 'upstream', 'repository': 'https://example.invalid/project',
            'revision': 'b' * 40, 'path': 'LICENSE'}}
        if member:
            notice['crate_member'] = member
        row['notices'] = [notice]
        if unresolved:
            row['unresolved'] = unresolved
        self.policy['cargo_notices'].append(row)
        return notice

    def snapshot_inputs(self):
        lock = self.repo / LOCK
        lock.parent.mkdir(exist_ok=True, parents=True)
        lock.write_text(json.dumps(self.policy))
        (self.repo / 'Cargo.lock').write_text('version = 4\n' + '\n'.join(
            '\n[[package]]\n' + '\n'.join(f'{key} = {json.dumps(row[key])}'
                                           for key in ('name', 'version', 'source', 'checksum'))
            for row in self.crates))
        shutil.copytree(self.repo, self.snapshot)
        inputs = {p.relative_to(self.repo).as_posix(): sources.sha(p)
                  for p in self.repo.rglob('*') if p.is_file()}
        return {'inputs': inputs, 'cargo_lock_sha256': inputs['Cargo.lock'], 'crates': self.crates}

    def stage(self, manifest, owners):
        with patch.object(stage, 'REPO', self.repo):
            return stage.crate_notices(self.app, manifest, self.appdir, owners)


class CargoNoticeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.fixture = CargoNoticeFixture(Path(temporary.name))

    def test_exact_archive_members_and_supplement_keep_distinct_origins(self):
        f = self.fixture
        crate = f.crate('example', {'legal/nested.txt': b'archived\r\n', 'src/borrowed.rs': b'header'},
                        declared='legal/nested.txt')
        note = f.supplement(crate, 'BORROWED', b'immutable upstream\n', member='src/borrowed.rs')
        not_selected = f.crate('unused', {'LICENSE': b'not a runtime or build dependency'}, role='not-selected')
        owners = {}
        records = f.stage(f.snapshot_inputs(), owners)
        prefix = 'usr/share/licenses/rust/example-1.0.0/'
        archived, borrowed = prefix + 'legal/nested.txt', prefix + 'BORROWED'
        self.assertEqual((f.appdir / archived).read_bytes(), b'archived\r\n')
        self.assertEqual((f.appdir / borrowed).read_bytes(), b'immutable upstream\n')
        self.assertEqual(owners[archived]['source'], 'sources/example-1.0.0.crate:example-1.0.0/legal/nested.txt')
        self.assertEqual(owners[borrowed]['source'], note['file'])
        self.assertEqual(owners[borrowed]['origin'], note['origin'])
        self.assertEqual(owners[borrowed]['crate_member'], 'src/borrowed.rs')
        self.assertEqual(owners[borrowed]['crate']['checksum'], crate['checksum'])
        self.assertEqual(records[0]['notices'], sorted([archived, borrowed]))
        self.assertEqual(records[1]['name'], not_selected['name'])
        self.assertEqual(records[1]['notices'], [])

    def test_missing_text_reports_all_linked_crates_without_writing(self):
        f = self.fixture
        f.crate('absent', {})
        incomplete = f.crate('incomplete', {})
        f.supplement(incomplete, 'LICENSE', b'partial', unresolved='borrowed notice absent')
        f.crate('macro', {}, role='build-only')
        with self.assertRaisesRegex(ValueError, r'absent-1\.0\.0.*incomplete-1\.0\.0.*borrowed notice absent'):
            f.stage(f.snapshot_inputs(), {})
        self.assertEqual(list(f.appdir.iterdir()), [])

    def test_wrong_crate_identity_cannot_pick_up_supplement(self):
        f = self.fixture
        crate = f.crate('example', {})
        f.supplement(crate, 'LICENSE', b'notice')
        f.policy['cargo_notices'][0]['checksum'] = '0' * 64
        with self.assertRaisesRegex(ValueError, 'Cargo.lock.*example'):
            f.stage(f.snapshot_inputs(), {})

    def test_archive_bytes_not_vendor_cannot_be_substituted(self):
        f = self.fixture
        f.crate('example', {'LICENSE': b'original'})
        manifest = f.snapshot_inputs()
        (f.app / 'sources/example-1.0.0.crate').write_bytes(b'wrong archive')
        with self.assertRaisesRegex(ValueError, 'SHA256.*example'):
            f.stage(manifest, {})

    def test_snapshot_cannot_use_modified_notice_or_parent_symlink(self):
        f = self.fixture
        crate = f.crate('example', {})
        note = f.supplement(crate, 'LICENSE', b'notice')
        manifest = f.snapshot_inputs()
        path = f.snapshot / note['file']
        path.write_bytes(b'changed')
        with self.assertRaisesRegex(ValueError, 'SHA256.*LICENSE'):
            f.stage(manifest, {})
        path.unlink()
        path.parent.rmdir()
        path.parent.symlink_to((f.repo / note['file']).parent, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'regular nonsymlink'):
            f.stage(manifest, {})

    def test_archive_notice_symlink_is_not_followed(self):
        f = self.fixture
        f.crate('example', {'LICENSE': 'src/borrowed.rs', 'src/borrowed.rs': b'bytes'})
        with self.assertRaisesRegex(ValueError, 'regular.*LICENSE'):
            f.stage(f.snapshot_inputs(), {})

    def test_supplement_cannot_replace_archive_member(self):
        f = self.fixture
        crate = f.crate('example', {'LICENSE': b'archive'})
        f.supplement(crate, 'LICENSE', b'supplement')
        with self.assertRaisesRegex(ValueError, 'collision.*LICENSE'):
            f.stage(f.snapshot_inputs(), {})

    def test_existing_destination_is_never_overwritten(self):
        f = self.fixture
        f.crate('example', {'LICENSE': b'archive'})
        manifest = f.snapshot_inputs()
        target = f.appdir / 'usr/share/licenses/rust/example-1.0.0/LICENSE'
        target.parent.mkdir(parents=True)
        target.write_bytes(b'already present')
        with self.assertRaisesRegex(ValueError, 'collision.*LICENSE'):
            f.stage(manifest, {})
        self.assertEqual(target.read_bytes(), b'already present')
