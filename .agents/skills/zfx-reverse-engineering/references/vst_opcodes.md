# VST 2.4 Internal ABI Reference for Zoom ZFX

## 1. AEffect Dispatcher Opcodes (Host -> Plugin)

| Opcode | Name | Value (`value`) | Index (`index`) | Ptr (`ptr`) | Opt (`opt`) | Description |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| `0` | `effOpen` | 0 | 0 | NULL | 0.0 | Plugin instantiation / setup |
| `1` | `effClose` | 0 | 0 | NULL | 0.0 | Teardown and deallocation |
| `2` | `effSetProgram` | program | 0 | NULL | 0.0 | Select preset index |
| `3` | `effGetProgram` | 0 | 0 | NULL | 0.0 | Return current preset |
| `10` | `effSetSampleRate` | 0 | 0 | NULL | sample_rate | Set sample rate (e.g., 44100.0, 48000.0) |
| `11` | `effSetBlockSize` | block_size | 0 | NULL | 0.0 | Set maximum processing block size |
| `12` | `effMainsChanged` | 1 / 0 | 0 | NULL | 0.0 | 1 = Resume processing; 0 = Suspend |
| `23` | `effGetChunk` | 0 | 0 | **ptr | 0.0 | Serialize internal preset state |
| `24` | `effSetChunk` | byte_size | 0 | *ptr | 0.0 | Restore internal preset state |

---

## 2. AudioMaster Callback Opcodes (Plugin -> Host)

| Opcode | Name | Return Expectation | Critical Note |
| :--- | :--- | :--- | :--- |
| `1` | `audioMasterVersion` | `2400` (isize) | Declares host support for VST 2.4 |
| `7` | `audioMasterGetTime` | `*const VstTimeInfo` or `0` | **CRITICAL**: Return valid pointer to `VstTimeInfo` or `0` (NULL). Never return `1`. |
| `16` | `audioMasterGetSampleRate`| Current sample rate (e.g. `44100`) | Integer sample rate |
| `17` | `audioMasterGetBlockSize` | Current block size (e.g. `256`) | Integer buffer size |
| `*` | Any other / unknown opcode | **`0`** (NULL/false) | **CRITICAL**: In VST 2.4, unknown opcodes must always return 0. |

---

## 3. VstTimeInfo Bitmask Flags

```c
enum VstTimeInfoFlags {
    kVstTransportChanged     = 1 << 0,
    kVstTransportPlaying     = 1 << 1,   // 0x0002
    kVstTransportCycleActive = 1 << 2,
    kVstTransportRecording   = 1 << 3,
    kVstAutomationWriting    = 1 << 6,
    kVstAutomationReading    = 1 << 7,
    kVstNanosValid           = 1 << 8,
    kVstPpqPosValid          = 1 << 9,   // 0x0200 (Musical beat position)
    kVstTempoValid           = 1 << 10,  // 0x0400 (BPM tempo valid)
    kVstBarsValid            = 1 << 11,
    kVstCyclePosValid        = 1 << 12,
    kVstTimeSigValid         = 1 << 13,  // 0x2000 (Time signature valid)
    kVstSmpteValid           = 1 << 14,
    kVstClockValid           = 1 << 15
};
```
