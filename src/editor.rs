#![allow(dead_code, unused_imports)]
use bytes::Bytes;
use mp4::{
    AacConfig, AudioObjectType, AvcConfig, ChannelConfig, Mp4Config, Mp4Reader, Mp4Sample,
    Mp4Writer, SampleFreqIndex, TrackConfig, TrackType,
};
use openh264::decoder::Decoder as OpenH264Decoder;
use openh264::formats::YUVSource;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::audio_engine::build_silent_aac_packet;
use crate::events::{Event, EventSink};
use crate::recorder::buffer::{AudioStemPackets, CompressedPacket, PacketKind, RingBuffer};
use crate::recorder::config::{parse_duration_secs, parse_resolution_pair, RecorderConfig};
use crate::recorder::encoder::{CodecContextInfo, EncoderPipeline};
use crate::recorder::priority;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioTrackRole {
    MasterMix,
    Game,
    Discord,
    Microphone,
    OtherApps,
}

impl AudioTrackRole {
    pub fn from_audio_index(idx: usize) -> Self {
        match idx {
            0 => Self::MasterMix,
            1 => Self::Game,
            2 => Self::Discord,
            3 => Self::Microphone,
            _ => Self::OtherApps,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MasterMix => "Master Mix",
            Self::Game => "Game Audio",
            Self::Discord => "Discord / Voice Chat",
            Self::Microphone => "Microphone",
            Self::OtherApps => "Other Apps",
        }
    }

    pub fn short_suffix(self) -> &'static str {
        match self {
            Self::MasterMix => "master",
            Self::Game => "game",
            Self::Discord => "discord",
            Self::Microphone => "mic",
            Self::OtherApps => "other",
        }
    }

    pub fn parse_str(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "master" | "mix" | "master_mix" | "all" => Some(Self::MasterMix),
            "game" | "gameplay" | "game_audio" => Some(Self::Game),
            "discord" | "voice" | "chat" | "comms" => Some(Self::Discord),
            "mic" | "microphone" => Some(Self::Microphone),
            "other" | "apps" | "other_apps" | "browser" | "music" => Some(Self::OtherApps),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioTrackMetadata {
    pub track_id: u32,
    pub audio_index: usize,
    pub role: AudioTrackRole,
    pub label: String,
    pub timescale: u32,
    pub sample_count: u32,
    pub duration_ms: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipMetadata {
    pub path: PathBuf,
    pub file_size_bytes: u64,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_track_id: Option<u32>,
    pub video_sample_count: u32,
    pub keyframe_timestamps_ms: Vec<u64>,
    pub audio_tracks: Vec<AudioTrackMetadata>,
    pub has_separated_stems: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutMode {
    FastKeyframeCopy,
    FrameExact,
    KeyframeSnap,
}

impl Default for CutMode {
    fn default() -> Self {
        Self::FastKeyframeCopy
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionPreset {
    Original,
    P480,
    P720,
    P900,
    P1080,
    P1440,
    Custom { width: u32, height: u32 },
}

impl ResolutionPreset {
    pub fn resolve_dimensions(self, src_w: u32, src_h: u32) -> (u32, u32) {
        let target_h = match self {
            Self::Original => return ((src_w.max(64)) & !1, (src_h.max(64)) & !1),
            Self::P480 => 480u32,
            Self::P720 => 720u32,
            Self::P900 => 900u32,
            Self::P1080 => 1080u32,
            Self::P1440 => 1440u32,
            Self::Custom { width, height } => {
                return ((width.clamp(64, 7680)) & !1, (height.clamp(64, 4320)) & !1)
            }
        };
        if src_h == 0 || src_w == 0 {
            return (1280, 720);
        }
        let scaled_w = ((src_w as u64 * target_h as u64) / src_h as u64) as u32;
        ((scaled_w.clamp(64, 7680)) & !1, (target_h.clamp(64, 4320)) & !1)
    }

    pub fn parse_str(raw: &str) -> Option<Self> {
        let clean = raw.trim().to_lowercase();
        match clean.as_str() {
            "original" | "source" | "native" | "same" => Some(Self::Original),
            "480p" | "sd" => Some(Self::P480),
            "720p" | "hd" => Some(Self::P720),
            "900p" => Some(Self::P900),
            "1080p" | "fhd" => Some(Self::P1080),
            "1440p" | "2k" | "qhd" => Some(Self::P1440),
            _ => {
                let (w, h) = parse_resolution_pair(&clean)?;
                Some(Self::Custom {
                    width: w,
                    height: h,
                })
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioStemMixConfig {
    pub game_enabled: bool,
    pub discord_enabled: bool,
    pub mic_enabled: bool,
    pub other_enabled: bool,
    pub game_gain: f32,
    pub discord_gain: f32,
    pub mic_gain: f32,
    pub other_gain: f32,
    pub keep_stem_tracks_in_output: bool,
}

impl Default for AudioStemMixConfig {
    fn default() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: true,
            mic_enabled: true,
            other_enabled: true,
            game_gain: 1.0,
            discord_gain: 1.0,
            mic_gain: 1.0,
            other_gain: 1.0,
            keep_stem_tracks_in_output: false,
        }
    }
}

impl AudioStemMixConfig {
    pub fn game_only() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: false,
            mic_enabled: false,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn discord_only() -> Self {
        Self {
            game_enabled: false,
            discord_enabled: true,
            mic_enabled: false,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn mic_only() -> Self {
        Self {
            game_enabled: false,
            discord_enabled: false,
            mic_enabled: true,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn other_only() -> Self {
        Self {
            game_enabled: false,
            discord_enabled: false,
            mic_enabled: false,
            other_enabled: true,
            ..Self::default()
        }
    }

    pub fn no_discord() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: false,
            mic_enabled: true,
            other_enabled: true,
            ..Self::default()
        }
    }

    pub fn no_mic() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: true,
            mic_enabled: false,
            other_enabled: true,
            ..Self::default()
        }
    }

    pub fn no_other_apps() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: true,
            mic_enabled: true,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn mute_all() -> Self {
        Self {
            game_enabled: false,
            discord_enabled: false,
            mic_enabled: false,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn is_all_muted(&self) -> bool {
        (!self.game_enabled || self.game_gain <= 0.001)
            && (!self.discord_enabled || self.discord_gain <= 0.001)
            && (!self.mic_enabled || self.mic_gain <= 0.001)
            && (!self.other_enabled || self.other_gain <= 0.001)
    }

    pub fn is_unmodified_full_mix(&self) -> bool {
        self.game_enabled
            && self.discord_enabled
            && self.mic_enabled
            && self.other_enabled
            && (self.game_gain - 1.0).abs() < 0.01
            && (self.discord_gain - 1.0).abs() < 0.01
            && (self.mic_gain - 1.0).abs() < 0.01
            && (self.other_gain - 1.0).abs() < 0.01
    }

    pub fn single_solo_role(&self) -> Option<(AudioTrackRole, f32)> {
        let mut active = Vec::with_capacity(4);
        if self.game_enabled && self.game_gain > 0.001 {
            active.push((AudioTrackRole::Game, self.game_gain));
        }
        if self.discord_enabled && self.discord_gain > 0.001 {
            active.push((AudioTrackRole::Discord, self.discord_gain));
        }
        if self.mic_enabled && self.mic_gain > 0.001 {
            active.push((AudioTrackRole::Microphone, self.mic_gain));
        }
        if self.other_enabled && self.other_gain > 0.001 {
            active.push((AudioTrackRole::OtherApps, self.other_gain));
        }
        if active.len() == 1 {
            Some(active[0])
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EditJob {
    pub input_path: PathBuf,
    pub output_path: Option<PathBuf>,
    pub trim_start_ms: Option<u64>,
    pub trim_end_ms: Option<u64>,
    pub cut_mode: CutMode,
    pub rescale: Option<(u32, u32)>,
    pub target_fps: Option<u32>,
    pub video_bitrate_kbps: Option<u32>,
    pub audio_mix: Option<AudioStemMixConfig>,
    pub mute_all_audio: bool,
    pub prefer_hardware: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditResult {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio_tracks_written: usize,
    pub backend: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SplitResult {
    pub input_path: PathBuf,
    pub parts: Vec<EditResult>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportedStemFile {
    pub role: AudioTrackRole,
    pub label: String,
    pub m4a_path: PathBuf,
    pub aac_path: PathBuf,
    pub bytes: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StemExportResult {
    pub input_path: PathBuf,
    pub stems: Vec<ExportedStemFile>,
    pub elapsed_ms: u64,
}

pub fn parse_timestamp_ms(raw: &str) -> Option<u64> {
    let s = raw.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Some(ms_str) = s.strip_suffix("ms") {
        let val = ms_str.trim().parse::<f64>().ok()?;
        if val >= 0.0 {
            return Some(val.round() as u64);
        }
    }
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 2 {
            let mins = parts[0].trim().parse::<u64>().ok()?;
            let secs = parts[1].trim().parse::<f64>().ok()?;
            if secs >= 0.0 {
                return Some(mins * 60_000 + (secs * 1000.0).round() as u64);
            }
        } else if parts.len() == 3 {
            let hrs = parts[0].trim().parse::<u64>().ok()?;
            let mins = parts[1].trim().parse::<u64>().ok()?;
            let secs = parts[2].trim().parse::<f64>().ok()?;
            if secs >= 0.0 {
                return Some(hrs * 3_600_000 + mins * 60_000 + (secs * 1000.0).round() as u64);
            }
        }
    }
    let clean_sec = s
        .strip_suffix("secs")
        .or_else(|| s.strip_suffix("sec"))
        .or_else(|| s.strip_suffix('s'))
        .unwrap_or(&s)
        .trim();
    if let Ok(sec_f) = clean_sec.parse::<f64>() {
        if sec_f >= 0.0 {
            return Some((sec_f * 1000.0).round() as u64);
        }
    }
    parse_duration_secs(&s).map(|secs| secs * 1000)
}

pub fn default_edited_output_path(input_path: &Path, suffix: &str) -> PathBuf {
    let parent = input_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");
    let ext = input_path
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| if e.eq_ignore_ascii_case("mkv") { "mkv" } else { "mp4" })
        .unwrap_or("mp4");
    let clean_suffix = suffix.trim_matches('_').trim();
    let label = if clean_suffix.is_empty() || clean_suffix.eq_ignore_ascii_case("edited") || clean_suffix.eq_ignore_ascii_case("edit") {
        "edited".to_string()
    } else {
        clean_suffix.to_string()
    };
    let first = parent.join(format!("{stem} ({label}).{ext}"));
    if !first.exists() {
        return first;
    }
    for n in 2..=9999u32 {
        let candidate = parent.join(format!("{stem} ({label} {n}).{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    parent.join(format!("{stem} ({label} {ts}).{ext}"))
}

fn convert_yuv_to_rgba_scaled(
    yuv: &openh264::decoder::DecodedYUV<'_>,
    dst_w: u32,
    dst_h: u32,
    rgba_out: &mut [u8],
) {
    let (src_w, src_h) = yuv.dimensions();
    let (ys, us, vs) = yuv.strides();
    let y_plane = yuv.y();
    let u_plane = yuv.u();
    let v_plane = yuv.v();
    let dw = dst_w.max(1) as usize;
    let dh = dst_h.max(1) as usize;
    let sw_max = src_w.saturating_sub(1);
    let sh_max = src_h.saturating_sub(1);
    let uv_w_max = (src_w / 2).saturating_sub(1);
    let uv_h_max = (src_h / 2).saturating_sub(1);

    for dy in 0..dh {
        let sy_fp = (dy * src_h * 256) / dh;
        let sy0 = (sy_fp >> 8).min(sh_max);
        let sy1 = (sy0 + 1).min(sh_max);
        let wy = (sy_fp & 0xFF) as i32;
        let inv_wy = 256 - wy;

        let sy_uv = (sy0 / 2).min(uv_h_max);
        let y_row0 = sy0 * ys;
        let y_row1 = sy1 * ys;
        let u_row = sy_uv * us;
        let v_row = sy_uv * vs;

        for dx in 0..dw {
            let sx_fp = (dx * src_w * 256) / dw;
            let sx0 = (sx_fp >> 8).min(sw_max);
            let sx1 = (sx0 + 1).min(sw_max);
            let wx = (sx_fp & 0xFF) as i32;
            let inv_wx = 256 - wx;

            let y00 = y_plane.get(y_row0 + sx0).copied().unwrap_or(16) as i32;
            let y01 = y_plane.get(y_row0 + sx1).copied().unwrap_or(y00 as u8) as i32;
            let y10 = y_plane.get(y_row1 + sx0).copied().unwrap_or(y00 as u8) as i32;
            let y11 = y_plane.get(y_row1 + sx1).copied().unwrap_or(y00 as u8) as i32;
            let y_top = (y00 * inv_wx + y01 * wx) >> 8;
            let y_bot = (y10 * inv_wx + y11 * wx) >> 8;
            let y_val = (y_top * inv_wy + y_bot * wy) >> 8;

            let sx_uv = (sx0 / 2).min(uv_w_max);
            let u_val = u_plane.get(u_row + sx_uv).copied().unwrap_or(128) as i32;
            let v_val = v_plane.get(v_row + sx_uv).copied().unwrap_or(128) as i32;

            let c = (y_val - 16).max(0);
            let d = u_val - 128;
            let e = v_val - 128;
            let r = ((298 * c + 459 * e + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 55 * d - 136 * e + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 541 * d + 128) >> 8).clamp(0, 255) as u8;
            let out_off = (dy * dw + dx) * 4;
            if out_off + 3 < rgba_out.len() {
                rgba_out[out_off] = r;
                rgba_out[out_off + 1] = g;
                rgba_out[out_off + 2] = b;
                rgba_out[out_off + 3] = 255;
            }
        }
    }
}

#[cfg(windows)]
struct PreviewAudioPlayer {
    active_mix: Arc<Mutex<AudioStemMixConfig>>,
    target_seek_ms: Arc< std::sync::atomic::AtomicU64>,
    playing_flag: Arc<std::sync::atomic::AtomicBool>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    last_video_poll: Arc<Mutex<Instant>>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl PreviewAudioPlayer {
    fn spawn(audio_tracks: Vec<(AudioTrackMetadata, Vec<Arc<CompressedPacket>>)>) -> Option<Self> {
        if audio_tracks.is_empty() || audio_tracks[0].1.is_empty() {
            return None;
        }
        let active_mix = Arc::new(Mutex::new(AudioStemMixConfig::default()));
        let target_seek_ms = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        let playing_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_video_poll = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(5)));

        let mix_c = active_mix.clone();
        let seek_c = target_seek_ms.clone();
        let play_c = playing_flag.clone();
        let stop_c = stop_flag.clone();
        let poll_c = last_video_poll.clone();

        let handle = std::thread::Builder::new()
            .name("clotch-preview-audio".into())
            .spawn(move || {
                run_preview_audio_thread(audio_tracks, mix_c, seek_c, play_c, stop_c, poll_c);
            })
            .ok();

        Some(Self {
            active_mix,
            target_seek_ms,
            playing_flag,
            stop_flag,
            last_video_poll,
            thread_handle: handle,
        })
    }

    fn notify_video_playhead(&self, target_ms: u64) {
        if let Ok(mut g) = self.last_video_poll.lock() {
            let elapsed = g.elapsed();
            *g = Instant::now();
            if elapsed <= Duration::from_millis(95) {
                self.playing_flag.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let prev = self.target_seek_ms.swap(target_ms, std::sync::atomic::Ordering::AcqRel);
        let _ = prev;
    }

    fn set_mix(&self, mix: AudioStemMixConfig) {
        if let Ok(mut g) = self.active_mix.lock() {
            *g = mix;
        }
    }

    fn set_playing(&self, playing: bool) {
        self.playing_flag.store(playing, std::sync::atomic::Ordering::Release);
        if playing {
            if let Ok(mut g) = self.last_video_poll.lock() {
                *g = Instant::now();
            }
        }
    }
}

#[cfg(windows)]
impl Drop for PreviewAudioPlayer {
    fn drop(&mut self) {
        self.stop_flag.store(true, std::sync::atomic::Ordering::Release);
        if let Some(h) = self.thread_handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(windows)]
fn decode_aac_packets_via_mft(packets: &[Arc<CompressedPacket>]) -> Vec<(f32, f32)> {
    use std::mem::ManuallyDrop;
    use windows::core::GUID;
    use windows::Win32::Media::MediaFoundation::{
        IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType,
        MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio, MFT_MESSAGE_COMMAND_FLUSH,
        MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
        MFT_OUTPUT_DATA_BUFFER, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
        MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
        MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_USER_DATA,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    const CLSID_CMS_AAC_DEC_MFT: GUID = GUID::from_u128(0x32d186a7_218f_4c75_8876_dd77273a8999);
    const MF_AUDIO_FORMAT_AAC_GUID: GUID = GUID::from_u128(0x00001610_0000_0010_8000_00aa00389b71);
    const MF_AUDIO_FORMAT_FLOAT_GUID: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    const MF_MT_AAC_PAYLOAD_TYPE_GUID: GUID = GUID::from_u128(0xbfbab799_c9df_4c10_8935_07228f43a8ef);

    let _ = crate::recorder::ensure_mf_initialized();
    let mut pcm_out = Vec::with_capacity(packets.len() * 1024);

    unsafe {
        let Ok(mft) = CoCreateInstance::<_, IMFTransform>(&CLSID_CMS_AAC_DEC_MFT, None, CLSCTX_INPROC_SERVER) else {
            return pcm_out;
        };
        let Ok(in_type) = MFCreateMediaType() else {
            return pcm_out;
        };
        let _ = in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
        let _ = in_type.SetGUID(&MF_MT_SUBTYPE, &MF_AUDIO_FORMAT_AAC_GUID);
        let _ = in_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000);
        let _ = in_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2);
        let _ = in_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE_GUID, 0);
        let user_data: [u8; 14] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x90];
        let _ = in_type.SetBlob(&MF_MT_USER_DATA, &user_data);

        if mft.SetInputType(0, &in_type, 0).is_err() {
            return pcm_out;
        }

        let mut out_configured = false;
        for idx in 0..16 {
            let Ok(avail) = mft.GetOutputAvailableType(0, idx) else {
                break;
            };
            if avail.GetGUID(&MF_MT_SUBTYPE) == Ok(MF_AUDIO_FORMAT_FLOAT_GUID) {
                if mft.SetOutputType(0, &avail, 0).is_ok() {
                    out_configured = true;
                    break;
                }
            }
        }
        if !out_configured {
            if let Ok(out_type) = MFCreateMediaType() {
                let _ = out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
                let _ = out_type.SetGUID(&MF_MT_SUBTYPE, &MF_AUDIO_FORMAT_FLOAT_GUID);
                let _ = out_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000);
                let _ = out_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2);
                let _ = out_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32);
                let _ = out_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 8);
                if mft.SetOutputType(0, &out_type, 0).is_ok() {
                    out_configured = true;
                }
            }
        }
        if !out_configured {
            return pcm_out;
        }

        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

        let Ok(out_sample) = MFCreateSample() else {
            return pcm_out;
        };
        let Ok(out_buf) = MFCreateMemoryBuffer(32768) else {
            return pcm_out;
        };
        let _ = out_sample.AddBuffer(&out_buf);

        for pkt in packets {
            if pkt.data.is_empty() {
                continue;
            }
            let Ok(in_s) = MFCreateSample() else {
                continue;
            };
            let Ok(in_b) = MFCreateMemoryBuffer(pkt.data.len() as u32) else {
                continue;
            };
            let mut ptr: *mut u8 = std::ptr::null_mut();
            if in_b.Lock(&mut ptr, None, None).is_ok() {
                if !ptr.is_null() {
                    std::ptr::copy_nonoverlapping(pkt.data.as_ptr(), ptr, pkt.data.len());
                    let _ = in_b.SetCurrentLength(pkt.data.len() as u32);
                }
                let _ = in_b.Unlock();
            }
            let _ = in_s.AddBuffer(&in_b);
            let _ = mft.ProcessInput(0, &in_s, 0);

            loop {
                let _ = out_buf.SetCurrentLength(0);
                let _ = out_sample.RemoveAllBuffers();
                let _ = out_sample.AddBuffer(&out_buf);
                let mut out_data = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(out_sample.clone())),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let hr = mft.ProcessOutput(0, &mut out_data, &mut status);
                let _ = ManuallyDrop::take(&mut out_data[0].pEvents);
                let prod = ManuallyDrop::take(&mut out_data[0].pSample);
                match hr {
                    Ok(()) => {
                        if let Some(s) = prod {
                            if let Ok(mb) = s.ConvertToContiguousBuffer() {
                                let mut rptr: *mut u8 = std::ptr::null_mut();
                                let mut clen = 0u32;
                                if mb.Lock(&mut rptr, None, Some(&mut clen)).is_ok() {
                                    if !rptr.is_null() && clen >= 8 {
                                        let f_slice = std::slice::from_raw_parts(
                                            rptr as *const f32,
                                            (clen as usize) / 4,
                                        );
                                        for pair in f_slice.chunks_exact(2) {
                                            let l = if pair[0].is_finite() { pair[0] } else { 0.0 };
                                            let r = if pair[1].is_finite() { pair[1] } else { 0.0 };
                                            pcm_out.push((l, r));
                                        }
                                    }
                                    let _ = mb.Unlock();
                                }
                            }
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        if let Ok(avail) = mft.GetOutputAvailableType(0, 0) {
                            let _ = mft.SetOutputType(0, &avail, 0);
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(_) => break,
                }
            }
        }
    }
    pcm_out
}

#[cfg(windows)]
fn run_preview_audio_thread(
    audio_tracks: Vec<(AudioTrackMetadata, Vec<Arc<CompressedPacket>>)>,
    active_mix: Arc<Mutex<AudioStemMixConfig>>,
    target_seek_ms: Arc<std::sync::atomic::AtomicU64>,
    playing_flag: Arc<std::sync::atomic::AtomicBool>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    last_video_poll: Arc<Mutex<Instant>>,
) {
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, WAVEFORMATEX,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let master_pcm = decode_aac_packets_via_mft(&audio_tracks[0].1);
    if master_pcm.is_empty() {
        return;
    }
    let has_stems = audio_tracks.len() >= 5;
    let game_pcm = if has_stems { decode_aac_packets_via_mft(&audio_tracks[1].1) } else { Vec::new() };
    let discord_pcm = if has_stems { decode_aac_packets_via_mft(&audio_tracks[2].1) } else { Vec::new() };
    let mic_pcm = if has_stems { decode_aac_packets_via_mft(&audio_tracks[3].1) } else { Vec::new() };
    let other_pcm = if has_stems { decode_aac_packets_via_mft(&audio_tracks[4].1) } else { Vec::new() };

    unsafe {
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return;
        };
        let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return;
        };
        let Ok(client) = device.Activate::<IAudioClient>(CLSCTX_ALL, None) else {
            return;
        };
        let wf = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        if client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0x8000_0000 | 0x0800_0000,
                1_000_000,
                0,
                &wf,
                None,
            )
            .is_err()
        {
            return;
        }
        let Ok(buf_frames) = client.GetBufferSize() else {
            return;
        };
        let Ok(render) = client.GetService::<IAudioRenderClient>() else {
            return;
        };
        let _ = client.Start();

        let mut cursor_sample: usize = 0;
        let mut last_target_ms: u64 = 0;

        while !stop_flag.load(std::sync::atomic::Ordering::Acquire) {
            let poll_stale = last_video_poll
                .lock()
                .map(|t| t.elapsed() > Duration::from_millis(140))
                .unwrap_or(true);
            let is_playing = playing_flag.load(std::sync::atomic::Ordering::Acquire) && !poll_stale;

            let req_ms = target_seek_ms.load(std::sync::atomic::Ordering::Acquire);
            if req_ms != u64::MAX {
                let target_sample = ((req_ms as usize) * 48).min(master_pcm.len());
                let diff = (target_sample as isize - cursor_sample as isize).abs();
                if diff > 4800 || req_ms != last_target_ms {
                    if diff > 3600 {
                        cursor_sample = target_sample;
                    }
                    last_target_ms = req_ms;
                }
            }

            if !is_playing || cursor_sample >= master_pcm.len() {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            if let Ok(padding) = client.GetCurrentPadding() {
                let avail = buf_frames.saturating_sub(padding) as usize;
                let to_write = avail.min(960);
                if to_write > 0 {
                    let mix_cfg = active_mix
                        .lock()
                        .map(|g| g.clone())
                        .unwrap_or_default();
                    if let Ok(buf_ptr) = render.GetBuffer(to_write as u32) {
                        let out_slice = std::slice::from_raw_parts_mut(buf_ptr as *mut f32, to_write * 2);
                        for i in 0..to_write {
                            let idx = cursor_sample + i;
                            let (l, r) = if mix_cfg.is_all_muted() || idx >= master_pcm.len() {
                                (0.0f32, 0.0f32)
                            } else if has_stems && !mix_cfg.is_unmodified_full_mix() {
                                let (gl, gr) = if mix_cfg.game_enabled {
                                    let s = game_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    (s.0 * mix_cfg.game_gain, s.1 * mix_cfg.game_gain)
                                } else {
                                    (0.0, 0.0)
                                };
                                let (dl, dr) = if mix_cfg.discord_enabled {
                                    let s = discord_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    (s.0 * mix_cfg.discord_gain, s.1 * mix_cfg.discord_gain)
                                } else {
                                    (0.0, 0.0)
                                };
                                let (ml, mr) = if mix_cfg.mic_enabled {
                                    let s = mic_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    (s.0 * mix_cfg.mic_gain, s.1 * mix_cfg.mic_gain)
                                } else {
                                    (0.0, 0.0)
                                };
                                let (ol, or_) = if mix_cfg.other_enabled {
                                    let s = other_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    (s.0 * mix_cfg.other_gain, s.1 * mix_cfg.other_gain)
                                } else {
                                    (0.0, 0.0)
                                };
                                ((gl + dl + ml + ol).clamp(-1.0, 1.0), (gr + dr + mr + or_).clamp(-1.0, 1.0))
                            } else {
                                master_pcm.get(idx).copied().unwrap_or((0.0, 0.0))
                            };
                            out_slice[i * 2] = l;
                            out_slice[i * 2 + 1] = r;
                        }
                        let _ = render.ReleaseBuffer(to_write as u32, 0);
                        cursor_sample = (cursor_sample + to_write).min(master_pcm.len());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(6));
        }
        let _ = client.Stop();
    }
}

pub struct VideoPreviewSession {
    metadata: ClipMetadata,
    sps: Vec<u8>,
    pps: Vec<u8>,
    video_packets: Vec<Arc<CompressedPacket>>,
    sw_dec: OpenH264Decoder,
    last_decoded_idx: Option<usize>,
    annex_b_buf: Vec<u8>,
    cached_rgba: Vec<u8>,
    cached_w: u32,
    cached_h: u32,
    cached_pts_ms: u64,
    #[cfg(windows)]
    audio_player: Option<PreviewAudioPlayer>,
}

impl VideoPreviewSession {
    pub fn open(input_path: impl AsRef<Path>) -> Result<Self, String> {
        let demuxed = demux_clip_full(input_path.as_ref())?;
        if demuxed.video_packets.is_empty() {
            return Err("clip contains no video packets".to_string());
        }
        let sw_dec = OpenH264Decoder::new()
            .map_err(|e| format!("OpenH264 decoder init failed: {e}"))?;
        #[cfg(windows)]
        let audio_player = PreviewAudioPlayer::spawn(demuxed.audio_tracks_packets);
        Ok(Self {
            metadata: demuxed.metadata,
            sps: demuxed.sps,
            pps: demuxed.pps,
            video_packets: demuxed.video_packets,
            sw_dec,
            last_decoded_idx: None,
            annex_b_buf: Vec::with_capacity(256 * 1024),
            cached_rgba: Vec::new(),
            cached_w: 0,
            cached_h: 0,
            cached_pts_ms: 0,
            #[cfg(windows)]
            audio_player,
        })
    }

    pub fn metadata(&self) -> &ClipMetadata {
        &self.metadata
    }

    pub fn set_preview_audio_mix(&self, mix: AudioStemMixConfig) {
        #[cfg(windows)]
        if let Some(ref ap) = self.audio_player {
            ap.set_mix(mix);
        }
        #[cfg(not(windows))]
        let _ = mix;
    }

    pub fn set_preview_playing(&self, playing: bool) {
        #[cfg(windows)]
        if let Some(ref ap) = self.audio_player {
            ap.set_playing(playing);
        }
        #[cfg(not(windows))]
        let _ = playing;
    }

    pub fn decode_frame_rgba(
        &mut self,
        target_ms: u64,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<(u64, Vec<u8>), String> {
        #[cfg(windows)]
        if let Some(ref ap) = self.audio_player {
            ap.notify_video_playhead(target_ms);
        }
        if self.video_packets.is_empty() {
            return Err("clip contains no video packets".to_string());
        }
        let dst_w = dst_w.max(32);
        let dst_h = dst_h.max(18);
        let expected_len = (dst_w as usize) * (dst_h as usize) * 4;

        let mut target_idx = 0usize;
        let mut kf_idx = 0usize;
        for (idx, pkt) in self.video_packets.iter().enumerate() {
            if pkt.pts_ms <= target_ms {
                target_idx = idx;
                if pkt.is_keyframe {
                    kf_idx = idx;
                }
            } else {
                break;
            }
        }

        if self.last_decoded_idx == Some(target_idx)
            && self.cached_w == dst_w
            && self.cached_h == dst_h
            && self.cached_rgba.len() == expected_len
        {
            return Ok((self.cached_pts_ms, self.cached_rgba.clone()));
        }

        let decode_start = match self.last_decoded_idx {
            Some(last) if last >= kf_idx && last < target_idx && (target_idx - last) <= 180 => {
                last + 1
            }
            _ => kf_idx,
        };

        if self.cached_rgba.len() != expected_len {
            self.cached_rgba.resize(expected_len, 0);
        }
        self.cached_w = dst_w;
        self.cached_h = dst_h;

        let mut decoded_any = false;
        for idx in decode_start..=target_idx {
            let pkt = &self.video_packets[idx];
            avcc_to_annex_b(
                &pkt.data,
                pkt.is_keyframe,
                &self.sps,
                &self.pps,
                &mut self.annex_b_buf,
            );
            if self.annex_b_buf.is_empty() {
                continue;
            }
            if let Ok(Some(yuv)) = self.sw_dec.decode(&self.annex_b_buf) {
                if idx == target_idx || !decoded_any {
                    convert_yuv_to_rgba_scaled(&yuv, dst_w, dst_h, &mut self.cached_rgba);
                }
                self.last_decoded_idx = Some(idx);
                self.cached_pts_ms = pkt.pts_ms;
                decoded_any = true;
            }
        }

        if decoded_any || !self.cached_rgba.is_empty() {
            Ok((self.cached_pts_ms, self.cached_rgba.clone()))
        } else {
            Err("failed to decode preview frame".to_string())
        }
    }
}

pub fn extract_frame_rgba(
    input_path: impl AsRef<Path>,
    target_ms: u64,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>, String> {
    let path = input_path.as_ref();
    let dst_w = dst_w.max(32);
    let dst_h = dst_h.max(18);
    let expected_len = (dst_w as usize) * (dst_h as usize) * 4;

    if let Some(ffmpeg_bin) = locate_ffmpeg_binary() {
        let mut cmd = Command::new(ffmpeg_bin);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-ss")
            .arg(format!("{:.3}", target_ms as f64 / 1000.0))
            .arg("-i")
            .arg(path)
            .arg("-frames:v")
            .arg("1")
            .arg("-vf")
            .arg(format!("scale={dst_w}:{dst_h}:flags=fast_bilinear"))
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("rgba")
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000 | 0x00004000);
        }
        if let Ok(out) = cmd.output() {
            if out.status.success() && out.stdout.len() >= expected_len {
                let mut buf = out.stdout;
                buf.truncate(expected_len);
                return Ok(buf);
            }
        }
    }

    let demuxed = demux_clip_full(path)?;
    if demuxed.video_packets.is_empty() {
        return Err("clip contains no video packets".to_string());
    }

    let mut start_idx = 0usize;
    for (idx, pkt) in demuxed.video_packets.iter().enumerate() {
        if pkt.is_keyframe && pkt.pts_ms <= target_ms {
            start_idx = idx;
        }
        if pkt.pts_ms > target_ms {
            break;
        }
    }

    let mut sw_dec = OpenH264Decoder::new()
        .map_err(|e| format!("OpenH264 decoder init failed: {e}"))?;
    let mut annex_b_buf = Vec::with_capacity(256 * 1024);
    let mut rgba_out = vec![0u8; expected_len];
    let mut decoded_any = false;

    let max_decode_idx = (start_idx + 90).min(demuxed.video_packets.len());
    for pkt in &demuxed.video_packets[start_idx..max_decode_idx] {
        avcc_to_annex_b(
            &pkt.data,
            pkt.is_keyframe,
            &demuxed.sps,
            &demuxed.pps,
            &mut annex_b_buf,
        );
        if annex_b_buf.is_empty() {
            continue;
        }
        if let Ok(Some(yuv)) = sw_dec.decode(&annex_b_buf) {
            if pkt.pts_ms >= target_ms || !decoded_any {
                convert_yuv_to_rgba_scaled(&yuv, dst_w, dst_h, &mut rgba_out);
            }
            decoded_any = true;
            if pkt.pts_ms >= target_ms {
                break;
            }
        }
    }

    if decoded_any {
        Ok(rgba_out)
    } else {
        Err("failed to decode video frame at playhead".to_string())
    }
}

struct DemuxedClip {
    metadata: ClipMetadata,
    sps: Vec<u8>,
    pps: Vec<u8>,
    video_packets: Vec<Arc<CompressedPacket>>,
    audio_tracks_packets: Vec<(AudioTrackMetadata, Vec<Arc<CompressedPacket>>)>,
}

fn ebml_read_id(data: &[u8], pos: usize) -> Option<(u32, usize)> {
    let b0 = *data.get(pos)?;
    if b0 & 0x80 != 0 {
        Some((b0 as u32, 1))
    } else if b0 & 0x40 != 0 {
        let b1 = *data.get(pos + 1)?;
        Some((((b0 as u32) << 8) | (b1 as u32), 2))
    } else if b0 & 0x20 != 0 {
        let b1 = *data.get(pos + 1)?;
        let b2 = *data.get(pos + 2)?;
        Some((((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32), 3))
    } else if b0 & 0x10 != 0 {
        let b1 = *data.get(pos + 1)?;
        let b2 = *data.get(pos + 2)?;
        let b3 = *data.get(pos + 3)?;
        Some((
            ((b0 as u32) << 24) | ((b1 as u32) << 16) | ((b2 as u32) << 8) | (b3 as u32),
            4,
        ))
    } else {
        None
    }
}

fn ebml_read_size(data: &[u8], pos: usize) -> Option<(u64, usize, bool)> {
    let b0 = *data.get(pos)?;
    if b0 == 0 {
        return None;
    }
    let lz = (b0.leading_zeros() as usize).min(7);
    let len = lz + 1;
    if pos + len > data.len() {
        return None;
    }
    let mask = (1u8 << (7 - lz)) - 1;
    let mut val = (b0 & mask) as u64;
    let mut all_ones = (b0 & mask) == mask;
    for i in 1..len {
        let b = data[pos + i];
        if b != 0xFF {
            all_ones = false;
        }
        val = (val << 8) | (b as u64);
    }
    Some((val, len, all_ones))
}

fn ebml_read_uint(payload: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in payload.iter().take(8) {
        v = (v << 8) | (b as u64);
    }
    v
}

fn ebml_read_float(payload: &[u8]) -> f64 {
    if payload.len() == 4 {
        let mut arr = [0u8; 4];
        arr.copy_from_slice(payload);
        f32::from_be_bytes(arr) as f64
    } else if payload.len() == 8 {
        let mut arr = [0u8; 8];
        arr.copy_from_slice(payload);
        f64::from_be_bytes(arr)
    } else {
        0.0
    }
}

fn parse_avcc_or_annexb_codec_private(priv_bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    if priv_bytes.len() >= 8 && priv_bytes[0] == 1 {
        let num_sps = (priv_bytes[5] & 0x1F) as usize;
        let mut pos = 6usize;
        for _ in 0..num_sps {
            if pos + 2 > priv_bytes.len() {
                break;
            }
            let slen = u16::from_be_bytes([priv_bytes[pos], priv_bytes[pos + 1]]) as usize;
            pos += 2;
            if pos + slen <= priv_bytes.len() {
                if sps.is_empty() {
                    sps = priv_bytes[pos..pos + slen].to_vec();
                }
                pos += slen;
            } else {
                break;
            }
        }
        if pos < priv_bytes.len() {
            let num_pps = priv_bytes[pos] as usize;
            pos += 1;
            for _ in 0..num_pps {
                if pos + 2 > priv_bytes.len() {
                    break;
                }
                let plen = u16::from_be_bytes([priv_bytes[pos], priv_bytes[pos + 1]]) as usize;
                pos += 2;
                if pos + plen <= priv_bytes.len() {
                    if pps.is_empty() {
                        pps = priv_bytes[pos..pos + plen].to_vec();
                    }
                    pos += plen;
                } else {
                    break;
                }
            }
        }
    } else {
        extract_sps_pps_from_annexb(priv_bytes, &mut sps, &mut pps);
    }
    (sps, pps)
}

fn extract_sps_pps_from_annexb(data: &[u8], sps: &mut Vec<u8>, pps: &mut Vec<u8>) {
    let mut nal_starts = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                nal_starts.push((i, i + 3));
                i += 3;
                continue;
            } else if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                nal_starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    for idx in 0..nal_starts.len() {
        let payload_start = nal_starts[idx].1;
        let payload_end = if idx + 1 < nal_starts.len() {
            nal_starts[idx + 1].0
        } else {
            data.len()
        };
        if payload_start < payload_end {
            let nal = &data[payload_start..payload_end];
            let nal_type = nal[0] & 0x1F;
            if nal_type == 7 && sps.is_empty() {
                *sps = nal.to_vec();
            } else if nal_type == 8 && pps.is_empty() {
                *pps = nal.to_vec();
            }
        }
    }
}

fn normalize_h264_packet_to_avcc(
    raw: &[u8],
    sps: &mut Vec<u8>,
    pps: &mut Vec<u8>,
) -> Bytes {
    let is_annexb = (raw.len() >= 4 && raw[0] == 0 && raw[1] == 0 && raw[2] == 0 && raw[3] == 1)
        || (raw.len() >= 3 && raw[0] == 0 && raw[1] == 0 && raw[2] == 1);
    if !is_annexb {
        if sps.is_empty() || pps.is_empty() {
            let mut pos = 0usize;
            while pos + 4 <= raw.len() {
                let nlen = u32::from_be_bytes([raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]])
                    as usize;
                pos += 4;
                if nlen == 0 || pos + nlen > raw.len() {
                    break;
                }
                let ntype = raw[pos] & 0x1F;
                if ntype == 7 && sps.is_empty() {
                    *sps = raw[pos..pos + nlen].to_vec();
                } else if ntype == 8 && pps.is_empty() {
                    *pps = raw[pos..pos + nlen].to_vec();
                }
                pos += nlen;
            }
        }
        return Bytes::copy_from_slice(raw);
    }

    if sps.is_empty() || pps.is_empty() {
        extract_sps_pps_from_annexb(raw, sps, pps);
    }

    let mut nal_starts = Vec::new();
    let mut i = 0usize;
    while i + 3 < raw.len() {
        if raw[i] == 0 && raw[i + 1] == 0 {
            if raw[i + 2] == 1 {
                nal_starts.push((i, i + 3));
                i += 3;
                continue;
            } else if i + 4 <= raw.len() && raw[i + 2] == 0 && raw[i + 3] == 1 {
                nal_starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    let mut avcc = Vec::with_capacity(raw.len() + 16);
    for idx in 0..nal_starts.len() {
        let p_start = nal_starts[idx].1;
        let p_end = if idx + 1 < nal_starts.len() {
            nal_starts[idx + 1].0
        } else {
            raw.len()
        };
        if p_start < p_end {
            let nal = &raw[p_start..p_end];
            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
        }
    }
    Bytes::from(avcc)
}

fn split_mkv_block_frames<'a>(block: & [u8], header_end: usize, lacing_mode: u8) -> Vec<& [u8]> {
    if header_end >= block.len() {
        return Vec::new();
    }
    let payload = &block[header_end..];
    if lacing_mode == 0 || payload.is_empty() {
        return vec![payload];
    }
    let frame_count = (payload[0] as usize) + 1;
    let mut pos = 1usize;
    let mut sizes: Vec<usize> = Vec::with_capacity(frame_count);

    match lacing_mode {
        1 => {
            for _ in 0..(frame_count.saturating_sub(1)) {
                let mut sz = 0usize;
                while pos < payload.len() {
                    let b = payload[pos];
                    pos += 1;
                    sz += b as usize;
                    if b < 255 {
                        break;
                    }
                }
                sizes.push(sz);
            }
        }
        2 => {
            let rem = payload.len().saturating_sub(1);
            let each = rem / frame_count.max(1);
            for _ in 0..(frame_count.saturating_sub(1)) {
                sizes.push(each);
            }
        }
        3 => {
            let mut last_sz: i64 = 0;
            for f_idx in 0..(frame_count.saturating_sub(1)) {
                let Some((val, vlen, _)) = ebml_read_size(payload, pos) else {
                    break;
                };
                pos += vlen;
                let sz = if f_idx == 0 {
                    val as i64
                } else {
                    let bias = (1i64 << (7 * vlen - 1)) - 1;
                    last_sz + ((val as i64) - bias)
                };
                last_sz = sz.max(0);
                sizes.push(last_sz as usize);
            }
        }
        _ => return vec![payload],
    }

    let mut out = Vec::with_capacity(frame_count);
    for sz in sizes {
        if pos + sz <= payload.len() {
            out.push(&payload[pos..pos + sz]);
            pos += sz;
        } else {
            break;
        }
    }
    if pos < payload.len() {
        out.push(&payload[pos..]);
    }
    out
}

pub fn probe_clip_header_fast(input_path: impl AsRef<Path>) -> Result<(f64, bool), String> {
    use std::io::Read;
    let path = input_path.as_ref();
    let mut file = File::open(path)
        .map_err(|e| format!("failed to open clip '{}': {e}", path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|e| format!("failed to read metadata '{}': {e}", path.display()))?
        .len();
    if file_size == 0 {
        return Err("clip file is empty".to_string());
    }

    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok() && magic == [0x1A, 0x45, 0xDF, 0xA3] {
        let mut head_buf = vec![0u8; (file_size.min(256 * 1024)) as usize];
        head_buf[..4].copy_from_slice(&magic);
        let n = file.read(&mut head_buf[4..]).unwrap_or(0);
        head_buf.truncate(4 + n);

        let mut tc_scale_ns: u64 = 1_000_000;
        let mut dur_scaled: f64 = 0.0;
        let mut audio_track_count = 0usize;

        let mut pos = 0usize;
        while pos < head_buf.len() {
            let Some((id, id_len)) = ebml_read_id(&head_buf, pos) else {
                break;
            };
            let Some((sz, sz_len, is_unk)) = ebml_read_size(&head_buf, pos + id_len) else {
                break;
            };
            let body_start = pos + id_len + sz_len;
            if id == 0x18538067 {
                pos = body_start;
                continue;
            }
            if id == 0x1F43B675 {
                break;
            }
            let body_end = if is_unk {
                head_buf.len()
            } else {
                (body_start + (sz as usize)).min(head_buf.len())
            };
            let body = &head_buf[body_start..body_end];
            if id == 0x1549A966 {
                let mut ip = 0usize;
                while ip < body.len() {
                    let Some((iid, ilen)) = ebml_read_id(body, ip) else {
                        break;
                    };
                    let Some((isz, islen, _)) = ebml_read_size(body, ip + ilen) else {
                        break;
                    };
                    let ib_start = ip + ilen + islen;
                    let ib_end = (ib_start + (isz as usize)).min(body.len());
                    if iid == 0x2AD7B1 {
                        tc_scale_ns = ebml_read_uint(&body[ib_start..ib_end]).max(1);
                    } else if iid == 0x4489 {
                        dur_scaled = ebml_read_float(&body[ib_start..ib_end]);
                    }
                    ip = ib_end;
                }
            } else if id == 0x1654AE6B {
                let mut tp = 0usize;
                while tp < body.len() {
                    let Some((tid, tlen)) = ebml_read_id(body, tp) else {
                        break;
                    };
                    let Some((tsz, tslen, _)) = ebml_read_size(body, tp + tlen) else {
                        break;
                    };
                    let tb_start = tp + tlen + tslen;
                    let tb_end = (tb_start + (tsz as usize)).min(body.len());
                    if tid == 0xAE {
                        let entry = &body[tb_start..tb_end];
                        let mut ep = 0usize;
                        while ep < entry.len() {
                            let Some((eid, elen)) = ebml_read_id(entry, ep) else {
                                break;
                            };
                            let Some((esz, eslen, _)) = ebml_read_size(entry, ep + elen) else {
                                break;
                            };
                            let eb_start = ep + elen + eslen;
                            let eb_end = (eb_start + (esz as usize)).min(entry.len());
                            if eid == 0x83 && ebml_read_uint(&entry[eb_start..eb_end]) == 2 {
                                audio_track_count += 1;
                            }
                            ep = eb_end;
                        }
                    }
                    tp = tb_end;
                }
            }
            pos = body_end;
        }

        let dur_ms = ((dur_scaled * (tc_scale_ns as f64)) / 1_000_000.0).round() as u64;
        let dur_secs = if dur_ms > 100 {
            dur_ms as f64 / 1000.0
        } else {
            15.0
        };
        return Ok((dur_secs.max(1.0), audio_track_count >= 5));
    }

    let file = File::open(path)
        .map_err(|e| format!("failed to reopen clip '{}': {e}", path.display()))?;
    let reader = BufReader::with_capacity(128 * 1024, file);
    let mp4 = Mp4Reader::read_header(reader, file_size)
        .map_err(|e| format!("invalid MP4 header '{}': {e}", path.display()))?;

    let mut v_dur_ms = mp4.duration().as_millis() as u64;
    let mut audio_track_count = 0usize;
    for track in mp4.tracks().values() {
        match track.track_type() {
            Ok(TrackType::Video) => {
                let td = track.duration().as_millis() as u64;
                if td > 0 {
                    v_dur_ms = td;
                }
            }
            Ok(TrackType::Audio) => {
                audio_track_count += 1;
            }
            _ => {}
        }
    }
    let dur_secs = (v_dur_ms as f64 / 1000.0).max(1.0);
    Ok((dur_secs, audio_track_count >= 5))
}

fn demux_mkv_full(input_path: &Path) -> Result<DemuxedClip, String> {
    let raw_bytes = std::fs::read(input_path)
        .map_err(|e| format!("failed to read MKV file '{}': {e}", input_path.display()))?;
    let file_size = raw_bytes.len() as u64;
    if file_size < 16 {
        return Err("MKV file is too small".to_string());
    }

    let mut tc_scale_ns: u64 = 1_000_000;
    let mut duration_scaled: f64 = 0.0;
    let mut video_track_num: Option<u64> = None;
    let mut src_w: u32 = 1920;
    let mut src_h: u32 = 1080;
    let mut default_frame_ns: u64 = 0;
    let mut sps: Vec<u8> = Vec::new();
    let mut pps: Vec<u8> = Vec::new();

    struct MkvAudioTrackDef {
        track_num: u64,
        sample_rate: u32,
        packets: Vec<Arc<CompressedPacket>>,
        total_bytes: u64,
    }
    let mut audio_tracks: Vec<MkvAudioTrackDef> = Vec::new();
    let mut video_packets: Vec<Arc<CompressedPacket>> = Vec::new();
    let mut keyframe_timestamps_ms: Vec<u64> = Vec::new();

    let mut pos = 0usize;
    while pos < raw_bytes.len() {
        let Some((id, id_len)) = ebml_read_id(&raw_bytes, pos) else {
            break;
        };
        let Some((sz, sz_len, is_unk)) = ebml_read_size(&raw_bytes, pos + id_len) else {
            break;
        };
        let body_start = pos + id_len + sz_len;
        if id == 0x18538067 {
            pos = body_start;
            continue;
        }
        let body_end = if is_unk {
            raw_bytes.len()
        } else {
            (body_start + (sz as usize)).min(raw_bytes.len())
        };
        let body = &raw_bytes[body_start..body_end];

        match id {
            0x1549A966 => {
                let mut ip = 0usize;
                while ip < body.len() {
                    let Some((iid, ilen)) = ebml_read_id(body, ip) else {
                        break;
                    };
                    let Some((isz, islen, _)) = ebml_read_size(body, ip + ilen) else {
                        break;
                    };
                    let ib_start = ip + ilen + islen;
                    let ib_end = (ib_start + (isz as usize)).min(body.len());
                    if iid == 0x2AD7B1 {
                        tc_scale_ns = ebml_read_uint(&body[ib_start..ib_end]).max(1);
                    } else if iid == 0x4489 {
                        duration_scaled = ebml_read_float(&body[ib_start..ib_end]);
                    }
                    ip = ib_end;
                }
            }
            0x1654AE6B => {
                let mut tp = 0usize;
                while tp < body.len() {
                    let Some((tid, tlen)) = ebml_read_id(body, tp) else {
                        break;
                    };
                    let Some((tsz, tslen, _)) = ebml_read_size(body, tp + tlen) else {
                        break;
                    };
                    let tb_start = tp + tlen + tslen;
                    let tb_end = (tb_start + (tsz as usize)).min(body.len());
                    if tid == 0xAE {
                        let entry = &body[tb_start..tb_end];
                        let mut t_num = 0u64;
                        let mut t_type = 0u64;
                        let mut codec_priv: &[u8] = &[];
                        let mut def_dur = 0u64;
                        let mut vw = 1920u32;
                        let mut vh = 1080u32;
                        let mut srate = 48_000u32;

                        let mut ep = 0usize;
                        while ep < entry.len() {
                            let Some((eid, elen)) = ebml_read_id(entry, ep) else {
                                break;
                            };
                            let Some((esz, eslen, _)) = ebml_read_size(entry, ep + elen) else {
                                break;
                            };
                            let eb_start = ep + elen + eslen;
                            let eb_end = (eb_start + (esz as usize)).min(entry.len());
                            let epay = &entry[eb_start..eb_end];
                            match eid {
                                0xD7 => t_num = ebml_read_uint(epay),
                                0x83 => t_type = ebml_read_uint(epay),
                                0x63A2 => codec_priv = epay,
                                0x23E383 => def_dur = ebml_read_uint(epay),
                                0xE0 => {
                                    let mut vp = 0usize;
                                    while vp < epay.len() {
                                        let Some((vid, vlen)) = ebml_read_id(epay, vp) else {
                                            break;
                                        };
                                        let Some((vsz, vslen, _)) =
                                            ebml_read_size(epay, vp + vlen)
                                        else {
                                            break;
                                        };
                                        let vb_s = vp + vlen + vslen;
                                        let vb_e = (vb_s + (vsz as usize)).min(epay.len());
                                        if vid == 0xB0 {
                                            vw = ebml_read_uint(&epay[vb_s..vb_e]).max(64) as u32;
                                        } else if vid == 0xBA {
                                            vh = ebml_read_uint(&epay[vb_s..vb_e]).max(64) as u32;
                                        }
                                        vp = vb_e;
                                    }
                                }
                                0xE1 => {
                                    let mut ap = 0usize;
                                    while ap < epay.len() {
                                        let Some((aid, alen)) = ebml_read_id(epay, ap) else {
                                            break;
                                        };
                                        let Some((asz, aslen, _)) =
                                            ebml_read_size(epay, ap + alen)
                                        else {
                                            break;
                                        };
                                        let ab_s = ap + alen + aslen;
                                        let ab_e = (ab_s + (asz as usize)).min(epay.len());
                                        if aid == 0xB5 {
                                            let f = ebml_read_float(&epay[ab_s..ab_e]);
                                            if f >= 8000.0 {
                                                srate = f.round() as u32;
                                            }
                                        }
                                        ap = ab_e;
                                    }
                                }
                                _ => {}
                            }
                            ep = eb_end;
                        }

                        if t_type == 1 && video_track_num.is_none() && t_num > 0 {
                            video_track_num = Some(t_num);
                            src_w = vw;
                            src_h = vh;
                            default_frame_ns = def_dur;
                            if !codec_priv.is_empty() {
                                let (s, p) = parse_avcc_or_annexb_codec_private(codec_priv);
                                sps = s;
                                pps = p;
                            }
                        } else if t_type == 2 && t_num > 0 {
                            audio_tracks.push(MkvAudioTrackDef {
                                track_num: t_num,
                                sample_rate: srate,
                                packets: Vec::new(),
                                total_bytes: 0,
                            });
                        }
                    }
                    tp = tb_end;
                }
            }
            0x1F43B675 => {
                let mut cluster_tc: u64 = 0;
                let mut cp = 0usize;
                while cp < body.len() {
                    let Some((cid, clen)) = ebml_read_id(body, cp) else {
                        break;
                    };
                    let Some((csz, cslen, c_unk)) = ebml_read_size(body, cp + clen) else {
                        break;
                    };
                    let cb_start = cp + clen + cslen;
                    if c_unk && cid == 0x1F43B675 {
                        break;
                    }
                    let cb_end = if c_unk {
                        body.len()
                    } else {
                        (cb_start + (csz as usize)).min(body.len())
                    };
                    let cpay = &body[cb_start..cb_end];

                    if cid == 0xE7 {
                        cluster_tc = ebml_read_uint(cpay);
                    } else if cid == 0xA3 || cid == 0xA0 {
                        let (block_slice, is_simple, has_ref_block) = if cid == 0xA3 {
                            (Some(cpay), true, false)
                        } else {
                            let mut blk_opt: Option<&[u8]> = None;
                            let mut has_ref = false;
                            let mut bgp = 0usize;
                            while bgp < cpay.len() {
                                let Some((bid, blen)) = ebml_read_id(cpay, bgp) else {
                                    break;
                                };
                                let Some((bsz, bslen, _)) = ebml_read_size(cpay, bgp + blen) else {
                                    break;
                                };
                                let bb_s = bgp + blen + bslen;
                                let bb_e = (bb_s + (bsz as usize)).min(cpay.len());
                                if bid == 0xA1 {
                                    blk_opt = Some(&cpay[bb_s..bb_e]);
                                } else if bid == 0xFB {
                                    has_ref = true;
                                }
                                bgp = bb_e;
                            }
                            (blk_opt, false, has_ref)
                        };

                        if let Some(blk) = block_slice {
                            if let Some((t_num, vlen, _)) = ebml_read_size(blk, 0) {
                                if blk.len() >= vlen + 3 {
                                    let rel_tc =
                                        i16::from_be_bytes([blk[vlen], blk[vlen + 1]]) as i64;
                                    let flags = blk[vlen + 2];
                                    let is_kf = if is_simple {
                                        (flags & 0x80) != 0
                                    } else {
                                        !has_ref_block
                                    };
                                    let lacing = (flags >> 1) & 0x03;
                                    let abs_tc = ((cluster_tc as i64) + rel_tc).max(0) as u128;
                                    let pts_ms =
                                        ((abs_tc * (tc_scale_ns as u128)) / 1_000_000) as u64;

                                    let frames = split_mkv_block_frames(blk, vlen + 3, lacing);
                                    if Some(t_num) == video_track_num {
                                        let nom_dur_ms = if default_frame_ns > 0 {
                                            (default_frame_ns / 1_000_000).max(1) as i64
                                        } else {
                                            16
                                        };
                                        for (f_idx, f_bytes) in frames.iter().enumerate() {
                                            if f_bytes.is_empty() {
                                                continue;
                                            }
                                            let f_pts =
                                                pts_ms + (f_idx as u64) * (nom_dur_ms as u64);
                                            let avcc_bytes = normalize_h264_packet_to_avcc(
                                                f_bytes, &mut sps, &mut pps,
                                            );
                                            if is_kf && f_idx == 0 {
                                                keyframe_timestamps_ms.push(f_pts);
                                            }
                                            video_packets.push(Arc::new(CompressedPacket {
                                                kind: PacketKind::Video,
                                                pts_ms: f_pts,
                                                pts: f_pts as i64,
                                                dts: f_pts as i64,
                                                duration: nom_dur_ms,
                                                is_keyframe: is_kf && f_idx == 0,
                                                data: avcc_bytes,
                                                stems: None,
                                            }));
                                        }
                                    } else if let Some(a_def) =
                                        audio_tracks.iter_mut().find(|a| a.track_num == t_num)
                                    {
                                        for (f_idx, f_bytes) in frames.iter().enumerate() {
                                            if f_bytes.is_empty() {
                                                continue;
                                            }
                                            let clean_aac = if f_bytes.len() > 7
                                                && f_bytes[0] == 0xFF
                                                && (f_bytes[1] & 0xF0) == 0xF0
                                            {
                                                let hdr_len = if (f_bytes[1] & 0x01) == 0 {
                                                    9
                                                } else {
                                                    7
                                                };
                                                if f_bytes.len() > hdr_len {
                                                    &f_bytes[hdr_len..]
                                                } else {
                                                    *f_bytes
                                                }
                                            } else {
                                                *f_bytes
                                            };
                                            let f_pts = pts_ms + (f_idx as u64) * 21;
                                            let start_ticks = (f_pts * 48_000) / 1000;
                                            a_def.total_bytes += clean_aac.len() as u64;
                                            a_def.packets.push(Arc::new(CompressedPacket {
                                                kind: PacketKind::Audio,
                                                pts_ms: f_pts,
                                                pts: start_ticks as i64,
                                                dts: start_ticks as i64,
                                                duration: 1024,
                                                is_keyframe: true,
                                                data: Bytes::copy_from_slice(clean_aac),
                                                stems: None,
                                            }));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    cp = cb_end;
                }
            }
            _ => {}
        }
        pos = body_end;
    }

    if video_packets.is_empty() {
        if let Some(ffmpeg_bin) = locate_ffmpeg_binary() {
            let tmp_mp4 = std::env::temp_dir().join(format!(
                "clotch_mkv_remux_{}.mp4",
                std::process::id()
            ));
            let mut cmd = Command::new(ffmpeg_bin);
            cmd.arg("-y")
                .arg("-hide_banner")
                .arg("-loglevel")
                .arg("error")
                .arg("-i")
                .arg(input_path)
                .arg("-map")
                .arg("0:v:0")
                .arg("-map")
                .arg("0:a?")
                .arg("-c:v")
                .arg("libx264")
                .arg("-preset")
                .arg("ultrafast")
                .arg("-crf")
                .arg("18")
                .arg("-c:a")
                .arg("aac")
                .arg(&tmp_mp4)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000 | 0x00004000);
            }
            if let Ok(st) = cmd.status() {
                if st.success() && tmp_mp4.exists() {
                    let res = demux_mp4_full(&tmp_mp4, Some(input_path));
                    let _ = std::fs::remove_file(&tmp_mp4);
                    return res;
                }
            }
            let _ = std::fs::remove_file(&tmp_mp4);
        }
        return Err("MKV clip contains no readable H.264 video packets".to_string());
    }

    video_packets.sort_by_key(|p| p.pts_ms);
    for i in 0..video_packets.len() {
        if i + 1 < video_packets.len() {
            let d = video_packets[i + 1]
                .pts_ms
                .saturating_sub(video_packets[i].pts_ms)
                .clamp(1, 1000) as i64;
            if let Some(pkt) = Arc::get_mut(&mut video_packets[i]) {
                pkt.duration = d;
            }
        }
    }

    let mut v_dur_ms = ((duration_scaled * (tc_scale_ns as f64)) / 1_000_000.0).round() as u64;
    if let Some(last_v) = video_packets.last() {
        let last_end = last_v.pts_ms.saturating_add(last_v.duration.max(16) as u64);
        if v_dur_ms < last_end {
            v_dur_ms = last_end;
        }
    }

    let fps = if !video_packets.is_empty() && v_dur_ms > 0 {
        (((video_packets.len() as u64 * 1000) + (v_dur_ms / 2)) / v_dur_ms.max(1)) as u32
    } else {
        60
    }
    .clamp(15, 240);

    let mut audio_tracks_meta = Vec::with_capacity(audio_tracks.len());
    let mut audio_tracks_packets = Vec::with_capacity(audio_tracks.len());

    for (audio_idx, mut a_def) in audio_tracks.into_iter().enumerate() {
        a_def.packets.sort_by_key(|p| p.pts_ms);
        let role = AudioTrackRole::from_audio_index(audio_idx);
        let eff_dur = a_def
            .packets
            .last()
            .map(|p| p.pts_ms + 21)
            .unwrap_or(v_dur_ms);
        let meta = AudioTrackMetadata {
            track_id: a_def.track_num as u32,
            audio_index: audio_idx,
            role,
            label: role.label().to_string(),
            timescale: a_def.sample_rate.max(8000),
            sample_count: a_def.packets.len() as u32,
            duration_ms: eff_dur,
            total_bytes: a_def.total_bytes,
        };
        audio_tracks_meta.push(meta.clone());
        audio_tracks_packets.push((meta, a_def.packets));
    }

    if audio_tracks_packets.len() >= 5 {
        let master_len = audio_tracks_packets[0].1.len();
        let mut enriched_master = Vec::with_capacity(master_len);
        for i in 0..master_len {
            let base_pkt = &audio_tracks_packets[0].1[i];
            let game_b = audio_tracks_packets[1]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| base_pkt.data.clone());
            let discord_b = audio_tracks_packets[2]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));
            let mic_b = audio_tracks_packets[3]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));
            let other_b = audio_tracks_packets[4]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));

            enriched_master.push(Arc::new(CompressedPacket {
                kind: PacketKind::Audio,
                pts_ms: base_pkt.pts_ms,
                pts: base_pkt.pts,
                dts: base_pkt.dts,
                duration: base_pkt.duration,
                is_keyframe: true,
                data: base_pkt.data.clone(),
                stems: Some(Arc::new(AudioStemPackets {
                    game: game_b,
                    discord: discord_b,
                    mic: mic_b,
                    other: other_b,
                })),
            }));
        }
        audio_tracks_packets[0].1 = enriched_master;
    }

    let has_separated_stems = audio_tracks_meta.len() >= 5;
    let metadata = ClipMetadata {
        path: input_path.to_path_buf(),
        file_size_bytes: file_size,
        duration_ms: v_dur_ms,
        width: src_w,
        height: src_h,
        fps,
        video_track_id: video_track_num.map(|n| n as u32),
        video_sample_count: video_packets.len() as u32,
        keyframe_timestamps_ms,
        audio_tracks: audio_tracks_meta,
        has_separated_stems,
    };

    Ok(DemuxedClip {
        metadata,
        sps,
        pps,
        video_packets,
        audio_tracks_packets,
    })
}

pub fn demux_mkv_for_discord(
    input_path: &Path,
) -> Result<
    (
        u32,
        u32,
        u32,
        u64,
        Vec<u8>,
        Vec<u8>,
        Vec<(u64, bool, Bytes)>,
        Vec<Arc<CompressedPacket>>,
        u64,
    ),
    String,
> {
    let demuxed = demux_mkv_full(input_path)?;
    let video_samples: Vec<(u64, bool, Bytes)> = demuxed
        .video_packets
        .iter()
        .map(|p| (p.pts_ms, p.is_keyframe, p.data.clone()))
        .collect();
    let (audio_packets, audio_total_bytes) = if let Some((meta, pkts)) =
        demuxed.audio_tracks_packets.into_iter().next()
    {
        (pkts, meta.total_bytes)
    } else {
        (Vec::new(), 0)
    };
    Ok((
        demuxed.metadata.width,
        demuxed.metadata.height,
        demuxed.metadata.fps,
        demuxed.metadata.duration_ms,
        demuxed.sps,
        demuxed.pps,
        video_samples,
        audio_packets,
        audio_total_bytes,
    ))
}

fn demux_clip_full(input_path: &Path) -> Result<DemuxedClip, String> {
    if let Ok(mut f) = File::open(input_path) {
        use std::io::Read;
        let mut magic = [0u8; 4];
        if f.read_exact(&mut magic).is_ok() && magic == [0x1A, 0x45, 0xDF, 0xA3] {
            return demux_mkv_full(input_path);
        }
    }
    demux_mp4_full(input_path, None)
}

fn demux_mp4_full(input_path: &Path, logical_path: Option<&Path>) -> Result<DemuxedClip, String> {
    let file = File::open(input_path)
        .map_err(|e| format!("failed to open clip '{}': {e}", input_path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|e| format!("failed to read metadata '{}': {e}", input_path.display()))?
        .len();
    if file_size == 0 {
        return Err("clip file is empty (0 bytes)".to_string());
    }
    let reader = BufReader::with_capacity(2 * 1024 * 1024, file);
    let mut mp4 = Mp4Reader::read_header(reader, file_size)
        .map_err(|e| format!("invalid MP4 header '{}': {e}", input_path.display()))?;

    let mut video_track_id: Option<u32> = None;
    let mut src_w = 1920u32;
    let mut src_h = 1080u32;
    let mut v_timescale = 90_000u32;
    let mut v_sample_count = 0u32;
    let mut v_dur_ms = mp4.duration().as_millis() as u64;
    let mut sps = Vec::new();
    let mut pps = Vec::new();

    let mut raw_audio_tracks: Vec<(u32, u32, u32, u64)> = Vec::new();
    let mut sorted_track_ids: Vec<u32> = mp4.tracks().keys().copied().collect();
    sorted_track_ids.sort_unstable();

    for tid in sorted_track_ids {
        let Some(track) = mp4.tracks().get(&tid) else {
            continue;
        };
        match track.track_type() {
            Ok(TrackType::Video) if video_track_id.is_none() => {
                video_track_id = Some(track.track_id());
                src_w = (track.width() as u32).max(64);
                src_h = (track.height() as u32).max(64);
                v_timescale = track.timescale().max(1);
                v_sample_count = track.sample_count();
                let td = track.duration().as_millis() as u64;
                if td > 0 {
                    v_dur_ms = td;
                }
                if let Ok(s) = track.sequence_parameter_set() {
                    sps = s.to_vec();
                }
                if let Ok(p) = track.picture_parameter_set() {
                    pps = p.to_vec();
                }
            }
            Ok(TrackType::Audio) => {
                let ts = track.timescale().max(1);
                let sc = track.sample_count();
                let dur = track.duration().as_millis() as u64;
                raw_audio_tracks.push((track.track_id(), ts, sc, dur));
            }
            _ => {}
        }
    }

    let mut video_packets = Vec::with_capacity(v_sample_count as usize);
    let mut keyframe_timestamps_ms = Vec::new();

    if let Some(v_id) = video_track_id {
        for sample_idx in 1..=v_sample_count {
            if let Ok(Some(s)) = mp4.read_sample(v_id, sample_idx) {
                let pts_ms = (s.start_time.saturating_mul(1000)) / (v_timescale as u64);
                let dur_ms = ((s.duration as u64).saturating_mul(1000) / (v_timescale as u64)).max(1);
                let ticks_90k = if v_timescale == 90_000 {
                    (s.start_time as i64).saturating_add(s.rendering_offset as i64)
                } else {
                    (((s.start_time as i128 + s.rendering_offset as i128) * 90_000)
                        / (v_timescale as i128)) as i64
                };
                if s.is_sync {
                    keyframe_timestamps_ms.push(pts_ms);
                }
                video_packets.push(Arc::new(CompressedPacket {
                    kind: PacketKind::Video,
                    pts_ms,
                    pts: ticks_90k.max(0),
                    dts: ticks_90k.max(0),
                    duration: dur_ms as i64,
                    is_keyframe: s.is_sync,
                    data: s.bytes,
                    stems: None,
                }));
            }
        }
    }

    if v_dur_ms == 0 {
        if let Some(last_v) = video_packets.last() {
            v_dur_ms = last_v.pts_ms.saturating_add(last_v.duration.max(16) as u64);
        }
    }

    let fps = if !video_packets.is_empty() && v_dur_ms > 0 {
        (((video_packets.len() as u64 * 1000) + (v_dur_ms / 2)) / v_dur_ms.max(1)) as u32
    } else {
        60
    }
    .clamp(15, 240);

    let mut audio_tracks_meta = Vec::with_capacity(raw_audio_tracks.len());
    let mut audio_tracks_packets = Vec::with_capacity(raw_audio_tracks.len());

    for (audio_idx, (a_id, a_ts, a_sc, a_dur)) in raw_audio_tracks.into_iter().enumerate() {
        let role = AudioTrackRole::from_audio_index(audio_idx);
        let mut pkts = Vec::with_capacity(a_sc as usize);
        let mut total_bytes = 0u64;

        for sample_idx in 1..=a_sc {
            if let Ok(Some(s)) = mp4.read_sample(a_id, sample_idx) {
                let pts_ms = (s.start_time.saturating_mul(1000)) / (a_ts as u64);
                let dur_ticks = if a_ts == 48_000 {
                    s.duration as i64
                } else {
                    ((s.duration as u64 * 48_000) / a_ts as u64) as i64
                };
                total_bytes += s.bytes.len() as u64;
                pkts.push(Arc::new(CompressedPacket {
                    kind: PacketKind::Audio,
                    pts_ms,
                    pts: s.start_time as i64,
                    dts: s.start_time as i64,
                    duration: dur_ticks.max(1024),
                    is_keyframe: true,
                    data: s.bytes,
                    stems: None,
                }));
            }
        }

        let eff_dur = if a_dur > 0 {
            a_dur
        } else {
            pkts.last().map(|p| p.pts_ms + 21).unwrap_or(v_dur_ms)
        };

        let meta = AudioTrackMetadata {
            track_id: a_id,
            audio_index: audio_idx,
            role,
            label: role.label().to_string(),
            timescale: a_ts,
            sample_count: pkts.len() as u32,
            duration_ms: eff_dur,
            total_bytes,
        };
        audio_tracks_meta.push(meta.clone());
        audio_tracks_packets.push((meta, pkts));
    }

    if audio_tracks_packets.len() >= 5 {
        let master_len = audio_tracks_packets[0].1.len();
        let mut enriched_master = Vec::with_capacity(master_len);
        for i in 0..master_len {
            let base_pkt = &audio_tracks_packets[0].1[i];
            let game_b = audio_tracks_packets[1]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| base_pkt.data.clone());
            let discord_b = audio_tracks_packets[2]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));
            let mic_b = audio_tracks_packets[3]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));
            let other_b = audio_tracks_packets[4]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));

            enriched_master.push(Arc::new(CompressedPacket {
                kind: PacketKind::Audio,
                pts_ms: base_pkt.pts_ms,
                pts: base_pkt.pts,
                dts: base_pkt.dts,
                duration: base_pkt.duration,
                is_keyframe: true,
                data: base_pkt.data.clone(),
                stems: Some(Arc::new(AudioStemPackets {
                    game: game_b,
                    discord: discord_b,
                    mic: mic_b,
                    other: other_b,
                })),
            }));
        }
        audio_tracks_packets[0].1 = enriched_master;
    }

    let has_separated_stems = audio_tracks_meta.len() >= 5;
    let metadata = ClipMetadata {
        path: input_path.to_path_buf(),
        file_size_bytes: file_size,
        duration_ms: v_dur_ms,
        width: src_w,
        height: src_h,
        fps,
        video_track_id,
        video_sample_count: video_packets.len() as u32,
        keyframe_timestamps_ms,
        audio_tracks: audio_tracks_meta,
        has_separated_stems,
    };

    Ok(DemuxedClip {
        metadata,
        sps,
        pps,
        video_packets,
        audio_tracks_packets,
    })
}

pub fn probe_clip(input_path: impl AsRef<Path>) -> Result<ClipMetadata, String> {
    let demuxed = demux_clip_full(input_path.as_ref())?;
    Ok(demuxed.metadata)
}

fn filter_video_slice_for_trim(
    video_packets: &[Arc<CompressedPacket>],
    start_ms: u64,
    end_ms: u64,
) -> (Vec<Arc<CompressedPacket>>, u64, u64) {
    if video_packets.is_empty() {
        return (Vec::new(), start_ms, end_ms);
    }
    let first_key_idx = video_packets
        .iter()
        .rposition(|p| p.pts_ms <= start_ms && p.is_keyframe)
        .or_else(|| {
            video_packets
                .iter()
                .position(|p| p.pts_ms >= start_ms && p.is_keyframe)
        })
        .unwrap_or(0);

    let mut out = Vec::new();
    for pkt in video_packets.iter().skip(first_key_idx) {
        if pkt.pts_ms > end_ms {
            break;
        }
        out.push(Arc::clone(pkt));
    }

    let actual_start = out.first().map(|p| p.pts_ms).unwrap_or(start_ms);
    let actual_end = out
        .last()
        .map(|p| p.pts_ms.saturating_add(p.duration.max(16) as u64))
        .unwrap_or(end_ms);
    (out, actual_start, actual_end)
}

fn build_remixed_audio_packets(
    demuxed: &DemuxedClip,
    start_ms: u64,
    end_ms: u64,
    mix_opt: Option<&AudioStemMixConfig>,
    mute_all: bool,
) -> (Vec<Arc<CompressedPacket>>, bool) {
    if mute_all || demuxed.audio_tracks_packets.is_empty() {
        return (Vec::new(), false);
    }
    if let Some(mix) = mix_opt {
        if mix.is_all_muted() {
            return (Vec::new(), false);
        }
    }

    let has_stems = demuxed.audio_tracks_packets.len() >= 5;
    let keep_stems = mix_opt
        .map(|m| m.keep_stem_tracks_in_output && has_stems)
        .unwrap_or(false);

    if has_stems {
        if let Some(mix) = mix_opt {
            if let Some((solo_role, _gain)) = mix.single_solo_role() {
                let track_idx = match solo_role {
                    AudioTrackRole::MasterMix => 0,
                    AudioTrackRole::Game => 1,
                    AudioTrackRole::Discord => 2,
                    AudioTrackRole::Microphone => 3,
                    AudioTrackRole::OtherApps => 4,
                };
                if let Some((_, ref track_pkts)) = demuxed.audio_tracks_packets.get(track_idx) {
                    let master_pkts = &demuxed.audio_tracks_packets[0].1;
                    let mut out = Vec::new();
                    for (idx, p) in track_pkts.iter().enumerate() {
                        if p.pts_ms.saturating_add(22) < start_ms {
                            continue;
                        }
                        if p.pts_ms > end_ms.saturating_add(40) {
                            break;
                        }
                        let stems_clone = if keep_stems {
                            master_pkts.get(idx).and_then(|mp| mp.stems.clone())
                        } else {
                            None
                        };
                        out.push(Arc::new(CompressedPacket {
                            kind: PacketKind::Audio,
                            pts_ms: p.pts_ms,
                            pts: p.pts,
                            dts: p.dts,
                            duration: p.duration,
                            is_keyframe: true,
                            data: p.data.clone(),
                            stems: stems_clone,
                        }));
                    }
                    return (out, keep_stems);
                }
            }

            if !mix.is_unmodified_full_mix() {
                #[cfg(windows)]
                {
                    let master_pkts = &demuxed.audio_tracks_packets[0].1;
                    let game_pcm = if mix.game_enabled && mix.game_gain > 0.001 {
                        decode_aac_packets_via_mft(&demuxed.audio_tracks_packets[1].1)
                    } else {
                        Vec::new()
                    };
                    let discord_pcm = if mix.discord_enabled && mix.discord_gain > 0.001 {
                        decode_aac_packets_via_mft(&demuxed.audio_tracks_packets[2].1)
                    } else {
                        Vec::new()
                    };
                    let mic_pcm = if mix.mic_enabled && mix.mic_gain > 0.001 {
                        decode_aac_packets_via_mft(&demuxed.audio_tracks_packets[3].1)
                    } else {
                        Vec::new()
                    };
                    let other_pcm = if mix.other_enabled && mix.other_gain > 0.001 {
                        decode_aac_packets_via_mft(&demuxed.audio_tracks_packets[4].1)
                    } else {
                        Vec::new()
                    };
                    let max_samples = game_pcm
                        .len()
                        .max(discord_pcm.len())
                        .max(mic_pcm.len())
                        .max(other_pcm.len());
                    if max_samples >= 1024 {
                        if let Some(mut enc) = crate::audio_engine::build_aac_encoder(128) {
                            let silent_pkt = build_silent_aac_packet(128);
                            let mut pcm_chunk = vec![0i16; 2048];
                            let mut aac_buf = vec![0u8; 2048];
                            let mut out = Vec::new();
                            for (pkt_idx, p) in master_pkts.iter().enumerate() {
                                if p.pts_ms.saturating_add(22) < start_ms {
                                    continue;
                                }
                                if p.pts_ms > end_ms.saturating_add(40) {
                                    break;
                                }
                                let base_sample = pkt_idx * 1024;
                                for s_idx in 0..1024 {
                                    let idx = base_sample + s_idx;
                                    let (gl, gr) = game_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    let (dl, dr) = discord_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    let (ml, mr) = mic_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    let (ol, or_) = other_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    let l = (gl * mix.game_gain
                                        + dl * mix.discord_gain
                                        + ml * mix.mic_gain
                                        + ol * mix.other_gain)
                                        .clamp(-1.0, 1.0);
                                    let r = (gr * mix.game_gain
                                        + dr * mix.discord_gain
                                        + mr * mix.mic_gain
                                        + or_ * mix.other_gain)
                                        .clamp(-1.0, 1.0);
                                    pcm_chunk[s_idx * 2] = (l * 32767.0) as i16;
                                    pcm_chunk[s_idx * 2 + 1] = (r * 32767.0) as i16;
                                }
                                let encoded_bytes = enc
                                    .encode(&pcm_chunk, &mut aac_buf)
                                    .ok()
                                    .filter(|r| r.out_bytes() > 0)
                                    .map(|r| Bytes::copy_from_slice(&aac_buf[..r.out_bytes()]))
                                    .unwrap_or_else(|| silent_pkt.clone());
                                let new_stems = if keep_stems {
                                    p.stems.as_ref().map(|st| {
                                        Arc::new(AudioStemPackets {
                                            game: if mix.game_enabled { st.game.clone() } else { silent_pkt.clone() },
                                            discord: if mix.discord_enabled { st.discord.clone() } else { silent_pkt.clone() },
                                            mic: if mix.mic_enabled { st.mic.clone() } else { silent_pkt.clone() },
                                            other: if mix.other_enabled { st.other.clone() } else { silent_pkt.clone() },
                                        })
                                    })
                                } else {
                                    None
                                };
                                out.push(Arc::new(CompressedPacket {
                                    kind: PacketKind::Audio,
                                    pts_ms: p.pts_ms,
                                    pts: p.pts,
                                    dts: p.dts,
                                    duration: p.duration,
                                    is_keyframe: true,
                                    data: encoded_bytes,
                                    stems: new_stems,
                                }));
                            }
                            if !out.is_empty() {
                                return (out, keep_stems);
                            }
                        }
                    }
                }
                let master_pkts = &demuxed.audio_tracks_packets[0].1;
                let silent_pkt = build_silent_aac_packet(128);
                let mut out = Vec::new();

                for p in master_pkts {
                    if p.pts_ms.saturating_add(22) < start_ms {
                        continue;
                    }
                    if p.pts_ms > end_ms.saturating_add(40) {
                        break;
                    }
                    let chosen_data = if let Some(ref st) = p.stems {
                        let g_on = mix.game_enabled
                            && mix.game_gain > 0.001
                            && st.game.as_ref() != silent_pkt.as_ref();
                        let d_on = mix.discord_enabled
                            && mix.discord_gain > 0.001
                            && st.discord.as_ref() != silent_pkt.as_ref();
                        let m_on = mix.mic_enabled
                            && mix.mic_gain > 0.001
                            && st.mic.as_ref() != silent_pkt.as_ref();
                        let o_on = mix.other_enabled
                            && mix.other_gain > 0.001
                            && st.other.as_ref() != silent_pkt.as_ref();

                        let muted_discord_was_active = !mix.discord_enabled
                            && st.discord.as_ref() != silent_pkt.as_ref();
                        let muted_mic_was_active =
                            !mix.mic_enabled && st.mic.as_ref() != silent_pkt.as_ref();
                        let muted_other_was_active =
                            !mix.other_enabled && st.other.as_ref() != silent_pkt.as_ref();
                        let muted_game_was_active =
                            !mix.game_enabled && st.game.as_ref() != silent_pkt.as_ref();

                        if !g_on && !d_on && !m_on && !o_on {
                            silent_pkt.clone()
                        } else if !muted_discord_was_active
                            && !muted_mic_was_active
                            && !muted_other_was_active
                            && !muted_game_was_active
                        {
                            p.data.clone()
                        } else if g_on && !d_on && !m_on && !o_on {
                            st.game.clone()
                        } else if d_on && !g_on && !m_on && !o_on {
                            st.discord.clone()
                        } else if m_on && !g_on && !d_on && !o_on {
                            st.mic.clone()
                        } else if o_on && !g_on && !d_on && !m_on {
                            st.other.clone()
                        } else if g_on {
                            st.game.clone()
                        } else if d_on {
                            st.discord.clone()
                        } else if m_on {
                            st.mic.clone()
                        } else {
                            st.other.clone()
                        }
                    } else {
                        p.data.clone()
                    };

                    let new_stems = if keep_stems {
                        p.stems.as_ref().map(|st| {
                            Arc::new(AudioStemPackets {
                                game: if mix.game_enabled {
                                    st.game.clone()
                                } else {
                                    silent_pkt.clone()
                                },
                                discord: if mix.discord_enabled {
                                    st.discord.clone()
                                } else {
                                    silent_pkt.clone()
                                },
                                mic: if mix.mic_enabled {
                                    st.mic.clone()
                                } else {
                                    silent_pkt.clone()
                                },
                                other: if mix.other_enabled {
                                    st.other.clone()
                                } else {
                                    silent_pkt.clone()
                                },
                            })
                        })
                    } else {
                        None
                    };

                    out.push(Arc::new(CompressedPacket {
                        kind: PacketKind::Audio,
                        pts_ms: p.pts_ms,
                        pts: p.pts,
                        dts: p.dts,
                        duration: p.duration,
                        is_keyframe: true,
                        data: chosen_data,
                        stems: new_stems,
                    }));
                }
                return (out, keep_stems);
            }
        }
    }

    let master_pkts = &demuxed.audio_tracks_packets[0].1;
    let mut out = Vec::new();
    for p in master_pkts {
        if p.pts_ms.saturating_add(22) < start_ms {
            continue;
        }
        if p.pts_ms > end_ms.saturating_add(40) {
            break;
        }
        out.push(Arc::new(CompressedPacket {
            kind: PacketKind::Audio,
            pts_ms: p.pts_ms,
            pts: p.pts,
            dts: p.dts,
            duration: p.duration,
            is_keyframe: true,
            data: p.data.clone(),
            stems: if keep_stems { p.stems.clone() } else { None },
        }));
    }
    (out, keep_stems)
}

fn locate_ffmpeg_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("ffmpeg.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
            let candidate_bin = dir.join("bin").join("ffmpeg.exe");
            if candidate_bin.is_file() {
                return Some(candidate_bin);
            }
        }
    }
    let local = PathBuf::from("ffmpeg.exe");
    if local.is_file() {
        return Some(local);
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000 | 0x00004000);
    }
    if cmd.status().map(|s| s.success()).unwrap_or(false) {
        return Some(PathBuf::from("ffmpeg"));
    }
    None
}

fn try_ffmpeg_edit_job(
    ffmpeg_bin: &Path,
    job: &EditJob,
    meta: &ClipMetadata,
    dst_w: u32,
    dst_h: u32,
    dst_fps: u32,
    video_kbps: u32,
    temp_output: &Path,
) -> Result<String, String> {
    let trim_off_keyframe = start_ms > 0
        && !meta
            .keyframe_timestamps_ms
            .iter()
            .any(|&kf| kf.abs_diff(start_ms) <= 150);

    let need_reencode_video = job.cut_mode == CutMode::FrameExact
        || (job.cut_mode == CutMode::FastKeyframeCopy && trim_off_keyframe)
        || dst_w != meta.width
        || dst_h != meta.height
        || dst_fps != meta.fps
        || job.video_bitrate_kbps.is_some();

    let encoders: Vec<&str> = if !need_reencode_video {
        vec!["copy"]
    } else if job.prefer_hardware {
        vec!["h264_nvenc", "h264_amf", "h264_qsv", "libx264"]
    } else {
        vec!["libx264"]
    };

    for enc_name in encoders {
        let _ = std::fs::remove_file(temp_output);
        let mut cmd = Command::new(ffmpeg_bin);
        cmd.arg("-y").arg("-hide_banner").arg("-loglevel").arg("error");

        if let Some(s_ms) = job.trim_start_ms {
            if s_ms > 0 {
                cmd.arg("-ss").arg(format!("{:.3}", s_ms as f64 / 1000.0));
            }
        }
        if let Some(e_ms) = job.trim_end_ms {
            cmd.arg("-to").arg(format!("{:.3}", e_ms as f64 / 1000.0));
        }

        cmd.arg("-i").arg(&job.input_path);

        if enc_name == "copy" {
            cmd.arg("-map").arg("0:v:0").arg("-c:v").arg("copy");
        } else {
            let vf = format!("scale={dst_w}:{dst_h}:flags=fast_bilinear,fps={dst_fps}");
            cmd.arg("-map")
                .arg("0:v:0")
                .arg("-vf")
                .arg(vf)
                .arg("-c:v")
                .arg(enc_name)
                .arg("-b:v")
                .arg(format!("{video_kbps}k"))
                .arg("-pix_fmt")
                .arg("yuv420p");
        }

        if job.mute_all_audio
            || job
                .audio_mix
                .as_ref()
                .map(|m| m.is_all_muted())
                .unwrap_or(false)
        {
            cmd.arg("-an");
        } else if let Some(ref mix) = job.audio_mix {
            if meta.has_separated_stems && !mix.is_unmodified_full_mix() {
                if let Some((solo_role, gain)) = mix.single_solo_role() {
                    let a_idx = match solo_role {
                        AudioTrackRole::MasterMix => 0,
                        AudioTrackRole::Game => 1,
                        AudioTrackRole::Discord => 2,
                        AudioTrackRole::Microphone => 3,
                        AudioTrackRole::OtherApps => 4,
                    };
                    if (gain - 1.0).abs() < 0.01 {
                        cmd.arg("-map")
                            .arg(format!("0:a:{a_idx}"))
                            .arg("-c:a")
                            .arg("copy");
                    } else {
                        cmd.arg("-map")
                            .arg(format!("0:a:{a_idx}"))
                            .arg("-af")
                            .arg(format!("volume={gain:.3}"))
                            .arg("-c:a")
                            .arg("aac")
                            .arg("-b:a")
                            .arg("128k");
                    }
                } else {
                    let mut filter_parts = Vec::new();
                    let mut mix_labels = Vec::new();
                    let stem_cfgs = [
                        (1usize, mix.game_enabled, mix.game_gain, "g"),
                        (2usize, mix.discord_enabled, mix.discord_gain, "d"),
                        (3usize, mix.mic_enabled, mix.mic_gain, "m"),
                        (4usize, mix.other_enabled, mix.other_gain, "o"),
                    ];
                    for &(idx, enabled, gain, tag) in &stem_cfgs {
                        if enabled && gain > 0.001 {
                            filter_parts.push(format!("[0:a:{idx}]volume={gain:.3}[{tag}]"));
                            mix_labels.push(format!("[{tag}]"));
                        }
                    }
                    if !mix_labels.is_empty() {
                        let inputs_str = mix_labels.join("");
                        let n_in = mix_labels.len();
                        let fc = format!(
                            "{};{inputs_str}amix=inputs={n_in}:duration=longest:normalize=0[aout]",
                            filter_parts.join(";")
                        );
                        cmd.arg("-filter_complex")
                            .arg(fc)
                            .arg("-map")
                            .arg("[aout]")
                            .arg("-c:a")
                            .arg("aac")
                            .arg("-b:a")
                            .arg("128k");
                    } else {
                        cmd.arg("-an");
                    }
                }
            } else {
                cmd.arg("-map").arg("0:a:0?").arg("-c:a").arg("copy");
            }
        } else {
            cmd.arg("-map").arg("0:a:0?").arg("-c:a").arg("copy");
        }

        cmd.arg("-movflags").arg("+faststart").arg(temp_output);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000 | 0x00004000);
        }

        if let Ok(st) = cmd.status() {
            if st.success() {
                if let Ok(m) = std::fs::metadata(temp_output) {
                    if m.len() > 512 {
                        return Ok(format!("FFmpeg ({enc_name})"));
                    }
                }
            }
        }
    }

    Err("FFmpeg edit pipeline not available or failed".to_string())
}

fn avcc_to_annex_b(
    sample_bytes: &[u8],
    is_sync: bool,
    sps: &[u8],
    pps: &[u8],
    out: &mut Vec<u8>,
) {
    out.clear();
    if is_sync && !sps.is_empty() && !pps.is_empty() {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(sps);
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(pps);
    }

    if sample_bytes.len() >= 4
        && ((sample_bytes[0] == 0
            && sample_bytes[1] == 0
            && sample_bytes[2] == 0
            && sample_bytes[3] == 1)
            || (sample_bytes[0] == 0 && sample_bytes[1] == 0 && sample_bytes[2] == 1))
    {
        out.extend_from_slice(sample_bytes);
        return;
    }

    let mut offset = 0usize;
    while offset + 4 <= sample_bytes.len() {
        let nal_len = u32::from_be_bytes([
            sample_bytes[offset],
            sample_bytes[offset + 1],
            sample_bytes[offset + 2],
            sample_bytes[offset + 3],
        ]) as usize;
        offset += 4;
        if nal_len == 0 || offset + nal_len > sample_bytes.len() {
            break;
        }
        let nal_type = sample_bytes[offset] & 0x1F;
        if nal_type != 9 {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&sample_bytes[offset..offset + nal_len]);
        }
        offset += nal_len;
    }
}

fn i420_to_scaled_nv12(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    src_h: usize,
    y_stride: usize,
    u_stride: usize,
    v_stride: usize,
    dst_w: usize,
    dst_h: usize,
    x_map_y: &[usize],
    x_map_uv: &[usize],
    dst_nv12: &mut [u8],
) {
    for y in 0..dst_h {
        let sy = (y * src_h) / dst_h.max(1);
        let s_row = sy * y_stride;
        let d_row = y * dst_w;
        for (dx, &sx) in x_map_y.iter().enumerate() {
            if s_row + sx < y_plane.len() {
                dst_nv12[d_row + dx] = y_plane[s_row + sx];
            }
        }
    }

    let dst_uv_base = dst_w * dst_h;
    let src_uv_h = (src_h / 2).max(1);
    let dst_uv_h = (dst_h / 2).max(1);
    for y in 0..dst_uv_h {
        let sy = (y * src_uv_h) / dst_uv_h;
        let u_row = sy * u_stride;
        let v_row = sy * v_stride;
        let d_row = dst_uv_base + y * dst_w;
        for (uv_idx, &sx_pair) in x_map_uv.iter().enumerate() {
            let su_x = sx_pair / 2;
            let dx = uv_idx * 2;
            if u_row + su_x < u_plane.len() && v_row + su_x < v_plane.len() {
                dst_nv12[d_row + dx] = u_plane[u_row + su_x];
                dst_nv12[d_row + dx + 1] = v_plane[v_row + su_x];
            }
        }
    }
}

#[cfg(windows)]
fn reencode_video_native_mft(
    video_packets: &[Arc<CompressedPacket>],
    src_w: u32,
    _src_h: u32,
    sps: &[u8],
    pps: &[u8],
    start_ms: u64,
    end_ms: u64,
    dst_w: u32,
    dst_h: u32,
    dst_fps: u32,
    video_bitrate_bps: u32,
) -> Result<(Vec<Arc<CompressedPacket>>, CodecContextInfo, String), String> {
    let dur_span_ms = end_ms.saturating_sub(start_ms).max(1000);
    let out_ring = RingBuffer::new_with_capacities(
        Duration::from_millis(dur_span_ms + 10_000),
        video_packets.len().max(128),
        64,
    );
    let out_codec_info: Arc<Mutex<Option<CodecContextInfo>>> = Arc::new(Mutex::new(None));

    let mut encoder = crate::recorder::capture::HardwareH264Encoder::new_standalone(
        dst_w,
        dst_h,
        dst_fps,
        video_bitrate_bps,
        (dst_fps * 2).max(30),
        start_ms,
        &out_codec_info,
    )?;

    let backend_label = format!("NativeMFT ({:?})", encoder.backend_kind);
    let x_map_y: Vec<usize> = (0..dst_w as usize)
        .map(|dx| (dx * src_w as usize) / dst_w.max(1) as usize)
        .collect();
    let x_map_uv: Vec<usize> = (0..(dst_w as usize / 2))
        .map(|uv_dx| ((uv_dx * (src_w as usize / 2)) / (dst_w.max(2) as usize / 2)) * 2)
        .collect();

    let mut dst_nv12 = vec![0u8; ((dst_w * dst_h * 3) / 2) as usize];
    let mut annex_b_buf = Vec::with_capacity(256 * 1024);
    let mut avcc_scratch = Vec::with_capacity(256 * 1024);

    let min_step_ms = (1000 / dst_fps.max(1)).saturating_sub(2) as u64;
    let frame_dur_hns = (10_000_000 / dst_fps.max(1) as i64).max(1);
    let mut next_keep_ms = start_ms;
    let mut encoded_count = 0usize;

    let mut sw_dec = OpenH264Decoder::new()
        .map_err(|e| format!("OpenH264 decoder init failed: {e}"))?;

    for pkt in video_packets {
        if pkt.pts_ms > end_ms {
            break;
        }
        avcc_to_annex_b(&pkt.data, pkt.is_keyframe, sps, pps, &mut annex_b_buf);
        if annex_b_buf.is_empty() {
            continue;
        }
        if let Ok(Some(yuv)) = sw_dec.decode(&annex_b_buf) {
            if pkt.pts_ms >= start_ms && (pkt.pts_ms >= next_keep_ms || encoded_count == 0) {
                let (_, h) = yuv.dimensions();
                let (ys, us, vs) = yuv.strides();
                i420_to_scaled_nv12(
                    yuv.y(),
                    yuv.u(),
                    yuv.v(),
                    h,
                    ys,
                    us,
                    vs,
                    dst_w as usize,
                    dst_h as usize,
                    &x_map_y,
                    &x_map_uv,
                    &mut dst_nv12,
                );
                let rel_hns = (pkt.pts_ms.saturating_sub(start_ms) as i64) * 10_000;
                let force_idr = encoded_count == 0;
                let _ = encoder.encode_nv12_bytes(
                    &dst_nv12,
                    pkt.pts_ms,
                    rel_hns,
                    frame_dur_hns,
                    force_idr,
                    &mut avcc_scratch,
                    &out_ring,
                    &out_codec_info,
                );
                encoded_count += 1;
                next_keep_ms = pkt.pts_ms.saturating_add(min_step_ms);
            }
        }
    }

    encoder.drain_output_packets(end_ms, &mut avcc_scratch, &out_ring, &out_codec_info);
    let (v_pkts, _) = out_ring.snapshot_since(Some(0));
    if v_pkts.is_empty() {
        return Err("native video re-encode produced 0 packets".to_string());
    }
    let info = out_codec_info
        .lock()
        .map_err(|_| "codec_info lock poisoned".to_string())?
        .clone()
        .ok_or_else(|| "missing SPS/PPS headers after native re-encode".to_string())?;

    Ok((v_pkts, info, backend_label))
}

#[cfg(not(windows))]
fn reencode_video_native_mft(
    _video_packets: &[Arc<CompressedPacket>],
    _src_w: u32,
    _src_h: u32,
    _sps: &[u8],
    _pps: &[u8],
    _start_ms: u64,
    _end_ms: u64,
    _dst_w: u32,
    _dst_h: u32,
    _dst_fps: u32,
    _video_bitrate_bps: u32,
) -> Result<(Vec<Arc<CompressedPacket>>, CodecContextInfo, String), String> {
    Err("native MFT video re-encoding requires Windows".to_string())
}

pub fn run_edit_job(job: EditJob) -> Result<EditResult, String> {
    priority::set_low_priority();
    let started = Instant::now();

    let demuxed = demux_clip_full(&job.input_path)?;
    let meta = &demuxed.metadata;
    let start_ms = job.trim_start_ms.unwrap_or(0);
    let end_ms = job
        .trim_end_ms
        .unwrap_or(meta.duration_ms.max(1))
        .max(start_ms.saturating_add(100));

    let (dst_w, dst_h) = job
        .rescale
        .map(|(w, h)| ((w.clamp(64, 7680)) & !1, (h.clamp(64, 4320)) & !1))
        .unwrap_or((meta.width, meta.height));
    let dst_fps = job.target_fps.unwrap_or(meta.fps).clamp(15, 240);
    let default_v_kbps = if dst_h <= 480 {
        2500
    } else if dst_h <= 720 {
        5000
    } else if dst_h <= 1080 {
        9000
    } else {
        14000
    };
    let video_kbps = job.video_bitrate_kbps.unwrap_or(default_v_kbps).max(250);

    let out_path = job
        .output_path
        .clone()
        .unwrap_or_else(|| default_edited_output_path(&job.input_path, "edited"));

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    let trim_off_keyframe = start_ms > 0
        && !meta
            .keyframe_timestamps_ms
            .iter()
            .any(|&kf| kf.abs_diff(start_ms) <= 150);

    let need_reencode_video = job.cut_mode == CutMode::FrameExact
        || (job.cut_mode == CutMode::FastKeyframeCopy && trim_off_keyframe)
        || dst_w != meta.width
        || dst_h != meta.height
        || dst_fps != meta.fps
        || job.video_bitrate_kbps.is_some();

    let need_custom_gain_mix = job
        .audio_mix
        .as_ref()
        .map(|m| {
            !m.is_unmodified_full_mix()
                && !m.is_all_muted()
                && m.single_solo_role()
                    .map(|(_, g)| (g - 1.0).abs() > 0.01)
                    .unwrap_or(true)
        })
        .unwrap_or(false);

    if need_reencode_video || need_custom_gain_mix {
        if let Some(ffmpeg_bin) = locate_ffmpeg_binary() {
            let temp_out = out_path.with_extension("edit_ff_tmp.mp4");
            if let Ok(backend_name) = try_ffmpeg_edit_job(
                &ffmpeg_bin,
                &job,
                meta,
                dst_w,
                dst_h,
                dst_fps,
                video_kbps,
                &temp_out,
            ) {
                let out_bytes = std::fs::metadata(&temp_out).map(|m| m.len()).unwrap_or(0);
                if out_path.exists() {
                    let _ = std::fs::remove_file(&out_path);
                }
                std::fs::rename(&temp_out, &out_path)
                    .map_err(|e| format!("failed to finalize edited clip: {e}"))?;
                let out_dur = end_ms.saturating_sub(start_ms);
                return Ok(EditResult {
                    input_path: job.input_path,
                    output_path: out_path,
                    input_bytes: meta.file_size_bytes,
                    output_bytes: out_bytes,
                    duration_ms: out_dur,
                    width: dst_w,
                    height: dst_h,
                    fps: dst_fps,
                    audio_tracks_written: if job.mute_all_audio { 0 } else { 1 },
                    backend: backend_name,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
            }
            let _ = std::fs::remove_file(&temp_out);
        }
    }

    let (v_slice, actual_start_ms, actual_end_ms) =
        filter_video_slice_for_trim(&demuxed.video_packets, start_ms, end_ms);

    if v_slice.is_empty() {
        return Err("no video frames found inside the requested trim window".to_string());
    }

    let (final_v_pkts, final_info, backend_label) = if need_reencode_video {
        reencode_video_native_mft(
            &v_slice,
            meta.width,
            meta.height,
            &demuxed.sps,
            &demuxed.pps,
            start_ms,
            end_ms,
            dst_w,
            dst_h,
            dst_fps,
            video_kbps * 1000,
        )?
    } else {
        let info = CodecContextInfo {
            width: meta.width,
            height: meta.height,
            fps: meta.fps,
            sps: demuxed.sps.clone(),
            pps: demuxed.pps.clone(),
        };
        (
            v_slice,
            info,
            "NativeStreamCopy (Zero-Reencode)".to_string(),
        )
    };

    let a_win_start = if need_reencode_video {
        start_ms
    } else {
        actual_start_ms
    };
    let a_win_end = if need_reencode_video {
        end_ms
    } else {
        actual_end_ms
    };

    let (final_a_pkts, keep_stems_in_mux) = build_remixed_audio_packets(
        &demuxed,
        a_win_start,
        a_win_end,
        job.audio_mix.as_ref(),
        job.mute_all_audio,
    );

    let mut mux_cfg = RecorderConfig::default();
    mux_cfg.separate_audio_tracks = keep_stems_in_mux;

    EncoderPipeline::mux_clip(&final_v_pkts, &final_a_pkts, &out_path, &mux_cfg, &final_info)?;

    let output_bytes = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    let out_dur = a_win_end.saturating_sub(a_win_start);
    let tracks_written = if final_a_pkts.is_empty() {
        0
    } else if keep_stems_in_mux {
        5
    } else {
        1
    };

    Ok(EditResult {
        input_path: job.input_path,
        output_path: out_path,
        input_bytes: meta.file_size_bytes,
        output_bytes,
        duration_ms: out_dur,
        width: final_info.width,
        height: final_info.height,
        fps: final_info.fps,
        audio_tracks_written: tracks_written,
        backend: backend_label,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn cut_clip(
    input_path: impl AsRef<Path>,
    start_ms: u64,
    end_ms: u64,
    mode: CutMode,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    let in_p = input_path.as_ref().to_path_buf();
    let out_p = output_path.or_else(|| Some(default_edited_output_path(&in_p, "cut")));
    run_edit_job(EditJob {
        input_path: in_p,
        output_path: out_p,
        trim_start_ms: Some(start_ms),
        trim_end_ms: Some(end_ms),
        cut_mode: mode,
        prefer_hardware: true,
        ..EditJob::default()
    })
}

pub fn split_clip(
    input_path: impl AsRef<Path>,
    split_at_ms: u64,
    mode: CutMode,
    output_dir: Option<&Path>,
) -> Result<SplitResult, String> {
    split_multi_clip(input_path, &[split_at_ms], mode, output_dir)
}

pub fn split_multi_clip(
    input_path: impl AsRef<Path>,
    split_points_ms: &[u64],
    mode: CutMode,
    output_dir: Option<&Path>,
) -> Result<SplitResult, String> {
    let started = Instant::now();
    let in_p = input_path.as_ref().to_path_buf();
    let meta = probe_clip(&in_p)?;
    if meta.duration_ms < 200 {
        return Err("clip is too short to split".to_string());
    }

    let mut sorted_pts: Vec<u64> = split_points_ms
        .iter()
        .copied()
        .filter(|&t| t > 100 && t + 100 < meta.duration_ms)
        .collect();
    sorted_pts.sort_unstable();
    sorted_pts.dedup();

    if sorted_pts.is_empty() {
        return Err(format!(
            "split timestamp must be between 100ms and {}ms",
            meta.duration_ms.saturating_sub(100)
        ));
    }

    let parent = output_dir
        .map(Path::to_path_buf)
        .or_else(|| in_p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&parent);
    let stem = in_p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");

    let mut boundaries = Vec::with_capacity(sorted_pts.len() + 2);
    boundaries.push(0u64);
    boundaries.extend(sorted_pts);
    boundaries.push(meta.duration_ms);

    let mut parts = Vec::with_capacity(boundaries.len() - 1);
    for idx in 0..(boundaries.len() - 1) {
        let seg_start = boundaries[idx];
        let seg_end = boundaries[idx + 1];
        let ext = in_p
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| if e.eq_ignore_ascii_case("mkv") { "mkv" } else { "mp4" })
            .unwrap_or("mp4");
        let part_path = parent.join(format!("{stem}_part{}.{ext}", idx + 1));
        let effective_mode = if mode == CutMode::KeyframeSnap {
            CutMode::KeyframeSnap
        } else {
            CutMode::FrameExact
        };
        let res = cut_clip(&in_p, seg_start, seg_end, effective_mode, Some(part_path))?;
        parts.push(res);
    }

    Ok(SplitResult {
        input_path: in_p,
        parts,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn remove_middle_segment(
    input_path: impl AsRef<Path>,
    remove_start_ms: u64,
    remove_end_ms: u64,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    priority::set_low_priority();
    let started = Instant::now();
    let in_p = input_path.as_ref().to_path_buf();
    let demuxed = demux_clip_full(&in_p)?;
    let meta = &demuxed.metadata;

    if remove_start_ms >= remove_end_ms || remove_end_ms >= meta.duration_ms {
        return Err("invalid middle segment bounds to remove".to_string());
    }

    let gap_removed_ms = remove_end_ms.saturating_sub(remove_start_ms);
    let (v_part1, _, _) =
        filter_video_slice_for_trim(&demuxed.video_packets, 0, remove_start_ms);
    let (v_part2_raw, actual_p2_start, _) =
        filter_video_slice_for_trim(&demuxed.video_packets, remove_end_ms, meta.duration_ms);

    let shift_ms = actual_p2_start.saturating_sub(remove_start_ms).max(gap_removed_ms);
    let mut joined_v = Vec::with_capacity(v_part1.len() + v_part2_raw.len());
    joined_v.extend(v_part1);
    for p in v_part2_raw {
        let shifted_pts = p.pts_ms.saturating_sub(shift_ms);
        joined_v.push(Arc::new(CompressedPacket {
            kind: PacketKind::Video,
            pts_ms: shifted_pts,
            pts: shifted_pts as i64,
            dts: shifted_pts as i64,
            duration: p.duration,
            is_keyframe: p.is_keyframe,
            data: p.data.clone(),
            stems: None,
        }));
    }

    let (a_part1, _) =
        build_remixed_audio_packets(&demuxed, 0, remove_start_ms, None, false);
    let (a_part2_raw, _) = build_remixed_audio_packets(
        &demuxed,
        actual_p2_start,
        meta.duration_ms,
        None,
        false,
    );

    let mut joined_a = Vec::with_capacity(a_part1.len() + a_part2_raw.len());
    joined_a.extend(a_part1);
    for p in a_part2_raw {
        let shifted_pts = p.pts_ms.saturating_sub(shift_ms);
        joined_a.push(Arc::new(CompressedPacket {
            kind: PacketKind::Audio,
            pts_ms: shifted_pts,
            pts: shifted_pts as i64,
            dts: shifted_pts as i64,
            duration: p.duration,
            is_keyframe: true,
            data: p.data.clone(),
            stems: None,
        }));
    }

    let out_p = output_path.unwrap_or_else(|| default_edited_output_path(&in_p, "spliced"));
    let info = CodecContextInfo {
        width: meta.width,
        height: meta.height,
        fps: meta.fps,
        sps: demuxed.sps,
        pps: demuxed.pps,
    };
    let mut cfg = RecorderConfig::default();
    cfg.separate_audio_tracks = false;
    EncoderPipeline::mux_clip(&joined_v, &joined_a, &out_p, &cfg, &info)?;

    let out_bytes = std::fs::metadata(&out_p).map(|m| m.len()).unwrap_or(0);
    Ok(EditResult {
        input_path: in_p,
        output_path: out_p,
        input_bytes: meta.file_size_bytes,
        output_bytes: out_bytes,
        duration_ms: meta.duration_ms.saturating_sub(shift_ms),
        width: meta.width,
        height: meta.height,
        fps: meta.fps,
        audio_tracks_written: if joined_a.is_empty() { 0 } else { 1 },
        backend: "NativeSplice (Zero-Reencode)".to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn rescale_clip(
    input_path: impl AsRef<Path>,
    preset: ResolutionPreset,
    target_fps: Option<u32>,
    video_bitrate_kbps: Option<u32>,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    let in_p = input_path.as_ref().to_path_buf();
    let meta = probe_clip(&in_p)?;
    let (dst_w, dst_h) = preset.resolve_dimensions(meta.width, meta.height);
    let suffix = format!("{dst_w}x{dst_h}");
    let out_p = output_path.or_else(|| Some(default_edited_output_path(&in_p, &suffix)));

    run_edit_job(EditJob {
        input_path: in_p,
        output_path: out_p,
        rescale: Some((dst_w, dst_h)),
        target_fps,
        video_bitrate_kbps,
        prefer_hardware: true,
        ..EditJob::default()
    })
}

pub fn remix_clip_audio(
    input_path: impl AsRef<Path>,
    mix_config: AudioStemMixConfig,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    let in_p = input_path.as_ref().to_path_buf();
    let suffix = if mix_config.is_all_muted() {
        "muted"
    } else if let Some((role, _)) = mix_config.single_solo_role() {
        role.short_suffix()
    } else if !mix_config.discord_enabled {
        "no_discord"
    } else if !mix_config.mic_enabled {
        "no_mic"
    } else if !mix_config.other_enabled {
        "no_other"
    } else {
        "remixed"
    };
    let out_p = output_path.or_else(|| Some(default_edited_output_path(&in_p, suffix)));

    run_edit_job(EditJob {
        input_path: in_p,
        output_path: out_p,
        audio_mix: Some(mix_config),
        prefer_hardware: true,
        ..EditJob::default()
    })
}

pub fn isolate_single_stem(
    input_path: impl AsRef<Path>,
    role: AudioTrackRole,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    let mix = match role {
        AudioTrackRole::MasterMix => AudioStemMixConfig::default(),
        AudioTrackRole::Game => AudioStemMixConfig::game_only(),
        AudioTrackRole::Discord => AudioStemMixConfig::discord_only(),
        AudioTrackRole::Microphone => AudioStemMixConfig::mic_only(),
        AudioTrackRole::OtherApps => AudioStemMixConfig::other_only(),
    };
    remix_clip_audio(input_path, mix, output_path)
}

fn build_adts_header(aac_frame_len: usize) -> [u8; 7] {
    let full_len = (aac_frame_len + 7) & 0x1FFF;
    let profile_aac_lc = 1u8;
    let freq_idx_48k = 3u8;
    let chan_stereo = 2u8;
    let b0 = 0xFF;
    let b1 = 0xF1;
    let b2 = (profile_aac_lc << 6) | (freq_idx_48k << 2) | (chan_stereo >> 2);
    let b3 = ((chan_stereo & 3) << 6) | ((full_len >> 11) as u8 & 0x03);
    let b4 = ((full_len >> 3) & 0xFF) as u8;
    let b5 = (((full_len & 0x07) as u8) << 5) | 0x1F;
    let b6 = 0xFC;
    [b0, b1, b2, b3, b4, b5, b6]
}

fn write_single_audio_m4a(
    packets: &[Arc<CompressedPacket>],
    output_path: &Path,
) -> Result<u64, String> {
    let file = File::create(output_path)
        .map_err(|e| format!("failed to create audio file '{}': {e}", output_path.display()))?;
    let buf_writer = BufWriter::with_capacity(512 * 1024, file);

    let brand_m4a = "M4A ".parse().map_err(|e| format!("{e}"))?;
    let brand_isom = "isom".parse().map_err(|e| format!("{e}"))?;
    let brand_iso2 = "iso2".parse().map_err(|e| format!("{e}"))?;
    let brand_mp41 = "mp41".parse().map_err(|e| format!("{e}"))?;

    let mp4_cfg = Mp4Config {
        major_brand: brand_isom,
        minor_version: 512,
        compatible_brands: vec![brand_m4a, brand_isom, brand_iso2, brand_mp41],
        timescale: 48_000,
    };

    let mut writer = Mp4Writer::write_start(buf_writer, &mp4_cfg)
        .map_err(|e| format!("failed to start m4a writer: {e}"))?;

    let aac_conf = AacConfig {
        bitrate: 128_000,
        profile: AudioObjectType::AacLowComplexity,
        freq_index: SampleFreqIndex::Freq48000,
        chan_conf: ChannelConfig::Stereo,
    };
    let mut track_conf = TrackConfig::from(aac_conf);
    track_conf.timescale = 48_000;
    writer
        .add_track(&track_conf)
        .map_err(|e| format!("failed to add m4a track: {e}"))?;

    let mut cur_ticks = 0u64;
    for p in packets {
        let sample = Mp4Sample {
            start_time: cur_ticks,
            duration: 1024,
            rendering_offset: 0,
            is_sync: true,
            bytes: p.data.clone(),
        };
        writer
            .write_sample(1, &sample)
            .map_err(|e| format!("failed to write m4a sample: {e}"))?;
        cur_ticks += 1024;
    }

    writer
        .write_end()
        .map_err(|e| format!("failed to finalize m4a: {e}"))?;
    let mut bw = writer.into_writer();
    let _ = bw.flush();
    drop(bw);

    let sz = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    Ok(sz)
}

fn write_single_audio_adts_aac(
    packets: &[Arc<CompressedPacket>],
    output_path: &Path,
) -> Result<u64, String> {
    let file = File::create(output_path)
        .map_err(|e| format!("failed to create AAC file '{}': {e}", output_path.display()))?;
    let mut bw = BufWriter::with_capacity(512 * 1024, file);
    for p in packets {
        if p.data.is_empty() {
            continue;
        }
        let hdr = build_adts_header(p.data.len());
        bw.write_all(&hdr)
            .map_err(|e| format!("failed to write ADTS header: {e}"))?;
        bw.write_all(&p.data)
            .map_err(|e| format!("failed to write AAC frame: {e}"))?;
    }
    bw.flush()
        .map_err(|e| format!("failed to flush AAC file: {e}"))?;
    drop(bw);
    let sz = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    Ok(sz)
}

pub fn export_separated_stems(
    input_path: impl AsRef<Path>,
    output_dir: Option<&Path>,
) -> Result<StemExportResult, String> {
    priority::set_low_priority();
    let started = Instant::now();
    let in_p = input_path.as_ref().to_path_buf();
    let demuxed = demux_clip_full(&in_p)?;

    if demuxed.audio_tracks_packets.is_empty() {
        return Err("clip contains no audio tracks to export".to_string());
    }

    let parent = output_dir
        .map(Path::to_path_buf)
        .or_else(|| in_p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&parent);

    let file_stem = in_p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");

    let mut exported = Vec::with_capacity(demuxed.audio_tracks_packets.len());
    for (meta, pkts) in &demuxed.audio_tracks_packets {
        if pkts.is_empty() {
            continue;
        }
        let suffix = meta.role.short_suffix();
        let m4a_path = parent.join(format!("{file_stem}_{suffix}.m4a"));
        let aac_path = parent.join(format!("{file_stem}_{suffix}.aac"));

        let m4a_bytes = write_single_audio_m4a(pkts, &m4a_path)?;
        let _ = write_single_audio_adts_aac(pkts, &aac_path)?;

        exported.push(ExportedStemFile {
            role: meta.role,
            label: meta.label.clone(),
            m4a_path,
            aac_path,
            bytes: m4a_bytes,
            duration_ms: meta.duration_ms,
        });
    }

    Ok(StemExportResult {
        input_path: in_p,
        stems: exported,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub async fn run_edit_job_async(
    job: EditJob,
    sink: Option<EventSink>,
) -> Result<EditResult, String> {
    let in_path_str = job.input_path.to_string_lossy().to_string();
    if let Some(ref s) = sink {
        s.emit(
            "editor.job_started",
            None,
            json!({
                "input_path": in_path_str,
                "trim_start_ms": job.trim_start_ms,
                "trim_end_ms": job.trim_end_ms,
                "rescale": job.rescale,
                "target_fps": job.target_fps,
            }),
        );
    }

    let res = tokio::task::spawn_blocking(move || run_edit_job(job)).await;
    match res {
        Ok(Ok(info)) => {
            if let Some(ref s) = sink {
                s.emit(
                    "editor.job_done",
                    None,
                    serde_json::to_value(&info).unwrap_or_else(|_| json!({})),
                );
            }
            Ok(info)
        }
        Ok(Err(e)) => {
            if let Some(ref s) = sink {
                s.emit(
                    "editor.error",
                    None,
                    json!({
                        "input_path": in_path_str,
                        "message": e,
                    }),
                );
            }
            Err(e)
        }
        Err(join_err) => {
            let msg = format!("editor worker panicked: {join_err}");
            if let Some(ref s) = sink {
                s.emit(
                    "editor.error",
                    None,
                    json!({
                        "input_path": in_path_str,
                        "message": msg,
                    }),
                );
            }
            Err(msg)
        }
    }
}

pub fn handle_editor_event(event: &Event, sink: &Option<EventSink>) {
    let d = &event.data;
    let Some(path_str) = d
        .get("path")
        .or_else(|| d.get("input_path"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        if let Some(ref s) = sink {
            s.emit(
                "editor.error",
                None,
                json!({ "message": "editor event missing 'path' field" }),
            );
        }
        return;
    };

    let input_path = PathBuf::from(path_str);
    let output_path = d
        .get("output_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let sink_clone = sink.clone();
    let kind = event.kind.clone();
    let data_clone = d.clone();

    tokio::spawn(async move {
        match kind.as_str() {
            "editor.probe" => {
                let res = tokio::task::spawn_blocking(move || probe_clip(input_path)).await;
                match res {
                    Ok(Ok(meta)) => {
                        if let Some(ref s) = sink_clone {
                            s.emit(
                                "editor.probe_result",
                                None,
                                serde_json::to_value(&meta).unwrap_or_else(|_| json!({})),
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        if let Some(ref s) = sink_clone {
                            s.emit("editor.error", None, json!({ "message": e }));
                        }
                    }
                    Err(e) => {
                        if let Some(ref s) = sink_clone {
                            s.emit("editor.error", None, json!({ "message": format!("{e}") }));
                        }
                    }
                }
            }
            "editor.cut" | "editor.trim" => {
                let start_ms = data_clone
                    .get("start_ms")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        data_clone
                            .get("start")
                            .and_then(Value::as_str)
                            .and_then(parse_timestamp_ms)
                    })
                    .unwrap_or(0);
                let end_ms = data_clone
                    .get("end_ms")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        data_clone
                            .get("end")
                            .and_then(Value::as_str)
                            .and_then(parse_timestamp_ms)
                    });
                let exact = data_clone
                    .get("exact")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let job = EditJob {
                    input_path,
                    output_path,
                    trim_start_ms: Some(start_ms),
                    trim_end_ms: end_ms,
                    cut_mode: if exact {
                        CutMode::FrameExact
                    } else {
                        CutMode::FastKeyframeCopy
                    },
                    prefer_hardware: true,
                    ..EditJob::default()
                };
                let _ = run_edit_job_async(job, sink_clone).await;
            }
            "editor.split" => {
                let split_ms = data_clone
                    .get("at_ms")
                    .or_else(|| data_clone.get("split_at_ms"))
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        data_clone
                            .get("at")
                            .and_then(Value::as_str)
                            .and_then(parse_timestamp_ms)
                    })
                    .unwrap_or(5000);
                let res = tokio::task::spawn_blocking(move || {
                    split_clip(
                        input_path,
                        split_ms,
                        CutMode::FastKeyframeCopy,
                        output_path.as_deref(),
                    )
                })
                .await;
                if let Some(ref s) = sink_clone {
                    match res {
                        Ok(Ok(info)) => s.emit(
                            "editor.split_done",
                            None,
                            serde_json::to_value(&info).unwrap_or_else(|_| json!({})),
                        ),
                        Ok(Err(e)) => s.emit("editor.error", None, json!({ "message": e })),
                        Err(e) => {
                            s.emit("editor.error", None, json!({ "message": format!("{e}") }))
                        }
                    }
                }
            }
            "editor.rescale" => {
                let preset = data_clone
                    .get("resolution")
                    .or_else(|| data_clone.get("preset"))
                    .and_then(Value::as_str)
                    .and_then(ResolutionPreset::parse_str)
                    .unwrap_or(ResolutionPreset::P720);
                let fps = data_clone
                    .get("fps")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                let bitrate = data_clone
                    .get("bitrate_kbps")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                let res = tokio::task::spawn_blocking(move || {
                    rescale_clip(input_path, preset, fps, bitrate, output_path)
                })
                .await;
                if let Some(ref s) = sink_clone {
                    match res {
                        Ok(Ok(info)) => s.emit(
                            "editor.job_done",
                            None,
                            serde_json::to_value(&info).unwrap_or_else(|_| json!({})),
                        ),
                        Ok(Err(e)) => s.emit("editor.error", None, json!({ "message": e })),
                        Err(e) => {
                            s.emit("editor.error", None, json!({ "message": format!("{e}") }))
                        }
                    }
                }
            }
            "editor.remix_audio" | "editor.separate_audio" => {
                let mix = AudioStemMixConfig {
                    game_enabled: data_clone
                        .get("game_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    discord_enabled: data_clone
                        .get("discord_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    mic_enabled: data_clone
                        .get("mic_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    other_enabled: data_clone
                        .get("other_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    game_gain: data_clone
                        .get("game_gain")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0) as f32,
                    discord_gain: data_clone
                        .get("discord_gain")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0) as f32,
                    mic_gain: data_clone
                        .get("mic_gain")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0) as f32,
                    other_gain: data_clone
                        .get("other_gain")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0) as f32,
                    keep_stem_tracks_in_output: data_clone
                        .get("keep_stems")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                let job = EditJob {
                    input_path,
                    output_path,
                    audio_mix: Some(mix),
                    prefer_hardware: true,
                    ..EditJob::default()
                };
                let _ = run_edit_job_async(job, sink_clone).await;
            }
            "editor.export_stems" => {
                let res = tokio::task::spawn_blocking(move || {
                    export_separated_stems(input_path, output_path.as_deref())
                })
                .await;
                if let Some(ref s) = sink_clone {
                    match res {
                        Ok(Ok(info)) => s.emit(
                            "editor.stems_exported",
                            None,
                            serde_json::to_value(&info).unwrap_or_else(|_| json!({})),
                        ),
                        Ok(Err(e)) => s.emit("editor.error", None, json!({ "message": e })),
                        Err(e) => {
                            s.emit("editor.error", None, json!({ "message": format!("{e}") }))
                        }
                    }
                }
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::config::RecorderConfig;
    use crate::recorder::encoder::{CodecContextInfo, EncoderPipeline};

    fn create_sample_multi_stem_mp4(path: &Path) {
        let mut v_pkts = Vec::new();
        for i in 0..90u64 {
            let pts_ms = i * 16;
            let is_key = i % 30 == 0;
            let nal_type: u8 = if is_key { 0x65 } else { 0x41 };
            let payload = [nal_type, 0x88, 0x84, 0x21, 0xA0];
            let mut avcc = Vec::with_capacity(9);
            avcc.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            avcc.extend_from_slice(&payload);
            v_pkts.push(Arc::new(CompressedPacket {
                kind: PacketKind::Video,
                pts_ms,
                pts: pts_ms as i64,
                dts: pts_ms as i64,
                duration: 16,
                is_keyframe: is_key,
                data: Bytes::from(avcc),
                stems: None,
            }));
        }

        let mut a_pkts = Vec::new();
        let raw_aac = Bytes::from_static(&[0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C]);
        for i in 0..68u64 {
            let pts_ms = (i * 1024 * 1000) / 48_000;
            a_pkts.push(Arc::new(CompressedPacket {
                kind: PacketKind::Audio,
                pts_ms,
                pts: (i * 1024) as i64,
                dts: (i * 1024) as i64,
                duration: 1024,
                is_keyframe: true,
                data: raw_aac.clone(),
                stems: Some(Arc::new(AudioStemPackets {
                    game: raw_aac.clone(),
                    discord: raw_aac.clone(),
                    mic: raw_aac.clone(),
                    other: raw_aac.clone(),
                })),
            }));
        }

        let mut cfg = RecorderConfig::default();
        cfg.separate_audio_tracks = true;
        let info = CodecContextInfo {
            width: 1280,
            height: 720,
            fps: 60,
            sps: vec![
                0x67, 0x64, 0x00, 0x1F, 0xAC, 0xD9, 0x40, 0x50, 0x05, 0xBB, 0x01, 0x10, 0x00,
                0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xC0, 0xF1, 0x83, 0x19, 0x60,
            ],
            pps: vec![0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0],
        };
        let _ = EncoderPipeline::mux_clip(&v_pkts, &a_pkts, path, &cfg, &info);
    }

    #[test]
    fn test_probe_cut_split_and_stem_isolation() {
        let dir = std::env::temp_dir().join("clotch_editor_unit_test");
        let _ = std::fs::create_dir_all(&dir);
        let src_mp4 = dir.join("source_multi.mp4");
        create_sample_multi_stem_mp4(&src_mp4);

        let meta = probe_clip(&src_mp4).unwrap();
        assert!(meta.has_separated_stems);
        assert_eq!(meta.audio_tracks.len(), 5);
        assert_eq!(meta.audio_tracks[1].role, AudioTrackRole::Game);
        assert_eq!(meta.audio_tracks[2].role, AudioTrackRole::Discord);
        assert_eq!(meta.audio_tracks[3].role, AudioTrackRole::Microphone);
        assert_eq!(meta.audio_tracks[4].role, AudioTrackRole::OtherApps);

        let cut_out = dir.join("cut_clip.mp4");
        let cut_res = cut_clip(&src_mp4, 0, 800, CutMode::FastKeyframeCopy, Some(cut_out.clone())).unwrap();
        assert!(cut_out.is_file());
        assert!(cut_res.output_bytes > 100);

        let split_res = split_clip(&src_mp4, 600, CutMode::FastKeyframeCopy, Some(&dir)).unwrap();
        assert_eq!(split_res.parts.len(), 2);

        let game_only_out = dir.join("game_only.mp4");
        let iso_res = isolate_single_stem(&src_mp4, AudioTrackRole::Game, Some(game_only_out.clone())).unwrap();
        assert_eq!(iso_res.audio_tracks_written, 1);
        let iso_meta = probe_clip(&game_only_out).unwrap();
        assert_eq!(iso_meta.audio_tracks.len(), 1);

        let stems_res = export_separated_stems(&src_mp4, Some(&dir)).unwrap();
        assert_eq!(stems_res.stems.len(), 5);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
