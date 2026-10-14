# Zoom ZFX Plugin Catalog & Routing Matrix

Total plugins: **86 production audio plugins** + internal routing utilities.

## 1. Category Classification & Channel Routing

| Category | Count | Channel I/O | Stereo Architecture | Examples |
| :--- | :--- | :--- | :--- | :--- |
| **Cabinet** | 11 | 1-in / 1-out | Dual-DSP True Stereo | `1959_Stack`, `BM100_Stack_4x12`, `Tangerine_Stack_4x12`, `UK30_Combo_2x12` |
| **Microphone** | 4 | 1-in / 1-out | Dual-DSP True Stereo | `Condenser_414`, `Condenser_87`, `Dynamic_421`, `Dynamic_57` |
| **Amp Head** | 16 | 1-in / 1-out | Dual-DSP True Stereo | `100BassMan`, `1959`, `DZ_Bert`, `HW_100_Custom`, `MB_Dual_Head`, `MS_Crunch` |
| **Distortion / Fuzz** | 13 | 1-in / 1-out | Dual-DSP True Stereo | `FUZZ_SMILE`, `+Distortion`, `GREAT_MUFF`, `SQUEAK`, `THE_governor`, `TubeScreamen` |
| **Delay / Echo** | 4 | 1-in / 2-out | Native Stereo Engine | `Analog_Delay`, `Delay`, `Reverse_Delay`, `TapeEcho` |
| **Reverb** | 4 | 1-in / 2-out | Native Stereo Engine | `Hall`, `Room`, `Plate`, `Spring` |
| **Modulation** | 8 | 1-in / 2-out / Dual | Native Stereo / Dual | `Chorus`, `Chorus_Vibrato`, `Flanger`, `Phaser`, `Tremolo`, `Ring_Modulator`, `Step` |
| **Pitch** | 4 | 1-in / 1-out & 2-out| Dual / Native Stereo | `HPS`, `Mono_Pitch`, `Octave`, `Pitch_Shifter`, `Pedal_Pitch` |
| **Filter / Wah** | 4 | 1-in / 1-out | Dual-DSP True Stereo | `Auto_Wah`, `Cry`, `EQ`, `Isolator`, `PEDAL_CRY` |
| **Dynamics** | 4 | 1-in / 1-out | Dual-DSP True Stereo | `Compressor`, `Dynamic_Compressor`, `Limiter`, `ZNR` |
| **Utility** | 14 | 1-in / 2-out & Dual | Utility / Split / Mix | `Mixer`, `Splitter`, `Volume_Pedal`, `inMaster`, `outMaster` |

---

## 2. Key Parameter Behaviors

* **Parameter 0**: Dedicated **Bypass** or **Power On/Off** control.
  - In plugins named `*Bypass*`: value `>= 0.5` = Bypassed.
  - In plugins named `*On/Off*` or `*Power*`: value `< 0.5` = Bypassed.
  - Handled by host CLAP wrapper for **True Software Passthrough**.
* **Microphone Parameters**:
  - `Parameter 1`: **`RoomSize`** (Acoustic reflections and distance from speaker cone).
  - `Parameter 2`: **`Sideways`** (Off-axis angle across the speaker cone).
* **Delay Parameters**:
  - `BPM Sync`: When set to host sync (100.0), queries opcode 7 (`audioMasterGetTime`) for real-time project tempo sync.
