"""Deb822 source metadata must retain signed checksum fields and notices."""
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("furami_sources", Path(__file__).with_name("sources.py"))
sources = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sources)


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


if __name__ == "__main__":
    unittest.main()
