import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import release_notes


class ReleaseNotesTests(unittest.TestCase):
    def test_sections_and_fences_are_exact(self):
        text = '## [Unreleased]\nfuture\n## [0.1.0-rc.1]\ncandidate\n```md\n## [0.1.0-rc.1]\nfenced\n```\n## [0.1.0]\nstable\n'
        self.assertEqual(release_notes.extract_release_notes(text, '0.1.0-rc.1'), 'candidate\n```md\n## [0.1.0-rc.1]\nfenced\n```\n')
        self.assertEqual(release_notes.extract_release_notes(text, '0.1.0'), 'stable\n')
        for invalid in ('## [0.1.0-rc.1]\n', '## [0.1.0]\nstable\n',
                        '## [0.1.0-rc.1]\none\n## [0.1.0-rc.1]\ntwo\n'):
            with self.assertRaises(release_notes.ReleaseNotesError):
                release_notes.extract_release_notes(invalid, '0.1.0-rc.1')

    def git_fixture(self, root, changelog):
        env = {**os.environ, 'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.invalid',
               'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.invalid'}
        def git(*args):
            return subprocess.run(['git', *args], cwd=root, env=env,
                                  check=True, capture_output=True, text=True).stdout.strip()
        git('init', '-q')
        (root / 'CHANGELOG.md').write_text(changelog)
        git('add', 'CHANGELOG.md')
        git('-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture')
        return git, env, git('rev-parse', 'HEAD')

    def notes_cli(self, root, env, tag, commit):
        return subprocess.run(
            [sys.executable, str(Path(release_notes.__file__).resolve()), '--tag', tag,
             '--commit', commit, '--output', str(root / 'notes')],
            cwd=root, env=env, capture_output=True, text=True)

    def test_cli_reads_both_sections_from_tagged_commit_for_both_tag_kinds(self):
        for annotated in (False, True):
            with self.subTest(annotated=annotated), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                git, env, commit = self.git_fixture(
                    root, '## [Unreleased]\nfuture\n## [0.1.0-rc.1] - 2026-10-17\n'
                    'tagged candidate\n```md\n## [0.1.0]\nfenced stable heading\n```\n'
                    '## [0.1.0] - 2026-10-17\ntagged stable\n')
                for tag in ('v0.1.0-rc.1', 'v0.1.0'):
                    git('-c', 'tag.gpgsign=false', 'tag',
                        *(['-a', '-m', 'release fixture'] if annotated else []), tag)
                (root / 'CHANGELOG.md').write_text('working tree is not release notes')
                for tag, body in [
                        ('v0.1.0-rc.1', 'tagged candidate\n```md\n## [0.1.0]\nfenced stable heading\n```\n'),
                        ('v0.1.0', 'tagged stable\n')]:
                    result = self.notes_cli(root, env, tag, commit)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual((root / 'notes').read_text(), body)

    def test_cli_refuses_wrong_commit_and_missing_empty_or_duplicate_exact_sections(self):
        for version in ('0.1.0', '0.1.0-rc.1'):
            for mutation in ('missing', 'empty', 'duplicate', 'wrong-commit'):
                with self.subTest(version=version, mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    other = '0.1.0-rc.1' if version == '0.1.0' else '0.1.0'
                    text = f'## [Unreleased]\nfuture\n## [{other}]\nwrong channel\n'
                    if mutation == 'empty':
                        text += f'## [{version}]\n \n'
                    elif mutation == 'duplicate':
                        text += f'## [{version}]\none\n## [{version}]\ntwo\n'
                    elif mutation == 'wrong-commit':
                        text += f'## [{version}]\nvalid tagged body\n'
                    git, env, commit = self.git_fixture(root, text)
                    git('-c', 'tag.gpgsign=false', 'tag', 'v' + version)
                    if mutation == 'wrong-commit':
                        git('-c', 'commit.gpgsign=false', 'commit', '--allow-empty', '-qm', 'new commit')
                        commit = git('rev-parse', 'HEAD')
                    (root / 'CHANGELOG.md').write_text(f'## [{version}]\nworking tree cannot rescue refusal\n')
                    result = self.notes_cli(root, env, 'v' + version, commit)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn({'missing': 'no ', 'empty': 'is empty', 'duplicate': 'duplicate ',
                                   'wrong-commit': 'expected '}[mutation], result.stderr)
                    self.assertFalse((root / 'notes').exists())

    def test_cli_refuses_malformed_tag_grammar(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _, env, commit = self.git_fixture(root, '## [0.1.0]\nvalid\n')
            for tag in ('v01.1.0', 'v0.01.0', 'v0.1.00', 'v0.1.0-rc.0', 'v0.1.0-rc.01',
                        'v0.1.0-beta.1', 'v0.1.0+build', 'v0.1.0\n', 'v0.1.0-rc.1\n'):
                with self.subTest(tag=tag):
                    result = self.notes_cli(root, env, tag, commit)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('not a release tag', result.stderr)
                    self.assertFalse((root / 'notes').exists())
