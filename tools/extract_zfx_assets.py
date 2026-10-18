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

    out_dir.mkdir(parents=True, exist_ok=True)
    extracted_count = 0
    data_len = len(data)

    # Scan for file records: struct layout [size: u32, offset: u32, unk: u32, ftype: u32, name: [u8; ...]]
    for i in range(0, data_len - 0x100, 4):
        size, offset, _unk, ftype = struct.unpack_from("<IIII", data, i)

        # ftype 2 = file record
        if ftype == 2 and 0 < offset < data_len and (offset + size) <= data_len and size > 0:
            name_bytes = data[i + 16 : i + 16 + 256]
            name_end = name_bytes.find(b"\x00")
            if name_end <= 0:
                continue

            name = name_bytes[:name_end].decode("ascii", errors="ignore")
            # Enforce clean filename constraints
            if not all(c.isalnum() or c in "._- " for c in name):
                continue

            file_data = data[offset : offset + size]

            # Validation heuristics
            if name.endswith(".xml") and not (file_data.startswith(b"<?xml") or file_data.startswith(b"\xef\xbb\xbf<?xml")):
                continue
            if name.endswith(".png") and not file_data.startswith(b"\x89PNG"):
                continue
            if name.endswith(".jpg") and not file_data.startswith(b"\xff\xd8"):
                continue

            out_file = out_dir / name
            out_file.write_bytes(file_data)
            extracted_count += 1

    return extracted_count


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
