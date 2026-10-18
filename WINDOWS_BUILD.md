# Zoom ZFX CLAP Plugin Suite — Windows & Linux Multi-Platform Guide

This document describes the cross-platform architecture, cross-compilation pipeline, and native build procedures for the Zoom ZFX CLAP plugin bridge supporting both **Linux (x86_64)** and **Windows (x86_64 host / i686 worker)**.

---

## 1. Architectural Model

Modern Digital Audio Workstations (e.g. REAPER, Bitwig, Cubase, Studio One) on both 64-bit Linux and 64-bit Windows run strictly as 64-bit processes. Because the original Zoom ZFX DSP algorithms reside in legacy 32-bit PE DLLs, loading them requires a two-tier IPC bridge on both operating systems:

```
[ 64-bit DAW Host (REAPER / Bitwig / etc.) ]
                    │
                    ▼
[ zfx-bootstrap (64-bit CLAP Plugin) ]
  • Linux:   .so ELF shared library (X11 embedding)
  • Windows: .clap PE-COFF DLL (Win32 HWND embedding)
                    │
                    │  Shared Memory IPC (`ZfxSharedBridge`)
                    │  Atomic Futex (Linux: SYS_futex / Win: WaitOnAddress)
                    ▼
[ zfx-worker32 (32-bit DSP Worker Process) ]
  • Linux:   ELF 32-bit i686 process with custom PE Loader + TLS TEB
  • Windows: PE 32-bit i686 process running natively with OS TEB
                    │
                    ▼
[ In-Memory Zoom ZFX DSP DLL ] (1:1 Bit-Exact Emulation)
```

### Platform Abstraction Mapping

| Feature | Linux (`cfg(target_os = "linux")`) | Windows (`cfg(target_os = "windows")`) |
| :--- | :--- | :--- |
| **Shared Memory** | `memfd_create` + anonymous fd `mmap` | `CreateFileMappingA` (`Local\ZfxBridge_*`) + `MapViewOfFile` |
| **Atomic Futex** | `libc::syscall(SYS_futex, ...)` | `WaitOnAddress` / `WakeByAddressSingle` (`synchapi.h`) |
| **Supervisor / Auto-Kill** | `prctl(PR_SET_PDEATHSIG, SIGKILL)` | Win32 Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` |
| **GUI Window Embedding** | X11 `Window` (`raw_window_handle::Xlib`) | Win32 `HWND` (`raw_window_handle::Win32`) |
| **32-bit PE Memory Alloc** | `libc::mmap(PROT_READ\|WRITE\|EXEC)` | `VirtualAlloc(PAGE_EXECUTE_READWRITE)` |
| **Thread Environment Block** | GDT TLS allocation via `SYS_set_thread_area` | Native OS TEB via segment register `FS` |

---

## 2. Building for Windows

### Option A: Cross-Compiling from Linux via Nix Flakes (Recommended)

The workspace flake provides pre-configured environments with `rust-overlay` (providing official prebuilt multi-target standard libraries) and MinGW-w64 cross compilers:

#### Step 1: Build the 32-bit Worker Executable (`zfx-worker32.exe`)
```bash
nix develop .#win32 --command cargo build --release \
  --target i686-pc-windows-gnu \
  --manifest-path zfx-worker32/Cargo.toml
```
*Artifact generated: `zfx-worker32/target/i686-pc-windows-gnu/release/zfx-worker32.exe`*

#### Step 2: Build All or Specific Plugins for Windows
To build all plugins for 64-bit Windows:
```bash
nix develop .#win64 --command python3 tools/build_all_plugins.py \
  --os windows \
  --target-triple x86_64-pc-windows-gnu
```

To build a single plugin:
```bash
nix develop .#win64 --command python3 tools/build_all_plugins.py \
  --os windows \
  --target-triple x86_64-pc-windows-gnu \
  --plugin Great_Muff
```
*Artifacts generated in `clap_dist/standalone/<PluginName>.clap` (64-bit PE DLLs).*

---

### Option B: Building Natively on Windows

On a native Windows machine with Rust and MSVC or MinGW installed:

#### Prerequisites
- Rust 1.80+ (`x86_64-pc-windows-msvc` or `x86_64-pc-windows-gnu`)
- 32-bit target: `rustup target add i686-pc-windows-msvc` (or `i686-pc-windows-gnu`)
- Python 3.9+

#### Step 1: Build the 32-bit Worker
```powershell
cargo build --release --target i686-pc-windows-msvc --manifest-path zfx-worker32\Cargo.toml
```

#### Step 2: Build All Plugins
```powershell
python tools\build_all_plugins.py --os windows --install
```
*Plugins are automatically installed to `%COMMONPROGRAMFILES%\CLAP\` (e.g. `C:\Program Files\Common Files\CLAP`).*

---

## 3. Optional Features

By default, all plugins are compiled in **pure 1:1 original Zoom mode** without modifications (`default = []`).

You can optionally enable modern enhancements using build flags:
- `--enable-oversampling`: Enables high-rate linear-phase polyphase oversampling badge and engine resampler.
- `--enable-mic-ui`: Enables the custom egui microphone positioner panel for studio cabinet/mic plugins.

Example:
```bash
nix develop .#win64 --command python3 tools/build_all_plugins.py \
  --os windows \
  --target-triple x86_64-pc-windows-gnu \
  --plugin Tangerine \
  --enable-oversampling
```

---

## 4. Lint and Code Hygiene

Both Linux and Windows code paths strictly adhere to zero-warning Clippy standards:
```bash
# Check Linux 64-bit
nix develop --command cargo clippy --manifest-path zfx-bootstrap/Cargo.toml --all-targets

# Check Windows 64-bit
nix develop .#win64 --command cargo clippy --target x86_64-pc-windows-gnu --manifest-path zfx-bootstrap/Cargo.toml --all-targets

# Check Windows 32-bit Worker
nix develop .#win32 --command cargo clippy --target i686-pc-windows-gnu --manifest-path zfx-worker32/Cargo.toml --all-targets
```
