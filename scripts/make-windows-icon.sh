#!/bin/bash
# Generates windows-sender/resources/app.ico from scripts/make-icon.swift.
#
# The .ico is committed to the repository because it is drawn with AppKit, and the machine that
# builds the Windows half - a GitHub Actions runner, or your PC - has no AppKit. Run this on a Mac
# whenever the icon changes.
#
#   ./scripts/make-windows-icon.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="$ROOT/windows-sender/resources/app.ico"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "==> drawing the frames"
swift "$ROOT/scripts/make-icon.swift" "$WORK" --windows >/dev/null

echo "==> packing $TARGET"
mkdir -p "$(dirname "$TARGET")"
python3 - "$WORK" "$TARGET" <<'PY'
import struct, sys, pathlib

work, target = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
sizes = [16, 20, 24, 32, 48, 64, 128, 256]
frames = [(size, (work / f"{size}.png").read_bytes()) for size in sizes]

# Every frame is stored as a PNG, which every Windows version since Vista understands, and which
# keeps a 256 px frame at a few kB instead of a quarter of a megabyte of raw BGRA.
header = struct.pack("<HHH", 0, 1, len(frames))
offset = len(header) + 16 * len(frames)
directory, payload = b"", b""
for size, png in frames:
    directory += struct.pack(
        "<BBBBHHII", size if size < 256 else 0, size if size < 256 else 0, 0, 0, 1, 32,
        len(png), offset,
    )
    payload += png
    offset += len(png)
target.write_bytes(header + directory + payload)
print(f"    {len(frames)} frames, {len(header) + len(directory) + len(payload)} bytes")
PY
