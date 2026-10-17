---
name: zfx-reverse-engineering
description: >-
  Deep technical guide and architectural playbook for reverse engineering,
  PE-loading, bridging, and packaging 32-bit Windows Zoom ZFX DSP plugins into
  native 64-bit Linux CLAP audio plugins. Use when analyzing legacy audio DLLs,
  debugging VST 2.4 / CLAP interop, handling PE loader IAT hooks/TEB, fixing
  shared memory futex synchronization, or managing dual-mono and stereo DSP routing.
---

# Zoom ZFX Reverse Engineering, Emulation & CLAP Bridge Architecture

This skill documents the complete technical knowledge base gathered while reverse-engineering, emulating, bridging, and deploying the 86 legacy 32-bit Windows Zoom ZFX DSP plugins (from the Zoom C5.1t / S2t hardware generation) as native 64-bit Linux CLAP plugins.

---

## 1. System Overview & Architecture

The Zoom ZFX system consists of 86 dedicated guitar effect, amplifier, cabinet, and microphone DLLs originally built for 32-bit Windows (MSVC 2005 runtime `msvcr80.dll`, PE format) and locked to a proprietary USB hardware dongle.

To run these on modern 64-bit Linux DAWs (REAPER, Bitwig, etc.) with zero runtime Wine dependency and native real-time performance, the project uses a two-tier bridged architecture:

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│                             64-bit Host DAW (REAPER)                             │
│                                                                                  │
│   ┌──────────────────────────────────────────────────────────────────────────┐   │
│   │                    Native 64-bit CLAP Plugin Bundle                      │   │
│   │                         (libzfx_bootstrap.so)                            │   │
│   │                                                                          │   │
│   │   • CLAP AudioProcessor & GUI (Egui / baseview)                          │   │
│   │   • Embedded 32-bit Worker Payload (include_bytes!)                      │   │
│   │   • Embedded 32-bit DLL Payload (include_bytes!)                         │   │
│   │   • Anonymous memfd extraction & worker process supervisor               │   │
│   └────────────────────────────────────┬─────────────────────────────────────┘   │
└────────────────────────────────────────┼─────────────────────────────────────────┘
                                         │ POSIX Shared Memory (ZfxSharedBridge)
                                         │ Lock-free Linux Futex Signaling
┌────────────────────────────────────────┴─────────────────────────────────────────┐
│                    32-bit Standalone Worker (zfx-worker32)                       │
│                                                                                  │
│   • In-Memory PE Loader (peldr.rs)                                               │
│   • Hardware Dongle Patching (mov al, 1; ret)                                    │
│   • Win32 TEB / GDT LDT segment descriptor (selector 0x6b)                       │
│   • Custom IAT resolution (kernel32, msvcr80, setupapi hooks)                    │
│   • VST 2.4 Host AudioMaster & Dispatcher implementation                        │
│   • Dual-DSP Engine (True Stereo for mono effects)                               │
└──────────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. In-Memory Windows PE Loader (`peldr.rs`)

The 32-bit worker loads raw Windows PE bytes directly into memory without invoking Wine or Windows loaders.

### 2.1 Memory Mapping & Base Relocation
1. **Mapping**: Memory is allocated via `libc::mmap` with `PROT_READ | PROT_WRITE | PROT_EXEC` and `MAP_PRIVATE | MAP_ANONYMOUS`.
2. **Section Copy**: PE headers and individual sections (`.text`, `.rdata`, `.data`, `.rsrc`, `.reloc`) are copied to their respective `virtual_address` offsets.
3. **Relocation Math**: When the mapped address (`base_ptr`) differs from `image_base`, base relocations are applied:
   $$\Delta = \text{base\_ptr} - \text{image\_base}$$
   For type 3 (`IMAGE_REL_BASED_HIGHLOW`), the 32-bit value at target offset is patched:
   $$*(\text{u32*})\text{target} \mathrel{+}= \Delta$$

### 2.2 Win32 Thread Environment Block (TEB)
MSVC CRT exception handlers and thread-local routines read the Win32 TEB via x86 segment register `FS` (`fs:[0x18]` self-pointer, `fs:[0x0]` Structured Exception Handling frame).

On Linux, `peldr.rs` sets up a valid 32-bit LDT segment via `libc::syscall(SYS_set_thread_area)`:
```rust
#[repr(C)]
struct UserDesc {
    entry_number: i32,
    base_addr: u32,
    limit: u32,
    seg_32bit: u32, // 1
    contents: u32,  // 0 (data)
    read_exec_only: u32, // 0
    limit_in_pages: u32, // 0
    seg_not_present: u32, // 0
    useable: u32,   // 1
}
// Installs segment descriptor returning selector 0x6b ((entry << 3) | 3)
// Assembly then loads: mov fs, 0x6b
```

### 2.3 Hardware Dongle Bypass
Zoom DLLs verify USB dongle connectivity via `setupapi.dll`. The verification function contains an identifiable prologue:
* **Signature**: `sub esp, 0x44c` (`[0x81, 0xec, 0x4c, 0x04, 0x00, 0x00]`).
* **Binary Patch**: `mov al, 1; ret; nop; nop; nop` (`[0xb0, 0x01, 0xc3, 0x90, 0x90, 0x90]`).
* **Stack Balancing Rule**: The dongle check function is `__cdecl`. The caller cleans up arguments with `add esp, 0x18`. Using `ret 8` corrupts the caller stack; using `ret` (`0xc3`) preserves exact stack balance.

---

## 3. VST 2.4 Internal ABI & Protocol Rules

The Zoom DLLs implement Steinberg VST 2.4 internal interfaces rather than standard Win32 exports.

### 3.1 Entry Point & Dispatcher
* **Export**: `getinterface(audioMasterCallback) -> *mut AEffect`.
* **AEffect Dispatcher (`effect.dispatcher`)**:
  - Opcode 10: `effSetSampleRate` (`opt = sample_rate`)
  - Opcode 11: `effSetBlockSize` (`value = block_size`)
  - Opcode 12: `effMainsChanged` (`value = 1` resume, `value = 0` suspend)
* **DSP Processing**: `vtable[9]` (`process_replacing_f64`), taking planar double pointers:
  ```rust
  ((*effect).process_replacing_f64)(effect, in_ptrs.as_ptr(), out_ptrs.as_mut_ptr(), frames);
  ```

### 3.2 Host Master Callback Rules (`default_audio_master`)

```rust
pub unsafe extern "C" fn default_audio_master(
    _effect: *mut c_void, opcode: i32, index: i32, value: isize, _ptr: *mut c_void, opt: f32
) -> isize
```

#### Rule 1: Unknown Opcodes MUST Return `0` (NULL), NEVER `1`
* **Fatal Bug**: Delay and modulation plugins call opcode 7 (`audioMasterGetTime`). If unhandled or defaulting to `1`, the plugin treats `0x00000001` as a pointer to `VstTimeInfo*`. Dereferencing `[0x1 + offset]` causes an immediate `SIGSEGV` map error.
* **Fix**: In VST 2.4, unknown/unsupported opcodes must always return `0`.

#### Rule 2: Opcode 7 (`audioMasterGetTime`) Structure
Opcode 7 expects a pointer to a populated `VstTimeInfo` structure:
```rust
#[repr(C)]
pub struct VstTimeInfo {
    pub sample_pos: f64,
    pub sample_rate: f64,
    pub nano_seconds: f64,
    pub ppq_pos: f64,            // Beat position
    pub tempo: f64,              // BPM
    pub bar_start_pos: f64,
    pub cycle_start_pos: f64,
    pub cycle_end_pos: f64,
    pub time_sig_numerator: i32,
    pub time_sig_denominator: i32,
    pub smpte_offset: i32,
    pub smpte_frame_rate: i32,
    pub samples_to_next_clock: i32,
    pub flags: i32,              // Bitmask
}
```
**Flags Bitmask**:
- `kVstTransportPlaying = 1 << 1` (0x02)
- `kVstPpqPosValid = 1 << 9` (0x0200)
- `kVstTempoValid = 1 << 10` (0x0400)
- `kVstTimeSigValid = 1 << 13` (0x2000)

#### Rule 3: Buffer Pointer Layout Rule (`.max(2)`)
* **Assembly Discovery in `Analog_Delay.dll`**:
  ```assembly
  mov ecx, DWORD PTR [eax]     ; inputs[0] (Channel 0)
  mov edx, DWORD PTR [eax+0x4] ; inputs[1] (Channel 1) - dereferenced unconditionally!
  ```
* Even if a plugin reports `num_inputs = 1`, its inner DSP loop may unconditionally read both `inputs[0]` and `inputs[1]`.
* **Law**: `in_ptrs` and `out_ptrs` passed to `process_replacing_f64` must **always allocate at least 2 planar pointers** (`dsp_ins.max(2)`). For mono inputs, duplicate pointer 0 into pointer 1.

---

## 4. 64-bit to 32-bit Out-of-Process Bridge (`protocol.rs`)

### 4.1 Shared Memory Layout
```rust
#[repr(C)]
pub struct ZfxSharedBridge {
    pub magic: u32,               // 0x5A465842 ('ZFXB')
    pub host_seq: AtomicU32,      // Incremented by 64-bit host
    pub worker_seq: AtomicU32,    // Incremented by 32-bit worker
    pub command: AtomicU32,       // CMD_INIT, CMD_PROCESS, CMD_SHUTDOWN
    pub status: AtomicU32,        // STATUS_READY, STATUS_OK, STATUS_ERROR

    // Audio Configuration
    pub sample_rate: f32,
    pub block_size: u32,
    pub num_frames: u32,

    // Host Transport / Tempo Synchronization
    pub tempo: f64,
    pub beat_pos: f64,
    pub time_sig_num: u16,
    pub time_sig_denom: u16,
    pub is_playing: u8,
    pub has_tempo: u8,
    pub _pad: [u8; 2],

    // Plugin Introspection
    pub num_params: u32,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub params: [ParamMeta; MAX_PARAMS],

    // Real-Time Automation Queue
    pub num_param_changes: u32,
    pub param_queue: [ParamChange; MAX_PARAM_QUEUE],

    // Planar Audio Buffers (up to 4096 frames)
    pub in_l: [f32; MAX_FRAMES],
    pub in_r: [f32; MAX_FRAMES],
    pub out_l: [f32; MAX_FRAMES],
    pub out_r: [f32; MAX_FRAMES],

    // DLL Payload
    pub payload_size: u32,
    pub payload_bytes: [u8; MAX_PAYLOAD_SIZE],
}
```

### 4.2 Synchronization
* Synchronization uses Linux `sys_futex` with `FUTEX_WAIT_PRIVATE` and `FUTEX_WAKE_PRIVATE`.
* The host updates inputs, tempo, parameters, increments `host_seq`, and wakes the worker.
* The worker processes audio, updates outputs, sets `worker_seq = host_seq`, and wakes the host.
* **Timeout Safeguard**: Futex waits employ a 100ms timeout to prevent permanently hanging the host DAW thread if a worker crashes.

### 4.3 True Software Passthrough
When parameter 0 (Bypass) is active:
* Bypassing the DSP plugin in code performs **bit-exact passthrough**:
  `out_buf[..len].copy_from_slice(&in_src[..len])`
* This avoids DC thumps, buffer switching latency, or DSP state flushing clicks.

### 4.4 Dual-DSP True Stereo vs. Native Stereo
* **1-in / 1-out Plugins** (67 plugins, including all Cabinets and Microphones):
  - In a stereo track, running a single mono instance collapses the stereo field.
  - The worker instantiates two separate DSP instances (`inst_l` and `inst_r`) from the same loaded PE module.
  - Left channel audio is processed by `inst_l`; Right channel audio is processed by `inst_r`. Both channels receive identical parameter automation.
* **1-in / 2-out and 2-in / 2-out Plugins** (Delays, Reverbs):
  - These use a single DSP instance running its native internal stereo pan/delay engine.

---

## 5. Reverse-Engineered DSP Insights

### 5.1 Stompboxes & Amplifiers (`FUZZ_SMILE`, `+Distortion`)
* **Asymmetrical Non-linear Waveshaping**: Rather than standard `tanh()` curves, Zoom implemented piecewise polynomial transfer functions with dynamic DC offset shifts that simulate germanium/silicon transistor bias drift under heavy input transients.

### 5.2 Cabinets
* Algorithmic multi-pole/multi-zero IIR filters paired with resonant peaks that model speaker cone physical breakup, avoiding the phase smearing and static latency of FIR impulse responses.

### 5.3 Microphones (`Condenser_414`, `Dynamic_57`, etc.)
* **Physical Acoustic Chamber Engine**:
  - Each mic plugin allocates **44,100 samples (1 second)** of memory buffer and initializes **14 comb-filter delay lines**.
  - **`RoomSize` (Param 1)**: Modulates room reflection times, acoustic wave dissipation, and distance from speaker cone.
  - **`Sideways` (Param 2)**: Modulates off-axis high-frequency shelf filtering and proximity bass boost.
* **Parallel Dual-Micing (The Fredman Setup)**:
  - In REAPER, placing two mic plugins in parallel inside an FX Container (one on-axis center, one off-axis edge, panned L/R) sums naturally due to the built-in comb filtering and delay modeling.

### 5.4 The `AmpMdl` Box & GUI Asset Disambiguation
* The `AmpMdl` stompbox graphics in the original software (`TO AMP`, `FROM MIC`) have **no DSP DLL**. It was a GUI visual router (`type="sys"`) representing the FX loop between the floor stompboxes and the top amp/cab/mic booth.
* **Asset Isolation Law**: `AmpMdl` contains `bkgd.png` and `Thumb.png`. It must **never** be included in global or common asset fallback lists in `build.rs`, otherwise any plugin lacking an explicit thumbnail (such as microphones with unrenamed assets) will mistakenly resolve `AmpMdl/Thumb.png` and render a direct box on top of the speaker stage.

### 5.5 Pitch Shifter Expression Pedals (`Pedal_Pitch`, `Pedal_Mono_Pitch`)
* **The $O(N^2)$ Algorithmic Explosion Bug**:
  - In `Pedal_Pitch.dll` (`ProcessReplacingF64` at `0x100030da`), the plugin linearly ramps the pedal position from previous to current value across the block, feeding a smoothing biquad filter.
  - On every single sample $i$ where the smoothed pedal value changes, the code calls `0x10002010` (`pow()` frequency calculation) and sub-engine method 1 (`0x100051f0`) to regenerate modulation envelope frames for `count = frameCount - i`.
  - This results in $\sum_{i=0}^{N-1} (N - i) = \frac{N(N+1)}{2}$ sub-engine iterations per block.
  - At $48\text{ kHz}$ with tiny ASIO buffers ($N=64$), this took negligible CPU in 2007 hardware. But under $2\times$ ($N=1024$) or $4\times$ ($N=2048$ to $4096$) oversampling, iterations balloon to **2,098,176 – 8,390,656 iterations per block**, exceeding 170% CPU load and choking real-time audio threads.
  - Sub-engine method 1 *already* linearly interpolates the pitch ratio across the entire block when called once at block start (`count = frameCount`). The per-sample call inside the inner loop overwrote subsequent envelope samples on every sample $i$ only for sample $i+1$ to overwrite them again.
* **The Hot-Patch**:
  - In `Pedal_Pitch.dll`, the conditional branch at RVA `0x321d` (`jnp 0x100032a5` -> `0f 8b 82 00 00 00`) is patched to an unconditional jump (`jmp 0x100032a5` -> `e9 83 00 00 00 90`).
  - In `Pedal_Mono_Pitch.dll`, the branch at RVA `0x3200` (`jnp 0x100033a2` -> `0f 8b 9c 01 00 00`) is patched to `jmp 0x100033a2` (`e9 9d 01 00 00 90`).
  - This restores true $O(N)$ linear complexity: envelope generation executes once per block, and per-sample loop execution time drops from **37.16 ms down to 0.27 ms (a 135x speedup)**, eliminating all audio stuttering while preserving seamless pitch gliding.

---

## 6. GUI & Virtual File System (VFS) Asset Pipeline

`zfx-bootstrap` compiles an authentic, self-contained egui GUI by embedding extracted graphical assets directly into the plugin binary at build time.

### 6.1 Asset Hierarchy & Namespace Resolution
Assets are organized in two tiers within `zfx-bootstrap/assets/`:
1. **Shared Common Assets (`assets/Common/`)**:
   - Contains universal stompbox widgets shared by all plugins: `Knob00.png`–`Knob48.png` (49-frame standard rotary knob), `FootSw00.png`–`FootSw01.png` (metal footswitch toggle), `led00.png`–`led01.png`, `PPOnf00`–`PPOnf01` (ping-pong switch), `SyncOnf00`–`SyncOnf01` (tempo sync toggle), `inc_sm00`–`inc_sm01`, `dec_sm00`–`dec_sm01`, and `tap00`–`tap01`.
   - In layout XML (`EfxType.xml`), shared controls use colon namespace notation: `<Img>Common:Knob</Img>`, `<Img>Common:FootSw</Img>`.
2. **Plugin-Specific Assets (`assets/<PluginName>/`)**:
   - Contains unique faceplates (`bkgd.png` / `Bkgd.png`), plugin thumbnails (`thumb.png` / `Thumb.png`), custom knobs (e.g. `100BassMan`, `ClassicAmp`), display graphics, and specialized animation frames (e.g. `TapeEcho/Sync00.png`–`Sync26.png`, `PEDAL_CRY/Pedal00.png`–`Pedal16.png`).
   - Plugin-specific assets always take precedence over `Common` assets when keys collide.

### 6.2 Key Canonicalization in `build.rs`
During `cargo build`, `build.rs` constructs `asset_map: HashMap<String, String>` mapping every lookup key variation to its canonical file on disk:
* Direct path: `"Common/Knob00.png"`, `"Knob00.png"`
* Colon alias: `"Common:Knob00.png"`, `"Common:Knob00"`
* Lowercase aliases: `"common/knob00.png"`, `"knob00.png"`, `"knob00"`
* Extension-stripped stems: `"Common/Knob00"`, `"Knob00"`

Keys pointing to identical canonical files are deduplicated (`keys.dedup()`) into single `match` arms:
```rust
"Common/Knob00.png" | "Common:Knob00.png" | "Knob00.png" => Some(include_bytes!(r"...")),
```

### 6.3 ZRC Archive Extraction & Uppercase Gotchas
Zoom ZRC archives store packed assets with a null-terminated TOC. A common pitfall in asset carving scripts is case-sensitive string matching (`b".png\x00"`):
* Several ZRC archives specify uppercase extensions (`thumb.PNG`, `Pedal00.PNG`–`Pedal16.PNG`, `M-00.PNG`–`M-48.PNG`).
* If case-insensitive carving is not used, these assets remain unrenamed as `image_*.png` (e.g. `Dynamic_57/image_1.png` instead of `thumb.png`, `PEDAL_CRY/image_53.png` instead of `Pedal00.png`, `PatchLvl/image_0.png` instead of `M-00.png`).
* Always enforce case-insensitive regex parsing (`(?i)\.(png|jpg|jpeg)`) when carving ZRC archives.

---

## 7. Build & Packaging Pipeline

### 7.1 Nix Shells
* 32-bit compilation: `nix develop .#shell32 --command cargo build --release --manifest-path zfx-worker32/Cargo.toml`
* 64-bit plugin generation: `nix develop --command python3 build_all_plugins.py --install`

### 7.2 Self-Extracting Bundles
`build_all_plugins.py` embeds both the target Windows DLL and the compiled `zfx-worker32` binary directly into the `.clap` shared library using `include_bytes!`. When instantiated in a DAW, the plugin creates an anonymous executable `memfd`, writes the worker payload, forks, and connects via shared memory. No auxiliary installation files are required.

---

## 8. Troubleshooting Playbook

| Symptom | Root Cause | Solution |
| :--- | :--- | :--- |
| **Missing knobs & footswitch renders as solid circle** | `assets/Common` omitted from `build.rs` asset scan. | Restore `Common` directory scanning with `"Common"` and empty (`""`) prefixes. |
| **Microphone polar stage displays direct box (`AmpMdl`)** | `AmpMdl` included in global fallback assets; mic's `thumb.png` was missing or named `image_1.png`. | Never include `AmpMdl` in common assets. Rename carved `image_1.png` to `thumb.png` in mic directory. |
| **Missing treadle pedal animation (`PEDAL_CRY`)** | `Pedal00.PNG`–`Pedal16.PNG` left unrenamed as `image_53.png`–`image_69.png`. | Carve ZRC with case-insensitive matching; copy `image_53`–`69` to `Pedal00.png`–`Pedal16.png`. |
| **Missing meter knobs (`PatchLvl`, `outMaster`)** | `M-00.PNG`–`M-48.PNG` left unrenamed as `image_0.png`–`image_48.png`. | Copy `image_0`–`48` to `M-00.png`–`M-48.png` in respective asset folders. |
| **Gray/chrome knobs rendered off-center (e.g. `Tangerine`)** | `Common/Knob00.png` (45x45) shadowed local `knob00.png` (54x53) due to uppercase/lowercase casing mismatch. | Filter out shadowed Common root assets in `build.rs` and generate capitalized aliases so plugin-local assets strictly take precedence. |
| **DAW playback / play cursor hangs on play** | Worker segfaulted; host blocked on `futex_wait(&bridge.worker_seq)`. | Inspect worker logs. Ensure unknown VST opcodes return `0`, check opcode 7 (`audioMasterGetTime`). |
| **Instant crash in `process_replacing_f64`** | Plugin reads `inputs[1]` even though `num_inputs == 1`. | Guarantee `in_ptrs` and `out_ptrs` have at least 2 valid pointers (`.max(2)`). Duplicate channel 0. |
| **Loud pop/click when toggling bypass** | DSP internal state discontinuously muted or dry audio processed through DSP filters. | Implement True Software Passthrough in `lib.rs`: copy input slice directly to output slice when bypassed. |
| **Stack corruption on plugin initialization** | Dongle patch used `ret 8` instead of `ret`. | Caller was `__cdecl` (`add esp, 0x18`). Use `ret` (`0xc3`) so caller correctly balances stack. |
| **Tempo sync / delay time does not follow DAW** | Transport events not forwarded across shared memory. | Forward `_process.transport` fields (`tempo`, `song_pos_beats`) to `ZfxSharedBridge` on each process block. |

---

## 9. References & Technical Deep Dives

* [C++ DSP Class Architecture & Memory Layouts](references/cpp_class_architecture.md): Complete specification of C++ classes, vtables, sub-objects, and hot-patching offsets.
* [VST 2.4 Internal ABI Reference](references/vst_opcodes.md): Dispatcher and host callback opcodes and timing structs.
* [Plugin Catalog & Topology](references/plugins_catalog.md): Complete catalog of 86 Zoom ZFX plugins with parameter counts and channel configs.

