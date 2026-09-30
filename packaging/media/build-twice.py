#!/usr/bin/env python3
"""Build two fresh prefixes, reject any changed or missing deliverable SHA256."""

import argparse
import json
import sys
from pathlib import Path

from build import build, compare_deliverables, sha256


def main(root):
    root.mkdir(parents=True, exist_ok=True)
    source_cache = root / "source-cache"
    first, second = root / "run-1", root / "run-2"
    build(first, source_cache)
    build(second, source_cache)
    entries = compare_deliverables(first / "prefix", second / "prefix")
    # Build provenance must match, not merely machine code. Independent installs
    # check APT archive package hashes and the exact resolved dependency closure.
    for name in ("apt-installed.tsv", "apt-debs.sha256", "apt-direct-packages.txt",
                 "x11-pkg-config.tsv", "build-flags.txt"):
        if sha256(first / name) != sha256(second / name):
            raise ValueError(f"non-reproducible build provenance: {name}")
    result = {"schema": 1, "first": str(first), "second": str(second),
              "deliverables": entries,
              "apt_packages_sha256": sha256(first / "apt-installed.tsv"),
              "apt_debs_sha256": sha256(first / "apt-debs.sha256")}
    (root / "reproducibility.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(f"Identical SHA256 for {len(entries)} deliverables; {root / 'reproducibility.json'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="new directory for two fresh isolated builds")
    args = parser.parse_args()
    try:
        main(args.root.resolve())
    except (OSError, ValueError, RuntimeError) as error:
        sys.exit(str(error))
