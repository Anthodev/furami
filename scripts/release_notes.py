#!/usr/bin/env python3
"""Extract release notes for a stable tag from the tagged CHANGELOG.md.

Contract:
- Strict stable tag format: vMAJOR.MINOR.PATCH with no leading zeros.
- The CI-provided commit SHA must equal refs/tags/<tag>^{commit} (covers
  lightweight and annotated tags).
- The changelog is read from the tagged commit via ``git show``, never from
  the working tree.
- The section heading must match ``## [X.Y.Z]`` exactly (optional date
  suffix); 1.2.0 must not match 1.20.0.
- The body runs to the next level-two heading outside fenced code blocks.
- Missing, empty, or duplicate version sections are fatal. There is no
  fallback to commit-generated notes.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys

TAG_RE = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
SECTION_RE_TEMPLATE = r"^## \[{version}\]( - .*)?$"
H2_RE = re.compile(r"^## ")


class ReleaseNotesError(Exception):
    """A contract violation. Rendered directly as the CLI error."""


FENCE_OPEN_RE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")


def _fence_open(line: str) -> tuple[str, int] | None:
    """CommonMark opening fence: 0-3 leading spaces, 3+ same chars.

    A backtick fence's info string may not contain a backtick; tilde
    fences have no info-string restriction.
    """
    match = FENCE_OPEN_RE.match(line)
    if not match:
        return None
    fence_char = match.group(1)[0]
    if fence_char == "`" and "`" in match.group(2):
        return None
    return fence_char, len(match.group(1))


def _fence_close(line: str, fence_char: str, fence_len: int) -> bool:
    """CommonMark closing fence: 0-3 leading spaces, same char, at least
    the opener's length, nothing but whitespace after."""
    match = re.match(r"^ {0,3}", line)
    stripped = line[match.end():].rstrip()
    return len(stripped) >= fence_len and set(stripped) == {fence_char}


def extract_release_notes(changelog_text: str, version: str) -> str:
    """Return the body of the unique ``## [version]`` section.

    Pure function: no repository or filesystem access.

    The whole document is scanned line by line. Level-two headings inside
    fenced code blocks (backtick or tilde) are ignored, both when locating
    the target section and when detecting duplicates. A fence closes only
    on a line of the same character, at least the opener's length, with no
    trailing content.
    """
    lines = changelog_text.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    heading_re = re.compile(SECTION_RE_TEMPLATE.format(version=re.escape(version)))

    matches: list[int] = []  # line indexes of target headings outside fences
    fence_char = ""
    fence_len = 0
    for index, line in enumerate(lines):
        if fence_char:
            if _fence_close(line, fence_char, fence_len):
                fence_char = ""
                fence_len = 0
        elif heading_re.match(line):
            matches.append(index)
        else:
            opened = _fence_open(line)
            if opened:
                fence_char, fence_len = opened

    if not matches:
        raise ReleaseNotesError(f"no '## [{version}]' section in tagged CHANGELOG.md")
    if len(matches) > 1:
        raise ReleaseNotesError(f"duplicate '## [{version}]' sections in tagged CHANGELOG.md")

    body: list[str] = []
    fence_char = ""
    fence_len = 0
    for line in lines[matches[0] + 1:]:
        if fence_char:
            if _fence_close(line, fence_char, fence_len):
                fence_char = ""
                fence_len = 0
        elif H2_RE.match(line):
            break
        else:
            opened = _fence_open(line)
            if opened:
                fence_char, fence_len = opened
        body.append(line)

    text = "\n".join(body).strip("\n")
    if not text.strip():
        raise ReleaseNotesError(f"'## [{version}]' section in tagged CHANGELOG.md is empty")
    return text + "\n"


def _git(*args: str) -> str:
    # Note: no --end-of-options; this local/CI git may not support it. The
    # tag is validated by TAG_RE before reaching argv and every argument
    # is passed as a literal argv element, never through a shell.
    result = subprocess.run(
        ["git", *args],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseNotesError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    # Strip only the trailing newline, never whitespace/indentation: the
    # changelog body must be preserved verbatim. Callers that need single
    # tokens (rev-parse) are unaffected.
    return result.stdout.rstrip("\n")


def resolve_tag(tag: str, expected_commit: str) -> str:
    """Resolve tag to a commit and require equality with expected_commit."""
    if not TAG_RE.match(tag):
        raise ReleaseNotesError(f"tag '{tag}' is not a stable SemVer tag (vX.Y.Z, no leading zeros)")
    peeled = _git("rev-parse", f"refs/tags/{tag}^{{commit}}")
    if not re.fullmatch(r"[0-9a-f]{40}", peeled):
        raise ReleaseNotesError(f"refs/tags/{tag} did not resolve to a commit")
    if peeled != expected_commit:
        raise ReleaseNotesError(
            f"tag '{tag}' points at {peeled}, expected {expected_commit}"
        )
    return peeled


def tagged_changelog(tag: str, commit: str, path: str) -> str:
    resolved = resolve_tag(tag, commit)
    text = _git("show", f"{resolved}:{path}")
    if not text:
        raise ReleaseNotesError(f"{path} in {resolved} is empty")
    return text


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tag", required=True, help="stable release tag, e.g. v1.2.0")
    parser.add_argument("--commit", required=True, help="commit SHA the build/publish runs on")
    parser.add_argument("--changelog-path", default="CHANGELOG.md")
    parser.add_argument("--output", required=True, help="path of the notes file to write")
    args = parser.parse_args(argv)

    try:
        version = args.tag[1:]
        notes = extract_release_notes(
            tagged_changelog(args.tag, args.commit, args.changelog_path), version
        )
    except ReleaseNotesError as exc:
        print(f"release notes: {exc}", file=sys.stderr)
        return 1

    with open(args.output, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(notes)
    print(f"wrote {args.output} from {args.tag} ({args.commit})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
