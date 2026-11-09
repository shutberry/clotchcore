#![allow(dead_code, unused_imports)]
use serde::{Deserialize, Serialize};
use crate::stats::PRIVACY_SETTING_PATH;
use crate::integrations::discord;
pub use crate::recorder::{
    list_input_devices, parse_duration_secs, parse_resolution_pair, AudioMode, Container,
    DiscordCompressOptions, Fps, Keybind, KeybindsConfig, Quality, RecorderConfig,
    StorageConfig, StorageMode, SystemHardwareProfile, DEFAULT_DISCORD_TARGET_MB,
};

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
