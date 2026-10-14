#![allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity
)]

pub mod gui;
pub mod protocol;

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

pub fn scan_log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/zfx_clap_scan.log")
    {
        use std::io::Write;
        let _ = writeln!(f, "[pid {}] {}", std::process::id(), msg);
    }
}

#[derive(Copy, Clone, Debug)]
pub enum GuiParamEvent {
    BeginGesture(u32),
    SetValue(u32, f64),
    EndGesture(u32),
}

pub struct ZfxBootstrapPlugin;

impl Plugin for ZfxBootstrapPlugin {
    type AudioProcessor<'a> = ZfxAudioProcessor<'a>;
    type Shared<'a> = ZfxShared;
    type MainThread<'a> = ZfxMainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, shared: Option<&Self::Shared<'_>>) {
        scan_log(&format!("declare_extensions (shared={})", shared.is_some()));
        builder.register::<clack_extensions::params::PluginParams>();
        builder.register::<clack_extensions::gui::PluginGui>();
        builder.register::<PluginAudioPorts>();
        builder.register::<PluginState>();
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
    pub bridge_fd: libc::c_int,
    pub worker_pid: libc::pid_t,
    pub layout: Option<EfxTypeInfo>,
    pub params: Vec<PluginParamDef>,
    pub param_values: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<u32, f64>>>,
    pub gui_events: std::sync::Arc<std::sync::Mutex<Vec<GuiParamEvent>>>,
}

// SAFETY: `bridge_ptr` points to POSIX shared memory synchronized across threads using atomic operations and futexes.
unsafe impl Send for ZfxShared {}
// SAFETY: Concurrent access to `ZfxShared` fields is guarded by atomic operations, Arc<RwLock>, and Arc<Mutex>.
unsafe impl Sync for ZfxShared {}
impl PluginShared<'_> for ZfxShared {}

impl Drop for ZfxShared {
    fn drop(&mut self) {
        scan_log("ZfxShared drop called");
        if !self.bridge_ptr.is_null() {
            // SAFETY: `bridge_ptr` is verified non-null and was mapped via mmap to the size of ZfxSharedBridge.
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

                libc::munmap(
                    self.bridge_ptr.cast::<libc::c_void>(),
                    std::mem::size_of::<protocol::ZfxSharedBridge>(),
                );
                libc::close(self.bridge_fd);
            }
        }

        if self.worker_pid > 0 {
            // SAFETY: Terminating child worker process via standard POSIX signal and waitpid.
            unsafe {
                libc::kill(self.worker_pid, libc::SIGTERM);
                let mut status = 0;
                libc::waitpid(self.worker_pid, &raw mut status, libc::WNOHANG);
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
        self.shared
            .param_values
            .read()
            .unwrap()
            .get(&param_id.get())
            .copied()
            .or_else(|| {
                self.shared
                    .params
                    .iter()
                    .find(|p| p.id == param_id.get())
                    .map(|p| p.default_value)
            })
    }

    fn value_to_text(
        &self,
        _param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> core::fmt::Result {
        use core::fmt::Write;
        write!(writer, "{value:.2}")
    }

    fn text_to_value(&self, _param_id: ClapId, text: &CStr) -> Option<f64> {
        text.to_str().ok()?.parse::<f64>().ok()
    }

    fn flush(
        &self,
        input_parameter_changes: &InputEvents,
        output_parameter_changes: &mut OutputEvents,
    ) {
        {
            let mut param_values = self.shared.param_values.write().unwrap();
            for event in input_parameter_changes {
                if let Some(CoreEventSpace::ParamValue(param_value_event)) = event.as_core_event() {
                    if let Some(param_id) = param_value_event.param_id() {
                        param_values.insert(param_id.get(), param_value_event.value());
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

impl PluginStateImpl for ZfxMainThread<'_> {
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        let mut sorted_entries: Vec<(u32, f64)> = {
            let values = self
                .shared
                .param_values
                .read()
                .map_err(|_| PluginError::Message("Failed to read params"))?;
            values.iter().map(|(&k, &v)| (k, v)).collect()
        };
        sorted_entries.sort_by_key(|&(k, _)| k);

        let mut data = Vec::with_capacity(12 + sorted_entries.len() * 12);
        data.extend_from_slice(b"ZFXS");
        data.extend_from_slice(&1u32.to_le_bytes());
        let count_u32 = u32::try_from(sorted_entries.len())
            .map_err(|_| PluginError::Message("Too many parameters to serialize"))?;
        data.extend_from_slice(&count_u32.to_le_bytes());

        for (id, val) in sorted_entries {
            data.extend_from_slice(&id.to_le_bytes());
            data.extend_from_slice(&val.to_le_bytes());
        }

        output
            .write_all(&data)
            .map_err(|_| PluginError::Message("Failed to write state stream"))?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut header = [0u8; 12];
        input
            .read_exact(&mut header)
            .map_err(|_| PluginError::Message("State stream too short"))?;

        if &header[0..4] != b"ZFXS" {
            return Err(PluginError::Message("Invalid ZFX state magic"));
        }
        let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if version != 1 {
            return Err(PluginError::Message("Unsupported ZFX state version"));
        }
        let count = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;

        let mut loaded_entries = Vec::with_capacity(count);
        let mut entry = [0u8; 12];
        for _ in 0..count {
            input
                .read_exact(&mut entry)
                .map_err(|_| PluginError::Message("Truncated state stream"))?;
            let id = u32::from_le_bytes(entry[0..4].try_into().unwrap());
            let val = f64::from_le_bytes(entry[4..12].try_into().unwrap());
            loaded_entries.push((id, val));
        }

        {
            let mut values = self
                .shared
                .param_values
                .write()
                .map_err(|_| PluginError::Message("Failed to write params"))?;
            for (id, val) in loaded_entries {
                values.insert(id, val);
            }
        } // CRITICAL: Write guard `values` is dropped here BEFORE calling host_params.rescan!

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
    #[serde(rename = "SlWidth", default, deserialize_with = "de_opt_f32")]
    pub sl_width: Option<f32>,
    #[serde(rename = "SlHeight", default, deserialize_with = "de_opt_f32")]
    pub sl_height: Option<f32>,
    #[serde(rename = "Name", default)]
    pub name: Option<String>,
}

pub struct ZfxAudioProcessor<'a> {
    pub shared: &'a ZfxShared,
    pub in_l: Vec<f32>,
    pub in_r: Vec<f32>,
    pub out_l: Vec<f32>,
    pub out_r: Vec<f32>,
    pub seq: u32,
    pub last_sent_params: std::collections::HashMap<u32, f32>,
}

impl PluginAudioProcessorParams for ZfxAudioProcessor<'_> {
    #[allow(clippy::too_many_lines)]
    fn flush(
        &mut self,
        input_parameter_changes: &InputEvents,
        output_parameter_changes: &mut OutputEvents,
    ) {
        let mut param_values = self.shared.param_values.write().unwrap();
        for event in input_parameter_changes {
            if let Some(CoreEventSpace::ParamValue(param_value_event)) = event.as_core_event() {
                if let Some(param_id) = param_value_event.param_id() {
                    param_values.insert(param_id.get(), param_value_event.value());
                }
            }
        }

        // Synchronize changed parameters to shared bridge even when transport is stopped
        let bridge_ptr = self.shared.bridge_ptr;
        if !bridge_ptr.is_null() {
            let mut num_chgs = 0;
            for param in &self.shared.params {
                if let Some(&val) = param_values.get(&param.id) {
                    let range = param.max_value - param.min_value;
                    let norm = if range > 0.0 {
                        ((val - param.min_value) / range).clamp(0.0, 1.0) as f32
                    } else {
                        val.clamp(0.0, 1.0) as f32
                    };

                    let changed = match self.last_sent_params.get(&param.id) {
                        Some(&last) => (last - norm).abs() > 1e-5,
                        None => true,
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
                        self.last_sent_params.insert(param.id, norm);
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
        let max_frames = (config.max_frames_count as usize).max(256);
        let bridge_ptr = shared.bridge_ptr;
        if !bridge_ptr.is_null() {
            // SAFETY: `bridge_ptr` is checked non-null and points to the mapped `ZfxSharedBridge` instance.
            unsafe {
                let bridge = &mut *bridge_ptr;
                bridge.sample_rate = config.sample_rate as f32;
                bridge.block_size = config.max_frames_count.max(64);
            }
        }
        Ok(Self {
            shared,
            in_l: vec![0.0; max_frames],
            in_r: vec![0.0; max_frames],
            out_l: vec![0.0; max_frames],
            out_r: vec![0.0; max_frames],
            seq: 1,
            last_sent_params: std::collections::HashMap::new(),
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

        // 1. Process parameter automation changes
        {
            let mut param_values = self.shared.param_values.write().unwrap();
            for event in events.input {
                if let Some(CoreEventSpace::ParamValue(param_value_event)) = event.as_core_event() {
                    if let Some(param_id) = param_value_event.param_id() {
                        let val = param_value_event.value();
                        param_values.insert(param_id.get(), val);
                    }
                }
            }

            // Synchronize changed parameters to shared bridge
            let mut num_chgs = 0;
            for param in &self.shared.params {
                if let Some(&val) = param_values.get(&param.id) {
                    let range = param.max_value - param.min_value;
                    let norm = if range > 0.0 {
                        ((val - param.min_value) / range).clamp(0.0, 1.0) as f32
                    } else {
                        val.clamp(0.0, 1.0) as f32
                    };

                    let changed = match self.last_sent_params.get(&param.id) {
                        Some(&last) => (last - norm).abs() > 1e-5,
                        None => true,
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
                        self.last_sent_params.insert(param.id, norm);
                    }
                }
            }
            // SAFETY: `bridge_ptr` points to valid shared memory allocated via mmap.
            unsafe {
                (*bridge_ptr).num_param_changes = num_chgs as u32;
            }
        }

        // Determine whether plugin is currently bypassed via prm 0
        let is_bypassed = {
            let val_opt = self.shared.param_values.read().unwrap().get(&0).copied();
            let mut bypassed = false;
            if let Some(val) = val_opt {
                for param in &self.shared.params {
                    if param.id == 0 {
                        let name_lower = param.name.to_lowercase();
                        if name_lower.contains("bypass") {
                            bypassed = val >= 50.0 || val >= 0.5;
                        } else if name_lower.contains("on/off") || name_lower.contains("power") {
                            bypassed = val < 50.0;
                        }
                        break;
                    }
                }
            }
            bypassed
        };

        // 2. Audio processing through Zoom ZFX 32-bit DSP companion
        for mut port_pair in &mut audio {
            if let Ok(Some(channel_pairs)) = port_pair
                .channels()
                .map(clack_plugin::prelude::SampleType::into_f32)
            {
                let pairs: Vec<_> = channel_pairs.into_iter().collect();
                let mut frames = 0;

                for (ch, pair) in pairs.iter().enumerate() {
                    match pair {
                        clack_plugin::process::audio::ChannelPair::InputOnly(in_buf)
                        | clack_plugin::process::audio::ChannelPair::InputOutput(in_buf, _) => {
                            frames = in_buf.len();
                            if ch == 0 {
                                if self.in_l.len() < frames {
                                    self.in_l.resize(frames, 0.0);
                                }
                                self.in_l[..frames].copy_from_slice(in_buf);
                            } else if ch == 1 {
                                if self.in_r.len() < frames {
                                    self.in_r.resize(frames, 0.0);
                                }
                                self.in_r[..frames].copy_from_slice(in_buf);
                            }
                        }
                        clack_plugin::process::audio::ChannelPair::InPlace(buf) => {
                            frames = buf.len();
                            if ch == 0 {
                                if self.in_l.len() < frames {
                                    self.in_l.resize(frames, 0.0);
                                }
                                self.in_l[..frames].copy_from_slice(buf);
                            } else if ch == 1 {
                                if self.in_r.len() < frames {
                                    self.in_r.resize(frames, 0.0);
                                }
                                self.in_r[..frames].copy_from_slice(buf);
                            }
                        }
                        clack_plugin::process::audio::ChannelPair::OutputOnly(buf) => {
                            frames = buf.len();
                        }
                    }
                }

                if is_bypassed {
                    // True Software Passthrough: Bit-exact dry audio when bypassed
                    for (ch, pair) in pairs.into_iter().enumerate() {
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
                    continue;
                }

                if frames > 0 {
                    let clamped_frames = frames.min(protocol::MAX_FRAMES);
                    // SAFETY: `bridge_ptr` points to valid shared memory allocated via mmap and `clamped_frames` <= `MAX_FRAMES`.
                    unsafe {
                        let bridge = &mut *bridge_ptr;
                        bridge.num_frames = clamped_frames as u32;
                        bridge.in_l[..clamped_frames].copy_from_slice(&self.in_l[..clamped_frames]);
                        if pairs.len() >= 2 {
                            bridge.in_r[..clamped_frames]
                                .copy_from_slice(&self.in_r[..clamped_frames]);
                        } else {
                            bridge.in_r[..clamped_frames]
                                .copy_from_slice(&self.in_l[..clamped_frames]);
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
                                    // Timeout safety guard
                                    eprintln!("[zfx-bootstrap] Warning: Audio process futex timeout after spins={spins}");
                                    break;
                                }
                            } else {
                                std::hint::spin_loop();
                            }
                        }

                        if self.out_l.len() < clamped_frames {
                            self.out_l.resize(clamped_frames, 0.0);
                        }
                        if self.out_r.len() < clamped_frames {
                            self.out_r.resize(clamped_frames, 0.0);
                        }

                        // Sanitize & clamp all samples against NaN, Inf, and excessive DC spikes
                        for (dst, &src) in self.out_l[..clamped_frames]
                            .iter_mut()
                            .zip(&bridge.out_l[..clamped_frames])
                        {
                            *dst = if src.is_finite() {
                                src.clamp(-4.0, 4.0)
                            } else {
                                0.0
                            };
                        }

                        for (dst, &src) in self.out_r[..clamped_frames]
                            .iter_mut()
                            .zip(&bridge.out_r[..clamped_frames])
                        {
                            *dst = if src.is_finite() {
                                src.clamp(-4.0, 4.0)
                            } else {
                                0.0
                            };
                        }
                    }
                }

                for (ch, pair) in pairs.into_iter().enumerate() {
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

clack_export_entry!(SinglePluginEntry<ZfxBootstrapPlugin>);

impl clack_plugin::entry::DefaultPluginFactory for ZfxBootstrapPlugin {
    fn get_descriptor() -> PluginDescriptor {
        scan_log(&format!(
            "get_descriptor called: id={PLUGIN_ID}, name={PLUGIN_NAME}"
        ));
        PluginDescriptor::new(PLUGIN_ID, PLUGIN_NAME)
            .with_vendor("ZFX Porting Project")
            .with_features(PLUGIN_FEATURES.iter().copied())
    }

    #[allow(clippy::too_many_lines)]
    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        scan_log(&format!("new_shared start: id={PLUGIN_ID}"));
        if EMBEDDED_WORKER_PAYLOAD.is_empty() {
            scan_log("new_shared ERR: worker payload empty");
            return Err(PluginError::Message("Embedded worker payload is empty"));
        }
        if EMBEDDED_DLL_PAYLOAD.is_empty() {
            scan_log("new_shared ERR: dll payload empty");
            return Err(PluginError::Message("Embedded DLL payload is empty"));
        }

        // 1. Create anonymous shared memory for the bridge
        let bridge_size = std::mem::size_of::<protocol::ZfxSharedBridge>();
        // SAFETY: Calling libc memfd_create with a valid null-terminated name.
        let bridge_fd = unsafe { libc::memfd_create(c"zfx-bridge".as_ptr(), 0) };
        if bridge_fd < 0 {
            return Err(PluginError::Message("Failed to create bridge memfd"));
        }
        let off_size = libc::off_t::try_from(bridge_size)
            .map_err(|_| PluginError::Message("Bridge size overflow for off_t"))?;
        // SAFETY: Sizing bridge_fd to match the size of ZfxSharedBridge.
        if unsafe { libc::ftruncate(bridge_fd, off_size) } != 0 {
            // SAFETY: Closing bridge_fd on ftruncate error.
            unsafe {
                libc::close(bridge_fd);
            }
            return Err(PluginError::Message("Failed to ftruncate bridge memfd"));
        }
        // SAFETY: Mapping shared memory for IPC bridge communication.
        let bridge_addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bridge_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                bridge_fd,
                0,
            )
        };
        if bridge_addr == libc::MAP_FAILED {
            // SAFETY: Closing bridge_fd on mmap error.
            unsafe {
                libc::close(bridge_fd);
            }
            return Err(PluginError::Message("Failed to mmap bridge"));
        }
        // SAFETY: bridge_addr is non-null, mapped to bridge_size, and aligned to ZfxSharedBridge.
        let bridge = unsafe { &mut *bridge_addr.cast::<protocol::ZfxSharedBridge>() };
        bridge.init_header();

        // 2. Copy embedded DLL payload into bridge
        let dll_len = EMBEDDED_DLL_PAYLOAD.len();
        if dll_len > protocol::MAX_PAYLOAD_SIZE {
            // SAFETY: Cleaning up bridge resources when payload exceeds max buffer size.
            unsafe {
                libc::munmap(bridge_addr, bridge_size);
                libc::close(bridge_fd);
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

        // 3. Create anonymous memfd for the 32-bit worker
        // SAFETY: Creating an anonymous memfd for the worker binary with MFD_CLOEXEC.
        let worker_fd = unsafe { libc::memfd_create(c"zfx-worker32".as_ptr(), libc::MFD_CLOEXEC) };
        if worker_fd < 0 {
            // SAFETY: Cleaning up bridge resources on memfd creation failure.
            unsafe {
                libc::munmap(bridge_addr, bridge_size);
                libc::close(bridge_fd);
            }
            return Err(PluginError::Message("Failed to create worker memfd"));
        }
        let mut written = 0;
        while written < EMBEDDED_WORKER_PAYLOAD.len() {
            // SAFETY: Writing the worker payload bytes into worker_fd.
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

        // 4. Spawn child worker process
        // SAFETY: Forking a child worker process to execute the 32-bit loader.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            // SAFETY: Cleaning up resources on fork failure.
            unsafe {
                libc::close(worker_fd);
                libc::munmap(bridge_addr, bridge_size);
                libc::close(bridge_fd);
            }
            return Err(PluginError::Message("Failed to fork worker process"));
        }
        if pid == 0 {
            // Child process:
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

        // 5. Handshake: wait for worker to signal STATUS_READY
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
            scan_log("new_shared ERR: Timeout waiting for zfx-worker32 ready");
            // SAFETY: Terminating unresponsive worker process and cleaning up bridge resources.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                let mut st = 0;
                libc::waitpid(pid, &raw mut st, libc::WNOHANG);
                libc::munmap(bridge_addr, bridge_size);
                libc::close(bridge_fd);
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
            scan_log("new_shared ERR: zfx-worker32 failed CMD_INIT");
            // SAFETY: Terminating failed worker and cleaning up shared memory.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                let mut st = 0;
                libc::waitpid(pid, &raw mut st, libc::WNOHANG);
                libc::munmap(bridge_addr, bridge_size);
                libc::close(bridge_fd);
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

        let get_bridge_param = |id: u32| -> (String, f64) {
            for i in 0..(bridge.num_params as usize).min(protocol::MAX_PARAMS) {
                if bridge.params[i].id == id {
                    let name_end = bridge.params[i]
                        .name
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(bridge.params[i].name.len());
                    let name_str = String::from_utf8_lossy(&bridge.params[i].name[..name_end])
                        .trim()
                        .to_string();
                    return (name_str, f64::from(bridge.params[i].default_val));
                }
            }
            (String::new(), 0.0)
        };

        if let Some(l) = &layout {
            if let Some(ctrls) = &l.ctrls {
                for ctrl in &ctrls.ctrl {
                    let id = ctrl.prm;
                    if id > 0 && seen_ids.insert(id) {
                        let (dll_name, init_norm) = get_bridge_param(id);
                        let name = if !dll_name.is_empty() {
                            format!("{dll_name}\0")
                        } else if let Some(n) = &ctrl.name {
                            if n.trim().is_empty() {
                                format!("Param {id}\0")
                            } else {
                                format!("{} (Prm {})\0", n.trim(), id)
                            }
                        } else {
                            format!("Param {id}\0")
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

        for i in 0..(bridge.num_params as usize).min(protocol::MAX_PARAMS) {
            let id = bridge.params[i].id;
            if seen_ids.insert(id) {
                let name_end = bridge.params[i]
                    .name
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(bridge.params[i].name.len());
                let dll_name = String::from_utf8_lossy(&bridge.params[i].name[..name_end])
                    .trim()
                    .to_string();
                let name = if !dll_name.is_empty() {
                    format!("{dll_name}\0")
                } else if id == 0 {
                    "Bypass\0".to_string()
                } else {
                    format!("Param {id}\0")
                };
                let init_norm = f64::from(bridge.params[i].default_val);
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

        let mut initial_param_values = std::collections::HashMap::new();
        for p in &params {
            initial_param_values.insert(p.id, p.default_value);
        }

        scan_log(&format!(
            "new_shared OK: id={}, params={}",
            PLUGIN_ID,
            params.len()
        ));

        Ok(ZfxShared {
            bridge_ptr: bridge_addr.cast::<protocol::ZfxSharedBridge>(),
            bridge_fd,
            worker_pid: pid,
            layout,
            params,
            param_values: std::sync::Arc::new(std::sync::RwLock::new(initial_param_values)),
            gui_events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    fn new_main_thread<'a>(
        host: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        scan_log(&format!("new_main_thread for {PLUGIN_ID}"));
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
        let ctx = egui::Context::default();
        let bytes = gui::find_asset("bkgd")
            .or_else(|| gui::find_asset("Bkgd"))
            .expect("Bkgd asset not found");
        let tex = gui::load_texture(&ctx, "bkgd", bytes);
        assert!(tex.is_some(), "load_texture returned None!");
    }
}
