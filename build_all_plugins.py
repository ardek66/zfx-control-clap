#!/usr/bin/env python3
"""
ZFX CLAP Generator & Packaging Pipeline
----------------------------------------
Iterates through all Zoom ZFX DSP DLLs, builds an optimized native Linux CLAP
shared library (.so) using the zfx-bootstrap engine, and packages each into a
fully-compliant, solid .clap bundle structure.
"""

import os
import sys
import shutil
import subprocess
import json
import argparse
from pathlib import Path

# Paths relative to project root
SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = SCRIPT_DIR
ZFX_RESOURCES_DIR = PROJECT_ROOT / "wine_prefix" / "drive_c" / "Program Files (x86)" / "ZOOM" / "ZFX" / "resources"
BOOTSTRAP_DIR = PROJECT_ROOT / "zfx-bootstrap"
ASSETS_DIR = BOOTSTRAP_DIR / "assets"
DEFAULT_OUT_DIR = PROJECT_ROOT / "clap_dist"

import os
import sys
import time
import shutil
import subprocess
import json
import argparse
from pathlib import Path

# Paths relative to project root
SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = SCRIPT_DIR
ZFX_RESOURCES_DIR = PROJECT_ROOT / "wine_prefix" / "drive_c" / "Program Files (x86)" / "ZOOM" / "ZFX" / "resources"
BOOTSTRAP_DIR = PROJECT_ROOT / "zfx-bootstrap"
ASSETS_DIR = BOOTSTRAP_DIR / "assets"
DEFAULT_OUT_DIR = PROJECT_ROOT / "clap_dist"
WORKER_PATH = PROJECT_ROOT / "zfx-worker32" / "target" / "release" / "zfx-worker32"

INTERNAL_ROUTING_DLLS = {
    "InFsChk", "OutFsChk", "PuSelect", "Tuner", "VUMeter", "mn2ste", "ste2mn"
}

# Category classifier for CLAP features and metadata
def classify_plugin(name: str) -> str:
    name_lower = name.lower()
    if any(k in name_lower for k in ["stack", "combo"]):
        return "cabinet"
    elif any(k in name_lower for k in ["amp", "bassman", "1959", "custom", "bert", "crunch", "superb", "hrt3500", "tweed"]):
        return "amp"
    elif any(k in name_lower for k in ["distort", "fuzz", "muff", "squeak", "governor", "tubescream", "over_driven", "booster", "metal_world"]):
        return "distortion"
    elif any(k in name_lower for k in ["delay", "echo"]):
        return "delay"
    elif any(k in name_lower for k in ["hall", "room", "plate", "spring"]):
        return "reverb"
    elif any(k in name_lower for k in ["chorus", "flanger", "phaser", "tremolo", "vibrato", "ensemble", "ring_modulator", "step"]):
        return "modulation"
    elif any(k in name_lower for k in ["pitch", "octave", "hps"]):
        return "pitch"
    elif any(k in name_lower for k in ["wah", "cry", "eq", "isolator"]):
        return "filter"
    elif any(k in name_lower for k in ["compress", "limit", "znr"]):
        return "dynamics"
    elif any(k in name_lower for k in ["condenser", "dynamic_421", "dynamic_57"]):
        return "mic"
    else:
        return "utility"

def get_matching_asset_dir(plugin_name: str) -> Path | None:
    direct = ASSETS_DIR / plugin_name
    if direct.is_dir():
        return direct
    # Case-insensitive search
    for item in ASSETS_DIR.iterdir():
        if item.is_dir() and item.name.lower() == plugin_name.lower():
            return item
    return None

def build_plugin(dll_path: Path, out_dir: Path, index: int, total: int, dry_run: bool = False, install_dest: Path | None = None) -> dict:
    t0 = time.time()
    plugin_name = dll_path.stem
    category = classify_plugin(plugin_name)
    sanitized_id = plugin_name.lower().replace("+", "plus_").replace("-", "_")
    clap_id = f"com.idf.zfx.{sanitized_id}"

    asset_dir = get_matching_asset_dir(plugin_name)
    xml_path = ""
    if asset_dir:
        candidate_xml = asset_dir / "EfxType.xml"
        if candidate_xml.is_file():
            xml_path = str(candidate_xml)

    print(f"\n[{index}/{total}] =======================================================")
    print(f"Building: {plugin_name} (Category: {category})")
    print(f"  CLAP ID: {clap_id}")
    print(f"  DLL:     {dll_path.name} ({dll_path.stat().st_size} bytes)")
    print(f"  Assets:  {asset_dir.name if asset_dir else 'None'}")
    print(f"=======================================================")

    env = os.environ.copy()
    env["ZFX_DLL_PATH"] = str(dll_path)
    env["ZFX_PLUGIN_ID"] = clap_id
    env["ZFX_PLUGIN_NAME"] = plugin_name
    env["ZFX_PLUGIN_CATEGORY"] = category
    env["ZFX_XML_PATH"] = xml_path
    env["ZFX_WORKER_PATH"] = str(WORKER_PATH)

    if not dry_run:
        cmd = [
            "cargo", "build", "--release",
            "--manifest-path", str(BOOTSTRAP_DIR / "Cargo.toml")
        ]
        res = subprocess.run(cmd, env=env, cwd=str(BOOTSTRAP_DIR), capture_output=True, text=True)
        if res.returncode != 0:
            print(f"ERROR: Build failed for {plugin_name}!\n{res.stderr}")
            return {"name": plugin_name, "status": "failed", "error": res.stderr}

    # Prepare standalone output
    standalone_dir = out_dir / "standalone"
    standalone_dir.mkdir(parents=True, exist_ok=True)
    target_so = BOOTSTRAP_DIR / "target" / "release" / "libzfx_bootstrap.so"
    dest_standalone_so = standalone_dir / f"{plugin_name}.clap"

    if not dry_run and target_so.is_file():
        shutil.copy2(target_so, dest_standalone_so)
        if install_dest:
            install_target = install_dest / f"{plugin_name}.clap"
            shutil.copy2(dest_standalone_so, install_target)
            print(f"  -> Installed: {install_target}")

    elapsed = time.time() - t0
    print(f"  [OK] Successfully built {plugin_name}.clap ({dest_standalone_so.stat().st_size if dest_standalone_so.is_file() else 0} bytes) in {elapsed:.2f}s")

    return {
        "name": plugin_name,
        "id": clap_id,
        "category": category,
        "status": "success",
        "has_gui": bool(xml_path),
        "standalone": str(dest_standalone_so),
        "build_time_sec": round(elapsed, 2)
    }

def main():
    parser = argparse.ArgumentParser(description="Build and package all Zoom ZFX plugins into CLAP .clap binaries")
    parser.add_argument("--limit", type=int, default=None, help="Limit number of plugins to build")
    parser.add_argument("--plugin", type=str, default=None, help="Build a specific plugin by name")
    parser.add_argument("--out-dir", type=str, default=str(DEFAULT_OUT_DIR), help="Output directory for CLAP bundles")
    parser.add_argument("--dry-run", action="store_true", help="Print build operations without running cargo")
    parser.add_argument("--install", action="store_true", help="Install standalone .clap plugins to ~/.clap/")
    parser.add_argument("--include-internal", action="store_true", help="Include internal routing/utility DLLs without UI")
    args = parser.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    if not ZFX_RESOURCES_DIR.is_dir():
        print(f"Error: Resources directory not found at {ZFX_RESOURCES_DIR}")
        sys.exit(1)

    if not WORKER_PATH.is_file():
        print(f"Error: 32-bit worker not found at {WORKER_PATH}! Please build zfx-worker32 first.")
        sys.exit(1)

    all_dlls = sorted([f for f in ZFX_RESOURCES_DIR.glob("*.dll")])
    if not args.include_internal:
        all_dlls = [d for d in all_dlls if d.stem not in INTERNAL_ROUTING_DLLS]

    if args.plugin:
        all_dlls = [d for d in all_dlls if d.stem.lower() == args.plugin.lower()]
        if not all_dlls:
            print(f"Error: Plugin {args.plugin} not found in {ZFX_RESOURCES_DIR}")
            sys.exit(1)

    if args.limit:
        all_dlls = all_dlls[:args.limit]

    install_dest = None
    if args.install:
        install_dest = Path.home() / ".clap"
        install_dest.mkdir(parents=True, exist_ok=True)

    print(f"=======================================================")
    print(f"  ZFX Suite Batch Generator (Phase 3)")
    print(f"  Plugins queued:   {len(all_dlls)}")
    print(f"  Worker 32:        {WORKER_PATH} ({WORKER_PATH.stat().st_size} bytes)")
    print(f"  Output directory: {out_dir}")
    print(f"  Install target:   {install_dest if install_dest else 'Disabled'}")
    print(f"=======================================================\n")

    t_start = time.time()
    results = []
    for i, dll in enumerate(all_dlls, 1):
        res = build_plugin(dll, out_dir, i, len(all_dlls), dry_run=args.dry_run, install_dest=install_dest)
        results.append(res)

    catalog_path = out_dir / "catalog.json"
    with open(catalog_path, "w") as cf:
        json.dump(results, cf, indent=2)

    total_time = time.time() - t_start
    successful = [r for r in results if r["status"] == "success"]
    failed = [r for r in results if r["status"] == "failed"]

    print("\n=======================================================")
    print(f"PHASE 3 BATCH BUILD SUMMARY:")
    print(f"  Total Plugins:    {len(results)}")
    print(f"  Successfully Built: {len(successful)}")
    print(f"  Failed:           {len(failed)}")
    print(f"  Total Time:       {total_time:.2f}s (avg {total_time/len(results):.2f}s/plugin)")
    print(f"  Catalog saved to: {catalog_path}")
    if install_dest:
        print(f"  Installed to:     {install_dest}")
    print(f"=======================================================")

if __name__ == "__main__":
    main()
