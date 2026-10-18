#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity
)]

pub mod gui;
pub mod params;
pub mod protocol;
#[cfg(feature = "feat_oversampling")]
pub mod resampler;
pub mod zrc;

use clack_plugin::prelude::*;
use std::sync::atomic::Ordering;

#[allow(clippy::all, unused)]
mod plugin_config {
    include!(concat!(env!("OUT_DIR"), "/zfx_plugin_config.rs"));
}
pub use plugin_config::*;

use clack_common::events::event_types::TransportFlags;
use clack_common::stream::{InputStream, OutputStream};
use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts,
    PluginAudioPortsImpl,
};
use clack_extensions::state::{PluginState, PluginStateImpl};
use std::io::{Read, Write};

#[derive(Copy, Clone, Debug)]
pub enum GuiParamEvent {
    BeginGesture(u32),
    SetValue(u32, f64),
    EndGesture(u32),
}

#[cfg(feature = "feat_oversampling")]
pub const PARAM_OVERSAMPLING: u32 = 9900;
#[cfg(feature = "feat_oversampling")]
pub const PARAM_OVERSAMPLING_METHOD: u32 = 9901;

pub struct ZfxBootstrapPlugin;

impl Plugin for ZfxBootstrapPlugin {
    type AudioProcessor<'a> = ZfxAudioProcessor<'a>;
    type Shared<'a> = ZfxShared;
    type MainThread<'a> = ZfxMainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&Self::Shared<'_>>) {
        builder.register::<clack_extensions::params::PluginParams>();
        builder.register::<clack_extensions::gui::PluginGui>();
        builder.register::<PluginAudioPorts>();
        builder.register::<PluginState>();
        builder.register::<clack_extensions::latency::PluginLatency>();
    }
}

#[derive(Debug, Clone)]
pub struct PluginParamDef {
    pub id: u32,
    pub name: String,
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
}

pub struct ZfxShared {
    pub bridge_ptr: *mut protocol::ZfxSharedBridge,
    #[cfg(target_os = "linux")]
    pub bridge_fd: libc::c_int,
    #[cfg(target_os = "linux")]
    pub worker_pid: libc::pid_t,
    #[cfg(target_os = "windows")]
    pub h_map: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(target_os = "windows")]
    pub worker_handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(target_os = "windows")]
    pub job_handle: windows_sys::Win32::Foundation::HANDLE,
    pub layout: Option<EfxTypeInfo>,
    pub params: Vec<PluginParamDef>,
    pub param_values: std::sync::Arc<params::ParamStore>,
    pub gui_events: std::sync::Arc<std::sync::Mutex<Vec<GuiParamEvent>>>,
    pub is_active: std::sync::atomic::AtomicBool,
    #[cfg(feature = "feat_oversampling")]
    pub oversampling_mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    #[cfg(feature = "feat_oversampling")]
    pub oversampling_phase: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl ZfxShared {
    #[cfg(feature = "feat_oversampling")]
    #[must_use]
    pub fn oversampling_mode(&self) -> resampler::OversamplingMode {
        resampler::OversamplingMode::from_u8(self.oversampling_mode.load(Ordering::Acquire))
    }

    #[cfg(feature = "feat_oversampling")]
    pub fn set_oversampling_mode(&self, mode: resampler::OversamplingMode) {
        self.oversampling_mode
            .store(mode.to_u8(), Ordering::Release);
    }

    #[cfg(feature = "feat_oversampling")]
    #[must_use]
    pub fn oversampling_phase(&self) -> resampler::OversamplingPhase {
        resampler::OversamplingPhase::from_u8(self.oversampling_phase.load(Ordering::Acquire))
    }

    #[cfg(feature = "feat_oversampling")]
    pub fn set_oversampling_phase(&self, phase: resampler::OversamplingPhase) {
        self.oversampling_phase
            .store(phase.to_u8(), Ordering::Release);
    }

    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn latency_samples(&self) -> u32 {
        #[cfg(feature = "feat_oversampling")]
        return self
            .oversampling_mode()
            .latency_samples(self.oversampling_phase());
        #[cfg(not(feature = "feat_oversampling"))]
        0
    }
}

// SAFETY: `bridge_ptr` points to POSIX shared memory synchronized across threads using atomic operations and futexes.
unsafe impl Send for ZfxShared {}
// SAFETY: Concurrent access to `ZfxShared` fields is guarded by atomic operations, Arc<ParamStore>, and Arc<Mutex>.
unsafe impl Sync for ZfxShared {}
impl PluginShared<'_> for ZfxShared {}

impl Drop for ZfxShared {
    fn drop(&mut self) {
        if !self.bridge_ptr.is_null() {
            // SAFETY: `bridge_ptr` is verified non-null and was mapped to the size of ZfxSharedBridge.
            unsafe {
                let bridge = &mut *self.bridge_ptr;
                bridge
                    .command
                    .store(protocol::CMD_SHUTDOWN, Ordering::Release);
                let seq = bridge.host_seq.load(Ordering::Relaxed).wrapping_add(1);
                bridge.host_seq.store(seq, Ordering::Release);
                protocol::futex_wake(&raw const bridge.host_seq);

                // Wait up to 50ms for worker to gracefully shut down
                for _ in 0..5 {
                    let wseq = bridge.worker_seq.load(Ordering::Acquire);
                    if wseq == seq {
                        break;
                    }
                    protocol::futex_wait(&raw const bridge.worker_seq, wseq, Some(10_000));
                }

                #[cfg(target_os = "linux")]
                {
                    libc::munmap(
                        self.bridge_ptr.cast::<libc::c_void>(),
                        std::mem::size_of::<protocol::ZfxSharedBridge>(),
                    );
                    libc::close(self.bridge_fd);
                }
                #[cfg(target_os = "windows")]
                {
                    use windows_sys::Win32::Foundation::CloseHandle;
                    use windows_sys::Win32::System::Memory::{
                        UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS,
                    };
                    UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.bridge_ptr.cast(),
                    });
                    if !self.h_map.is_null() {
                        CloseHandle(self.h_map);
                    }
                }
            }
        }

        #[cfg(target_os = "linux")]
        if self.worker_pid > 0 {
            // SAFETY: Terminating child worker process via standard POSIX signal and waitpid.
            unsafe {
                libc::kill(self.worker_pid, libc::SIGTERM);
                let mut status = 0;
                libc::waitpid(self.worker_pid, &raw mut status, libc::WNOHANG);
            }
        }
        #[cfg(target_os = "windows")]
        // SAFETY: Terminating child worker process via Win32 TerminateProcess and closing open handles.
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::TerminateProcess;
            if !self.worker_handle.is_null() {
                TerminateProcess(self.worker_handle, 0);
                CloseHandle(self.worker_handle);
            }
            if !self.job_handle.is_null() {
                CloseHandle(self.job_handle);
            }
        }
    }
}

pub struct ZfxMainThread<'a> {
    pub shared: &'a ZfxShared,
    pub host: HostMainThreadHandle<'a>,
    pub gui_window: std::cell::RefCell<Option<baseview::Window>>,
}
impl<'a> PluginMainThread<'a, ZfxShared> for ZfxMainThread<'a> {}

impl clack_extensions::latency::PluginLatencyImpl for ZfxMainThread<'_> {
    fn get(&self) -> u32 {
        self.shared.latency_samples()
    }
}

impl ZfxMainThread<'_> {
    #[cfg(feature = "feat_oversampling")]
    pub fn on_oversampling_changed(
        &self,
        mode: resampler::OversamplingMode,
        phase: resampler::OversamplingPhase,
    ) {
        self.shared.set_oversampling_mode(mode);
        self.shared.set_oversampling_phase(phase);
        if self.shared.is_active.load(Ordering::Acquire) {
            if let Some(host_latency) = self
                .host
                .get_extension::<clack_extensions::latency::HostLatency>()
            {
                host_latency.changed(&self.host);
            }
        }
    }
}

use clack_extensions::params::{
    ParamDisplayWriter, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams,
};
use clack_plugin::events::io::{InputEvents, OutputEvents};
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::utils::ClapId;
use core::ffi::CStr;

impl PluginMainThreadParams for ZfxMainThread<'_> {
    fn count(&self) -> u32 {
        self.shared.params.len() as u32
    }

    fn get_info(&self, param_index: u32, info: &mut ParamInfoWriter) {
        if let Some(param) = self.shared.params.get(param_index as usize) {
            let param_info = clack_extensions::params::ParamInfo {
                id: ClapId::new(param.id),
                flags: ParamInfoFlags::IS_AUTOMATABLE | ParamInfoFlags::IS_STEPPED,
                cookie: clack_plugin::utils::Cookie::empty(),
                name: param.name.as_bytes(),
                module: b"\0",
                min_value: param.min_value,
                max_value: param.max_value,
                default_value: param.default_value,
            };
            info.set(&param_info);
        }
    }

    fn get_value(&self, param_id: ClapId) -> Option<f64> {
        self.shared.param_values.get(param_id.get()).or_else(|| {
            self.shared
                .params
                .iter()
                .find(|p| p.id == param_id.get())
                .map(|p| p.default_value)
        })
    }

    fn value_to_text(
        &self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> core::fmt::Result {
        use core::fmt::Write;
        match param_id.get() {
            #[cfg(feature = "feat_oversampling")]
            PARAM_OVERSAMPLING => match value.round() as u8 {
                1 => write!(writer, "2x"),
                2 => write!(writer, "4x"),
                _ => write!(writer, "1x"),
            },
            #[cfg(feature = "feat_oversampling")]
            PARAM_OVERSAMPLING_METHOD => match value.round() as u8 {
                0 => write!(writer, "Linear"),
                _ => write!(writer, "Minimal"),
            },
            _ => write!(writer, "{value:.2}"),
        }
    }

    fn text_to_value(&self, param_id: ClapId, text: &CStr) -> Option<f64> {
        let s = text.to_str().ok()?;
        match param_id.get() {
            #[cfg(feature = "feat_oversampling")]
            PARAM_OVERSAMPLING => {
                let lower = s.to_lowercase();
                match () {
                    () if lower.contains("4x") => Some(2.0),
                    () if lower.contains("2x") => Some(1.0),
                    () if lower.contains("1x") || lower.contains("off") => Some(0.0),
                    () => s.parse::<f64>().ok(),
                }
            }
            #[cfg(feature = "feat_oversampling")]
            PARAM_OVERSAMPLING_METHOD => {
                let lower = s.to_lowercase();
                match () {
                    () if lower.contains("lin") => Some(0.0),
                    () if lower.contains("min") => Some(1.0),
                    () => s.parse::<f64>().ok(),
                }
            }
            _ => s.parse::<f64>().ok(),
        }
    }

    fn flush(
        &self,
        input_parameter_changes: &InputEvents,
        output_parameter_changes: &mut OutputEvents,
    ) {
        #[cfg(feature = "feat_oversampling")]
        let mut new_os_mode: Option<resampler::OversamplingMode> = None;
        #[cfg(feature = "feat_oversampling")]
        let mut new_os_phase: Option<resampler::OversamplingPhase> = None;
        for (id, val) in input_parameter_changes.into_iter().filter_map(|event| {
            match event.as_core_event() {
                Some(CoreEventSpace::ParamValue(p)) => {
                    p.param_id().map(|id| (id.get(), p.value()))
                }
                _ => None,
            }
        }) {
            let _ = self.shared.param_values.set(id, val);
            #[cfg(feature = "feat_oversampling")]
            match id {
                PARAM_OVERSAMPLING => {
                    new_os_mode = Some(resampler::OversamplingMode::from_u8(val.round() as u8));
                }
                PARAM_OVERSAMPLING_METHOD => {
                    new_os_phase = Some(resampler::OversamplingPhase::from_u8(val.round() as u8));
                }
                _ => {}
            }
        }
        #[cfg(feature = "feat_oversampling")]
        match (new_os_mode, new_os_phase) {
            (None, None) => {}
            (mode_opt, phase_opt) => {
                let mode = mode_opt.unwrap_or_else(|| self.shared.oversampling_mode());
                let phase = phase_opt.unwrap_or_else(|| self.shared.oversampling_phase());
                self.on_oversampling_changed(mode, phase);
            }
        }

        if let Ok(mut gui_evs) = self.shared.gui_events.try_lock() {
            for ev in gui_evs.drain(..) {
                match ev {
                    GuiParamEvent::BeginGesture(id) => {
                        let clap_ev =
                            clack_common::events::event_types::ParamGestureBeginEvent::new(
                                0,
                                ClapId::new(id),
                            );
                        output_parameter_changes.try_push(clap_ev.as_unknown()).ok();
                    }
                    GuiParamEvent::SetValue(id, val) => {
                        let clap_ev = clack_common::events::event_types::ParamValueEvent::new(
                            0,
                            ClapId::new(id),
                            clack_common::events::Pckn::match_all(),
                            val,
                        );
                        output_parameter_changes.try_push(clap_ev.as_unknown()).ok();
                    }
                    GuiParamEvent::EndGesture(id) => {
                        let clap_ev = clack_common::events::event_types::ParamGestureEndEvent::new(
                            0,
                            ClapId::new(id),
                        );
                        output_parameter_changes.try_push(clap_ev.as_unknown()).ok();
                    }
                }
            }
        }
    }
}

use zerocopy::{little_endian as le, FromBytes, Immutable, IntoBytes, KnownLayout};

/// Header for ZFX serialized plugin state (12 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ZfxStateHeader {
    pub magic: [u8; 4],
    pub version: le::U32,
    pub count: le::U32,
}

/// Header extension for version 2 state (oversampling mode & phase, 4 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ZfxStateV2Ext {
    pub os_mode: u8,
    pub os_phase: u8,
    pub pad: [u8; 2],
}

/// Serialized parameter entry (4-byte ID + 8-byte float value = 12 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ZfxStateParamEntry {
    pub id: le::U32,
    pub val: le::F64,
}

const _: () = assert!(std::mem::size_of::<ZfxStateHeader>() == 12);
const _: () = assert!(std::mem::size_of::<ZfxStateV2Ext>() == 4);
const _: () = assert!(std::mem::size_of::<ZfxStateParamEntry>() == 12);

impl ZfxMainThread<'_> {
    fn apply_loaded_params(&self, loaded_entries: impl IntoIterator<Item = (u32, f64)>) {
        #[cfg(feature = "feat_oversampling")]
        let mut new_os_mode: Option<resampler::OversamplingMode> = None;
        #[cfg(feature = "feat_oversampling")]
        let mut new_os_phase: Option<resampler::OversamplingPhase> = None;
        for (id, val) in loaded_entries {
            let _ = self.shared.param_values.set(id, val);
            #[cfg(feature = "feat_oversampling")]
            match id {
                PARAM_OVERSAMPLING => {
                    new_os_mode = Some(resampler::OversamplingMode::from_u8(val.round() as u8));
                }
                PARAM_OVERSAMPLING_METHOD => {
                    new_os_phase = Some(resampler::OversamplingPhase::from_u8(val.round() as u8));
                }
                _ => {}
            }
        }

        #[cfg(feature = "feat_oversampling")]
        match (new_os_mode, new_os_phase) {
            (None, None) => {}
            (mode_opt, phase_opt) => {
                let mode = mode_opt.unwrap_or_else(|| self.shared.oversampling_mode());
                let phase = phase_opt.unwrap_or_else(|| self.shared.oversampling_phase());
                self.on_oversampling_changed(mode, phase);
            }
        }

        // Notify host that parameter values were updated from state load
        if let Some(host_params) = self
            .host
            .get_extension::<clack_extensions::params::HostParams>()
        {
            host_params.rescan(
                &self.host,
                clack_extensions::params::ParamRescanFlags::VALUES,
            );
        }
    }
}

impl PluginStateImpl for ZfxMainThread<'_> {
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        let mut sorted_entries = self.shared.param_values.snapshot();
        sorted_entries.sort_by_key(|&(k, _)| k);

        let count_u32 = u32::try_from(sorted_entries.len())
            .map_err(|_| PluginError::Message("Too many parameters to serialize"))?;

        #[cfg(feature = "feat_oversampling")]
        let version = 2u32;
        #[cfg(not(feature = "feat_oversampling"))]
        let version = 1u32;

        let header = ZfxStateHeader {
            magic: *b"ZFXS",
            version: le::U32::new(version),
            count: le::U32::new(count_u32),
        };

        output
            .write_all(header.as_bytes())
            .map_err(|_| PluginError::Message("Failed to write state header"))?;

        #[cfg(feature = "feat_oversampling")]
        {
            let v2_ext = ZfxStateV2Ext {
                os_mode: self.shared.oversampling_mode().to_u8(),
                os_phase: self.shared.oversampling_phase().to_u8(),
                pad: [0; 2],
            };
            output
                .write_all(v2_ext.as_bytes())
                .map_err(|_| PluginError::Message("Failed to write v2 header ext"))?;
        }

        let entries: Vec<ZfxStateParamEntry> = sorted_entries
            .iter()
            .map(|&(id, val)| ZfxStateParamEntry {
                id: le::U32::new(id),
                val: le::F64::new(val),
            })
            .collect();

        output
            .write_all(entries.as_bytes())
            .map_err(|_| PluginError::Message("Failed to write state stream"))?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut header = ZfxStateHeader {
            magic: [0; 4],
            version: le::U32::ZERO,
            count: le::U32::ZERO,
        };
        input
            .read_exact(header.as_mut_bytes())
            .map_err(|_| PluginError::Message("State stream too short"))?;

        if &header.magic != b"ZFXS" {
            return Err(PluginError::Message("Invalid ZFX state magic"));
        }
        let version = header.version.get();
        if version != 1 && version != 2 {
            return Err(PluginError::Message("Unsupported ZFX state version"));
        }
        let count = usize::try_from(header.count.get()).unwrap_or(0);

        if version >= 2 {
            let mut v2_extra = ZfxStateV2Ext {
                os_mode: 0,
                os_phase: 0,
                pad: [0; 2],
            };
            input
                .read_exact(v2_extra.as_mut_bytes())
                .map_err(|_| PluginError::Message("Truncated v2 state header"))?;
            #[cfg(feature = "feat_oversampling")]
            {
                let os_mode = resampler::OversamplingMode::from_u8(v2_extra.os_mode);
                let os_phase = resampler::OversamplingPhase::from_u8(v2_extra.os_phase);
                self.on_oversampling_changed(os_mode, os_phase);
            }
        }

        let mut entries = vec![
            ZfxStateParamEntry {
                id: le::U32::ZERO,
                val: le::F64::ZERO,
            };
            count
        ];
        input
            .read_exact(entries.as_mut_bytes())
            .map_err(|_| PluginError::Message("Truncated state stream"))?;

        self.apply_loaded_params(entries.iter().map(|e| (e.id.get(), e.val.get())));
        Ok(())
    }
}

impl PluginAudioPortsImpl for ZfxMainThread<'_> {
    fn count(&self, _is_input: bool) -> u32 {
        1
    }

    fn get(&self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index != 0 {
            return;
        }

        let name: &[u8] = if is_input {
            b"Main Input"
        } else {
            b"Main Output"
        };
        let info = AudioPortInfo {
            id: ClapId::new(u32::from(!is_input)),
            name,
            channel_count: 2,
            flags: AudioPortFlags::IS_MAIN,
            port_type: Some(AudioPortType::STEREO),
            in_place_pair: Some(ClapId::new(u32::from(is_input))),
        };
        writer.set(&info);
    }
}

use serde::Deserialize;

fn de_opt_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    opt.map_or_else(
        || Ok(None),
        |s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<u32>()
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
        },
    )
}

fn de_opt_f32<'de, D>(deserializer: D) -> Result<Option<f32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    opt.map_or_else(
        || Ok(None),
        |s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<f32>()
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
        },
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct EfxTypeInfo {
    #[serde(rename = "Total")]
    pub total: Total,
    #[serde(rename = "Ctrls", default)]
    pub ctrls: Option<Ctrls>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Total {
    #[serde(rename = "Width")]
    pub width: f32,
    #[serde(rename = "Height")]
    pub height: f32,
    #[serde(rename = "BkgdImg", default)]
    pub bkgd_img: Option<BkgdImg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BkgdImg {
    #[serde(rename = "Img", default)]
    pub img: Option<Img>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Img {
    #[serde(rename = "$value", default)]
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ctrls {
    #[serde(rename = "Ctrl", default)]
    pub ctrl: Vec<Ctrl>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ctrl {
    #[serde(rename = "@index")]
    pub index: u32,
    #[serde(rename = "@prm", default)]
    pub prm: u32,

    #[serde(rename = "Type")]
    pub type_tag: String,
    #[serde(rename = "Img")]
    pub img: String,
    #[serde(rename = "Max", default, deserialize_with = "de_opt_u32")]
    pub max: Option<u32>,
    #[serde(rename = "Delta", default, deserialize_with = "de_opt_f32")]
    pub delta: Option<f32>,
    #[serde(rename = "PosX")]
    pub pos_x: f32,
    #[serde(rename = "PosY")]
    pub pos_y: f32,
    #[serde(rename = "Width")]
    pub width: f32,
    #[serde(rename = "Height")]
    pub height: f32,
    #[serde(rename = "CenterX", default, deserialize_with = "de_opt_f32")]
    pub center_x: Option<f32>,
    #[serde(rename = "CenterY", default, deserialize_with = "de_opt_f32")]
    pub center_y: Option<f32>,
    #[serde(rename = "SlWidth", default, deserialize_with = "de_opt_f32")]
    pub sl_width: Option<f32>,
    #[serde(rename = "SlHeight", default, deserialize_with = "de_opt_f32")]
    pub sl_height: Option<f32>,
    #[serde(rename = "Name", default)]
    pub name: Option<String>,
    #[serde(rename = "ParamOfIndex", default, deserialize_with = "de_opt_u32")]
    pub param_of_index: Option<u32>,
    #[serde(rename = "SubCtrl", default, deserialize_with = "de_opt_u32")]
    pub sub_ctrl: Option<u32>,
    #[serde(rename = "DisableToolTip", default, deserialize_with = "de_opt_u32")]
    pub disable_tool_tip: Option<u32>,
}

pub struct ZfxAudioProcessor<'a> {
    pub shared: &'a ZfxShared,
    pub in_l: Vec<f32>,
    pub in_r: Vec<f32>,
    pub out_l: Vec<f32>,
    pub out_r: Vec<f32>,
    #[cfg(feature = "feat_oversampling")]
    pub os_in_l: Vec<f32>,
    #[cfg(feature = "feat_oversampling")]
    pub os_in_r: Vec<f32>,
    #[cfg(feature = "feat_oversampling")]
    pub os_out_l: Vec<f32>,
    #[cfg(feature = "feat_oversampling")]
    pub os_out_r: Vec<f32>,
    #[cfg(feature = "feat_oversampling")]
    pub oversampler: resampler::StereoOversampler,
    pub host_sample_rate: f64,
    pub seq: u32,
    /// Normalized values last sent to the worker. `f32::NAN` = never sent.
    /// Parallel to `shared.params`.
    pub last_sent_norm: Vec<f32>,
}

impl PluginAudioProcessorParams for ZfxAudioProcessor<'_> {
    #[allow(clippy::too_many_lines)]
    fn flush(
        &mut self,
        input_parameter_changes: &InputEvents,
        output_parameter_changes: &mut OutputEvents,
    ) {
        for (id, val) in input_parameter_changes.into_iter().filter_map(|event| {
            match event.as_core_event() {
                Some(CoreEventSpace::ParamValue(p)) => {
                    p.param_id().map(|id| (id.get(), p.value()))
                }
                _ => None,
            }
        }) {
            let _ = self.shared.param_values.set(id, val);
            #[cfg(feature = "feat_oversampling")]
            match id {
                PARAM_OVERSAMPLING => {
                    let mode = resampler::OversamplingMode::from_u8(val.round() as u8);
                    self.shared.set_oversampling_mode(mode);
                }
                PARAM_OVERSAMPLING_METHOD => {
                    let phase = resampler::OversamplingPhase::from_u8(val.round() as u8);
                    self.shared.set_oversampling_phase(phase);
                }
                _ => {}
            }
        }

        // Synchronize changed parameters to shared bridge even when transport is stopped
        let bridge_ptr = self.shared.bridge_ptr;
        if !bridge_ptr.is_null() {
            let mut num_chgs = 0;
            for (idx, param) in self.shared.params.iter().enumerate() {
                #[cfg(feature = "feat_oversampling")]
                match param.id {
                    PARAM_OVERSAMPLING | PARAM_OVERSAMPLING_METHOD => continue,
                    _ => {}
                }
                if let Some(val) = self.shared.param_values.get(param.id) {
                    let range = param.max_value - param.min_value;
                    let norm = if range > 0.0 {
                        ((val - param.min_value) / range).clamp(0.0, 1.0) as f32
                    } else {
                        val.clamp(0.0, 1.0) as f32
                    };

                    let changed = match self.last_sent_norm.get(idx).copied() {
                        Some(last) if last.is_finite() => (last - norm).abs() > 1e-5,
                        _ => true,
                    };

                    if changed && num_chgs < protocol::MAX_PARAM_QUEUE {
                        // SAFETY: `bridge_ptr` is non-null and `num_chgs` is bounded by `MAX_PARAM_QUEUE`.
                        unsafe {
                            (*bridge_ptr).param_queue[num_chgs] = protocol::ParamChange {
                                id: param.id,
                                value: norm,
                            };
                        }
                        num_chgs += 1;
                        if let Some(slot) = self.last_sent_norm.get_mut(idx) {
                            *slot = norm;
                        }
                    }
                }
            }

            if num_chgs > 0 {
                // SAFETY: `bridge_ptr` points to valid shared memory allocated via mmap.
                unsafe {
                    let bridge = &mut *bridge_ptr;
                    bridge.num_param_changes = num_chgs as u32;
                    bridge.num_frames = 0;
                    bridge
                        .command
                        .store(protocol::CMD_PROCESS, Ordering::Release);
                    self.seq = self.seq.wrapping_add(1);
                    if self.seq == 0 {
                        self.seq = 1;
                    }
                    bridge.host_seq.store(self.seq, Ordering::Release);
                    protocol::futex_wake(&raw const bridge.host_seq);

                    for _ in 0..100 {
                        let wseq = bridge.worker_seq.load(Ordering::Acquire);
                        if wseq == self.seq {
                            break;
                        }
                        protocol::futex_wait(&raw const bridge.worker_seq, wseq, Some(2_000));
                    }
                }
            }
        }

        if let Ok(mut gui_evs) = self.shared.gui_events.try_lock() {
            for ev in gui_evs.drain(..) {
                match ev {
                    GuiParamEvent::BeginGesture(id) => {
                        let clap_ev =
                            clack_common::events::event_types::ParamGestureBeginEvent::new(
                                0,
                                ClapId::new(id),
                            );
                        output_parameter_changes.try_push(clap_ev.as_unknown()).ok();
                    }
                    GuiParamEvent::SetValue(id, val) => {
                        let clap_ev = clack_common::events::event_types::ParamValueEvent::new(
                            0,
                            ClapId::new(id),
                            clack_common::events::Pckn::match_all(),
                            val,
                        );
                        output_parameter_changes.try_push(clap_ev.as_unknown()).ok();
                    }
                    GuiParamEvent::EndGesture(id) => {
                        let clap_ev = clack_common::events::event_types::ParamGestureEndEvent::new(
                            0,
                            ClapId::new(id),
                        );
                        output_parameter_changes.try_push(clap_ev.as_unknown()).ok();
                    }
                }
            }
        }
    }
}

impl<'a> PluginAudioProcessor<'a, ZfxShared, ZfxMainThread<'a>> for ZfxAudioProcessor<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &ZfxMainThread<'a>,
        shared: &'a ZfxShared,
        config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        let max_frames = (config.max_frames_count as usize).clamp(4096, protocol::MAX_FRAMES);

        let bridge_ptr = shared.bridge_ptr;
        if !bridge_ptr.is_null() {
            // SAFETY: `bridge_ptr` is checked non-null and points to the mapped `ZfxSharedBridge` instance.
            unsafe {
                let bridge = &mut *bridge_ptr;
                #[cfg(feature = "feat_oversampling")]
                let target_sr = {
                    let factor = shared.oversampling_mode().factor() as f32;
                    (config.sample_rate as f32) * factor
                };
                #[cfg(not(feature = "feat_oversampling"))]
                let target_sr = config.sample_rate as f32;
                bridge.sample_rate = target_sr;
                #[cfg(feature = "feat_oversampling")]
                {
                    bridge.block_size = (config.max_frames_count
                        * (shared.oversampling_mode().factor() as u32))
                        .max(64);
                }
                #[cfg(not(feature = "feat_oversampling"))]
                {
                    bridge.block_size = config.max_frames_count.max(64);
                }
            }
        }

        shared.is_active.store(true, Ordering::Release);

        #[cfg(feature = "feat_oversampling")]
        let max_os_frames = (max_frames * 4).min(protocol::MAX_FRAMES);
        #[cfg(feature = "feat_oversampling")]
        let oversampler = {
            let mut s = resampler::StereoOversampler::new(
                shared.oversampling_mode(),
                shared.oversampling_phase(),
            );
            s.ensure_capacity(max_frames);
            s
        };

        Ok(Self {
            shared,
            in_l: vec![0.0; max_frames],
            in_r: vec![0.0; max_frames],
            out_l: vec![0.0; max_frames],
            out_r: vec![0.0; max_frames],
            #[cfg(feature = "feat_oversampling")]
            os_in_l: vec![0.0; max_os_frames],
            #[cfg(feature = "feat_oversampling")]
            os_in_r: vec![0.0; max_os_frames],
            #[cfg(feature = "feat_oversampling")]
            os_out_l: vec![0.0; max_os_frames],
            #[cfg(feature = "feat_oversampling")]
            os_out_r: vec![0.0; max_os_frames],
            #[cfg(feature = "feat_oversampling")]
            oversampler,
            host_sample_rate: config.sample_rate,
            seq: 1,
            last_sent_norm: vec![f32::NAN; shared.params.len()],
        })
    }

    #[allow(clippy::too_many_lines)]
    fn process(
        &mut self,
        process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        let bridge_ptr = self.shared.bridge_ptr;
        if bridge_ptr.is_null() {
            return Ok(ProcessStatus::Continue);
        }

        // 0. Synchronize host transport & tempo to 32-bit DSP companion
        if let Some(transport) = process.transport {
            // SAFETY: `bridge_ptr` points to valid shared memory allocated via mmap.
            unsafe {
                let bridge = &mut *bridge_ptr;
                if transport.flags.contains(TransportFlags::HAS_TEMPO) {
                    bridge.tempo = transport.tempo;
                    bridge.has_tempo = 1;
                }
                if transport.flags.contains(TransportFlags::HAS_BEATS_TIMELINE) {
                    bridge.beat_pos = transport.song_pos_beats.to_float();
                }
                if transport.flags.contains(TransportFlags::HAS_TIME_SIGNATURE) {
                    bridge.time_sig_num = transport.time_signature_numerator;
                    bridge.time_sig_denom = transport.time_signature_denominator;
                }
                bridge.is_playing = u8::from(transport.flags.contains(TransportFlags::IS_PLAYING));
            }
        }

        #[cfg(feature = "feat_oversampling")]
        {
            let cur_mode = self.shared.oversampling_mode();
            let cur_phase = self.shared.oversampling_phase();
            if self.oversampler.mode() != cur_mode || self.oversampler.phase() != cur_phase {
                self.last_sent_norm.fill(f32::NAN); // Force full parameter resync
                self.oversampler.set_config(cur_mode, cur_phase);
            }
        }

        // 1. Process parameter automation changes
        {
            for (id, val) in events.input.into_iter().filter_map(|event| {
                match event.as_core_event() {
                    Some(CoreEventSpace::ParamValue(p)) => {
                        p.param_id().map(|id| (id.get(), p.value()))
                    }
                    _ => None,
                }
            }) {
                let _ = self.shared.param_values.set(id, val);
                #[cfg(feature = "feat_oversampling")]
                match id {
                    PARAM_OVERSAMPLING => {
                        let mode = resampler::OversamplingMode::from_u8(val.round() as u8);
                        self.shared.set_oversampling_mode(mode);
                    }
                    PARAM_OVERSAMPLING_METHOD => {
                        let phase = resampler::OversamplingPhase::from_u8(val.round() as u8);
                        self.shared.set_oversampling_phase(phase);
                    }
                    _ => {}
                }
            }

            // Synchronize changed parameters to shared bridge
            let mut num_chgs = 0;
            for (idx, param) in self.shared.params.iter().enumerate() {
                #[cfg(feature = "feat_oversampling")]
                match param.id {
                    PARAM_OVERSAMPLING | PARAM_OVERSAMPLING_METHOD => continue,
                    _ => {}
                }
                if let Some(val) = self.shared.param_values.get(param.id) {
                    let range = param.max_value - param.min_value;
                    let norm = if range > 0.0 {
                        ((val - param.min_value) / range).clamp(0.0, 1.0) as f32
                    } else {
                        val.clamp(0.0, 1.0) as f32
                    };

                    let changed = match self.last_sent_norm.get(idx).copied() {
                        Some(last) if last.is_finite() => (last - norm).abs() > 1e-5,
                        _ => true,
                    };

                    if changed && num_chgs < protocol::MAX_PARAM_QUEUE {
                        // SAFETY: `bridge_ptr` is non-null and `num_chgs` is bounded by `MAX_PARAM_QUEUE`.
                        unsafe {
                            (*bridge_ptr).param_queue[num_chgs] = protocol::ParamChange {
                                id: param.id,
                                value: norm,
                            };
                        }
                        num_chgs += 1;
                        if let Some(slot) = self.last_sent_norm.get_mut(idx) {
                            *slot = norm;
                        }
                    }
                }
            }
            // SAFETY: `bridge_ptr` points to valid shared memory allocated via mmap.
            unsafe {
                (*bridge_ptr).num_param_changes = num_chgs as u32;
            }
        }

        // Determine whether plugin is currently bypassed via prm 0
        let is_bypassed = self.shared.param_values.get(0).is_some_and(|val| {
            self.shared
                .params
                .iter()
                .find(|p| p.id == 0)
                .is_some_and(|param| {
                    let name_lower = param.name.to_lowercase();
                    match () {
                        () if name_lower.contains("bypass") => val >= 50.0 || val >= 0.5,
                        () if name_lower.contains("on/off") || name_lower.contains("power") => {
                            val < 50.0
                        }
                        () => false,
                    }
                })
        });

        // 2. Audio processing through Zoom ZFX 32-bit DSP companion
        for mut port_pair in &mut audio {
            if let Ok(Some(channel_pairs)) = port_pair
                .channels()
                .map(clack_plugin::prelude::SampleType::into_f32)
            {
                let mut pairs: [Option<clack_plugin::process::audio::ChannelPair<'_, f32>>; 2] =
                    [None, None];
                let mut num_pairs = 0;
                for (slot, pair) in pairs.iter_mut().zip(channel_pairs) {
                    *slot = Some(pair);
                    num_pairs += 1;
                }
                let mut frames = 0;

                for (ch, pair) in pairs[..num_pairs].iter().enumerate() {
                    if let Some(pair) = pair {
                        match pair {
                            clack_plugin::process::audio::ChannelPair::InputOnly(in_buf)
                            | clack_plugin::process::audio::ChannelPair::InputOutput(in_buf, _) => {
                                frames = in_buf.len().min(self.in_l.len());
                                match ch {
                                    0 => {
                                        self.in_l[..frames].copy_from_slice(&in_buf[..frames]);
                                    }
                                    1 => {
                                        frames = frames.min(self.in_r.len());
                                        self.in_r[..frames].copy_from_slice(&in_buf[..frames]);
                                    }
                                    _ => {}
                                }
                            }
                            clack_plugin::process::audio::ChannelPair::InPlace(buf) => {
                                frames = buf.len().min(self.in_l.len());
                                match ch {
                                    0 => {
                                        self.in_l[..frames].copy_from_slice(&buf[..frames]);
                                    }
                                    1 => {
                                        frames = frames.min(self.in_r.len());
                                        self.in_r[..frames].copy_from_slice(&buf[..frames]);
                                    }
                                    _ => {}
                                }
                            }
                            clack_plugin::process::audio::ChannelPair::OutputOnly(buf) => {
                                frames = buf.len().min(self.out_l.len());
                            }
                        }
                    }
                }
                frames = frames.min(self.out_l.len()).min(self.out_r.len());

                if is_bypassed {
                    // True Software Passthrough: Bit-exact dry audio when bypassed
                    for (ch, pair) in pairs[..num_pairs].iter_mut().enumerate() {
                        if let Some(pair) = pair.take() {
                            match pair {
                                clack_plugin::process::audio::ChannelPair::InputOnly(_)
                                | clack_plugin::process::audio::ChannelPair::InPlace(_) => {}
                                clack_plugin::process::audio::ChannelPair::OutputOnly(out_buf) => {
                                    let in_src = if ch == 0 { &self.in_l } else { &self.in_r };
                                    let len = out_buf.len().min(in_src.len()).min(frames);
                                    out_buf[..len].copy_from_slice(&in_src[..len]);
                                }
                                clack_plugin::process::audio::ChannelPair::InputOutput(
                                    in_buf,
                                    out_buf,
                                ) => {
                                    let len = out_buf.len().min(in_buf.len()).min(frames);
                                    out_buf[..len].copy_from_slice(&in_buf[..len]);
                                }
                            }
                        }
                    }
                    continue;
                }

                if frames > 0 {
                    #[cfg(feature = "feat_oversampling")]
                    let (dsp_frames, use_oversampling) = {
                        let cur_mode = self.shared.oversampling_mode();
                        let cur_phase = self.shared.oversampling_phase();
                        self.oversampler.set_config(cur_mode, cur_phase);

                        if cur_mode == resampler::OversamplingMode::Off {
                            (frames, false)
                        } else {
                            let factor = cur_mode.factor();
                            let of = (frames * factor)
                                .min(self.os_in_l.len())
                                .min(self.os_in_r.len());
                            self.oversampler.upsample(
                                &self.in_l[..frames],
                                &self.in_r[..frames],
                                &mut self.os_in_l[..of],
                                &mut self.os_in_r[..of],
                            );
                            (of, true)
                        }
                    };
                    #[cfg(not(feature = "feat_oversampling"))]
                    let (dsp_frames, _use_oversampling) = (frames, false);

                    let clamped_dsp_frames = dsp_frames.min(protocol::MAX_FRAMES);
                    #[cfg(feature = "feat_oversampling")]
                    let (feed_l, feed_r) = if use_oversampling {
                        (
                            &self.os_in_l[..clamped_dsp_frames],
                            &self.os_in_r[..clamped_dsp_frames],
                        )
                    } else {
                        (
                            &self.in_l[..clamped_dsp_frames],
                            &self.in_r[..clamped_dsp_frames],
                        )
                    };
                    #[cfg(not(feature = "feat_oversampling"))]
                    let (feed_l, feed_r) = (
                        &self.in_l[..clamped_dsp_frames],
                        &self.in_r[..clamped_dsp_frames],
                    );

                    // SAFETY: `bridge_ptr` points to valid shared memory allocated via mmap and `clamped_dsp_frames` <= `MAX_FRAMES`.
                    unsafe {
                        let bridge = &mut *bridge_ptr;
                        #[cfg(feature = "feat_oversampling")]
                        let worker_sr = {
                            let cur_mode = self.shared.oversampling_mode();
                            (self.host_sample_rate as f32) * (cur_mode.factor() as f32)
                        };
                        #[cfg(not(feature = "feat_oversampling"))]
                        let worker_sr = self.host_sample_rate as f32;
                        bridge.sample_rate = worker_sr;
                        bridge.block_size = clamped_dsp_frames as u32;
                        bridge.num_frames = clamped_dsp_frames as u32;
                        bridge.in_l[..clamped_dsp_frames].copy_from_slice(feed_l);
                        if num_pairs >= 2 {
                            bridge.in_r[..clamped_dsp_frames].copy_from_slice(feed_r);
                        } else {
                            bridge.in_r[..clamped_dsp_frames].copy_from_slice(feed_l);
                        }

                        // Send CMD_PROCESS across bridge
                        bridge
                            .command
                            .store(protocol::CMD_PROCESS, Ordering::Release);
                        self.seq = self.seq.wrapping_add(1);
                        if self.seq == 0 {
                            self.seq = 1;
                        }
                        bridge.host_seq.store(self.seq, Ordering::Release);
                        protocol::futex_wake(&raw const bridge.host_seq);

                        // Low-latency hybrid spin + futex wait
                        let mut timed_out = false;
                        let mut spins = 0;
                        loop {
                            let wseq = bridge.worker_seq.load(Ordering::Acquire);
                            if wseq == self.seq {
                                break;
                            }
                            spins += 1;
                            if spins > 1000 {
                                protocol::futex_wait(
                                    &raw const bridge.worker_seq,
                                    wseq,
                                    Some(5_000),
                                );
                                if spins > 1050 {
                                    timed_out = true;
                                    break;
                                }
                            } else {
                                std::hint::spin_loop();
                            }
                        }

                        if timed_out {
                            self.out_l[..frames].fill(0.0);
                            self.out_r[..frames].fill(0.0);
                        } else {
                            #[cfg(feature = "feat_oversampling")]
                            if use_oversampling {
                                let os_frames = clamped_dsp_frames
                                    .min(self.os_out_l.len())
                                    .min(self.os_out_r.len());

                                for (dst, &src) in self.os_out_l[..os_frames]
                                    .iter_mut()
                                    .zip(&bridge.out_l[..os_frames])
                                {
                                    *dst = if src.is_finite() {
                                        src.clamp(-4.0, 4.0)
                                    } else {
                                        0.0
                                    };
                                }

                                for (dst, &src) in self.os_out_r[..os_frames]
                                    .iter_mut()
                                    .zip(&bridge.out_r[..os_frames])
                                {
                                    *dst = if src.is_finite() {
                                        src.clamp(-4.0, 4.0)
                                    } else {
                                        0.0
                                    };
                                }

                                // Downsample oversampled DSP audio back to host rate
                                self.oversampler.downsample(
                                    &self.os_out_l[..os_frames],
                                    &self.os_out_r[..os_frames],
                                    &mut self.out_l[..frames],
                                    &mut self.out_r[..frames],
                                );
                            }

                            #[cfg(feature = "feat_oversampling")]
                            let needs_direct_copy = !use_oversampling;
                            #[cfg(not(feature = "feat_oversampling"))]
                            let needs_direct_copy = true;

                            if needs_direct_copy {
                                let copy_frames = clamped_dsp_frames
                                    .min(self.out_l.len())
                                    .min(self.out_r.len());

                                for (dst, &src) in self.out_l[..copy_frames]
                                    .iter_mut()
                                    .zip(&bridge.out_l[..copy_frames])
                                {
                                    *dst = if src.is_finite() {
                                        src.clamp(-4.0, 4.0)
                                    } else {
                                        0.0
                                    };
                                }

                                for (dst, &src) in self.out_r[..copy_frames]
                                    .iter_mut()
                                    .zip(&bridge.out_r[..copy_frames])
                                {
                                    *dst = if src.is_finite() {
                                        src.clamp(-4.0, 4.0)
                                    } else {
                                        0.0
                                    };
                                }
                            }
                        }
                    }
                }

                for (ch, pair) in pairs[..num_pairs].iter_mut().enumerate() {
                    if let Some(pair) = pair.take() {
                        match pair {
                            clack_plugin::process::audio::ChannelPair::InputOnly(_) => {}
                            clack_plugin::process::audio::ChannelPair::OutputOnly(out_buf)
                            | clack_plugin::process::audio::ChannelPair::InputOutput(_, out_buf)
                            | clack_plugin::process::audio::ChannelPair::InPlace(out_buf) => {
                                let len = out_buf.len().min(frames);
                                if ch == 0 {
                                    out_buf[..len].copy_from_slice(&self.out_l[..len]);
                                } else {
                                    out_buf[..len].copy_from_slice(&self.out_r[..len]);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 3. Drain pending GUI automation events into host output
        if let Ok(mut gui_evs) = self.shared.gui_events.try_lock() {
            for ev in gui_evs.drain(..) {
                match ev {
                    GuiParamEvent::BeginGesture(id) => {
                        let clap_ev =
                            clack_common::events::event_types::ParamGestureBeginEvent::new(
                                0,
                                ClapId::new(id),
                            );
                        events.output.try_push(clap_ev.as_unknown()).ok();
                    }
                    GuiParamEvent::SetValue(id, val) => {
                        let clap_ev = clack_common::events::event_types::ParamValueEvent::new(
                            0,
                            ClapId::new(id),
                            clack_common::events::Pckn::match_all(),
                            val,
                        );
                        events.output.try_push(clap_ev.as_unknown()).ok();
                    }
                    GuiParamEvent::EndGesture(id) => {
                        let clap_ev = clack_common::events::event_types::ParamGestureEndEvent::new(
                            0,
                            ClapId::new(id),
                        );
                        events.output.try_push(clap_ev.as_unknown()).ok();
                    }
                }
            }
        }

        Ok(ProcessStatus::Continue)
    }
}

impl Drop for ZfxAudioProcessor<'_> {
    fn drop(&mut self) {
        self.shared.is_active.store(false, Ordering::Release);
    }
}

clack_export_entry!(SinglePluginEntry<ZfxBootstrapPlugin>);

impl clack_plugin::entry::DefaultPluginFactory for ZfxBootstrapPlugin {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new(PLUGIN_ID, PLUGIN_NAME)
            .with_vendor("ZFX Porting Project")
            .with_features(PLUGIN_FEATURES.iter().copied())
    }

    #[allow(clippy::too_many_lines)]
    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        if EMBEDDED_WORKER_PAYLOAD.is_empty() {
            return Err(PluginError::Message("Embedded worker payload is empty"));
        }
        if EMBEDDED_DLL_PAYLOAD.is_empty() {
            return Err(PluginError::Message("Embedded DLL payload is empty"));
        }

        // 1. Create shared memory for the bridge
        let bridge_size = std::mem::size_of::<protocol::ZfxSharedBridge>();
        #[cfg(target_os = "linux")]
        let (bridge_addr, bridge_fd) = {
            // SAFETY: Creating an anonymous memory file descriptor for the IPC bridge.
            let fd = unsafe { libc::memfd_create(c"zfx-bridge".as_ptr(), 0) };
            if fd < 0 {
                return Err(PluginError::Message("Failed to create bridge memfd"));
            }
            let off_size = libc::off_t::try_from(bridge_size)
                .map_err(|_| PluginError::Message("Bridge size overflow for off_t"))?;
            // SAFETY: Truncating the bridge fd to the exact struct size.
            if unsafe { libc::ftruncate(fd, off_size) } != 0 {
                // SAFETY: Closing fd on ftruncate error.
                unsafe {
                    libc::close(fd);
                }
                return Err(PluginError::Message("Failed to ftruncate bridge memfd"));
            }
            // SAFETY: Mapping shared memory region for IPC communication with 32-bit worker.
            let addr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    bridge_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if addr == libc::MAP_FAILED {
                // SAFETY: Closing fd on mmap failure.
                unsafe {
                    libc::close(fd);
                }
                return Err(PluginError::Message("Failed to mmap bridge"));
            }
            (addr, fd)
        };

        #[cfg(target_os = "windows")]
        let (bridge_addr, h_map, map_name) = {
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::System::Memory::{
                CreateFileMappingA, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
            };
            static INSTANCE_COUNTER: std::sync::atomic::AtomicU32 =
                std::sync::atomic::AtomicU32::new(1);
            let pid = std::process::id();
            let inst_id = INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!("Local\\ZfxBridge_{pid}_{inst_id}\0");
            // SAFETY: Creating a named shared memory mapping backed by the system paging file.
            let h = unsafe {
                CreateFileMappingA(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    0,
                    bridge_size as u32,
                    name.as_ptr(),
                )
            };
            if h.is_null() {
                return Err(PluginError::Message(
                    "Failed to create Win32 bridge mapping",
                ));
            }
            // SAFETY: Mapping view of file for IPC shared memory.
            let addr = unsafe { MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, bridge_size) };
            if addr.Value.is_null() {
                // SAFETY: Closing mapping handle on view mapping failure.
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(h);
                }
                return Err(PluginError::Message("Failed to MapViewOfFile for bridge"));
            }
            (addr.Value, h, name)
        };

        // SAFETY: bridge_addr is non-null, aligned, and sized to ZfxSharedBridge.
        let bridge = unsafe { &mut *bridge_addr.cast::<protocol::ZfxSharedBridge>() };
        bridge.init_header();

        // 2. Copy embedded DLL payload into bridge
        let dll_len = EMBEDDED_DLL_PAYLOAD.len();
        if dll_len > protocol::MAX_PAYLOAD_SIZE {
            #[cfg(target_os = "linux")]
            // SAFETY: Cleaning up bridge resources when payload exceeds max buffer size.
            unsafe {
                libc::munmap(bridge_addr, bridge_size);
                libc::close(bridge_fd);
            }
            #[cfg(target_os = "windows")]
            // SAFETY: Cleaning up Win32 mapping resources on error.
            unsafe {
                windows_sys::Win32::System::Memory::UnmapViewOfFile(
                    windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: bridge_addr,
                    },
                );
                windows_sys::Win32::Foundation::CloseHandle(h_map);
            }
            return Err(PluginError::Message("DLL payload exceeds MAX_PAYLOAD_SIZE"));
        }
        bridge.payload_size = u32::try_from(dll_len).unwrap_or(0);
        // SAFETY: Copying embedded DLL payload into the payload buffer bounded by MAX_PAYLOAD_SIZE.
        unsafe {
            std::ptr::copy_nonoverlapping(
                EMBEDDED_DLL_PAYLOAD.as_ptr(),
                bridge.payload_bytes.as_mut_ptr(),
                dll_len,
            );
        }

        // 3. Spawn child worker process
        #[cfg(target_os = "linux")]
        let pid = {
            // SAFETY: Creating an anonymous memfd for the worker binary with MFD_CLOEXEC.
            let worker_fd =
                unsafe { libc::memfd_create(c"zfx-worker32".as_ptr(), libc::MFD_CLOEXEC) };
            if worker_fd < 0 {
                // SAFETY: Cleaning up bridge resources on memfd failure.
                unsafe {
                    libc::munmap(bridge_addr, bridge_size);
                    libc::close(bridge_fd);
                }
                return Err(PluginError::Message("Failed to create worker memfd"));
            }
            let mut written = 0;
            while written < EMBEDDED_WORKER_PAYLOAD.len() {
                // SAFETY: Writing worker executable payload bytes into worker_fd.
                let n = unsafe {
                    libc::write(
                        worker_fd,
                        EMBEDDED_WORKER_PAYLOAD[written..]
                            .as_ptr()
                            .cast::<libc::c_void>(),
                        EMBEDDED_WORKER_PAYLOAD.len() - written,
                    )
                };
                if n <= 0 {
                    // SAFETY: Cleaning up worker_fd and bridge resources on write error.
                    unsafe {
                        libc::close(worker_fd);
                        libc::munmap(bridge_addr, bridge_size);
                        libc::close(bridge_fd);
                    }
                    return Err(PluginError::Message(
                        "Failed to write worker binary to memfd",
                    ));
                }
                written += usize::try_from(n).unwrap_or(0);
            }

            // SAFETY: Forking a child worker process to execute the 32-bit loader.
            let child_pid = unsafe { libc::fork() };
            if child_pid < 0 {
                // SAFETY: Cleaning up resources on fork failure.
                unsafe {
                    libc::close(worker_fd);
                    libc::munmap(bridge_addr, bridge_size);
                    libc::close(bridge_fd);
                }
                return Err(PluginError::Message("Failed to fork worker process"));
            }
            if child_pid == 0 {
                // SAFETY: Setting PDEATHSIG, duplicating bridge_fd to fd 3, and executing worker from memfd.
                unsafe {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    libc::dup2(bridge_fd, 3);
                    let exe_path = format!("/proc/self/fd/{worker_fd}\0");
                    let worker_cmd = c"zfx-worker32";
                    let bridge_arg = c"3";
                    let exec_args = [worker_cmd.as_ptr(), bridge_arg.as_ptr(), std::ptr::null()];
                    libc::execv(exe_path.as_ptr().cast::<libc::c_char>(), exec_args.as_ptr());
                    libc::_exit(127);
                }
            }
            // SAFETY: Closing worker_fd in parent process after child has been spawned.
            unsafe {
                libc::close(worker_fd);
            }
            child_pid
        };

        #[cfg(target_os = "windows")]
        let (worker_handle, job_handle) = {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            use windows_sys::Win32::System::Threading::{
                CreateProcessA, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOA,
            };

            let temp_dir = std::env::temp_dir().join("ZoomZFX");
            let _ = std::fs::create_dir_all(&temp_dir);
            let worker_path = temp_dir.join("zfx-worker32.exe");
            let needs_write = std::fs::metadata(&worker_path).map_or(true, |meta| {
                meta.len() != EMBEDDED_WORKER_PAYLOAD.len() as u64
            });
            if needs_write && std::fs::write(&worker_path, EMBEDDED_WORKER_PAYLOAD).is_err() {
                // SAFETY: Unmapping shared memory view and closing file mapping handle on payload write failure.
                unsafe {
                    windows_sys::Win32::System::Memory::UnmapViewOfFile(
                        windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                            Value: bridge_addr,
                        },
                    );
                    CloseHandle(h_map);
                }
                return Err(PluginError::Message(
                    "Failed to write worker binary to temp dir",
                ));
            }

            // SAFETY: Creating Win32 job object to supervise child worker process.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if !job.is_null() {
                // SAFETY: Setting JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE on job object to automatically terminate worker if DAW crashes.
                unsafe {
                    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                    SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        std::ptr::from_ref(&info).cast(),
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    );
                }
            }

            // SAFETY: Zeroing STARTUPINFOA POD struct.
            let mut si: STARTUPINFOA = unsafe { std::mem::zeroed() };
            si.cb = std::mem::size_of::<STARTUPINFOA>() as u32;
            si.dwFlags = STARTF_USESHOWWINDOW;
            si.wShowWindow = 0; // SW_HIDE
                                // SAFETY: Zeroing PROCESS_INFORMATION POD struct.
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let cmdline = format!(
                "\"{}\" {}\0",
                worker_path.display(),
                map_name.trim_end_matches('\0')
            );
            let mut cmdline_bytes = cmdline.into_bytes();
            // SAFETY: Spawning 32-bit worker process using CreateProcessA with hidden window.
            let success = unsafe {
                CreateProcessA(
                    std::ptr::null(),
                    cmdline_bytes.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    &raw const si,
                    &raw mut pi,
                )
            };
            if success == 0 {
                // SAFETY: Cleaning up job object and bridge shared memory handles on worker process spawn failure.
                unsafe {
                    if !job.is_null() {
                        CloseHandle(job);
                    }
                    windows_sys::Win32::System::Memory::UnmapViewOfFile(
                        windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                            Value: bridge_addr,
                        },
                    );
                    CloseHandle(h_map);
                }
                return Err(PluginError::Message("Failed to spawn zfx-worker32.exe"));
            }
            if !job.is_null() {
                // SAFETY: Assigning child worker process to the supervising job object.
                unsafe {
                    AssignProcessToJobObject(job, pi.hProcess);
                }
            }
            // SAFETY: Closing child worker primary thread handle as we do not manage thread scheduling directly.
            unsafe {
                CloseHandle(pi.hThread);
            }
            (pi.hProcess, job)
        };

        // 4. Handshake: wait for worker to signal STATUS_READY
        let mut ready = false;
        for _ in 0..300 {
            let st = bridge.status.load(Ordering::Acquire);
            if st == protocol::STATUS_READY {
                ready = true;
                break;
            }
            // SAFETY: Waiting on status futex in shared memory.
            unsafe {
                protocol::futex_wait(&raw const bridge.status, st, Some(10_000));
            }
        }
        if !ready {
            #[cfg(target_os = "linux")]
            // SAFETY: Cleaning up worker process and bridge resources on ready timeout.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                let mut st = 0;
                libc::waitpid(pid, &raw mut st, libc::WNOHANG);
                libc::munmap(bridge_addr, bridge_size);
                libc::close(bridge_fd);
            }
            #[cfg(target_os = "windows")]
            // SAFETY: Cleaning up Win32 worker process and bridge handles on ready timeout.
            unsafe {
                use windows_sys::Win32::Foundation::CloseHandle;
                use windows_sys::Win32::System::Memory::{
                    UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS,
                };
                use windows_sys::Win32::System::Threading::TerminateProcess;
                if !worker_handle.is_null() {
                    TerminateProcess(worker_handle, 0);
                    CloseHandle(worker_handle);
                }
                if !job_handle.is_null() {
                    CloseHandle(job_handle);
                }
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: bridge_addr });
                CloseHandle(h_map);
            }
            return Err(PluginError::Message(
                "Timeout waiting for zfx-worker32 ready status",
            ));
        }

        // 6. Send CMD_INIT
        bridge.sample_rate = 44100.0;
        bridge.block_size = 256;
        bridge.command.store(protocol::CMD_INIT, Ordering::Release);
        bridge.host_seq.store(1, Ordering::Release);
        // SAFETY: Waking worker on host sequence update.
        unsafe {
            protocol::futex_wake(&raw const bridge.host_seq);
        }

        let mut init_ok = false;
        for _ in 0..500 {
            let wseq = bridge.worker_seq.load(Ordering::Acquire);
            if wseq == 1 {
                if bridge.status.load(Ordering::Acquire) == protocol::STATUS_OK {
                    init_ok = true;
                }
                break;
            }
            // SAFETY: Waiting on worker sequence futex.
            unsafe {
                protocol::futex_wait(&raw const bridge.worker_seq, wseq, Some(10_000));
            }
        }
        if !init_ok {
            #[cfg(target_os = "linux")]
            // SAFETY: Terminating failed worker and cleaning up shared memory.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                let mut st = 0;
                libc::waitpid(pid, &raw mut st, libc::WNOHANG);
                libc::munmap(bridge_addr, bridge_size);
                libc::close(bridge_fd);
            }
            #[cfg(target_os = "windows")]
            // SAFETY: Terminating child worker process and closing mapping handles on CMD_INIT timeout/failure.
            unsafe {
                use windows_sys::Win32::Foundation::CloseHandle;
                use windows_sys::Win32::System::Memory::{
                    UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS,
                };
                use windows_sys::Win32::System::Threading::TerminateProcess;
                if !worker_handle.is_null() {
                    TerminateProcess(worker_handle, 0);
                    CloseHandle(worker_handle);
                }
                if !job_handle.is_null() {
                    CloseHandle(job_handle);
                }
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: bridge_addr });
                CloseHandle(h_map);
            }
            return Err(PluginError::Message("zfx-worker32 failed CMD_INIT"));
        }

        // 7. Introspect plugin layout and parameters
        let layout = if XML_LAYOUT.trim().is_empty() {
            None
        } else {
            quick_xml::de::from_str::<EfxTypeInfo>(XML_LAYOUT).ok()
        };

        let mut params = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        let max_bridge_params = usize::try_from(bridge.num_params)
            .unwrap_or(0)
            .min(protocol::MAX_PARAMS);
        let get_bridge_param = |id: u32| -> (String, f64) {
            bridge.params[..max_bridge_params]
                .iter()
                .find(|p| p.id == id)
                .map_or_else(
                    || (String::new(), 0.0),
                    |p| {
                        let name_end = p.name.iter().position(|&c| c == 0).unwrap_or(p.name.len());
                        let name_str = String::from_utf8_lossy(&p.name[..name_end])
                            .trim()
                            .to_string();
                        (name_str, f64::from(p.default_val))
                    },
                )
        };

        if let Some(l) = &layout {
            if let Some(ctrls) = &l.ctrls {
                for ctrl in &ctrls.ctrl {
                    let id = ctrl.prm;
                    if id > 0 && seen_ids.insert(id) {
                        let (dll_name, init_norm) = get_bridge_param(id);
                        let name = match (dll_name.is_empty(), &ctrl.name) {
                            (false, _) => format!("{dll_name}\0"),
                            (true, Some(n)) if !n.trim().is_empty() => {
                                format!("{} (Prm {})\0", n.trim(), id)
                            }
                            _ => format!("Param {id}\0"),
                        };
                        let max_raw = f64::from(ctrl.max.unwrap_or(100));
                        let max_val = if max_raw > 0.0 { max_raw } else { 100.0 };
                        let default_val = (init_norm * max_val).clamp(0.0, max_val);
                        params.push(PluginParamDef {
                            id,
                            name,
                            min_value: 0.0,
                            max_value: max_val,
                            default_value: default_val,
                        });
                    }
                }
            }
        }

        for p in &bridge.params[..max_bridge_params] {
            let id = p.id;
            if seen_ids.insert(id) {
                let name_end = p.name.iter().position(|&c| c == 0).unwrap_or(p.name.len());
                let dll_name = String::from_utf8_lossy(&p.name[..name_end])
                    .trim()
                    .to_string();
                let name = match (dll_name.is_empty(), id) {
                    (false, _) => format!("{dll_name}\0"),
                    (true, 0) => "Bypass\0".to_string(),
                    _ => format!("Param {id}\0"),
                };
                let init_norm = f64::from(p.default_val);
                let default_val = (init_norm * 100.0).clamp(0.0, 100.0);
                params.push(PluginParamDef {
                    id,
                    name,
                    min_value: 0.0,
                    max_value: 100.0,
                    default_value: default_val,
                });
            }
        }

        if params.is_empty() {
            for id in 1..=4 {
                params.push(PluginParamDef {
                    id,
                    name: format!("Param {id}\0"),
                    min_value: 0.0,
                    max_value: 100.0,
                    default_value: 50.0,
                });
            }
        }

        #[cfg(feature = "feat_oversampling")]
        {
            params.push(PluginParamDef {
                id: PARAM_OVERSAMPLING,
                name: "Oversampling\0".to_string(),
                min_value: 0.0,
                max_value: 2.0,
                default_value: 0.0,
            });
            params.push(PluginParamDef {
                id: PARAM_OVERSAMPLING_METHOD,
                name: "Oversampling Method\0".to_string(),
                min_value: 0.0,
                max_value: 1.0,
                default_value: 1.0,
            });
        }

        Ok(ZfxShared {
            bridge_ptr: bridge_addr.cast::<protocol::ZfxSharedBridge>(),
            #[cfg(target_os = "linux")]
            bridge_fd,
            #[cfg(target_os = "linux")]
            worker_pid: pid,
            #[cfg(target_os = "windows")]
            h_map,
            #[cfg(target_os = "windows")]
            worker_handle,
            #[cfg(target_os = "windows")]
            job_handle,
            layout,
            param_values: params::ParamStore::new(params.iter().map(|p| (p.id, p.default_value))),
            params,
            gui_events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            is_active: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "feat_oversampling")]
            oversampling_mode: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                resampler::OversamplingMode::Off.to_u8(),
            )),
            #[cfg(feature = "feat_oversampling")]
            oversampling_phase: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                resampler::OversamplingPhase::Minimum.to_u8(),
            )),
        })
    }

    fn new_main_thread<'a>(
        host: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(ZfxMainThread {
            shared,
            host,
            gui_window: std::cell::RefCell::new(None),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[test]
    fn test_efxtype_parsing() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let assets_dir = manifest_dir.join("assets");
        if !assets_dir.exists() {
            return;
        }

        let mut count = 0;
        for entry in fs::read_dir(assets_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                let xml_path = path.join("EfxType.xml");
                if xml_path.exists() {
                    let xml_content = fs::read_to_string(&xml_path).unwrap();
                    let info: Result<EfxTypeInfo, _> = quick_xml::de::from_str(&xml_content);
                    assert!(info.is_ok(), "Failed to parse XML in {xml_path:?}");
                    count += 1;
                }
            }
        }
        println!("Successfully parsed {count} layout XML files!");
        assert!(count >= 80);
    }

    #[test]
    fn test_bkgd_img_parsing() {
        let xml = r#"<EfxTypeInfo><Total><Width>800</Width><Height>600</Height><BkgdImg><Img index="0">Bkgd.jpg</Img></BkgdImg></Total></EfxTypeInfo>"#;
        let result: EfxTypeInfo = quick_xml::de::from_str(xml).unwrap();
        let bkgd_val = result.total.bkgd_img.and_then(|b| b.img).map(|i| i.value);
        assert!(bkgd_val.is_some(), "BkgdImg/Img value should be Some!");
        assert_eq!(bkgd_val.unwrap(), "Bkgd.jpg");
    }

    #[test]
    fn test_load_texture_with_egui() {
        let Some(bytes) = gui::find_asset("bkgd").or_else(|| gui::find_asset("Bkgd")) else {
            eprintln!("Skipping test_load_texture_with_egui: assets not extracted (run tools/extract_zfx_assets.py)");
            return;
        };
        let ctx = egui::Context::default();
        let tex = gui::load_texture(&ctx, "bkgd", bytes);
        assert!(tex.is_some(), "load_texture returned None!");
    }

    #[test]
    fn test_common_assets_lookup() {
        let Some(_) = gui::find_frame_asset("Common:Knob", 0) else {
            eprintln!("Skipping test_common_assets_lookup: Common assets not extracted (run tools/extract_zfx_assets.py)");
            return;
        };
        assert!(
            gui::find_frame_asset("Common:FootSw", 0).is_some(),
            "FootSw frame 0 missing!"
        );
        assert_eq!(gui::probe_frame_count("Common:Knob", None), 49);
        assert_eq!(gui::probe_frame_count("Common:FootSw", None), 2);
        // Ensure declared_max does not short-circuit when numbered frames exist on disk
        assert_eq!(gui::probe_frame_count("Common:Knob", Some(1)), 49);
        assert_eq!(gui::probe_frame_count("Common:FootSw", Some(1)), 2);
    }

    #[test]
    fn test_channel_toggle_logic() {
        let min_val: f64 = 0.0;
        let max_val: f64 = 1.0;

        // Channel toggle function matching PushKey/Toggle implementation
        let compute_toggle = |cur_val: f64, target_idx: u32| -> f64 {
            let is_on = (cur_val.round() as u32) == target_idx;
            if (max_val - min_val - 1.0).abs() < 1e-4 {
                if is_on {
                    if target_idx == 0 { max_val } else { min_val }
                } else {
                    f64::from(target_idx)
                }
            } else {
                f64::from(target_idx)
            }
        };

        // When on Lead (1.0):
        // Clicking Rhythm (0) switches to Rhythm (0.0)
        assert_eq!(compute_toggle(1.0, 0), 0.0);
        // Clicking Lead (1) toggles to Rhythm (0.0)
        assert_eq!(compute_toggle(1.0, 1), 0.0);

        // When on Rhythm (0.0):
        // Clicking Lead (1) switches to Lead (1.0)
        assert_eq!(compute_toggle(0.0, 1), 1.0);
        // Clicking Rhythm (0) toggles to Lead (1.0)
        assert_eq!(compute_toggle(0.0, 0), 1.0);

        // Frame selection verification:
        let total_frames = 2;
        let compute_frame = |cur_val: f64, target_idx: u32| -> usize {
            let is_on = (cur_val.round() as u32) == target_idx;
            match total_frames {
                2 => usize::from(is_on),
                _ => 0,
            }
        };

        // On Rhythm (0.0): Rhythm LED is lit (frame 1), Lead LED is unlit (frame 0)
        assert_eq!(compute_frame(0.0, 0), 1, "Rhythm LED should glow on Rhythm channel");
        assert_eq!(compute_frame(0.0, 1), 0, "Lead LED should be dark on Rhythm channel");

        // On Lead (1.0): Rhythm LED is unlit (frame 0), Lead LED is lit (frame 1)
        assert_eq!(compute_frame(1.0, 0), 0, "Rhythm LED should be dark on Lead channel");
        assert_eq!(compute_frame(1.0, 1), 1, "Lead LED should glow on Lead channel");
    }

    #[test]
    fn test_ctrl_center_deserialization() {
        let xml = r#"<Ctrl index="0" type="dsp" prm="1"><Type>Knob</Type><Img>Knob</Img><PosX>312</PosX><PosY>124</PosY><Width>54</Width><Height>53</Height><CenterX>24</CenterX><CenterY>23</CenterY></Ctrl>"#;
        let ctrl: Ctrl = quick_xml::de::from_str(xml).unwrap();
        assert_eq!(ctrl.center_x, Some(24.0));
        assert_eq!(ctrl.center_y, Some(23.0));
    }

    #[test]
    fn test_zerocopy_state_roundtrip() {
        use zerocopy::IntoBytes;
        let header = ZfxStateHeader {
            magic: *b"ZFXS",
            version: le::U32::new(2),
            count: le::U32::new(3),
        };
        let v2_ext = ZfxStateV2Ext {
            os_mode: 2,
            os_phase: 1,
            pad: [0, 0],
        };
        let entries = [
            ZfxStateParamEntry {
                id: le::U32::new(0),
                val: le::F64::new(1.0),
            },
            ZfxStateParamEntry {
                id: le::U32::new(9900),
                val: le::F64::new(2.0),
            },
            ZfxStateParamEntry {
                id: le::U32::new(9901),
                val: le::F64::new(1.0),
            },
        ];

        let mut data = Vec::new();
        data.extend_from_slice(header.as_bytes());
        data.extend_from_slice(v2_ext.as_bytes());
        data.extend_from_slice(entries.as_bytes());

        assert_eq!(data.len(), 12 + 4 + 3 * 12);

        // Verify deserialization matches
        let (parsed_hdr, rest) =
            <ZfxStateHeader as zerocopy::FromBytes>::read_from_prefix(&data).unwrap();
        assert_eq!(&parsed_hdr.magic, b"ZFXS");
        assert_eq!(parsed_hdr.version.get(), 2);
        assert_eq!(parsed_hdr.count.get(), 3);

        let (parsed_ext, rest) =
            <ZfxStateV2Ext as zerocopy::FromBytes>::read_from_prefix(rest).unwrap();
        assert_eq!(parsed_ext.os_mode, 2);
        assert_eq!(parsed_ext.os_phase, 1);

        let parsed_entries = <[ZfxStateParamEntry] as zerocopy::FromBytes>::ref_from_bytes(rest).unwrap();
        assert_eq!(parsed_entries.len(), 3);
        assert_eq!(parsed_entries[0].id.get(), 0);
        assert_eq!(parsed_entries[0].val.get(), 1.0);
        assert_eq!(parsed_entries[1].id.get(), 9900);
        assert_eq!(parsed_entries[1].val.get(), 2.0);
        assert_eq!(parsed_entries[2].id.get(), 9901);
        assert_eq!(parsed_entries[2].val.get(), 1.0);
    }
}


