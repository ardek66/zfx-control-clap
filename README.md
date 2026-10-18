# Zoom ZFX CLAP Suite & Modernization Engine

A native 64-bit Linux CLAP plugin suite and reverse-engineering runtime resurrecting all **86 Zoom ZFX DSP plugins** (amplifiers, speaker cabinets, stompboxes, studio dynamics, delays, and reverbs) from the discontinued Zoom C5.1t / S2t hardware era.

Running natively on modern 64-bit DAWs (REAPER, Bitwig, Ardour) with **zero runtime Wine dependency**, real-time sample-rate adaptation up to 768 kHz, optional polyphase oversampling, and high-DPI skeuomorphic vector GUIs.

---

## Key Features

- **86 Native CLAP Plugins**: Standalone `.clap` bundles executing 32-bit DSP payloads via a native in-memory PE loader with zero-allocation POSIX shared memory and Linux futex IPC synchronization.
- **Legacy Hardware Compatibility**: In-memory emulation of legacy Zoom C5.1t USB vendor query protocols (`SetupAPI.dll` query resolution) to enable full processing without obsolete hardware drivers.
- **Infinite Sample Rate Freedom**: Analytical real-time bilinear filter re-warping (`analog_proto.rs`) that continuously adapts legacy 44.1k/48k 5th-order cabinet IIRs, biquads, and wah resonators to any DAW rate (96k, 192k, 768k).
- **Polyphase Oversampling (1x / 2x / 4x)**: Linear-phase sinc polyphase FIR and minimum-phase halfband IIR oversampling to eliminate Nyquist foldback and aliasing digital fizz.
- **64-bit Double Precision (`f64`)**: Native double-precision DSP pipeline preserving full $300\text{ dBFS}$ dynamic range with SSE FTZ/DAZ denormal float protection.
- **Skeuomorphic Vector UI**: High-DPI UI rendered via `egui`, featuring photorealistic textures, dynamic XML parameter binding, and custom studio microphone placement panels.

---

## Architecture & Technical Documentation

For technical specifications, mathematical derivations, reverse-engineered binary layouts, and historical analysis, refer to:

- 📖 **[ARCHITECTURE_AND_DSP_RETROSPECTIVE.md](file:///home/idf/dev/c51t/ARCHITECTURE_AND_DSP_RETROSPECTIVE.md)**: Master architectural guide covering:
  - Discovered PE header hierarchy (`ImageDosHeader`, `ImageNtHeaders32`, etc.).
  - Win32 TIB layout (`Win32Tib`) and LDT segment selector setup (`SYS_set_thread_area`).
  - 5th-order IIR cabinet transfer function equations and symmetry invariants.
  - Mathematical transcendental waveshaper modeling ($\arctan$, $\exp$, $\tanh$).
  - Full analysis of legacy failure modes and modern restoration techniques.
- ⚖️ **[LEGAL_NOTICE.md](file:///home/idf/dev/c51t/LEGAL_NOTICE.md)**: Trademark notices, fair-use declarations, and interoperability research statements.
- 🪟 **[WINDOWS_BUILD.md](file:///home/idf/dev/c51t/WINDOWS_BUILD.md)**: Dual-OS Windows native compilation guide.

---

## Quick Start & Building

### Prerequisites

Using the provided Nix flake ensures all cross-compilers, 32-bit multilib libraries, and host tools are pre-configured:

```bash
# Enter the developer environment
nix develop
```

### 1. Extract Graphic Assets & DSP Payloads (BYOA Model)

This project does not distribute proprietary Zoom binaries or copyrighted artwork. Extract assets and DLLs locally from your licensed Zoom ZFX installation or CD-ROM resources:

```bash
# Extracts XML layouts and image textures from .zrc archives into zfx-bootstrap/assets/
python3 tools/extract_zfx_assets.py --source /path/to/ZOOM/ZFX/resources
```

### 2. Build the 32-Bit Worker

```bash
nix develop .#shell32 --command cargo build --release --manifest-path zfx-worker32/Cargo.toml
```

### 3. Build and Install All 86 Plugins

```bash
# Builds all 86 plugins with oversampling and mic-UI enabled, installing to ~/.clap/
nix develop --command python3 tools/build_all_plugins.py --all-features --install
```

### 4. Validate Plugins with CLAP Validator

```bash
nix develop --command ./clap-validator/target/release/clap-validator validate --in-process ~/.clap/PEDAL_CRY.clap
```

---

## Verification & Code Standards

The codebase is hardened against strict Clippy rules with `-D warnings`:

```bash
# Check 64-bit host bootstrap
nix develop --command cargo clippy --manifest-path zfx-bootstrap/Cargo.toml --all-targets --all-features -- -D warnings

# Check 32-bit worker
nix develop --command cargo clippy --manifest-path zfx-worker32/Cargo.toml --target i686-unknown-linux-gnu --all-targets --all-features -- -D warnings

# Run all test suites
nix develop .#shell32 --command cargo test --manifest-path zfx-worker32/Cargo.toml
nix develop --command cargo test --manifest-path zfx-bootstrap/Cargo.toml
```

---

## License

This project is licensed under the **GNU General Public License v3.0 or later** ([GPL-3.0-or-later](file:///home/idf/dev/c51t/LICENSE)). See [`LICENSE`](file:///home/idf/dev/c51t/LICENSE) for the full text.

All trademarks are the property of their respective owners. See [`LEGAL_NOTICE.md`](file:///home/idf/dev/c51t/LEGAL_NOTICE.md) for details.
