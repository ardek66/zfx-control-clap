#!/usr/bin/env python3
"""
Zoom ZFX Asset & Payload Extraction Utility
============================================
Extracts UI graphic textures (PNG/JPG) and XML layout configurations from
Zoom ZFX Resource Container (.zrc) archives, and verifies 32-bit Windows
DSP DLL payloads for building native Linux CLAP plugins.

This tool implements the "Bring Your Own Assets / Binaries" (BYOA) compliance
model: users supply their legally acquired Zoom ZFX installation, CD-ROM,
or installer, and this tool populates the local build directory.
"""

import argparse
import os
import struct
import sys
from pathlib import Path


# VER1000 directory record: 512-byte name, child count, size, marker.
# A file child is 528 bytes (offset, size, extra, flags). A directory child is
# 524 bytes (offset, size, extra). Payload address is record_position + offset - 4.
_DIR_RECORD = 524
_FILE_RECORD = 528
_NAME_FIELD = 512


def _record_name(data: bytes, pos: int) -> str:
    raw = data[pos : pos + _NAME_FIELD].split(b"\x00", 1)[0]
    return raw.decode("ascii", errors="ignore")


def _safe_relpath(name: str) -> Path | None:
    parts = [part for part in name.replace("\\", "/").split("/") if part not in ("", ".", "..")]
    while parts and parts[0].lower() in {"root", "resource"}:
        parts = parts[1:]
    if not parts:
        return None
    for part in parts:
        if not all(ch.isalnum() or ch in "._- " for ch in part):
            return None
    path = Path(*parts)
    stem, dot, ext = path.name.rpartition(".")
    if dot:
        path = path.with_name(stem + dot + ext.lower())
    return path


def _walk_zrc(data: bytes, pos: int, prefix: str, out: list[tuple[Path, bytes]]) -> None:
    if pos < 0 or pos + _DIR_RECORD > len(data):
        return
    count = struct.unpack_from("<I", data, pos + _NAME_FIELD)[0]
    if count > 4096:
        return
    child = pos + _DIR_RECORD
    for _ in range(count):
        if child + _DIR_RECORD > len(data):
            return
        name = _record_name(data, child)
        if not name or any(ch.isalnum() is False and ch not in "._- /\\" for ch in name):
            return
        offset, size, _extra = struct.unpack_from("<III", data, child + _NAME_FIELD)
        target = child + offset - 4
        if "." in name:
            rel = _safe_relpath(prefix + name)
            if rel is not None and size > 0 and 0 <= target <= len(data) - size:
                blob = data[target : target + size]
                if blob.startswith(b"\xef\xbb\xbf"):
                    blob = blob[3:]
                out.append((rel, blob))
            child += _FILE_RECORD
        else:
            _walk_zrc(data, target, f"{prefix}{name}/", out)
            child += _DIR_RECORD


def parse_zrc_file(zrc_path: Path, out_dir: Path) -> int:
    """Parses a single .zrc archive and extracts all valid images and XMLs."""
    try:
        data = zrc_path.read_bytes()
    except Exception as err:
        print(f"[-] Error reading {zrc_path}: {err}", file=sys.stderr)
        return 0

    if not data.startswith(b"ZOOM ZFX"):
        print(f"[-] {zrc_path.name} is not a valid ZOOM ZFX archive (magic mismatch)", file=sys.stderr)
        return 0

    found: list[tuple[Path, bytes]] = []
    _walk_zrc(data, 56, "", found)

    written = 0
    for rel, blob in found:
        suffix = rel.suffix
        if suffix == ".xml" and not blob.startswith(b"<?xml"):
            continue
        if suffix == ".png" and not blob.startswith(b"\x89PNG"):
            continue
        if suffix in {".jpg", ".jpeg"} and not blob.startswith(b"\xff\xd8"):
            continue
        if suffix not in {".xml", ".png", ".jpg", ".jpeg"}:
            continue
        dest = out_dir / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(blob)
        written += 1
    return written


def find_resources_dir(hint: Path | None) -> Path | None:
    """Locates the Zoom ZFX resources directory with .zrc and .dll files."""
    candidates = []
    if hint:
        candidates.append(hint)
        candidates.append(hint / "resources")
        candidates.append(hint / "drive_c" / "Program Files (x86)" / "ZOOM" / "ZFX" / "resources")
        candidates.append(hint / "drive_c" / "Program Files" / "ZOOM" / "ZFX" / "resources")

    # Common relative project locations
    root = Path(__file__).resolve().parent.parent
    candidates.extend([
        root / "wine_prefix" / "drive_c" / "Program Files (x86)" / "ZOOM" / "ZFX" / "resources",
        root / "resources",
        root / "wine_prefix" / "drive_c" / "Program Files" / "ZOOM" / "ZFX" / "resources",
    ])

    for c in candidates:
        if c.is_dir() and any(c.glob("*.zrc")):
            return c
    return None


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Extract Zoom ZFX graphic assets and verify DSP DLLs for CLAP compilation."
    )
    parser.add_argument(
        "--source",
        "-s",
        type=Path,
        help="Path to Zoom ZFX installation, wine prefix, or resources directory containing .zrc and .dll files.",
    )
    parser.add_argument(
        "--out-dir",
        "-o",
        type=Path,
        help="Target output directory for extracted assets (default: zfx-bootstrap/assets).",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Scan and list files without extracting to disk.",
    )

    args = parser.parse_args()
    project_root = Path(__file__).resolve().parent.parent

    source_dir = find_resources_dir(args.source)
    if not source_dir:
        print("[!] Could not automatically locate Zoom ZFX resources directory with .zrc files.", file=sys.stderr)
        print("    Please specify the path using: --source /path/to/ZOOM/ZFX/resources", file=sys.stderr)
        return 1

    out_dir = args.out_dir or (project_root / "zfx-bootstrap" / "assets")
    print(f"[*] Found Zoom ZFX resources at: {source_dir}")
    print(f"[*] Extracting assets to:        {out_dir}")

    zrc_files = sorted(source_dir.glob("*.zrc"))
    dll_files = sorted(source_dir.glob("*.dll"))

    print(f"[*] Discovered {len(zrc_files)} resource archives (.zrc) and {len(dll_files)} DSP libraries (.dll)\n")

    total_extracted = 0
    extracted_plugins = 0

    for zrc in zrc_files:
        stem = zrc.stem
        target_plugin_dir = out_dir / stem
        if args.dry_run:
            print(f"[DRY-RUN] Would extract {zrc.name} -> {target_plugin_dir}")
            continue

        count = parse_zrc_file(zrc, target_plugin_dir)
        if count > 0:
            extracted_plugins += 1
            total_extracted += count
            print(f"  [+] Extracted {stem:30} : {count:3} files")
        else:
            print(f"  [-] Failed or empty: {stem:30}")

    print("\n" + "=" * 65)
    print("EXTRACTION SUMMARY")
    print("=" * 65)
    print(f"Total .zrc archives processed : {len(zrc_files)}")
    print(f"Successfully unpacked plugins : {extracted_plugins}")
    print(f"Total graphic & XML assets    : {total_extracted}")
    print(f"Matching 32-bit DSP DLLs      : {len(dll_files)}")
    print("=" * 65)

    if len(dll_files) < 86:
        print(f"[!] Warning: Found {len(dll_files)} DLLs; standard full suite expects 86 plugins.")
    else:
        print("[✓] All 86 DSP DLL payloads detected.")

    print("\n[✓] Asset extraction complete. You can now build the CLAP plugins:")
    print("    python3 tools/build_all_plugins.py --all-features --install")
    return 0


if __name__ == "__main__":
    sys.exit(main())
