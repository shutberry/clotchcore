#![allow(dead_code, unused_imports)]
use crate::tracker::Event;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sysinfo::{CpuRefreshKind, System};
use tokio::sync::{broadcast, watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::interval_at;

use crate::integrations::discord;
use crate::recorder::config::{
    clean_config_value, locate_keybinds_or_config_files, AudioMode, KeybindsConfig, Quality,
    RecorderConfig,
};
use crate::recorder::detected_gpu_vendor_id;

pub fn compute_kd(kills: u32, deaths: u32) -> f64 {
    let k = kills as f64;
    let d = (deaths.max(1)) as f64;
    ((k / d) * 100.0).round() / 100.0
}

pub fn compute_win_rate(wins: u32, losses: u32) -> f64 {
    let decided = wins + losses;
    if decided == 0 {
        0.0
    } else {
        (((wins as f64) / (decided as f64)) * 1000.0).round() / 10.0
    }
}

pub fn compute_hit_percentages(
    headshots: u32,
    bodyshots: u32,
    legshots: u32,
    fallback_kills: u32,
) -> (u32, u32, u32, f64, f64, f64) {
    let mut hs = headshots;
    let mut bs = bodyshots;
    let ls = legshots;
    if hs > 0 && bs == 0 && ls == 0 && fallback_kills > hs {
        bs = fallback_kills.saturating_sub(hs);
    }
    if hs > fallback_kills && bs == 0 && ls == 0 && fallback_kills > 0 {
        hs = fallback_kills;
    }
    let total_hits = hs + bs + ls;
    if total_hits == 0 {
        return (0, 0, 0, 0.0, 0.0, 0.0);
    }
    let hs_pct = (((hs as f64) / (total_hits as f64)) * 1000.0).round() / 10.0;
    let ls_pct = (((ls as f64) / (total_hits as f64)) * 1000.0).round() / 10.0;
    let bs_pct = (100.0 - hs_pct - ls_pct).clamp(0.0, 100.0);
    let bs_pct_round = (bs_pct * 10.0).round() / 10.0;
    (hs, bs, ls, hs_pct, bs_pct_round, ls_pct)
}

pub const DEFAULT_FIREBASE_PROJECT_ID: &str = "clotch-ffbe4";
pub const DEFAULT_FIREBASE_COLLECTION: &str = "telemetry";
pub const PRIVACY_SETTING_PATH: &str = "Settings -> Privacy -> Share anonymous stats";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalyticsConfig {
    pub enabled: bool,
    pub firebase_project_id: String,
    pub firebase_collection: String,
    pub firebase_api_key: Option<String>,
    pub flush_interval_secs: u64,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self::load()
    }
}

impl AnalyticsConfig {
    pub fn load() -> Self {
        let mut enabled = std::env::var("CLOTCH_TELEMETRY_ENABLED")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on" | "enabled"))
            .unwrap_or(false);

        let mut firebase_project_id = std::env::var("CLOTCH_FIREBASE_PROJECT_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_FIREBASE_PROJECT_ID.to_string());

        let mut firebase_collection = std::env::var("CLOTCH_FIREBASE_COLLECTION")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_FIREBASE_COLLECTION.to_string());

        let mut firebase_api_key = std::env::var("CLOTCH_FIREBASE_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let mut flush_interval_secs = 300u64;

        for path in locate_keybinds_or_config_files() {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty()
                    || trimmed.starts_with('#')
                    || trimmed.starts_with(';')
                    || trimmed.starts_with('[')
                {
                    continue;
                }
                let Some((k, v)) = trimmed.split_once('=') else {
                    continue;
                };
                let key = k.trim().to_lowercase();
                let val = clean_config_value(v);
                if val.is_empty() {
                    continue;
                }
                match key.as_str() {
                    "share_anonymous_stats"
                    | "privacy_share_anonymous_stats"
                    | "telemetry_enabled"
                    | "analytics_enabled"
                    | "telemetry" => {
                        enabled = !matches!(
                            val.to_lowercase().as_str(),
                            "false" | "0" | "no" | "off" | "disabled"
                        );
                    }
                    "firebase_project_id" | "telemetry_project_id" => {
                        firebase_project_id = val.to_string();
                    }
                    "firebase_collection" | "telemetry_collection" => {
                        firebase_collection = val.to_string();
                    }
                    "firebase_api_key" => {
                        firebase_api_key = Some(val.to_string());
                    }
                    "telemetry_flush_interval_secs" => {
                        if let Ok(secs) = val.parse::<u64>() {
                            flush_interval_secs = secs.clamp(30, 86400);
                        }
                    }
                    _ => {}
                }
            }
        }

        if let Ok(env_val) = std::env::var("CLOTCH_TELEMETRY_ENABLED") {
            enabled = !matches!(
                env_val.trim().to_lowercase().as_str(),
                "0" | "false" | "no" | "off" | "disabled"
            );
        }

        Self {
            enabled,
            firebase_project_id,
            firebase_collection,
            firebase_api_key,
            flush_interval_secs,
        }
    }
}

pub fn persist_share_anonymous_stats(enabled: bool) -> Result<PathBuf, String> {
    let existing = locate_keybinds_or_config_files();
    let target_path = if let Some(first) = existing
        .iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.eq_ignore_ascii_case("clotch_config.toml"))
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| existing.into_iter().next())
    {
        first
    } else if let Ok(exe) = std::env::current_exe() {
        exe.parent()
            .map(|d| d.join("clotch_config.toml"))
            .unwrap_or_else(|| PathBuf::from("clotch_config.toml"))
    } else {
        PathBuf::from("clotch_config.toml")
    };

    let val_str = if enabled { "true" } else { "false" };
    let current = std::fs::read_to_string(&target_path).unwrap_or_default();
    let mut out_lines: Vec<String> = Vec::new();
    let mut updated_share = false;
    let mut updated_telemetry = false;

    for line in current.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && !trimmed.starts_with(';')
            && !trimmed.starts_with('[')
        {
            if let Some((k, _)) = trimmed.split_once('=') {
                let key_low = k.trim().to_lowercase();
                if key_low == "share_anonymous_stats"
                    || key_low == "privacy_share_anonymous_stats"
                {
                    if !updated_share {
                        out_lines.push(format!("share_anonymous_stats = {val_str}"));
                        updated_share = true;
                    }
                    continue;
                }
                if key_low == "telemetry_enabled" || key_low == "analytics_enabled" {
                    if !updated_telemetry {
                        out_lines.push(format!("telemetry_enabled = {val_str}"));
                        updated_telemetry = true;
                    }
                    continue;
                }
            }
        }
        out_lines.push(line.to_string());
    }

    if !updated_share {
        out_lines.push(format!("share_anonymous_stats = {val_str}"));
    }
    if !updated_telemetry {
        out_lines.push(format!("telemetry_enabled = {val_str}"));
    }

    let mut final_text = out_lines.join("\n");
    if !final_text.ends_with('\n') {
        final_text.push('\n');
    }

    if let Some(parent) = target_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    std::fs::write(&target_path, final_text)
        .map_err(|e| format!("failed to persist privacy setting to {}: {e}", target_path.display()))?;

    Ok(target_path)
}

pub fn parse_privacy_toggle_event_value(data: &Value) -> Option<bool> {
    if let Some(b) = data
        .get("share_anonymous_stats")
        .or_else(|| data.get("enabled"))
        .or_else(|| data.get("value"))
        .and_then(Value::as_bool)
    {
        return Some(b);
    }
    if let Some(s) = data
        .get("share_anonymous_stats")
        .or_else(|| data.get("enabled"))
        .or_else(|| data.get("value"))
        .or_else(|| data.get("status"))
        .and_then(Value::as_str)
    {
        let low = s.trim().to_lowercase();
        if matches!(low.as_str(), "on" | "true" | "1" | "yes" | "enabled") {
            return Some(true);
        }
        if matches!(low.as_str(), "off" | "false" | "0" | "no" | "disabled") {
            return Some(false);
        }
    }
    None
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GameAggregateStats {
    pub game: String,
    pub sessions_count: u64,
    pub total_playtime_minutes: u64,
    pub auto_highlights_count: u64,
    pub manual_clips_count: u64,
    pub kills_or_objectives_logged: u64,
    pub matches_completed: u64,
    pub deaths_logged: u64,
    pub clutches_logged: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeybindTelemetrySummary {
    pub uses_custom_replay_key: bool,
    pub uses_custom_direct_key: bool,
    pub uses_custom_discord_key: bool,
    pub uses_custom_mic_mute_key: bool,
    pub uses_custom_mic: bool,
    pub replay_key_category: String,
    pub direct_key_category: String,
}

impl KeybindTelemetrySummary {
    pub fn from_keybinds(kb: &KeybindsConfig) -> Self {
        Self::from_keybinds_and_mic(kb, false)
    }

    pub fn from_config(cfg: &RecorderConfig) -> Self {
        let uses_custom_mic = cfg
            .mic_device
            .as_deref()
            .map(|s| {
                let t = s.trim();
                !t.is_empty() && !t.eq_ignore_ascii_case("default") && !t.eq_ignore_ascii_case("auto")
            })
            .unwrap_or(false);
        Self::from_keybinds_and_mic(&cfg.keybinds, uses_custom_mic)
    }

    pub fn from_keybinds_and_mic(kb: &KeybindsConfig, uses_custom_mic: bool) -> Self {
        let def = KeybindsConfig::default();
        Self {
            uses_custom_replay_key: kb.replay_clip != def.replay_clip,
            uses_custom_direct_key: kb.direct_clip != def.direct_clip,
            uses_custom_discord_key: kb.discord_clip != def.discord_clip,
            uses_custom_mic_mute_key: kb.mic_mute != def.mic_mute,
            uses_custom_mic,
            replay_key_category: classify_keybind_category(
                &kb.replay_clip.label,
                kb.replay_clip.modifiers,
            ),
            direct_key_category: classify_keybind_category(
                &kb.direct_clip.label,
                kb.direct_clip.modifiers,
            ),
        }
    }

    pub fn any_custom(&self) -> bool {
        self.uses_custom_replay_key
            || self.uses_custom_direct_key
            || self.uses_custom_discord_key
            || self.uses_custom_mic_mute_key
    }
}

fn classify_keybind_category(label: &str, modifiers: u32) -> String {
    let has_mod = modifiers != 0 || label.contains('+');
    let upper = label.to_uppercase();
    let base_type = if upper.contains("F1")
        || upper.contains("F2")
        || upper.contains("F3")
        || upper.contains("F4")
        || upper.contains("F5")
        || upper.contains("F6")
        || upper.contains("F7")
        || upper.contains("F8")
        || upper.contains("F9")
    {
        "FunctionKey"
    } else if upper.contains("NUMPAD") {
        "Numpad"
    } else {
        "AlphaOrSpecial"
    };
    if has_mod {
        format!("Modifier+{base_type}")
    } else {
        base_type.to_string()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameSession {
    pub game: String,
    pub duration_minutes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelemetryPayload {
    pub cpu_vendor: String,
    pub cpu_cores: usize,
    pub cpu_tier: String,
    pub ram_gb: u64,
    pub ram_tier: String,
    pub gpu_vendor: String,
    pub encoder_zero_copy_fallbacks: u64,
    pub encoder_software_fallbacks: u64,
    pub low_end_mode: bool,
    pub capture_resolution: String,
    pub capture_fps: u32,
    pub quality_preset: String,
    pub audio_mode: String,
    pub separate_audio_tracks: bool,
    pub discord_rpc_active: bool,
    pub keybind_summary: KeybindTelemetrySummary,
    pub auto_highlight_clips: u64,
    pub manual_replay_clips: u64,
    pub manual_direct_clips: u64,
    pub discord_compressed_clips: u64,
    pub editor_exports_count: u64,
    pub game_sessions: Vec<GameSession>,
    pub game_aggregates: HashMap<String, GameAggregateStats>,
    pub app_version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LocalCumulativeCache {
    pub hardware_profiles_count: u64,
    pub telemetry_flushes_count: u64,
    pub sum_ram_gb: u64,
    pub sum_cpu_cores: u64,
    pub gpu_nvidia_count: u64,
    pub gpu_amd_count: u64,
    pub gpu_intel_count: u64,
    pub gpu_other_count: u64,
    pub cpu_intel_count: u64,
    pub cpu_amd_count: u64,
    pub cpu_other_count: u64,
    pub low_end_mode_count: u64,
    pub standard_mode_count: u64,
    pub discord_rpc_active_count: u64,
    pub discord_rpc_inactive_count: u64,
    pub keybind_custom_count: u64,
    pub keybind_default_count: u64,
    pub mic_custom_count: u64,
    pub mic_default_count: u64,
    pub total_game_sessions: u64,
    pub total_playtime_minutes: u64,
    pub clips_auto_highlight: u64,
    pub clips_manual_replay: u64,
    pub clips_manual_direct: u64,
    pub clips_discord_compressed: u64,
    pub editor_exports_total: u64,
    pub game_playtime_minutes: HashMap<String, u64>,
    pub game_sessions_count: HashMap<String, u64>,
    pub game_highlights_count: HashMap<String, u64>,
    pub game_manual_clips_count: HashMap<String, u64>,
    pub game_clutch_events_count: HashMap<String, u64>,
}

fn resolve_local_cache_path() -> PathBuf {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let dir = PathBuf::from(local_app_data).join("Clotch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("telemetry_aggregates_cache.json");
    }
    PathBuf::from("telemetry_aggregates_cache.json")
}

impl LocalCumulativeCache {
    fn load() -> Self {
        let p = resolve_local_cache_path();
        std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str::<Self>(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let p = resolve_local_cache_path();
        if let Ok(json_str) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, json_str);
        }
    }

    fn absorb(&mut self, payload: &TelemetryPayload, include_hw: bool) {
        self.telemetry_flushes_count = self.telemetry_flushes_count.saturating_add(1);
        if payload.discord_rpc_active {
            self.discord_rpc_active_count = self.discord_rpc_active_count.saturating_add(1);
        } else {
            self.discord_rpc_inactive_count = self.discord_rpc_inactive_count.saturating_add(1);
        }

        if include_hw {
            self.hardware_profiles_count = self.hardware_profiles_count.saturating_add(1);
            self.sum_ram_gb = self.sum_ram_gb.saturating_add(payload.ram_gb);
            self.sum_cpu_cores = self.sum_cpu_cores.saturating_add(payload.cpu_cores as u64);

            match payload.gpu_vendor.as_str() {
                "NVIDIA" => self.gpu_nvidia_count += 1,
                "AMD" => self.gpu_amd_count += 1,
                "Intel" => self.gpu_intel_count += 1,
                _ => self.gpu_other_count += 1,
            }
            match payload.cpu_vendor.as_str() {
                "Intel" => self.cpu_intel_count += 1,
                "AMD" => self.cpu_amd_count += 1,
                _ => self.cpu_other_count += 1,
            }
            if payload.low_end_mode {
                self.low_end_mode_count += 1;
            } else {
                self.standard_mode_count += 1;
            }
            if payload.keybind_summary.any_custom() {
                self.keybind_custom_count += 1;
            } else {
                self.keybind_default_count += 1;
            }
            if payload.keybind_summary.uses_custom_mic {
                self.mic_custom_count += 1;
            } else {
                self.mic_default_count += 1;
            }
        }

        self.clips_auto_highlight = self
            .clips_auto_highlight
            .saturating_add(payload.auto_highlight_clips);
        self.clips_manual_replay = self
            .clips_manual_replay
            .saturating_add(payload.manual_replay_clips);
        self.clips_manual_direct = self
            .clips_manual_direct
            .saturating_add(payload.manual_direct_clips);
        self.clips_discord_compressed = self
            .clips_discord_compressed
            .saturating_add(payload.discord_compressed_clips);
        self.editor_exports_total = self
            .editor_exports_total
            .saturating_add(payload.editor_exports_count);

        for s in &payload.game_sessions {
            let slug = sanitize_firestore_key(&s.game);
            let mins = s.duration_minutes.max(1);
            self.total_game_sessions = self.total_game_sessions.saturating_add(1);
            self.total_playtime_minutes = self.total_playtime_minutes.saturating_add(mins);
            *self.game_playtime_minutes.entry(slug.clone()).or_insert(0) += mins;
            *self.game_sessions_count.entry(slug).or_insert(0) += 1;
        }

        for (game_key, agg) in &payload.game_aggregates {
            let slug = sanitize_firestore_key(game_key);
            *self.game_highlights_count.entry(slug.clone()).or_insert(0) +=
                agg.auto_highlights_count;
            *self
                .game_manual_clips_count
                .entry(slug.clone())
                .or_insert(0) += agg.manual_clips_count;
            *self.game_clutch_events_count.entry(slug).or_insert(0) +=
                agg.kills_or_objectives_logged;
        }
    }
}

struct AnalyticsState {
    active_games: HashMap<String, Instant>,
    game_aggregates: HashMap<String, GameAggregateStats>,
    pending_sessions: Vec<GameSession>,
    auto_highlight_clips: u64,
    manual_replay_clips: u64,
    manual_direct_clips: u64,
    discord_compressed_clips: u64,
    editor_exports_count: u64,
    encoder_zero_copy_fallbacks: u64,
    encoder_software_fallbacks: u64,
    discord_rpc_seen_active: bool,
    recorder_cfg_snapshot: RecorderConfig,
    hardware_reported_once: bool,
}

pub fn detect_gpu_vendor() -> String {
    let dxgi_vendor = detected_gpu_vendor_id();
    if dxgi_vendor == 0x10DE {
        return "NVIDIA".to_string();
    } else if dxgi_vendor == 0x1002 {
        return "AMD".to_string();
    } else if dxgi_vendor == 0x8086 {
        return "Intel".to_string();
    }

    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
        unsafe {
            if let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() {
                let mut idx = 0u32;
                while let Ok(adapter) = factory.EnumAdapters1(idx) {
                    idx += 1;
                    if let Ok(desc) = adapter.GetDesc1() {
                        match desc.VendorId {
                            0x10DE => return "NVIDIA".to_string(),
                            0x1002 => return "AMD".to_string(),
                            0x8086 => return "Intel".to_string(),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    "Other".to_string()
}

pub fn ram_tier(ram_mb: u64) -> String {
    match ram_mb {
        0..=4095 => "4GB or less".to_string(),
        4096..=8191 => "8GB".to_string(),
        8192..=15999 => "16GB".to_string(),
        16000..=32767 => "32GB".to_string(),
        _ => "32GB+".to_string(),
    }
}

pub fn cpu_vendor_name(brand: &str) -> String {
    let b = brand.to_lowercase();
    if b.contains("intel") {
        "Intel".to_string()
    } else if b.contains("amd") || b.contains("ryzen") {
        "AMD".to_string()
    } else {
        "Other".to_string()
    }
}

pub fn cpu_tier(brand: &str, cores: usize) -> String {
    let vendor = cpu_vendor_name(brand);
    let tier = if cores >= 16 {
        "High-end"
    } else if cores >= 8 {
        "Mid-range"
    } else {
        "Entry-level"
    };
    format!("{vendor} {tier}")
}

fn resolution_bucket(w: u32, h: u32) -> String {
    if h <= 720 {
        "720p".to_string()
    } else if h <= 900 {
        "900p".to_string()
    } else if h <= 1080 {
        "1080p".to_string()
    } else if h <= 1440 {
        "1440p".to_string()
    } else {
        format!("{w}x{h}")
    }
}

fn sanitize_firestore_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if (c == '_' || c == '-' || c == ' ') && !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn collect_snapshot(
    system: &mut System,
    state: &AnalyticsState,
    sessions: Vec<GameSession>,
    game_aggregates: HashMap<String, GameAggregateStats>,
) -> TelemetryPayload {
    system.refresh_cpu_specifics(CpuRefreshKind::new());
    system.refresh_memory();

    let cpu_brand = system
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_default();
    let cpu_cores = system.cpus().len().max(1);
    let ram_mb = system.total_memory() / (1024 * 1024);
    let ram_gb = ((ram_mb + 512) / 1024).max(1);

    let cfg = &state.recorder_cfg_snapshot;
    let res_label = resolution_bucket(cfg.width, cfg.height);
    let quality_label = match cfg.quality {
        Quality::Low => "low",
        Quality::Medium => "medium",
        Quality::High => "high",
        Quality::Lossless => "lossless",
    }
    .to_string();
    let audio_label = match cfg.audio {
        AudioMode::None => "none",
        AudioMode::SystemOnly => "system_only",
        AudioMode::MicOnly => "mic_only",
        AudioMode::Both => "both",
    }
    .to_string();

    let rpc_active = state.discord_rpc_seen_active || discord::is_discord_rpc_active();

    TelemetryPayload {
        cpu_vendor: cpu_vendor_name(&cpu_brand),
        cpu_cores,
        cpu_tier: cpu_tier(&cpu_brand, cpu_cores),
        ram_gb,
        ram_tier: ram_tier(ram_mb),
        gpu_vendor: detect_gpu_vendor(),
        encoder_zero_copy_fallbacks: state.encoder_zero_copy_fallbacks,
        encoder_software_fallbacks: state.encoder_software_fallbacks,
        low_end_mode: cfg.low_end_mode,
        capture_resolution: res_label,
        capture_fps: cfg.fps.as_u32(),
        quality_preset: quality_label,
        audio_mode: audio_label,
        separate_audio_tracks: cfg.separate_audio_tracks,
        discord_rpc_active: rpc_active,
        keybind_summary: KeybindTelemetrySummary::from_config(cfg),
        auto_highlight_clips: state.auto_highlight_clips,
        manual_replay_clips: state.manual_replay_clips,
        manual_direct_clips: state.manual_direct_clips,
        discord_compressed_clips: state.discord_compressed_clips,
        editor_exports_count: state.editor_exports_count,
        game_sessions: sessions,
        game_aggregates,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn make_inc_transform(field_path: &str, amount: i64) -> Value {
    json!({
        "fieldPath": field_path,
        "increment": {
            "integerValue": amount.to_string()
        }
    })
}

fn build_averages_body_from_cache(cache: &LocalCumulativeCache, payload: &TelemetryPayload) -> Value {
    let hw_samples = (cache.hardware_profiles_count.max(1)) as f64;
    let total_flushes = (cache.telemetry_flushes_count.max(1)) as f64;
    let total_sessions = (cache.total_game_sessions.max(1)) as f64;

    let avg_ram_gb = cache.sum_ram_gb as f64 / hw_samples;
    let avg_cpu_cores = cache.sum_cpu_cores as f64 / hw_samples;
    let avg_session_minutes = cache.total_playtime_minutes as f64 / total_sessions;

    let nvidia_pct = (cache.gpu_nvidia_count as f64 / hw_samples) * 100.0;
    let amd_pct = (cache.gpu_amd_count as f64 / hw_samples) * 100.0;
    let intel_pct = (cache.gpu_intel_count as f64 / hw_samples) * 100.0;

    let cpu_intel_pct = (cache.cpu_intel_count as f64 / hw_samples) * 100.0;
    let cpu_amd_pct = (cache.cpu_amd_count as f64 / hw_samples) * 100.0;

    let low_end_pct = (cache.low_end_mode_count as f64 / hw_samples) * 100.0;
    let discord_rpc_pct = (cache.discord_rpc_active_count as f64 / total_flushes) * 100.0;
    let custom_keybind_pct = (cache.keybind_custom_count as f64 / hw_samples) * 100.0;
    let custom_mic_pct = (cache.mic_custom_count as f64 / hw_samples) * 100.0;

    let mut top_game = "none".to_string();
    let mut top_game_minutes = 0u64;
    for (g_name, &mins) in &cache.game_playtime_minutes {
        if mins > top_game_minutes {
            top_game_minutes = mins;
            top_game = g_name.clone();
        }
    }

    let total_clips = cache.clips_auto_highlight
        + cache.clips_manual_replay
        + cache.clips_manual_direct;

    json!({
        "fields": {
            "avg_ram_gb": { "doubleValue": (avg_ram_gb * 10.0).round() / 10.0 },
            "ram_tier": { "stringValue": payload.ram_tier.clone() },
            "avg_cpu_cores": { "doubleValue": (avg_cpu_cores * 10.0).round() / 10.0 },
            "avg_game_session_minutes": { "doubleValue": (avg_session_minutes * 10.0).round() / 10.0 },
            "gpu_nvidia_share_pct": { "doubleValue": (nvidia_pct * 10.0).round() / 10.0 },
            "gpu_amd_share_pct": { "doubleValue": (amd_pct * 10.0).round() / 10.0 },
            "gpu_intel_share_pct": { "doubleValue": (intel_pct * 10.0).round() / 10.0 },
            "cpu_intel_share_pct": { "doubleValue": (cpu_intel_pct * 10.0).round() / 10.0 },
            "cpu_amd_share_pct": { "doubleValue": (cpu_amd_pct * 10.0).round() / 10.0 },
            "low_end_devices_share_pct": { "doubleValue": (low_end_pct * 10.0).round() / 10.0 },
            "discord_rpc_active_rate_pct": { "doubleValue": (discord_rpc_pct * 10.0).round() / 10.0 },
            "custom_keybind_adoption_pct": { "doubleValue": (custom_keybind_pct * 10.0).round() / 10.0 },
            "custom_mic_adoption_pct": { "doubleValue": (custom_mic_pct * 10.0).round() / 10.0 },
            "uses_custom_mic": { "booleanValue": payload.keybind_summary.uses_custom_mic },
            "active_resolution": { "stringValue": payload.capture_resolution.clone() },
            "active_fps": { "integerValue": payload.capture_fps.to_string() },
            "active_quality_preset": { "stringValue": payload.quality_preset.clone() },
            "active_audio_mode": { "stringValue": payload.audio_mode.clone() },
            "separate_audio_tracks_enabled": { "booleanValue": payload.separate_audio_tracks },
            "keybind_replay_category": { "stringValue": payload.keybind_summary.replay_key_category.clone() },
            "most_played_game": { "stringValue": top_game },
            "most_played_game_hours": { "doubleValue": ((top_game_minutes as f64 / 60.0) * 100.0).round() / 100.0 },
            "total_playtime_hours": { "doubleValue": ((cache.total_playtime_minutes as f64 / 60.0) * 100.0).round() / 100.0 },
            "total_game_sessions": { "integerValue": cache.total_game_sessions.to_string() },
            "total_clips_generated": { "integerValue": total_clips.to_string() },
            "clips_auto_highlight": { "integerValue": cache.clips_auto_highlight.to_string() },
            "clips_manual_replay": { "integerValue": cache.clips_manual_replay.to_string() },
            "clips_manual_direct": { "integerValue": cache.clips_manual_direct.to_string() },
            "clips_discord_compressed": { "integerValue": cache.clips_discord_compressed.to_string() },
            "editor_exports_total": { "integerValue": cache.editor_exports_total.to_string() },
            "app_version": { "stringValue": payload.app_version.clone() }
        }
    })
}

async fn post_aggregates_to_firebase(
    client: &reqwest::Client,
    cfg: &AnalyticsConfig,
    payload: &TelemetryPayload,
    include_hardware_profile: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let proj = cfg.firebase_project_id.trim();
    let coll = cfg.firebase_collection.trim();
    if proj.is_empty() {
        return Ok(());
    }

    let mut local_cache = LocalCumulativeCache::load();
    local_cache.absorb(payload, include_hardware_profile);
    local_cache.save();

    let key_suffix = cfg
        .firebase_api_key
        .as_ref()
        .map(|k| format!("?key={k}"))
        .unwrap_or_default();

    let commit_url = format!(
        "https://firestore.googleapis.com/v1/projects/{proj}/databases/(default)/documents:commit{key_suffix}"
    );
    let global_doc_path =
        format!("projects/{proj}/databases/(default)/documents/{coll}/global_aggregates");

    let mut global_transforms: Vec<Value> = Vec::new();
    global_transforms.push(make_inc_transform("telemetry_flushes_count", 1));

    if payload.discord_rpc_active {
        global_transforms.push(make_inc_transform("discord_rpc_active_count", 1));
    } else {
        global_transforms.push(make_inc_transform("discord_rpc_inactive_count", 1));
    }

    if include_hardware_profile {
        global_transforms.push(make_inc_transform("hardware_profiles_count", 1));
        global_transforms.push(make_inc_transform("sum_ram_gb", payload.ram_gb as i64));
        global_transforms.push(make_inc_transform(
            "sum_cpu_cores",
            payload.cpu_cores as i64,
        ));

        let gpu_field = match payload.gpu_vendor.as_str() {
            "NVIDIA" => "gpu_nvidia_count",
            "AMD" => "gpu_amd_count",
            "Intel" => "gpu_intel_count",
            _ => "gpu_other_count",
        };
        global_transforms.push(make_inc_transform(gpu_field, 1));

        let cpu_field = match payload.cpu_vendor.as_str() {
            "Intel" => "cpu_intel_count",
            "AMD" => "cpu_amd_count",
            _ => "cpu_other_count",
        };
        global_transforms.push(make_inc_transform(cpu_field, 1));

        let ram_bucket_field = match payload.ram_tier.as_str() {
            "4GB or less" | "8GB" => "ram_tier_8gb_or_less",
            "16GB" => "ram_tier_16gb",
            _ => "ram_tier_32gb_plus",
        };
        global_transforms.push(make_inc_transform(ram_bucket_field, 1));

        if payload.low_end_mode {
            global_transforms.push(make_inc_transform("low_end_mode_count", 1));
        } else {
            global_transforms.push(make_inc_transform("standard_mode_count", 1));
        }

        let res_slug = sanitize_firestore_key(&payload.capture_resolution);
        global_transforms.push(make_inc_transform(&format!("res_{res_slug}_count"), 1));
        global_transforms.push(make_inc_transform(
            &format!("fps_{}_count", payload.capture_fps),
            1,
        ));

        let qual_slug = sanitize_firestore_key(&payload.quality_preset);
        global_transforms.push(make_inc_transform(&format!("quality_{qual_slug}_count"), 1));

        let audio_slug = sanitize_firestore_key(&payload.audio_mode);
        global_transforms.push(make_inc_transform(
            &format!("audio_mode_{audio_slug}_count"),
            1,
        ));

        if payload.keybind_summary.any_custom() {
            global_transforms.push(make_inc_transform("keybind_custom_count", 1));
            let cat_slug = sanitize_firestore_key(&payload.keybind_summary.replay_key_category);
            global_transforms.push(make_inc_transform(
                &format!("keybind_cat_{cat_slug}"),
                1,
            ));
        } else {
            global_transforms.push(make_inc_transform("keybind_default_count", 1));
        }

        if payload.keybind_summary.uses_custom_mic {
            global_transforms.push(make_inc_transform("mic_custom_count", 1));
        } else {
            global_transforms.push(make_inc_transform("mic_default_count", 1));
        }

        if payload.separate_audio_tracks {
            global_transforms.push(make_inc_transform("audio_separate_stems_enabled_count", 1));
        }
    }

    if payload.auto_highlight_clips > 0 {
        global_transforms.push(make_inc_transform(
            "clips_auto_highlight",
            payload.auto_highlight_clips as i64,
        ));
    }
    if payload.manual_replay_clips > 0 {
        global_transforms.push(make_inc_transform(
            "clips_manual_replay",
            payload.manual_replay_clips as i64,
        ));
    }
    if payload.manual_direct_clips > 0 {
        global_transforms.push(make_inc_transform(
            "clips_manual_direct",
            payload.manual_direct_clips as i64,
        ));
    }
    if payload.discord_compressed_clips > 0 {
        global_transforms.push(make_inc_transform(
            "clips_discord_compressed",
            payload.discord_compressed_clips as i64,
        ));
    }
    if payload.editor_exports_count > 0 {
        global_transforms.push(make_inc_transform(
            "editor_exports_total",
            payload.editor_exports_count as i64,
        ));
    }
    if payload.encoder_zero_copy_fallbacks > 0 {
        global_transforms.push(make_inc_transform(
            "encoder_zero_copy_fallbacks",
            payload.encoder_zero_copy_fallbacks as i64,
        ));
    }
    if payload.encoder_software_fallbacks > 0 {
        global_transforms.push(make_inc_transform(
            "encoder_software_fallbacks",
            payload.encoder_software_fallbacks as i64,
        ));
    }

    let mut total_session_count = 0i64;
    let mut total_playtime_mins = 0i64;
    for s in &payload.game_sessions {
        let g_slug = sanitize_firestore_key(&s.game);
        let mins = (s.duration_minutes.max(1)) as i64;
        total_session_count += 1;
        total_playtime_mins += mins;
        global_transforms.push(make_inc_transform(
            &format!("playtime_minutes_{g_slug}"),
            mins,
        ));
        global_transforms.push(make_inc_transform(&format!("sessions_count_{g_slug}"), 1));
    }

    if total_session_count > 0 {
        global_transforms.push(make_inc_transform(
            "total_game_sessions",
            total_session_count,
        ));
        global_transforms.push(make_inc_transform(
            "total_playtime_minutes",
            total_playtime_mins,
        ));
    }

    let mut writes: Vec<Value> = vec![json!({
        "transform": {
            "document": global_doc_path,
            "fieldTransforms": global_transforms,
        }
    })];

    for (game_key, agg) in &payload.game_aggregates {
        let g_slug = sanitize_firestore_key(game_key);
        let game_doc_path =
            format!("projects/{proj}/databases/(default)/documents/{coll}/game_{g_slug}");
        let mut g_transforms = Vec::new();
        if agg.sessions_count > 0 {
            g_transforms.push(make_inc_transform(
                "sessions_count",
                agg.sessions_count as i64,
            ));
        }
        if agg.total_playtime_minutes > 0 {
            g_transforms.push(make_inc_transform(
                "total_playtime_minutes",
                agg.total_playtime_minutes as i64,
            ));
        }
        if agg.auto_highlights_count > 0 {
            g_transforms.push(make_inc_transform(
                "auto_highlights_count",
                agg.auto_highlights_count as i64,
            ));
        }
        if agg.manual_clips_count > 0 {
            g_transforms.push(make_inc_transform(
                "manual_clips_count",
                agg.manual_clips_count as i64,
            ));
        }
        if agg.kills_or_objectives_logged > 0 {
            g_transforms.push(make_inc_transform(
                "clutch_events_count",
                agg.kills_or_objectives_logged as i64,
            ));
        }
        if !g_transforms.is_empty() {
            writes.push(json!({
                "transform": {
                    "document": game_doc_path,
                    "fieldTransforms": g_transforms,
                }
            }));
        }
    }

    let _ = &local_cache;
    let commit_body = json!({ "writes": writes });
    let resp = client
        .post(&commit_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&commit_body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let err_txt = resp.text().await.unwrap_or_default();
        eprintln!(
            "[ANALYTICS WARN] Firebase rejected atomic commit (HTTP {status}): {err_txt}"
        );
        return Err(format!("Firebase returned HTTP {status}").into());
    }

    println!(
        "[ANALYTICS] Synced anonymous market averages to Firebase (project: {proj}, collection: {coll})"
    );
    Ok(())
}

pub struct AnalyticsHandle {
    shutdown_tx: watch::Sender<bool>,
    enabled: Arc<AtomicBool>,
    config: AnalyticsConfig,
    state: Arc<Mutex<AnalyticsState>>,
    system: Arc<Mutex<System>>,
    client: reqwest::Client,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
}

impl AnalyticsHandle {
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        let _ = persist_share_anonymous_stats(enabled);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_share_anonymous_stats(&self, enabled: bool) {
        self.set_enabled(enabled);
    }

    pub fn share_anonymous_stats(&self) -> bool {
        self.is_enabled()
    }

    pub async fn flush(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_enabled() {
            return Ok(());
        }

        let (
            sessions_to_send,
            game_aggs_to_send,
            payload,
            include_hw,
            saved_counters,
        ) = {
            let mut state = self.state.lock().await;
            if discord::is_discord_rpc_active() {
                state.discord_rpc_seen_active = true;
            }
            let now = Instant::now();
            let active_keys: Vec<String> = state.active_games.keys().cloned().collect();
            for g in active_keys {
                if let Some(start_inst) = state.active_games.get_mut(&g) {
                    let secs = now.saturating_duration_since(*start_inst).as_secs();
                    if secs >= 60 {
                        let mins = secs / 60;
                        *start_inst = now;
                        state.pending_sessions.push(GameSession {
                            game: g.clone(),
                            duration_minutes: mins,
                        });
                        let entry = state
                            .game_aggregates
                            .entry(g.clone())
                            .or_insert_with(|| GameAggregateStats {
                                game: g,
                                ..Default::default()
                            });
                        entry.total_playtime_minutes += mins;
                    }
                }
            }

            let has_any_activity = !state.pending_sessions.is_empty()
                || !state.game_aggregates.is_empty()
                || !state.hardware_reported_once
                || state.auto_highlight_clips > 0
                || state.manual_replay_clips > 0
                || state.manual_direct_clips > 0
                || state.discord_compressed_clips > 0
                || state.editor_exports_count > 0;

            if !has_any_activity {
                return Ok(());
            }

            let sessions = std::mem::take(&mut state.pending_sessions);
            let game_aggs = std::mem::take(&mut state.game_aggregates);
            let include_hw = !state.hardware_reported_once;

            let mut sys = self.system.lock().await;
            let payload = collect_snapshot(&mut sys, &state, sessions.clone(), game_aggs.clone());

            let counters = (
                state.auto_highlight_clips,
                state.manual_replay_clips,
                state.manual_direct_clips,
                state.discord_compressed_clips,
                state.editor_exports_count,
                state.encoder_zero_copy_fallbacks,
                state.encoder_software_fallbacks,
            );

            state.auto_highlight_clips = 0;
            state.manual_replay_clips = 0;
            state.manual_direct_clips = 0;
            state.discord_compressed_clips = 0;
            state.editor_exports_count = 0;
            state.encoder_zero_copy_fallbacks = 0;
            state.encoder_software_fallbacks = 0;
            state.hardware_reported_once = true;

            (sessions, game_aggs, payload, include_hw, counters)
        };

        if let Err(e) =
            post_aggregates_to_firebase(&self.client, &self.config, &payload, include_hw).await
        {
            let mut state = self.state.lock().await;
            state.pending_sessions.splice(0..0, sessions_to_send);
            for (k, v) in game_aggs_to_send {
                let entry = state.game_aggregates.entry(k).or_default();
                entry.sessions_count += v.sessions_count;
                entry.total_playtime_minutes += v.total_playtime_minutes;
                entry.auto_highlights_count += v.auto_highlights_count;
                entry.manual_clips_count += v.manual_clips_count;
                entry.kills_or_objectives_logged += v.kills_or_objectives_logged;
            }
            state.auto_highlight_clips += saved_counters.0;
            state.manual_replay_clips += saved_counters.1;
            state.manual_direct_clips += saved_counters.2;
            state.discord_compressed_clips += saved_counters.3;
            state.editor_exports_count += saved_counters.4;
            state.encoder_zero_copy_fallbacks += saved_counters.5;
            state.encoder_software_fallbacks += saved_counters.6;
            if include_hw {
                state.hardware_reported_once = false;
            }
            return Err(e);
        }

        Ok(())
    }

    pub async fn shutdown(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = self.shutdown_tx.send(true);
        let mut handle_lock = self.worker_handle.lock().await;
        if let Some(handle) = handle_lock.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }
        self.flush().await
    }

    pub async fn record_session(&self, game: impl Into<String>, duration_minutes: u64) {
        let g = game.into();
        let mut state = self.state.lock().await;
        state.pending_sessions.push(GameSession {
            game: g.clone(),
            duration_minutes,
        });
        let entry = state
            .game_aggregates
            .entry(g.clone())
            .or_insert_with(|| GameAggregateStats {
                game: g,
                ..Default::default()
            });
        entry.sessions_count += 1;
        entry.total_playtime_minutes += duration_minutes;
    }

    pub async fn current_snapshot(&self) -> TelemetryPayload {
        let mut sys = self.system.lock().await;
        let state = self.state.lock().await;
        collect_snapshot(
            &mut sys,
            &state,
            state.pending_sessions.clone(),
            state.game_aggregates.clone(),
        )
    }
}

pub struct AnalyticsCollector {
    config: AnalyticsConfig,
    enabled: Arc<AtomicBool>,
    client: reqwest::Client,
    system: Arc<Mutex<System>>,
    state: Arc<Mutex<AnalyticsState>>,
}

impl Default for AnalyticsCollector {
    fn default() -> Self {
        Self::new(AnalyticsConfig::load(), RecorderConfig::default())
    }
}

impl AnalyticsCollector {
    pub fn new(config: AnalyticsConfig, recorder_cfg: RecorderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut system = System::new();
        system.refresh_cpu_specifics(CpuRefreshKind::new());
        system.refresh_memory();

        let initial_enabled = config.enabled && recorder_cfg.share_anonymous_stats;
        let enabled = Arc::new(AtomicBool::new(initial_enabled));

        Self {
            config,
            enabled,
            client,
            system: Arc::new(Mutex::new(system)),
            state: Arc::new(Mutex::new(AnalyticsState {
                active_games: HashMap::new(),
                game_aggregates: HashMap::new(),
                pending_sessions: Vec::new(),
                auto_highlight_clips: 0,
                manual_replay_clips: 0,
                manual_direct_clips: 0,
                discord_compressed_clips: 0,
                editor_exports_count: 0,
                encoder_zero_copy_fallbacks: 0,
                encoder_software_fallbacks: 0,
                discord_rpc_seen_active: discord::is_discord_rpc_active(),
                recorder_cfg_snapshot: recorder_cfg,
                hardware_reported_once: false,
            })),
        }
    }

    pub fn start(self, mut rx: broadcast::Receiver<Event>) -> AnalyticsHandle {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let state_clone = Arc::clone(&self.state);
        let system_clone = Arc::clone(&self.system);
        let client_clone = self.client.clone();
        let enabled_clone = Arc::clone(&self.enabled);
        let cfg_clone = self.config.clone();

        let startup_state = Arc::clone(&self.state);
        let startup_sys = Arc::clone(&self.system);
        let startup_client = self.client.clone();
        let startup_cfg = self.config.clone();
        let startup_enabled = Arc::clone(&self.enabled);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            if !startup_enabled.load(Ordering::Acquire) {
                return;
            }
            let (payload, include_hw) = {
                let mut st = startup_state.lock().await;
                if st.hardware_reported_once {
                    return;
                }
                if discord::is_discord_rpc_active() {
                    st.discord_rpc_seen_active = true;
                }
                let mut sys = startup_sys.lock().await;
                let p = collect_snapshot(&mut sys, &st, Vec::new(), HashMap::new());
                st.hardware_reported_once = true;
                (p, true)
            };
            if post_aggregates_to_firebase(&startup_client, &startup_cfg, &payload, include_hw)
                .await
                .is_err()
            {
                let mut st = startup_state.lock().await;
                st.hardware_reported_once = false;
            }
        });

        let worker = tokio::spawn(async move {
            let flush_dur = Duration::from_secs(cfg_clone.flush_interval_secs.clamp(30, 86400));
            let mut ticker = interval_at(tokio::time::Instant::now() + flush_dur, flush_dur);

            loop {
                tokio::select! {
                    event_res = rx.recv() => {
                        match event_res {
                            Ok(event) => {
                                if matches!(
                                    event.kind.as_str(),
                                    "settings.privacy.share_anonymous_stats"
                                        | "privacy.share_anonymous_stats"
                                        | "telemetry.set_enabled"
                                        | "analytics.set_enabled"
                                ) {
                                    if let Some(en) = parse_privacy_toggle_event_value(&event.data) {
                                        enabled_clone.store(en, Ordering::Release);
                                        let mut st = state_clone.lock().await;
                                        st.recorder_cfg_snapshot.share_anonymous_stats = en;
                                        let _ = persist_share_anonymous_stats(en);
                                    }
                                    continue;
                                }

                                if matches!(event.kind.as_str(), "audio.set_mic_device" | "audio.mic_device_changed") {
                                    let dev_opt = event
                                        .data
                                        .get("device")
                                        .and_then(Value::as_str)
                                        .map(|s| s.trim())
                                        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("default") && !s.eq_ignore_ascii_case("auto"))
                                        .map(|s| s.to_string());
                                    let mut st = state_clone.lock().await;
                                    st.recorder_cfg_snapshot.mic_device = dev_opt;
                                }

                                if !enabled_clone.load(Ordering::Acquire) {
                                    continue;
                                }

                                let mut should_flush_now = false;
                                {
                                    let mut state = state_clone.lock().await;
                                    if discord::is_discord_rpc_active() {
                                        state.discord_rpc_seen_active = true;
                                    }
                                    match event.kind.as_str() {
                                        "game.started" => {
                                            let g = event.game.to_string();
                                            if !matches!(g.as_str(), "system" | "reader" | "engine" | "hotkey" | "Desktop") {
                                                let is_new = !state.active_games.contains_key(&g);
                                                state.active_games.entry(g.clone()).or_insert_with(Instant::now);
                                                if is_new {
                                                    state.pending_sessions.push(GameSession {
                                                        game: g.clone(),
                                                        duration_minutes: 1,
                                                    });
                                                    let entry = state.game_aggregates.entry(g.clone()).or_insert_with(|| GameAggregateStats {
                                                        game: g,
                                                        ..Default::default()
                                                    });
                                                    entry.sessions_count += 1;
                                                    entry.total_playtime_minutes += 1;
                                                    should_flush_now = true;
                                                }
                                            }
                                        }
                                        "game.stopped" => {
                                            let g = event.game.to_string();
                                            if let Some(start_time) = state.active_games.remove(&g) {
                                                let elapsed = start_time.elapsed().as_secs();
                                                let extra_minutes = (elapsed / 60).saturating_sub(1);
                                                if extra_minutes > 0 {
                                                    state.pending_sessions.push(GameSession {
                                                        game: g.clone(),
                                                        duration_minutes: extra_minutes,
                                                    });
                                                    let entry = state.game_aggregates.entry(g.clone()).or_insert_with(|| GameAggregateStats {
                                                        game: g,
                                                        ..Default::default()
                                                    });
                                                    entry.total_playtime_minutes += extra_minutes;
                                                }
                                                should_flush_now = true;
                                            }
                                        }
                                        "clip.saved" => {
                                            let ev_type = event.data.get("event").and_then(Value::as_str).unwrap_or("");
                                            let g = event.data.get("game").and_then(Value::as_str).unwrap_or(&event.game).to_string();
                                            if ev_type == "direct.clip" || ev_type == "manual.direct_clip" {
                                                state.manual_direct_clips += 1;
                                            } else if ev_type == "manual.clip" || ev_type == "replay.hotkey" {
                                                state.manual_replay_clips += 1;
                                                let entry = state.game_aggregates.entry(g.clone()).or_insert_with(|| GameAggregateStats {
                                                    game: g,
                                                    ..Default::default()
                                                });
                                                entry.manual_clips_count += 1;
                                            } else {
                                                state.auto_highlight_clips += 1;
                                                let entry = state.game_aggregates.entry(g.clone()).or_insert_with(|| GameAggregateStats {
                                                    game: g,
                                                    ..Default::default()
                                                });
                                                entry.auto_highlights_count += 1;
                                            }
                                            should_flush_now = true;
                                        }
                                        "discord.compress_done" => {
                                            state.discord_compressed_clips += 1;
                                            should_flush_now = true;
                                        }
                                        "editor.job_done" | "editor.split_done" | "editor.stems_exported" => {
                                            state.editor_exports_count += 1;
                                            should_flush_now = true;
                                        }
                                        "recorder.encoder_fallback" => {
                                            let kind = event.data.get("kind").and_then(Value::as_str).unwrap_or("");
                                            if kind == "zero_copy_to_cpu_staging" {
                                                state.encoder_zero_copy_fallbacks += 1;
                                            } else if kind == "hardware_to_software_mft" {
                                                state.encoder_software_fallbacks += 1;
                                            }
                                        }
                                        "stats.match_recorded" => {
                                            if let Some(m) = event.data.get("match") {
                                                let g = m.get("game").and_then(Value::as_str).unwrap_or(&event.game).to_string();
                                                let entry = state.game_aggregates.entry(g.clone()).or_insert_with(|| GameAggregateStats {
                                                    game: g,
                                                    ..Default::default()
                                                });
                                                entry.matches_completed += 1;
                                            }
                                        }
                                        k if k.starts_with("player.") || k.starts_with("sport.") || k.starts_with("bomb.") || k.starts_with("objective.") => {
                                            let g = event.game.to_string();
                                            let entry = state.game_aggregates.entry(g.clone()).or_insert_with(|| GameAggregateStats {
                                                game: g,
                                                ..Default::default()
                                            });
                                            if k == "player.death" || k == "sport.goal_conceded" {
                                                entry.deaths_logged += 1;
                                            } else {
                                                entry.kills_or_objectives_logged += 1;
                                            }
                                            if k == "player.clutch" || k == "player.round_multikill_win" {
                                                entry.clutches_logged += 1;
                                            }
                                        }
                                        _ => {}
                                    }
                                }

                                if should_flush_now {
                                    let (sessions, game_aggs, payload, include_hw) = {
                                        let mut state = state_clone.lock().await;
                                        let sessions = std::mem::take(&mut state.pending_sessions);
                                        let game_aggs = std::mem::take(&mut state.game_aggregates);
                                        let include_hw = !state.hardware_reported_once;
                                        let mut sys = system_clone.lock().await;
                                        let payload = collect_snapshot(&mut sys, &state, sessions.clone(), game_aggs.clone());
                                        state.auto_highlight_clips = 0;
                                        state.manual_replay_clips = 0;
                                        state.manual_direct_clips = 0;
                                        state.discord_compressed_clips = 0;
                                        state.editor_exports_count = 0;
                                        state.encoder_zero_copy_fallbacks = 0;
                                        state.encoder_software_fallbacks = 0;
                                        state.hardware_reported_once = true;
                                        (sessions, game_aggs, payload, include_hw)
                                    };
                                    if post_aggregates_to_firebase(&client_clone, &cfg_clone, &payload, include_hw).await.is_err() {
                                        let mut state = state_clone.lock().await;
                                        state.pending_sessions.splice(0..0, sessions);
                                        for (k, v) in game_aggs {
                                            let entry = state.game_aggregates.entry(k).or_default();
                                            entry.sessions_count += v.sessions_count;
                                            entry.total_playtime_minutes += v.total_playtime_minutes;
                                            entry.auto_highlights_count += v.auto_highlights_count;
                                            entry.manual_clips_count += v.manual_clips_count;
                                            entry.kills_or_objectives_logged += v.kills_or_objectives_logged;
                                        }
                                        if include_hw {
                                            state.hardware_reported_once = false;
                                        }
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = ticker.tick() => {
                        if !enabled_clone.load(Ordering::Acquire) {
                            continue;
                        }
                        let (sessions, game_aggs, payload, include_hw) = {
                            let mut state = state_clone.lock().await;
                            if state.pending_sessions.is_empty() && state.game_aggregates.is_empty() && state.hardware_reported_once {
                                continue;
                            }
                            let sessions = std::mem::take(&mut state.pending_sessions);
                            let game_aggs = std::mem::take(&mut state.game_aggregates);
                            let include_hw = !state.hardware_reported_once;
                            let mut sys = system_clone.lock().await;
                            let payload = collect_snapshot(&mut sys, &state, sessions.clone(), game_aggs.clone());
                            state.auto_highlight_clips = 0;
                            state.manual_replay_clips = 0;
                            state.manual_direct_clips = 0;
                            state.discord_compressed_clips = 0;
                            state.editor_exports_count = 0;
                            state.encoder_zero_copy_fallbacks = 0;
                            state.encoder_software_fallbacks = 0;
                            state.hardware_reported_once = true;
                            (sessions, game_aggs, payload, include_hw)
                        };

                        if post_aggregates_to_firebase(&client_clone, &cfg_clone, &payload, include_hw).await.is_err() {
                            let mut state = state_clone.lock().await;
                            state.pending_sessions.splice(0..0, sessions);
                            for (k, v) in game_aggs {
                                let entry = state.game_aggregates.entry(k).or_default();
                                entry.sessions_count += v.sessions_count;
                                entry.total_playtime_minutes += v.total_playtime_minutes;
                                entry.auto_highlights_count += v.auto_highlights_count;
                                entry.manual_clips_count += v.manual_clips_count;
                                entry.kills_or_objectives_logged += v.kills_or_objectives_logged;
                            }
                            if include_hw {
                                state.hardware_reported_once = false;
                            }
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_ok() && *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        AnalyticsHandle {
            shutdown_tx,
            enabled: self.enabled,
            config: self.config,
            state: self.state,
            system: self.system,
            client: self.client,
            worker_handle: Mutex::new(Some(worker)),
        }
    }
}

pub fn start_analytics(rx: broadcast::Receiver<Event>) -> AnalyticsHandle {
    AnalyticsCollector::default().start(rx)
}

pub fn start_analytics_with_config(
    rx: broadcast::Receiver<Event>,
    recorder_cfg: &RecorderConfig,
    override_enabled: Option<bool>,
) -> AnalyticsHandle {
    let mut cfg = AnalyticsConfig::load();
    if !recorder_cfg.share_anonymous_stats {
        cfg.enabled = false;
    }
    if let Some(en) = override_enabled {
        cfg.enabled = en;
    }
    let mut rec_clone = recorder_cfg.clone();
    rec_clone.share_anonymous_stats = cfg.enabled;
    AnalyticsCollector::new(cfg, rec_clone).start(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::config::Keybind;

    #[tokio::test]
    async fn test_session_tracking_and_opt_out_toggle() {
        let (_tx, rx) = broadcast::channel(16);
        let mut cfg = RecorderConfig::default();
        cfg.share_anonymous_stats = true;
        let handle = start_analytics_with_config(rx, &cfg, Some(true));
        assert!(handle.is_enabled());
        handle.record_session("cs2", 35).await;
        let snapshot = handle.current_snapshot().await;
        assert_eq!(snapshot.game_sessions.len(), 1);
        assert_eq!(snapshot.game_sessions[0].game, "cs2");
        assert_eq!(snapshot.game_sessions[0].duration_minutes, 35);
        assert!(!snapshot.keybind_summary.any_custom());
        assert!(!snapshot.keybind_summary.uses_custom_mic);

        handle.set_share_anonymous_stats(false);
        assert!(!handle.share_anonymous_stats());
    }

    #[test]
    fn test_custom_keybind_and_mic_detection() {
        let mut cfg = RecorderConfig::default();
        let initial = KeybindTelemetrySummary::from_config(&cfg);
        assert!(!initial.any_custom());
        assert!(!initial.uses_custom_mic);

        cfg.keybinds.replay_clip = Keybind::new("Alt+X", 0x58, 1);
        cfg.mic_device = Some("HyperX QuadCast".to_string());
        let summary = KeybindTelemetrySummary::from_config(&cfg);
        assert!(summary.any_custom());
        assert!(summary.uses_custom_replay_key);
        assert!(summary.uses_custom_mic);
        assert_eq!(summary.replay_key_category, "Modifier+AlphaOrSpecial");
    }

    #[test]
    fn test_ram_and_cpu_tiers() {
        assert_eq!(ram_tier(7000), "8GB");
        assert_eq!(ram_tier(15000), "16GB");
        assert_eq!(ram_tier(32000), "32GB");
        assert_eq!(cpu_tier("Intel Core i9", 16), "Intel High-end");
        assert_eq!(cpu_tier("AMD Ryzen 5", 6), "AMD Entry-level");
    }
}