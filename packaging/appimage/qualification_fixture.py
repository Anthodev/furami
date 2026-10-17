#!/usr/bin/env python3
"""Deterministic Y4M qualification fixture for the FUR-025 GUI smoke.

The fixture is the *only* video input for a qualification run: no camera, no
clock, no randomness, no fonts and no encoder are involved. It is a 320x180,
30 fps, 120-frame YUV4MPEG2 stream whose luminance plane encodes two things a
parent can check in real screenshots:

* a bright rectangle (luminance 200) that moves two pixels right per frame,
  starting at ``(16 + 2*i, 60)`` for frame ``i`` in ``0..119``;
* eight 16x16 cells at ``(80 + 20*j, 120)`` for ``j`` in ``0..7`` whose
  luminance is 220 when the corresponding bit of ``i`` is 1 and 32 otherwise,
  with the least significant bit at the leftmost cell.

Both chroma planes are constant 128, the initial luminance plane is 32 and the
cell row never overlaps the moving rectangle, so a scaled screenshot can be
decoded back to the frame index from cell centres alone and cross-checked
against the rectangle's horizontal centroid.

Run directly:

    python3 packaging/appimage/qualification_fixture.py /absolute/fixture.y4m

``write_fixture`` returns the exact byte facts (format, dimensions, rate,
frames, size, SHA256) that the smoke records next to the generated file.
"""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

WIDTH = 320
HEIGHT = 180
RATE_NUM = 30
RATE_DEN = 1
FRAMES = 120

HEADER = b"YUV4MPEG2 W320 H180 F30:1 Ip A1:1 C420jpeg XCOLORRANGE=LIMITED\n"
FRAME_MARKER = b"FRAME\n"

CELL_LUMINANCE_ON = 220
CELL_LUMINANCE_OFF = 32
CELL_LUMINANCE_BACKGROUND = 32
RECTANGLE_LUMINANCE = 200
CHROMA = 128
INITIAL_LUMA = 32
RECTANGLE_SIZE = 40
RECTANGLE_Y = 60
CELL_SIZE = 16
CELL_Y = 120
CELL_BASE_X = 80
CELL_STRIDE = 20
CELLS = 8

# ITU-R BT.601 limited-range YCbCr -> RGB, used only by tests/host tooling to
# interpret a rendered capture. Kept here as the single reference for the
# fixture's limited range declaration.
LUMA_PLANE_BYTES = WIDTH * HEIGHT
CHROMA_PLANE_BYTES = (WIDTH // 2) * (HEIGHT // 2)


def rectangle_origin(frame: int) -> tuple[int, int]:
    """Top-left corner of the moving rectangle for ``frame``."""
    return 16 + 2 * frame, RECTANGLE_Y


def cell_origins() -> list[tuple[int, int]]:
    return [(CELL_BASE_X + CELL_STRIDE * index, CELL_Y) for index in range(CELLS)]


def encode_bit(frame: int, bit: int) -> int:
    """Luminance of cell ``bit`` (0 = least significant, leftmost) for ``frame``."""
    return CELL_LUMINANCE_ON if (frame >> bit) & 1 else CELL_LUMINANCE_OFF


def _y_plane(frame: int) -> bytearray:
    plane = bytearray([INITIAL_LUMA]) * LUMA_PLANE_BYTES
    if not 0 <= frame < FRAMES:
        raise ValueError(f"frame {frame} outside 0..{FRAMES - 1}")
    # Cell row: one 16x16 cell per bit, least significant bit leftmost.
    for bit, (origin_x, origin_y) in enumerate(cell_origins()):
        value = encode_bit(frame, bit)
        for row in range(origin_y, origin_y + CELL_SIZE):
            start = row * WIDTH + origin_x
            plane[start:start + CELL_SIZE] = bytes([value]) * CELL_SIZE
    # Moving rectangle, drawn last so it can never be confused with a cell row.
    origin_x, origin_y = rectangle_origin(frame)
    for row in range(origin_y, origin_y + RECTANGLE_SIZE):
        start = row * WIDTH + origin_x
        plane[start:start + RECTANGLE_SIZE] = bytes([RECTANGLE_LUMINANCE]) * RECTANGLE_SIZE
    return plane


def frame_bytes(frame: int) -> bytes:
    return bytes(_y_plane(frame)) + bytes([CHROMA]) * (2 * CHROMA_PLANE_BYTES)


def write_fixture(path: Path) -> dict:
    """Write the fixture to ``path`` and return its exact byte facts."""
    path = Path(path)
    digest = hashlib.sha256()
    with path.open("wb") as stream:
        stream.write(HEADER)
        digest.update(HEADER)
        for frame in range(FRAMES):
            payload = FRAME_MARKER + frame_bytes(frame)
            stream.write(payload)
            digest.update(payload)
    return {
        "format": "YUV4MPEG2 Y4M C420jpeg limited range",
        "width": WIDTH,
        "height": HEIGHT,
        "rate_num": RATE_NUM,
        "rate_den": RATE_DEN,
        "frames": FRAMES,
        "size": path.stat().st_size,
        "sha256": digest.hexdigest(),
        "header": HEADER.decode("ascii").rstrip("\n"),
        "algorithm": {
            "rectangle": {
                "size": RECTANGLE_SIZE,
                "y": RECTANGLE_Y,
                "luminance": RECTANGLE_LUMINANCE,
                "origin_x": "16 + 2*i",
            },
            "cells": {
                "count": CELLS,
                "size": CELL_SIZE,
                "y": CELL_Y,
                "base_x": CELL_BASE_X,
                "stride": CELL_STRIDE,
                "on_luminance": CELL_LUMINANCE_ON,
                "off_luminance": CELL_LUMINANCE_OFF,
                "bit_order": "least significant bit at the leftmost cell",
            },
            "chroma": CHROMA,
        },
    }


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) != 1:
        print("usage: qualification_fixture.py /absolute/fixture.y4m", file=sys.stderr)
        return 2
    destination = Path(args[0])
    if not destination.is_absolute():
        print("fixture output path must be absolute", file=sys.stderr)
        return 2
    facts = write_fixture(destination)
    import json

    print(json.dumps(facts, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
