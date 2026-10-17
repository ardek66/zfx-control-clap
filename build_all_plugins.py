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

def find_target_binary(bootstrap_dir: Path, target_triple: str | None = None) -> Path | None:
    candidates = []
    if target_triple:
        candidates.extend([
            bootstrap_dir / "target" / target_triple / "release" / "libzfx_bootstrap.so",
            bootstrap_dir / "target" / target_triple / "release" / "zfx_bootstrap.dll",
        ])
    candidates.extend([
        bootstrap_dir / "target" / "release" / "libzfx_bootstrap.so",
        bootstrap_dir / "target" / "release" / "zfx_bootstrap.dll",
    ])
    for c in candidates:
        if c.is_file():
            return c
    return None

def find_worker_binary(project_root: Path, target_os: str) -> Path | None:
    if target_os == "windows":
        candidates = [
            project_root / "zfx-worker32" / "target" / "release" / "zfx-worker32.exe",
            project_root / "zfx-worker32" / "target" / "i686-pc-windows-gnu" / "release" / "zfx-worker32.exe",
            project_root / "zfx-worker32" / "target" / "i686-pc-windows-msvc" / "release" / "zfx-worker32.exe",
        ]
    else:
        candidates = [
            project_root / "zfx-worker32" / "target" / "release" / "zfx-worker32",
            project_root / "zfx-worker32" / "target" / "i686-unknown-linux-gnu" / "release" / "zfx-worker32",
        ]
    for c in candidates:
        if c.is_file():
            return c
    return None

def build_plugin(dll_path: Path, out_dir: Path, index: int, total: int, dry_run: bool = False, install_dest: Path | None = None, features_str: str | None = None, target_os: str = "linux", target_triple: str | None = None, worker_path: Path | None = None) -> dict:
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
    print(f"Building: {plugin_name} (Category: {category}, Target OS: {target_os})")
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
    if worker_path:
        env["ZFX_WORKER_PATH"] = str(worker_path)

    if not dry_run:
        cmd = [
            "cargo", "build", "--release",
            "--manifest-path", str(BOOTSTRAP_DIR / "Cargo.toml")
        ]
        if target_triple:
            cmd += ["--target", target_triple]
        if features_str:
            cmd += ["--features", features_str]
        res = subprocess.run(cmd, env=env, cwd=str(BOOTSTRAP_DIR), capture_output=True, text=True)
        if res.returncode != 0:
            print(f"ERROR: Build failed for {plugin_name}!\n{res.stderr}")
            return {"name": plugin_name, "status": "failed", "error": res.stderr}

    # Prepare standalone output
    standalone_dir = out_dir / "standalone"
    standalone_dir.mkdir(parents=True, exist_ok=True)
    target_bin = find_target_binary(BOOTSTRAP_DIR, target_triple)
    dest_standalone_clap = standalone_dir / f"{plugin_name}.clap"

    if not dry_run and target_bin and target_bin.is_file():
        shutil.copy2(target_bin, dest_standalone_clap)
        if install_dest:
            install_target = install_dest / f"{plugin_name}.clap"
            shutil.copy2(dest_standalone_clap, install_target)
            print(f"  -> Installed: {install_target}")

    elapsed = time.time() - t0
    print(f"  [OK] Successfully built {plugin_name}.clap ({dest_standalone_clap.stat().st_size if dest_standalone_clap.is_file() else 0} bytes) in {elapsed:.2f}s")

    return {
        "name": plugin_name,
        "id": clap_id,
        "category": category,
        "status": "success",
        "has_gui": bool(xml_path),
        "standalone": str(dest_standalone_clap),
        "build_time_sec": round(elapsed, 2)
    }

def main():
    parser = argparse.ArgumentParser(description="Build and package all Zoom ZFX plugins into CLAP .clap binaries")
    parser.add_argument("--limit", type=int, default=None, help="Limit number of plugins to build")
    parser.add_argument("--plugin", type=str, default=None, help="Build a specific plugin by name")
    parser.add_argument("--out-dir", type=str, default=str(DEFAULT_OUT_DIR), help="Output directory for CLAP bundles")
    parser.add_argument("--dry-run", action="store_true", help="Print build operations without running cargo")
    parser.add_argument("--install", action="store_true", help="Install standalone .clap plugins to default system CLAP folder")
    parser.add_argument("--include-internal", action="store_true", help="Include internal routing/utility DLLs without UI")
    parser.add_argument("--os", choices=["linux", "windows"], default="linux",
                        help="Target operating system (default: linux)")
    parser.add_argument("--target-triple", type=str, default=None,
                        help="Specific Cargo target triple (e.g. x86_64-pc-windows-gnu)")
    parser.add_argument("--enable-oversampling", action="store_true",
                        help="Enable oversampling quality badge UI and host-rate resampler (feat_oversampling Cargo feature)")
    parser.add_argument("--enable-mic-ui", action="store_true",
                        help="Enable custom studio microphone egui panel for mic-category plugins (feat_mic_ui Cargo feature)")
    args = parser.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    # Build up optional Cargo features list
    cargo_features = []
    if args.enable_oversampling:
        cargo_features.append("feat_oversampling")
    if args.enable_mic_ui:
        cargo_features.append("feat_mic_ui")
    features_str = ",".join(cargo_features) if cargo_features else None

    if not ZFX_RESOURCES_DIR.is_dir():
        print(f"Error: Resources directory not found at {ZFX_RESOURCES_DIR}")
        sys.exit(1)

    worker_bin = find_worker_binary(PROJECT_ROOT, args.os)
    if not worker_bin or not worker_bin.is_file():
        expected_name = "zfx-worker32.exe" if args.os == "windows" else "zfx-worker32"
        print(f"Error: 32-bit worker ({expected_name}) not found! Please build zfx-worker32 for {args.os} first.")
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
        if args.os == "windows":
            common_files = os.environ.get("COMMONPROGRAMFILES", "C:\\Program Files\\Common Files")
            install_dest = Path(common_files) / "CLAP"
        else:
            install_dest = Path.home() / ".clap"
        try:
            install_dest.mkdir(parents=True, exist_ok=True)
        except Exception as e:
            print(f"Warning: Could not create install destination {install_dest}: {e}")

    print(f"=======================================================")
    print(f"  ZFX Suite Batch Generator (Dual-OS)")
    print(f"  Target OS:        {args.os}")
    print(f"  Target triple:    {args.target_triple if args.target_triple else '(host default)'}")
    print(f"  Plugins queued:   {len(all_dlls)}")
    print(f"  Worker 32:        {worker_bin} ({worker_bin.stat().st_size} bytes)")
    print(f"  Output directory: {out_dir}")
    print(f"  Install target:   {install_dest if install_dest else 'Disabled'}")
    print(f"  Active features:  {features_str if features_str else '(default — no optional features)'}")
    print(f"=======================================================\n")

    t_start = time.time()
    results = []
    for i, dll in enumerate(all_dlls, 1):
        res = build_plugin(
            dll, out_dir, i, len(all_dlls),
            dry_run=args.dry_run, install_dest=install_dest,
            features_str=features_str, target_os=args.os,
            target_triple=args.target_triple, worker_path=worker_bin
        )
        results.append(res)

    catalog_path = out_dir / "catalog.json"
    with open(catalog_path, "w") as cf:
        json.dump(results, cf, indent=2)

    total_time = time.time() - t_start
    successful = [r for r in results if r["status"] == "success"]
    failed = [r for r in results if r["status"] == "failed"]

    print("\n=======================================================")
    print(f"DUAL-OS BATCH BUILD SUMMARY:")
    print(f"  Target OS:        {args.os}")
    print(f"  Total Plugins:    {len(results)}")
    print(f"  Successfully Built: {len(successful)}")
    print(f"  Failed:           {len(failed)}")
    print(f"  Total Time:       {total_time:.2f}s (avg {total_time/len(results) if results else 0:.2f}s/plugin)")
    print(f"  Catalog saved to: {catalog_path}")
    if install_dest:
        print(f"  Installed to:     {install_dest}")
    print(f"=======================================================")

if __name__ == "__main__":
    main()
