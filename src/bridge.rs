#![allow(dead_code, unused_imports)]
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::analytics::{self, PRIVACY_SETTING_PATH};
use crate::audio_engine::AudioInputDeviceInfo;
use crate::discord;
use crate::editor::{
    self, AudioStemMixConfig, AudioTrackRole, ClipMetadata, CutMode, EditJob, EditResult,
    ResolutionPreset, SplitResult, StemExportResult, VideoPreviewSession,
};
use crate::events::EventSink;
use crate::recorder::config::{
    parse_resolution_pair, AudioMode, Container, Fps, Keybind, KeybindsConfig, Quality,
    RecorderConfig, StorageMode,
};
use crate::recorder::{self, DiscordCompressOptions, DiscordCompressResult, StorageManager};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacySettings {
    pub setting_path: String,
    pub share_anonymous_stats: bool,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self {
            setting_path: PRIVACY_SETTING_PATH.to_string(),
            share_anonymous_stats: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub quality: String,
    pub container: String,
    pub low_end_mode: bool,
    pub buffer_duration_secs: u64,
    pub direct_clip_duration_secs: u64,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub min_free_gb: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSettings {
    pub mode: String,
    pub bitrate_kbps: u32,
    pub mic_device: String,
    pub mic_muted: bool,
    pub separate_audio_tracks: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppSettings {
    pub capture: CaptureSettings,
    pub audio: AudioSettings,
    pub keybinds: KeybindsConfig,
    pub discord_rich_presence: bool,
    pub privacy: PrivacySettings,
}

impl AppSettings {
    pub fn from_recorder_config(cfg: &RecorderConfig) -> Self {
        let quality = match cfg.quality {
            Quality::Low => "low",
            Quality::Medium => "medium",
            Quality::High => "high",
            Quality::Lossless => "lossless",
        }
        .to_string();

        let mode = match cfg.audio {
            AudioMode::None => "none",
            AudioMode::SystemOnly => "system_only",
            AudioMode::MicOnly => "mic_only",
            AudioMode::Both => "both",
        }
        .to_string();

        let mic_device = cfg
            .mic_device
            .clone()
            .unwrap_or_else(|| "default".to_string());

        Self {
            capture: CaptureSettings {
                width: cfg.width,
                height: cfg.height,
                fps: cfg.fps.as_u32(),
                quality,
                container: cfg.container.extension().to_uppercase(),
                low_end_mode: cfg.low_end_mode,
                buffer_duration_secs: cfg.buffer_duration_secs,
                direct_clip_duration_secs: cfg.direct_clip_duration_secs,
                pre_roll_secs: cfg.pre_roll_secs,
                post_roll_secs: cfg.post_roll_secs,
                min_free_gb: cfg.storage.min_free_gb,
            },
            audio: AudioSettings {
                mode,
                bitrate_kbps: cfg.audio_bitrate_kbps,
                mic_device,
                mic_muted: cfg.mic_muted,
                separate_audio_tracks: cfg.separate_audio_tracks,
            },
            keybinds: cfg.keybinds.clone(),
            discord_rich_presence: discord::is_discord_rpc_enabled(),
            privacy: PrivacySettings {
                setting_path: PRIVACY_SETTING_PATH.to_string(),
                share_anonymous_stats: cfg.share_anonymous_stats,
            },
        }
    }
}

#[derive(Clone)]
pub struct UiBridge {
    config: Arc<RwLock<RecorderConfig>>,
    sink: Option<EventSink>,
}

impl UiBridge {
    pub fn new(config: RecorderConfig, sink: Option<EventSink>) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            sink,
        }
    }

    pub fn from_shared(config: Arc<RwLock<RecorderConfig>>, sink: Option<EventSink>) -> Self {
        Self { config, sink }
    }

    pub fn sink(&self) -> Option<EventSink> {
        self.sink.clone()
    }

    pub fn recorder_config(&self) -> RecorderConfig {
        match self.config.read() {
            Ok(g) => g.clone(),
            Err(poison) => poison.into_inner().clone(),
        }
    }

    pub fn settings_snapshot(&self) -> AppSettings {
        let cfg = self.recorder_config();
        AppSettings::from_recorder_config(&cfg)
    }

    pub fn update_and_save_config<F>(&self, mutator: F) -> Result<PathBuf, String>
    where
        F: FnOnce(&mut RecorderConfig),
    {
        let snapshot = {
            let mut guard = match self.config.write() {
                Ok(g) => g,
                Err(poison) => poison.into_inner(),
            };
            mutator(&mut guard);
            guard.clone()
        };
        let saved_path = snapshot.save_to_disk()?;
        if let Some(ref s) = self.sink {
            s.emit(
                "settings.updated",
                None,
                json!({
                    "fps": snapshot.fps.as_u32(),
                    "width": snapshot.width,
                    "height": snapshot.height,
                    "low_end_mode": snapshot.low_end_mode,
                    "separate_audio_tracks": snapshot.separate_audio_tracks,
                    "audio_bitrate_kbps": snapshot.audio_bitrate_kbps,
                }),
            );
        }
        Ok(saved_path)
    }

    pub fn share_anonymous_stats(&self) -> bool {
        self.config
            .read()
            .map(|c| c.share_anonymous_stats)
            .unwrap_or(false)
    }

    pub fn appearance_mode(&self) -> String {
        self.config
            .read()
            .map(|c| c.appearance.clone())
            .unwrap_or_else(|_| "system".to_string())
    }

    pub fn set_appearance_mode(&self, mode: &str) -> Result<PathBuf, String> {
        let normalized = match mode.trim().to_lowercase().as_str() {
            "light" => "light".to_string(),
            "dark" => "dark".to_string(),
            _ => "system".to_string(),
        };
        self.update_and_save_config(|cfg| {
            cfg.appearance = normalized;
        })
    }

    pub fn set_hotkeys_paused(&self, paused: bool) {
        if let Some(ref s) = self.sink {
            s.emit(
                "settings.hotkeys.pause",
                None,
                json!({
                    "paused": paused,
                }),
            );
        }
    }

    pub fn trash_clip(
        &self,
        video_path: &Path,
        thumb_path: Option<&Path>,
    ) -> Result<(), String> {
        if video_path.exists() {
            trash::delete(video_path)
                .map_err(|e| format!("Failed to move clip to Recycle Bin: {e}"))?;
        }
        let sidecar = video_path.with_extension("json");
        if sidecar.exists() {
            let _ = trash::delete(&sidecar);
        }
        if let Some(tp) = thumb_path {
            if tp.exists() {
                let _ = trash::delete(tp);
            }
        }
        Ok(())
    }

    pub fn extract_frame_rgba(
        &self,
        input_path: impl AsRef<Path>,
        target_ms: u64,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<Vec<u8>, String> {
        editor::extract_frame_rgba(input_path, target_ms, dst_w, dst_h)
    }

    pub fn open_preview_session(
        &self,
        input_path: impl AsRef<Path>,
    ) -> Result<VideoPreviewSession, String> {
        editor::VideoPreviewSession::open(input_path)
    }

    pub fn validate_and_query_folder_free_gb(&self, path: &Path) -> Result<f64, String> {
        if !path.exists() {
            std::fs::create_dir_all(path)
                .map_err(|e| format!("Folder does not exist and cannot be created: {e}"))?;
        }
        if !path.is_dir() {
            return Err("Selected path is not a directory".to_string());
        }
        let probe_file = path.join(".clotch_write_test.tmp");
        std::fs::write(&probe_file, b"ok")
            .map_err(|e| format!("Folder is not writable: {e}"))?;
        let _ = std::fs::remove_file(&probe_file);

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::core::PCWSTR;
            use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            wide.push(0);
            let mut free_bytes = 0u64;
            let ok = unsafe {
                GetDiskFreeSpaceExW(
                    PCWSTR(wide.as_ptr()),
                    Some(&mut free_bytes),
                    None,
                    None,
                )
            };
            if ok.is_ok() {
                return Ok(free_bytes as f64 / (1024.0 * 1024.0 * 1024.0));
            }
        }
        Ok(50.0)
    }

    pub fn set_privacy_share_anonymous_stats(&self, enabled: bool) -> Result<PathBuf, String> {
        if let Ok(mut cfg) = self.config.write() {
            cfg.share_anonymous_stats = enabled;
        }
        let saved_path = analytics::persist_share_anonymous_stats(enabled)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "settings.privacy.share_anonymous_stats",
                None,
                json!({
                    "path": PRIVACY_SETTING_PATH,
                    "share_anonymous_stats": enabled,
                    "enabled": enabled,
                    "status": if enabled { "On" } else { "Off" },
                }),
            );
        }
        Ok(saved_path)
    }

    pub fn discord_rpc_enabled(&self) -> bool {
        discord::is_discord_rpc_enabled()
    }

    pub fn set_discord_rpc_enabled(&self, enabled: bool) -> Result<PathBuf, String> {
        discord::set_discord_rpc_enabled(enabled);
        let saved_path = discord::persist_discord_rpc_enabled(enabled)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "settings.discord.rich_presence",
                None,
                json!({
                    "enabled": enabled,
                    "discord_enabled": enabled,
                    "status": if enabled { "On" } else { "Off" },
                }),
            );
        }
        Ok(saved_path)
    }

    pub fn set_mic_device(&self, device: Option<String>) -> Result<PathBuf, String> {
        let clean_dev = device
            .map(|s| s.trim().to_string())
            .filter(|s| {
                !s.is_empty()
                    && !s.eq_ignore_ascii_case("default")
                    && !s.eq_ignore_ascii_case("auto")
            });
        let saved = self.update_and_save_config(|cfg| {
            cfg.mic_device = clean_dev.clone();
        })?;
        if let Some(ref s) = self.sink {
            s.emit(
                "audio.set_mic_device",
                None,
                json!({
                    "device": clean_dev.unwrap_or_else(|| "default".to_string()),
                }),
            );
        }
        Ok(saved)
    }

    pub fn set_keybinds(&self, keybinds: KeybindsConfig) -> Result<PathBuf, String> {
        let saved = self.update_and_save_config(|cfg| {
            cfg.keybinds = keybinds.clone();
        })?;
        if let Some(ref s) = self.sink {
            s.emit(
                "settings.keybinds.updated",
                None,
                json!({
                    "replay_clip": keybinds.replay_clip.label,
                    "direct_clip": keybinds.direct_clip.label,
                    "discord_clip": keybinds.discord_clip.label,
                    "mic_mute": keybinds.mic_mute.label,
                }),
            );
        }
        Ok(saved)
    }

    pub fn trigger_save_replay(&self) {
        let kb = self.recorder_config().keybinds.replay_clip.label;
        if let Some(ref s) = self.sink {
            s.emit("manual.clip", None, json!({ "hotkey": kb }));
        }
    }

    pub fn trigger_direct_clip(&self) {
        let kb = self.recorder_config().keybinds.direct_clip.label;
        if let Some(ref s) = self.sink {
            s.emit("manual.direct_clip", None, json!({ "hotkey": kb }));
        }
    }

    pub fn toggle_mic_mute(&self) {
        if let Some(ref s) = self.sink {
            s.emit("audio.toggle_mic_mute", None, json!({}));
        }
    }

    pub fn resolve_active_clips_dir(&self) -> PathBuf {
        let cfg = self.recorder_config();
        StorageManager::resolve_clip_dir(&cfg.storage, None)
            .unwrap_or_else(|_| PathBuf::from("ClotchClips"))
    }

    pub fn apply_setting_by_path(&self, path: &str, value: &str) -> Result<(), String> {
        let normalized = path
            .replace('\u{2192}', "->")
            .split("->")
            .map(|s| s.trim().to_lowercase())
            .collect::<Vec<_>>()
            .join(" -> ");

        let val_trimmed = value.trim();
        let val_low = val_trimmed.to_lowercase();

        match normalized.as_str() {
            "settings -> privacy -> share anonymous stats"
            | "privacy -> share anonymous stats"
            | "share_anonymous_stats"
            | "telemetry_enabled" => {
                let enabled = match val_low.as_str() {
                    "on" | "true" | "1" | "yes" | "enabled" => true,
                    "off" | "false" | "0" | "no" | "disabled" => false,
                    _ => return Err(format!("invalid boolean/toggle value: {val_trimmed}")),
                };
                let _ = self.set_privacy_share_anonymous_stats(enabled)?;
                Ok(())
            }
            "settings -> advanced -> discord rich presence"
            | "discord_rich_presence"
            | "discord_enabled" => {
                let enabled = matches!(val_low.as_str(), "on" | "true" | "1" | "yes" | "enabled");
                let _ = self.set_discord_rpc_enabled(enabled)?;
                Ok(())
            }
            "settings -> capture -> resolution" | "capture -> resolution" | "resolution" => {
                let Some((w, h)) = parse_resolution_pair(val_trimmed) else {
                    return Err(format!("unsupported resolution preset: {val_trimmed}"));
                };
                let _ = self.update_and_save_config(|cfg| {
                    cfg.width = w;
                    cfg.height = h;
                })?;
                Ok(())
            }
            "settings -> capture -> fps" | "capture -> fps" | "fps" => {
                let Some(fps) = Fps::parse_str(val_trimmed) else {
                    return Err(format!("unsupported fps value: {val_trimmed}"));
                };
                let _ = self.update_and_save_config(|cfg| {
                    cfg.fps = fps;
                })?;
                Ok(())
            }
            "settings -> capture -> quality" | "capture -> quality" | "quality" => {
                let Some(q) = Quality::parse_str(val_trimmed) else {
                    return Err(format!("unsupported quality preset: {val_trimmed}"));
                };
                let _ = self.update_and_save_config(|cfg| {
                    cfg.quality = q;
                })?;
                Ok(())
            }
            "settings -> audio -> separate tracks"
            | "audio -> separate tracks"
            | "separate_audio_tracks" => {
                let enabled = matches!(val_low.as_str(), "on" | "true" | "1" | "yes" | "enabled");
                let _ = self.update_and_save_config(|cfg| {
                    cfg.separate_audio_tracks = enabled;
                })?;
                Ok(())
            }
            "settings -> audio -> microphone" | "audio -> microphone" | "mic_device" => {
                let dev_opt = if val_low == "default" || val_low == "auto" || val_low.is_empty() {
                    None
                } else {
                    Some(val_trimmed.to_string())
                };
                let _ = self.set_mic_device(dev_opt)?;
                Ok(())
            }
            _ => Err(format!("unknown setting path: {path}")),
        }
    }

    pub fn list_microphones(&self) -> Vec<AudioInputDeviceInfo> {
        recorder::list_input_devices()
    }

    pub fn probe_clip(&self, path: impl AsRef<Path>) -> Result<ClipMetadata, String> {
        editor::probe_clip(path)
    }

    pub fn probe_clip_header_fast(&self, path: impl AsRef<Path>) -> Result<(f64, bool), String> {
        editor::probe_clip_header_fast(path)
    }

    pub fn run_edit_job(&self, job: EditJob) -> Result<EditResult, String> {
        let res = editor::run_edit_job(job)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "editor.job_done",
                None,
                json!({
                    "output_path": res.output_path.to_string_lossy(),
                    "duration_ms": res.duration_ms,
                }),
            );
        }
        Ok(res)
    }

    pub fn cut_clip(
        &self,
        input_path: impl AsRef<Path>,
        start_ms: u64,
        end_ms: u64,
        mode: CutMode,
        output_path: Option<PathBuf>,
    ) -> Result<EditResult, String> {
        let res = editor::cut_clip(input_path, start_ms, end_ms, mode, output_path)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "editor.job_done",
                None,
                json!({
                    "output_path": res.output_path.to_string_lossy(),
                    "duration_ms": res.duration_ms,
                }),
            );
        }
        Ok(res)
    }

    pub fn split_clip(
        &self,
        input_path: impl AsRef<Path>,
        split_at_ms: u64,
        mode: CutMode,
        output_dir: Option<&Path>,
    ) -> Result<SplitResult, String> {
        let res = editor::split_clip(input_path, split_at_ms, mode, output_dir)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "editor.split_done",
                None,
                json!({
                    "parts_count": res.parts.len(),
                }),
            );
        }
        Ok(res)
    }

    pub fn split_multi_clip(
        &self,
        input_path: impl AsRef<Path>,
        split_points_ms: &[u64],
        mode: CutMode,
        output_dir: Option<&Path>,
    ) -> Result<SplitResult, String> {
        let res = editor::split_multi_clip(input_path, split_points_ms, mode, output_dir)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "editor.split_done",
                None,
                json!({
                    "parts_count": res.parts.len(),
                }),
            );
        }
        Ok(res)
    }

    pub fn remove_middle_segment(
        &self,
        input_path: impl AsRef<Path>,
        remove_start_ms: u64,
        remove_end_ms: u64,
        output_path: Option<PathBuf>,
    ) -> Result<EditResult, String> {
        let res =
            editor::remove_middle_segment(input_path, remove_start_ms, remove_end_ms, output_path)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "editor.job_done",
                None,
                json!({
                    "output_path": res.output_path.to_string_lossy(),
                    "duration_ms": res.duration_ms,
                }),
            );
        }
        Ok(res)
    }

    pub fn rescale_clip(
        &self,
        input_path: impl AsRef<Path>,
        preset: ResolutionPreset,
        target_fps: Option<u32>,
        video_bitrate_kbps: Option<u32>,
        output_path: Option<PathBuf>,
    ) -> Result<EditResult, String> {
        let res = editor::rescale_clip(
            input_path,
            preset,
            target_fps,
            video_bitrate_kbps,
            output_path,
        )?;
        if let Some(ref s) = self.sink {
            s.emit(
                "editor.job_done",
                None,
                json!({
                    "output_path": res.output_path.to_string_lossy(),
                    "duration_ms": res.duration_ms,
                }),
            );
        }
        Ok(res)
    }

    pub fn remix_clip_audio(
        &self,
        input_path: impl AsRef<Path>,
        mix_config: AudioStemMixConfig,
        output_path: Option<PathBuf>,
    ) -> Result<EditResult, String> {
        let res = editor::remix_clip_audio(input_path, mix_config, output_path)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "editor.job_done",
                None,
                json!({
                    "output_path": res.output_path.to_string_lossy(),
                    "duration_ms": res.duration_ms,
                }),
            );
        }
        Ok(res)
    }

    pub fn export_separated_stems(
        &self,
        input_path: impl AsRef<Path>,
        output_dir: Option<PathBuf>,
    ) -> Result<StemExportResult, String> {
        let res = editor::export_separated_stems(input_path, output_dir.as_deref())?;
        if let Some(ref s) = self.sink {
            s.emit(
                "editor.stems_exported",
                None,
                json!({
                    "stems_count": res.stems.len(),
                }),
            );
        }
        Ok(res)
    }

    pub async fn compress_for_discord(
        &self,
        input_path: impl Into<PathBuf>,
        target_mb: Option<f64>,
    ) -> Result<DiscordCompressResult, String> {
        let opts = DiscordCompressOptions {
            target_mb: target_mb.unwrap_or(recorder::DEFAULT_DISCORD_TARGET_MB),
            output_path: None,
            max_fps: None,
            prefer_hardware: true,
        };
        recorder::make_it_for_discord_async(input_path.into(), opts, self.sink.clone()).await
    }

    pub fn compress_for_discord_blocking(
        &self,
        input_path: impl Into<PathBuf>,
        target_mb: Option<f64>,
    ) -> Result<DiscordCompressResult, String> {
        let opts = DiscordCompressOptions {
            target_mb: target_mb.unwrap_or(recorder::DEFAULT_DISCORD_TARGET_MB),
            output_path: None,
            max_fps: None,
            prefer_hardware: true,
        };
        let res = recorder::make_it_for_discord(input_path.into(), opts)?;
        if let Some(ref s) = self.sink {
            s.emit(
                "discord.compress_done",
                None,
                json!({
                    "output_path": res.output_path.to_string_lossy(),
                    "output_bytes": res.output_bytes,
                    "backend": res.backend,
                    "elapsed_ms": res.elapsed_ms,
                }),
            );
        }
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_privacy_and_discord_rpc_paths() {
        let cfg = RecorderConfig::default();
        let bridge = UiBridge::new(cfg, None);
        assert!(!bridge.share_anonymous_stats());

        let res = bridge.apply_setting_by_path(
            "Settings → Privacy → Share anonymous stats",
            "Off",
        );
        assert!(res.is_ok());
        assert!(!bridge.share_anonymous_stats());

        let res_rpc = bridge.apply_setting_by_path(
            "Settings -> Advanced -> Discord Rich Presence",
            "Off",
        );
        assert!(res_rpc.is_ok());
        assert!(!bridge.discord_rpc_enabled());

        let _ = bridge.apply_setting_by_path(
            "Settings -> Advanced -> Discord Rich Presence",
            "On",
        );
        assert!(bridge.discord_rpc_enabled());
    }
}