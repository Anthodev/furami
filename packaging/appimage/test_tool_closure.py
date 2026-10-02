"""Stage-only package gates: signed bytes and no changes to media APT identity."""
import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("furami_stage_tools", Path(__file__).with_name("tool-closure.py"))
tools = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tools)


class PackagingToolClosureTests(unittest.TestCase):
    def test_downloaded_packages_reject_unpinned_extra_or_modified_archive(self):
        with tempfile.TemporaryDirectory() as temporary:
            cache = Path(temporary)
            archive = cache / "file.deb"
            archive.write_bytes(b"signed archive")
            tool = {"packages": {"file": {"deb_filename": archive.name,
                                          "deb_sha256": hashlib.sha256(archive.read_bytes()).hexdigest()}}}
            media = {"downloaded_debs": [{"filename": "media.deb"}]}
            self.assertEqual(tools.downloaded(tool, media, cache)[archive.name], tool["packages"]["file"]["deb_sha256"])
            (cache / "unexpected.deb").write_bytes(b"outside lock")
            with self.assertRaisesRegex(ValueError, "stage-only package downloads differ"):
                tools.downloaded(tool, media, cache)
            (cache / "unexpected.deb").unlink()
            archive.write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "signed snapshot package SHA256 mismatch"):
                tools.downloaded(tool, media, cache)

    def test_installation_rejects_media_mutation_and_copyright_tampering(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "usr/bin/file"
            notice = root / "usr/share/doc/file/copyright"
            binary.parent.mkdir(parents=True)
            notice.parent.mkdir(parents=True)
            binary.write_bytes(b"versioned file tool")
            notice.write_bytes(b"copyright text")
            tool = {"packages": {"file": {"version": "1:5.45-3build1", "architecture": "amd64",
                                          "deb_filename": "file_1%3a5.45-3build1_amd64.deb",
                                          "deb_sha256": "a" * 64,
                                          "files_sha256": {"/usr/bin/file": tools.sha(binary)},
                                          "copyright_sha256": tools.sha(notice)}}}
            baseline = "libc6\t2.39\tamd64\n"
            current = baseline + "file\t1:5.45-3build1\tamd64\n"
            self.assertEqual(tools.installed(tool, baseline, current, root)["file"]["distribution"],
                             "build-only; not shipped")
            with self.assertRaisesRegex(ValueError, "changed media closure"):
                tools.installed(tool, baseline, "libc6\t2.40\tamd64\nfile\t1:5.45-3build1\tamd64\n", root)
            notice.write_bytes(b"changed notice")
            with self.assertRaisesRegex(ValueError, "copyright bytes mismatch"):
                tools.installed(tool, baseline, current, root)


if __name__ == "__main__":
    unittest.main()
