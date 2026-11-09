use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui::{
    self, Align, Color32, ColorImage, CursorIcon, FontData, FontDefinitions,
    FontFamily, FontId, Id, Key, Layout, Margin, Modifiers, Pos2, Rect, RichText, ScrollArea,
    Rounding, Sense, Stroke, TextureHandle, TextureOptions, UiBuilder, Vec2,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::audio_engine::AudioInputDeviceInfo;
use crate::bridge::UiBridge;
use crate::editor::{
    default_edited_output_path, AudioStemMixConfig, CutMode, EditJob,
};
use crate::events::Event;
use crate::recorder::config::{
    AudioMode, Container, Fps, Keybind, KeybindsConfig, Quality, StorageMode,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewTab {
    Clips,
    Editor,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimActionMode {
    Keep,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppearancePreference {
    System,
    Light,
    Dark,
}

impl AppearancePreference {
    pub fn from_config_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClipSidecarMeta {
    #[serde(default)]
    pub display_title: String,
    #[serde(default)]
    pub game: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub event_kind: String,
    #[serde(default)]
    pub map: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub weapon: String,
    #[serde(default)]
    pub kills: Option<u32>,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub created_epoch_secs: u64,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub has_separate_stems: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedClipIndexEntry {
    pub path_str: String,
    pub file_size_bytes: u64,
    pub mtime_secs: u64,
    pub duration_secs: f64,
    pub has_separate_stems: bool,
    pub display_title: String,
    pub game: String,
    pub tag: String,
    #[serde(default)]
    pub map: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub weapon: String,
    #[serde(default)]
    pub kills: Option<u32>,
    pub favorite: bool,
}

#[derive(Clone, Debug)]
pub struct ClipItem {
    pub id: u64,
    pub display_title: String,
    pub game: String,
    pub tag: String,
    pub map: String,
    pub mode: String,
    pub weapon: String,
    pub kills: Option<u32>,
    pub date_label: String,
    pub duration_secs: f64,
    pub duration_label: String,
    pub favorite: bool,
    pub has_separate_stems: bool,
    pub file_path: PathBuf,
    pub thumb_path: PathBuf,
    pub mtime_secs: u64,
}

#[derive(Clone, Debug)]
pub enum PreviewWorkerCmd {
    DecodeAt {
        target_ms: u64,
        width: u32,
        height: u32,
    },
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct AudioStemTrackState {
    pub id: &'static str,
    pub label: &'static str,
    pub enabled: bool,
    pub solo: bool,
    pub volume_pct: u32,
}

#[derive(Clone)]
pub struct EditorViewState {
    pub clip_id: u64,
    pub trim_start_secs: f64,
    pub trim_end_secs: f64,
    pub playhead_secs: f64,
    pub dragging_handle: Option<&'static str>,
    pub trim_mode: TrimActionMode,
    pub cut_mode: CutMode,
    pub splits_secs: Vec<f64>,
    pub keyframes_secs: Vec<f64>,
    pub resolution: String,
    pub fps: String,
    pub quality: String,
    pub remove_all_audio: bool,
    pub keep_separate_tracks: bool,
    pub stems: Vec<AudioStemTrackState>,
    pub busy: bool,
    pub status_line: String,
    pub cancel_flag: Option<Arc<AtomicBool>>,
    pub last_preview_req_ms: Option<u64>,
    pub pending_preview_ms: Option<(u64, Instant)>,
    pub preview_texture: Option<(u64, TextureHandle)>,
    pub is_playing: bool,
    pub last_play_tick: Option<Instant>,
    pub fullscreen_preview: bool,
    pub dragging_split_idx: Option<usize>,
    pub preview_worker_tx: Option<Sender<PreviewWorkerCmd>>,
}

impl EditorViewState {
    pub fn from_clip(clip: &ClipItem, keyframes_secs: Vec<f64>) -> Self {
        let dur = clip.duration_secs.max(1.0);
        Self {
            clip_id: clip.id,
            trim_start_secs: 0.0,
            trim_end_secs: dur,
            playhead_secs: 0.0,
            dragging_handle: None,
            trim_mode: TrimActionMode::Keep,
            cut_mode: CutMode::KeyframeSnap,
            splits_secs: Vec::new(),
            keyframes_secs,
            resolution: "Original".to_string(),
            fps: "Original".to_string(),
            quality: "High".to_string(),
            remove_all_audio: false,
            keep_separate_tracks: clip.has_separate_stems,
            stems: vec![
                AudioStemTrackState {
                    id: "game",
                    label: "Game",
                    enabled: true,
                    solo: false,
                    volume_pct: 100,
                },
                AudioStemTrackState {
                    id: "discord",
                    label: "Discord",
                    enabled: true,
                    solo: false,
                    volume_pct: 100,
                },
                AudioStemTrackState {
                    id: "mic",
                    label: "Microphone",
                    enabled: true,
                    solo: false,
                    volume_pct: 100,
                },
                AudioStemTrackState {
                    id: "other",
                    label: "Music / Browser",
                    enabled: true,
                    solo: false,
                    volume_pct: 80,
                },
            ],
            busy: false,
            status_line: String::new(),
            cancel_flag: None,
            last_preview_req_ms: None,
            pending_preview_ms: Some((0, Instant::now())),
            preview_texture: None,
            is_playing: false,
            last_play_tick: None,
            fullscreen_preview: false,
            dragging_split_idx: None,
            preview_worker_tx: None,
        }
    }

    pub fn snap_to_keyframe(&self, t: f64, duration: f64) -> f64 {
        let clamped = t.clamp(0.0, duration);
        if self.cut_mode == CutMode::FrameExact || self.keyframes_secs.is_empty() {
            return (clamped * 100.0).round() / 100.0;
        }
        let mut best = self.keyframes_secs[0];
        let mut best_dist = (clamped - best).abs();
        for &kf in &self.keyframes_secs {
            let d = (clamped - kf).abs();
            if d < best_dist {
                best = kf;
                best_dist = d;
            }
        }
        best.clamp(0.0, duration)
    }

    pub fn build_audio_mix_config(&self, clip_has_stems: bool) -> AudioStemMixConfig {
        if self.remove_all_audio {
            return AudioStemMixConfig::mute_all();
        }
        if !clip_has_stems {
            let mut full = AudioStemMixConfig::default();
            full.keep_stem_tracks_in_output = false;
            return full;
        }
        let any_solo = self.stems.iter().any(|s| s.solo);
        let mut cfg = AudioStemMixConfig {
            keep_stem_tracks_in_output: self.keep_separate_tracks,
            ..AudioStemMixConfig::default()
        };
        for st in &self.stems {
            let active = if any_solo { st.solo } else { st.enabled };
            let gain = (st.volume_pct as f32 / 100.0).clamp(0.0, 2.0);
            match st.id {
                "game" => {
                    cfg.game_enabled = active;
                    cfg.game_gain = gain;
                }
                "discord" => {
                    cfg.discord_enabled = active;
                    cfg.discord_gain = gain;
                }
                "mic" => {
                    cfg.mic_enabled = active;
                    cfg.mic_gain = gain;
                }
                "other" => {
                    cfg.other_enabled = active;
                    cfg.other_gain = gain;
                }
                _ => {}
            }
        }
        cfg
    }
}

#[derive(Clone, Debug)]
pub enum UiAsyncMessage {
    EngineEvent(Event),
    LibraryScanned(Vec<ClipItem>),
    SingleClipDiscovered(ClipItem),
    ThumbnailLoaded {
        clip_id: u64,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    EditorKeyframesLoaded {
        clip_id: u64,
        keyframes_secs: Vec<f64>,
    },
    EditorPreviewLoaded {
        clip_id: u64,
        target_ms: u64,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    EditorJobProgress(String),
    EditorJobFinished {
        message: String,
        reveal_path: Option<PathBuf>,
    },
    DiscordCompressFinished {
        clip_id: u64,
        message: String,
        reveal_path: Option<PathBuf>,
    },
    CustomSaveFolderPicked(PathBuf),
}

#[derive(Clone, Copy, Debug)]
struct ThemeColors {
    bg: Color32,
    surface: Color32,
    surface_2: Color32,
    border: Color32,
    border_strong: Color32,
    text: Color32,
    text_2: Color32,
    text_3: Color32,
    accent: Color32,
    accent_text: Color32,
    accent_soft: Color32,
    danger: Color32,
}

impl ThemeColors {
    fn light() -> Self {
        Self {
            bg: Color32::from_rgb(246, 245, 242),
            surface: Color32::from_rgb(255, 255, 255),
            surface_2: Color32::from_rgb(239, 237, 232),
            border: Color32::from_rgb(226, 223, 216),
            border_strong: Color32::from_rgb(201, 196, 186),
            text: Color32::from_rgb(22, 22, 21),
            text_2: Color32::from_rgb(107, 105, 99),
            text_3: Color32::from_rgb(154, 151, 143),
            accent: Color32::from_rgb(43, 92, 230),
            accent_text: Color32::from_rgb(255, 255, 255),
            accent_soft: Color32::from_rgba_unmultiplied(43, 92, 230, 22),
            danger: Color32::from_rgb(194, 59, 42),
        }
    }

    fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(17, 18, 20),
            surface: Color32::from_rgb(23, 25, 28),
            surface_2: Color32::from_rgb(31, 34, 38),
            border: Color32::from_rgb(38, 41, 46),
            border_strong: Color32::from_rgb(54, 58, 66),
            text: Color32::from_rgb(238, 238, 236),
            text_2: Color32::from_rgb(142, 146, 153),
            text_3: Color32::from_rgb(92, 96, 104),
            accent: Color32::from_rgb(107, 150, 255),
            accent_text: Color32::from_rgb(14, 18, 28),
            accent_soft: Color32::from_rgba_unmultiplied(107, 150, 255, 28),
            danger: Color32::from_rgb(232, 100, 82),
        }
    }
}

struct ThumbnailLruCache {
    map: HashMap<u64, TextureHandle>,
    order: VecDeque<u64>,
    capacity: usize,
}

impl ThumbnailLruCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity: capacity.max(8),
        }
    }

    fn get(&mut self, clip_id: u64) -> Option<&TextureHandle> {
        if self.map.contains_key(&clip_id) {
            self.order.retain(|&k| k != clip_id);
            self.order.push_back(clip_id);
            self.map.get(&clip_id)
        } else {
            None
        }
    }

    fn insert(&mut self, clip_id: u64, handle: TextureHandle) {
        if self.map.contains_key(&clip_id) {
            self.order.retain(|&k| k != clip_id);
        } else if self.map.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
        self.order.push_back(clip_id);
        self.map.insert(clip_id, handle);
    }

    fn remove(&mut self, clip_id: u64) {
        self.map.remove(&clip_id);
        self.order.retain(|&k| k != clip_id);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

struct EditorBusyResetGuard {
    tx: Sender<UiAsyncMessage>,
    ctx: egui::Context,
    completed_cleanly: bool,
}

impl Drop for EditorBusyResetGuard {
    fn drop(&mut self) {
        if !self.completed_cleanly {
            let _ = self.tx.send(UiAsyncMessage::EditorJobFinished {
                message: "Edit failed or was interrupted.".to_string(),
                reveal_path: None,
            });
            self.ctx.request_repaint();
        }
    }
}

pub struct ClotchGuiApp {
    bridge: UiBridge,
    async_tx: Sender<UiAsyncMessage>,
    async_rx: Receiver<UiAsyncMessage>,
    appearance: AppearancePreference,
    last_effective_dark: Option<bool>,
    view: ViewTab,
    filter: String,
    search: String,
    status_text: String,
    is_recording_active: bool,
    clips: Vec<ClipItem>,
    filtered_indices: Vec<usize>,
    available_games: Vec<String>,
    filters_dirty: bool,
    scan_in_progress: bool,
    thumb_cache: ThumbnailLruCache,
    thumb_inflight: HashSet<u64>,
    compressing_clips: HashSet<u64>,
    renaming_clip_id: Option<u64>,
    rename_buffer: String,
    rename_focus_requested: bool,
    confirm_delete_clip_id: Option<u64>,
    editor_state: Option<EditorViewState>,
    recording_keybind_slot: Option<&'static str>,
    keybind_error_msg: Option<String>,
    keybinds_custom_mode: bool,
    custom_path_buffer: String,
    folder_validation_status: Option<Result<f64, String>>,
    microphones: Vec<AudioInputDeviceInfo>,
    selected_mic_missing: Option<String>,
    toast: Option<(String, Option<PathBuf>, Instant)>,
    was_minimized: bool,
    active_clips_dir: PathBuf,
}

impl ClotchGuiApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        bridge: UiBridge,
        mut event_rx: broadcast::Receiver<Event>,
    ) -> Self {
        setup_unicode_system_fonts(&cc.egui_ctx);

        let (async_tx, async_rx) = unbounded();
        let tx_events = async_tx.clone();
        let ctx_events = cc.egui_ctx.clone();

        let _ = std::thread::Builder::new()
            .name("clotch-gui-events".to_string())
            .spawn(move || {
                while let Ok(ev) = event_rx.blocking_recv() {
                    if tx_events.send(UiAsyncMessage::EngineEvent(ev)).is_err() {
                        break;
                    }
                    ctx_events.request_repaint();
                }
            });

        let cfg = bridge.recorder_config();
        let default_kb = KeybindsConfig::default();
        let is_custom_kb = cfg.keybinds.replay_clip.label != default_kb.replay_clip.label
            || cfg.keybinds.direct_clip.label != default_kb.direct_clip.label
            || cfg.keybinds.discord_clip.label != default_kb.discord_clip.label
            || cfg.keybinds.mic_mute.label != default_kb.mic_mute.label;

        let initial_clips_dir = bridge.resolve_active_clips_dir();
        let custom_path_str = match &cfg.storage.mode {
            StorageMode::AutoPartition => initial_clips_dir.to_string_lossy().to_string(),
            StorageMode::Custom(p) => p.to_string_lossy().to_string(),
        };

        let appearance = AppearancePreference::from_config_str(&bridge.appearance_mode());
        let mics = bridge.list_microphones();
        let selected_mic_missing = check_missing_microphone(&cfg, &mics);
        let folder_validation_status = match &cfg.storage.mode {
            StorageMode::Custom(p) => Some(bridge.validate_and_query_folder_free_gb(p)),
            StorageMode::AutoPartition => {
                let p = bridge.resolve_active_clips_dir();
                Some(bridge.validate_and_query_folder_free_gb(&p))
            }
        };

        let mut app = Self {
            bridge,
            async_tx,
            async_rx,
            appearance,
            last_effective_dark: None,
            view: ViewTab::Clips,
            filter: "All".to_string(),
            search: String::new(),
            status_text: "Ready".to_string(),
            is_recording_active: false,
            clips: Vec::new(),
            filtered_indices: Vec::new(),
            available_games: Vec::new(),
            filters_dirty: true,
            scan_in_progress: false,
            thumb_cache: ThumbnailLruCache::new(60),
            thumb_inflight: HashSet::new(),
            compressing_clips: HashSet::new(),
            renaming_clip_id: None,
            rename_buffer: String::new(),
            rename_focus_requested: false,
            confirm_delete_clip_id: None,
            editor_state: None,
            recording_keybind_slot: None,
            keybind_error_msg: None,
            keybinds_custom_mode: is_custom_kb,
            custom_path_buffer: custom_path_str,
            folder_validation_status,
            microphones: mics,
            selected_mic_missing,
            toast: None,
            was_minimized: false,
            active_clips_dir: initial_clips_dir,
        };

        app.clips = load_cached_clips_from_index();
        app.filters_dirty = true;
        app.rebuild_filters_and_games_if_needed();
        app.spawn_background_library_scan(&cc.egui_ctx);
        let effective_dark = app.resolve_effective_dark(&cc.egui_ctx);
        app.apply_theme(&cc.egui_ctx, effective_dark);
        app
    }

    fn resolve_effective_dark(&self, ctx: &egui::Context) -> bool {
        match self.appearance {
            AppearancePreference::Dark => true,
            AppearancePreference::Light => false,
            AppearancePreference::System => match ctx.system_theme() {
                Some(egui::Theme::Light) => false,
                Some(egui::Theme::Dark) | None => true,
            },
        }
    }

    fn colors(&self, ctx: &egui::Context) -> ThemeColors {
        if self.resolve_effective_dark(ctx) {
            ThemeColors::dark()
        } else {
            ThemeColors::light()
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context, dark: bool) {
        self.last_effective_dark = Some(dark);
        let c = if dark {
            ThemeColors::dark()
        } else {
            ThemeColors::light()
        };
        let mut visuals = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.panel_fill = c.bg;
        visuals.window_fill = c.surface;
        visuals.faint_bg_color = c.surface_2;
        visuals.extreme_bg_color = c.surface;
        visuals.override_text_color = Some(c.text);
        visuals.selection.bg_fill = c.accent;
        visuals.selection.stroke = Stroke::new(1.0_f32, c.accent_text);
        visuals.widgets.noninteractive.bg_fill = c.surface;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, c.border);
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, c.text);
        visuals.widgets.inactive.bg_fill = c.surface;
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, c.border);
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, c.text);
        visuals.widgets.hovered.bg_fill = c.surface_2;
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, c.border_strong);
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, c.text);
        visuals.widgets.active.bg_fill = c.accent;
        visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, c.accent);
        visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, c.accent_text);
        ctx.set_visuals(visuals);
    }

    fn show_toast(&mut self, msg: impl Into<String>, reveal_path: Option<PathBuf>) {
        self.toast = Some((msg.into(), reveal_path, Instant::now()));
    }

    fn spawn_background_library_scan(&mut self, ctx: &egui::Context) {
        if self.scan_in_progress {
            return;
        }
        self.scan_in_progress = true;
        let clips_dir = self.bridge.resolve_active_clips_dir();
        self.active_clips_dir = clips_dir.clone();
        let bridge = self.bridge.clone();
        let tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();

        let _ = std::thread::Builder::new()
            .name("clotch-clip-scan".to_string())
            .spawn(move || {
                let items = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scan_clips_directory_worker(&clips_dir, &bridge)
                }))
                .unwrap_or_default();
                let _ = tx.send(UiAsyncMessage::LibraryScanned(items));
                ctx_clone.request_repaint();
            });
    }

    fn spawn_single_clip_probe(&self, path: PathBuf, ctx: &egui::Context) {
        let bridge = self.bridge.clone();
        let tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("clotch-clip-probe".to_string())
            .spawn(move || {
                let thumbs_dir = resolve_thumbs_cache_dir();
                let mut empty_index = HashMap::new();
                if let Some(item) =
                    build_clip_item_from_path(&path, &bridge, &thumbs_dir, &mut empty_index)
                {
                    let _ = tx.send(UiAsyncMessage::SingleClipDiscovered(item));
                    ctx_clone.request_repaint();
                }
            });
    }

    fn request_thumbnail_if_needed(&mut self, clip: &ClipItem, ctx: &egui::Context) {
        if self.thumb_cache.map.contains_key(&clip.id) || self.thumb_inflight.contains(&clip.id) {
            return;
        }
        self.thumb_inflight.insert(clip.id);
        let clip_id = clip.id;
        let video_path = clip.file_path.clone();
        let thumb_path = clip.thumb_path.clone();
        let dur_secs = clip.duration_secs;
        let bridge = self.bridge.clone();
        let tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();

        let _ = std::thread::Builder::new()
            .name("clotch-thumb-gen".to_string())
            .spawn(move || {
                let target_w = 320u32;
                let target_h = 180u32;
                let expected_bytes = (target_w as usize) * (target_h as usize) * 4;
                if let Ok(cached_bytes) = std::fs::read(&thumb_path) {
                    if cached_bytes.len() == expected_bytes {
                        let _ = tx.send(UiAsyncMessage::ThumbnailLoaded {
                            clip_id,
                            width: target_w,
                            height: target_h,
                            rgba: cached_bytes,
                        });
                        ctx_clone.request_repaint();
                        return;
                    }
                }
                let sample_ms = ((dur_secs * 250.0) as u64).min(2_500);
                if let Ok(rgba) =
                    bridge.extract_frame_rgba(&video_path, sample_ms, target_w, target_h)
                {
                    if rgba.len() == expected_bytes {
                        if let Some(parent) = thumb_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(&thumb_path, &rgba);
                        let _ = tx.send(UiAsyncMessage::ThumbnailLoaded {
                            clip_id,
                            width: target_w,
                            height: target_h,
                            rgba,
                        });
                        ctx_clone.request_repaint();
                    }
                }
            });
    }

    fn request_editor_keyframes(&self, clip_id: u64, video_path: PathBuf, ctx: &egui::Context) {
        let bridge = self.bridge.clone();
        let tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("clotch-keyframes".to_string())
            .spawn(move || {
                if let Ok(meta) = bridge.probe_clip(&video_path) {
                    let kf: Vec<f64> = meta
                        .keyframe_timestamps_ms
                        .iter()
                        .map(|&ms| ms as f64 / 1000.0)
                        .collect();
                    let _ = tx.send(UiAsyncMessage::EditorKeyframesLoaded {
                        clip_id,
                        keyframes_secs: kf,
                    });
                    ctx_clone.request_repaint();
                }
            });
    }

    fn ensure_editor_preview_worker(
        &mut self,
        clip_id: u64,
        video_path: PathBuf,
        ctx: &egui::Context,
    ) -> Option<Sender<PreviewWorkerCmd>> {
        let Some(ref mut ed) = self.editor_state else {
            return None;
        };
        if ed.clip_id != clip_id {
            return None;
        }
        if let Some(ref tx) = ed.preview_worker_tx {
            return Some(tx.clone());
        }
        let (cmd_tx, cmd_rx) = unbounded::<PreviewWorkerCmd>();
        ed.preview_worker_tx = Some(cmd_tx.clone());
        let bridge = self.bridge.clone();
        let ui_tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();

        let _ = std::thread::Builder::new()
            .name("clotch-preview-session".to_string())
            .spawn(move || {
                let mut session_opt = bridge.open_preview_session(&video_path).ok();
                while let Ok(cmd) = cmd_rx.recv() {
                    let mut latest_decode = match cmd {
                        PreviewWorkerCmd::Shutdown => break,
                        PreviewWorkerCmd::DecodeAt {
                            target_ms,
                            width,
                            height,
                        } => (target_ms, width, height),
                    };
                    while let Ok(newer) = cmd_rx.try_recv() {
                        match newer {
                            PreviewWorkerCmd::Shutdown => return,
                            PreviewWorkerCmd::DecodeAt {
                                target_ms,
                                width,
                                height,
                            } => {
                                latest_decode = (target_ms, width, height);
                            }
                        }
                    }
                    let (target_ms, w, h) = latest_decode;
                    let decoded_res = if let Some(ref mut sess) = session_opt {
                        sess.decode_frame_rgba(target_ms, w, h)
                    } else {
                        bridge
                            .extract_frame_rgba(&video_path, target_ms, w, h)
                            .map(|rgba| (target_ms, rgba))
                    };
                    if let Ok((actual_ms, rgba)) = decoded_res {
                        let _ = ui_tx.send(UiAsyncMessage::EditorPreviewLoaded {
                            clip_id,
                            target_ms: actual_ms,
                            width: w,
                            height: h,
                            rgba,
                        });
                        ctx_clone.request_repaint();
                    }
                }
            });

        Some(cmd_tx)
    }

    fn request_editor_preview_frame(
        &mut self,
        clip_id: u64,
        video_path: PathBuf,
        target_ms: u64,
        ctx: &egui::Context,
    ) {
        if let Some(cmd_tx) = self.ensure_editor_preview_worker(clip_id, video_path, ctx) {
            let _ = cmd_tx.send(PreviewWorkerCmd::DecodeAt {
                target_ms,
                width: 640,
                height: 360,
            });
        }
    }

    fn close_editor_session(&mut self) {
        if let Some(mut ed) = self.editor_state.take() {
            ed.is_playing = false;
            if let Some(tx) = ed.preview_worker_tx.take() {
                let _ = tx.send(PreviewWorkerCmd::Shutdown);
            }
        }
        trim_process_working_set();
    }

    fn rebuild_filters_and_games_if_needed(&mut self) {
        if !self.filters_dirty {
            return;
        }
        self.filters_dirty = false;

        let mut games_set: Vec<String> = Vec::new();
        for c in &self.clips {
            if !c.game.is_empty() && !games_set.iter().any(|g| g.eq_ignore_ascii_case(&c.game)) {
                games_set.push(c.game.clone());
            }
        }
        games_set.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        self.available_games = games_set;

        if self.filter != "All"
            && self.filter != "Favorites"
            && !self
                .available_games
                .iter()
                .any(|g| g.eq_ignore_ascii_case(&self.filter))
        {
            self.filter = "All".to_string();
        }

        let q = self.search.trim();
        let mut scored_indices: Vec<(u32, u64, usize)> = Vec::with_capacity(self.clips.len());
        for (idx, clip) in self.clips.iter().enumerate() {
            let match_filter = if self.filter == "All" {
                true
            } else if self.filter == "Favorites" {
                clip.favorite
            } else {
                clip.game.eq_ignore_ascii_case(&self.filter)
            };
            if !match_filter {
                continue;
            }
            if q.is_empty() {
                scored_indices.push((1, clip.mtime_secs, idx));
            } else if let Some(score) = compute_clip_search_score(clip, q) {
                scored_indices.push((score, clip.mtime_secs, idx));
            }
        }
        if !q.is_empty() {
            scored_indices.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        }
        self.filtered_indices = scored_indices.into_iter().map(|(_, _, idx)| idx).collect();
    }

    fn process_async_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.async_rx.try_recv() {
            match msg {
                UiAsyncMessage::EngineEvent(ev) => match ev.kind.as_str() {
                    "game.started" | "game.pid_updated" => {
                        self.is_recording_active = true;
                        let g =
                            if ev.game.trim().is_empty() || ev.game.eq_ignore_ascii_case("system") {
                                "Game"
                            } else {
                                &ev.game
                            };
                        self.status_text = format!("Recording {g}");
                    }
                    "game.stopped" => {
                        self.is_recording_active = false;
                        self.status_text = "Ready".to_string();
                    }
                    "clip.saved" => {
                        if let Some(p_str) = ev.data.get("path").and_then(|v| v.as_str()) {
                            let path = PathBuf::from(p_str);
                            self.spawn_single_clip_probe(path.clone(), ctx);
                            let name = path
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("clip.mp4");
                            self.show_toast(format!("Clip saved: {name}"), Some(path));
                        } else {
                            self.spawn_background_library_scan(ctx);
                        }
                    }
                    "discord.compress_done" => {
                        if let Some(out_p) = ev.data.get("output_path").and_then(|v| v.as_str()) {
                            let path = PathBuf::from(out_p);
                            let out_b = ev
                                .data
                                .get("output_bytes")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let mb = out_b as f64 / (1024.0 * 1024.0);
                            self.show_toast(
                                format!("Compressed for Discord ({mb:.2} MB)"),
                                Some(path.clone()),
                            );
                            self.spawn_single_clip_probe(path, ctx);
                        }
                    }
                    "discord.compress_error" | "recorder.error" => {
                        if let Some(err_m) = ev.data.get("message").and_then(|v| v.as_str()) {
                            self.show_toast(format!("Error: {err_m}"), None);
                        }
                    }
                    _ => {}
                },
                UiAsyncMessage::LibraryScanned(items) => {
                    self.scan_in_progress = false;
                    self.clips = items;
                    self.filters_dirty = true;
                }
                UiAsyncMessage::SingleClipDiscovered(item) => {
                    if let Some(pos) = self.clips.iter().position(|c| c.id == item.id) {
                        self.clips[pos] = item;
                    } else {
                        self.clips.insert(0, item);
                    }
                    self.filters_dirty = true;
                }
                UiAsyncMessage::ThumbnailLoaded {
                    clip_id,
                    width,
                    height,
                    rgba,
                } => {
                    self.thumb_inflight.remove(&clip_id);
                    if rgba.len() == (width as usize) * (height as usize) * 4 {
                        let image = ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        );
                        let tex = ctx.load_texture(
                            format!("clip_thumb_{clip_id}"),
                            image,
                            TextureOptions::LINEAR,
                        );
                        self.thumb_cache.insert(clip_id, tex);
                    }
                }
                UiAsyncMessage::EditorKeyframesLoaded {
                    clip_id,
                    keyframes_secs,
                } => {
                    if let Some(ref mut ed) = self.editor_state {
                        if ed.clip_id == clip_id && !keyframes_secs.is_empty() {
                            ed.keyframes_secs = keyframes_secs;
                        }
                    }
                }
                UiAsyncMessage::EditorPreviewLoaded {
                    clip_id,
                    target_ms,
                    width,
                    height,
                    rgba,
                } => {
                    if let Some(ref mut ed) = self.editor_state {
                        if ed.clip_id == clip_id
                            && rgba.len() == (width as usize) * (height as usize) * 4
                        {
                            let img = ColorImage::from_rgba_unmultiplied(
                                [width as usize, height as usize],
                                &rgba,
                            );
                            let tex = ctx.load_texture(
                                format!("editor_preview_{clip_id}"),
                                img,
                                TextureOptions::LINEAR,
                            );
                            ed.preview_texture = Some((target_ms, tex));
                        }
                    }
                }
                UiAsyncMessage::EditorJobProgress(line) => {
                    if let Some(ref mut ed) = self.editor_state {
                        ed.status_line = line;
                    }
                }
                UiAsyncMessage::EditorJobFinished {
                    message,
                    reveal_path,
                } => {
                    if let Some(ref mut ed) = self.editor_state {
                        ed.busy = false;
                        ed.cancel_flag = None;
                        ed.status_line = message.clone();
                    }
                    if let Some(ref p) = reveal_path {
                        if p.is_file() {
                            self.spawn_single_clip_probe(p.clone(), ctx);
                        } else {
                            self.spawn_background_library_scan(ctx);
                        }
                    }
                    self.show_toast(message, reveal_path);
                }
                UiAsyncMessage::DiscordCompressFinished {
                    clip_id,
                    message,
                    reveal_path,
                } => {
                    self.compressing_clips.remove(&clip_id);
                    if let Some(ref p) = reveal_path {
                        self.spawn_single_clip_probe(p.clone(), ctx);
                    }
                    self.show_toast(message, reveal_path);
                }
                UiAsyncMessage::CustomSaveFolderPicked(folder) => {
                    self.custom_path_buffer = folder.to_string_lossy().to_string();
                    self.apply_custom_save_folder(folder, ctx);
                }
            }
        }
    }

    fn apply_custom_save_folder(&mut self, path: PathBuf, ctx: &egui::Context) {
        let validation = self.bridge.validate_and_query_folder_free_gb(&path);
        match validation {
            Ok(free_gb) => {
                self.folder_validation_status = Some(Ok(free_gb));
                let _ = self.bridge.update_and_save_config(|c| {
                    c.storage.mode = StorageMode::Custom(path);
                });
                self.spawn_background_library_scan(ctx);
                self.show_toast(
                    format!("Save location updated ({free_gb:.1} GB free)"),
                    None,
                );
            }
            Err(err) => {
                self.folder_validation_status = Some(Err(err.clone()));
                self.show_toast(format!("Invalid folder: {err}"), None);
            }
        }
    }

    fn handle_keybind_recording(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.recording_keybind_slot else {
            return;
        };
        let mut cancel_pressed = false;
        let mut captured_combo: Option<String> = None;
        let mut unsupported_key_pressed = false;

        ctx.input(|i| {
            if i.key_pressed(Key::Escape) {
                cancel_pressed = true;
                return;
            }
            for ev in &i.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = ev
                {
                    if *key == Key::Escape {
                        cancel_pressed = true;
                        break;
                    }
                    if let Some(key_token) = egui_key_to_config_token(*key) {
                        let combo = format_key_combo(*modifiers, &key_token);
                        captured_combo = Some(combo);
                        break;
                    } else {
                        unsupported_key_pressed = true;
                    }
                }
            }
        });

        if cancel_pressed {
            self.recording_keybind_slot = None;
            self.keybind_error_msg = None;
            self.bridge.set_hotkeys_paused(false);
            return;
        }

        if unsupported_key_pressed && captured_combo.is_none() {
            self.keybind_error_msg = Some(
                "Unsupported key. Press F1-F24, a letter, number, or arrow key (Esc to cancel)."
                    .to_string(),
            );
            return;
        }

        if let Some(combo_str) = captured_combo {
            let Some(parsed_kb) = Keybind::parse(&combo_str) else {
                self.keybind_error_msg = Some(format!(
                    "'{combo_str}' is not supported as a global shortcut. Press Esc to cancel."
                ));
                return;
            };

            let mut kb_cfg = self.bridge.recorder_config().keybinds;
            let other_slots: [(&str, &str, &Keybind); 4] = [
                ("replay_clip", "Save replay", &kb_cfg.replay_clip),
                ("direct_clip", "Clip last seconds", &kb_cfg.direct_clip),
                ("discord_clip", "Compress for Discord", &kb_cfg.discord_clip),
                ("mic_mute", "Mute microphone", &kb_cfg.mic_mute),
            ];

            for &(other_id, other_label, other_kb) in &other_slots {
                if other_id != slot
                    && other_kb.vk == parsed_kb.vk
                    && other_kb.modifiers == parsed_kb.modifiers
                {
                    self.keybind_error_msg = Some(format!(
                        "{} is already used by {}",
                        parsed_kb.label, other_label
                    ));
                    return;
                }
            }

            let label_saved = parsed_kb.label.clone();
            match slot {
                "replay_clip" => kb_cfg.replay_clip = parsed_kb,
                "direct_clip" => kb_cfg.direct_clip = parsed_kb,
                "discord_clip" => kb_cfg.discord_clip = parsed_kb,
                "mic_mute" => kb_cfg.mic_mute = parsed_kb,
                _ => {}
            }
            self.recording_keybind_slot = None;
            self.keybind_error_msg = None;
            self.bridge.set_hotkeys_paused(false);
            if self.bridge.set_keybinds(kb_cfg).is_ok() {
                self.show_toast(format!("Shortcut updated to {label_saved}"), None);
            }
        }
    }

    fn save_clip_rename(&mut self, clip_id: u64) {
        let new_title = self.rename_buffer.trim().to_string();
        self.renaming_clip_id = None;
        if new_title.is_empty() {
            return;
        }
        if let Some(clip) = self.clips.iter_mut().find(|c| c.id == clip_id) {
            clip.display_title = new_title;
            persist_clip_sidecar(clip);
            self.filters_dirty = true;
            self.show_toast("Clip title saved", None);
        }
    }

    fn toggle_clip_favorite(&mut self, clip_id: u64) {
        if let Some(clip) = self.clips.iter_mut().find(|c| c.id == clip_id) {
            clip.favorite = !clip.favorite;
            persist_clip_sidecar(clip);
            self.filters_dirty = true;
        }
    }

    fn delete_clip_to_recycle_bin(&mut self, clip_id: u64) {
        self.confirm_delete_clip_id = None;
        let Some(pos) = self.clips.iter().position(|c| c.id == clip_id) else {
            return;
        };
        let removed = self.clips[pos].clone();
        match self
            .bridge
            .trash_clip(&removed.file_path, Some(&removed.thumb_path))
        {
            Ok(()) => {
                self.clips.remove(pos);
                self.thumb_cache.remove(clip_id);
                self.filters_dirty = true;
                self.show_toast(
                    format!("Moved '{}' to Recycle Bin", removed.display_title),
                    None,
                );
            }
            Err(err) => {
                self.show_toast(err, None);
            }
        }
    }

    fn trigger_editor_save_copy(&mut self, ctx: &egui::Context) {
        let Some(ref mut ed) = self.editor_state else {
            return;
        };
        if ed.busy {
            return;
        }
        let Some(clip) = self.clips.iter().find(|c| c.id == ed.clip_id).cloned() else {
            self.show_toast("Source clip file was not found on disk.", None);
            return;
        };
        if !clip.file_path.exists() {
            self.show_toast("Source clip file does not exist on disk.", None);
            return;
        }

        let start_ms = (ed.trim_start_secs.max(0.0) * 1000.0).round() as u64;
        let end_ms = (ed.trim_end_secs.max(ed.trim_start_secs + 0.1) * 1000.0).round() as u64;
        let cut_mode = ed.cut_mode;
        let trim_mode = ed.trim_mode;
        let splits_ms: Vec<u64> = ed
            .splits_secs
            .iter()
            .map(|&s| (s * 1000.0).round() as u64)
            .collect();
        let rescale_dims: Option<(u32, u32)> = match ed.resolution.as_str() {
            "1440p" => Some((2560, 1440)),
            "1080p" => Some((1920, 1080)),
            "720p" => Some((1280, 720)),
            _ => None,
        };
        let target_fps = match ed.fps.as_str() {
            "60 fps" => Some(60u32),
            "30 fps" => Some(30u32),
            _ => None,
        };
        let bitrate_kbps = match ed.quality.as_str() {
            "Medium" => Some(12_000u32),
            "Small file" => Some(6_000u32),
            _ => None,
        };
        let audio_mix = ed.build_audio_mix_config(clip.has_separate_stems);
        let remove_all_audio = ed.remove_all_audio;
        let input_path = clip.file_path.clone();
        let bridge = self.bridge.clone();
        let tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();

        let cancel_flag = Arc::new(AtomicBool::new(false));
        ed.busy = true;
        ed.cancel_flag = Some(cancel_flag.clone());
        ed.status_line = "Preparing edit pipeline...".to_string();

        let _ = std::thread::Builder::new()
            .name("clotch-editor-save".to_string())
            .spawn(move || {
                let mut guard = EditorBusyResetGuard {
                    tx: tx.clone(),
                    ctx: ctx_clone.clone(),
                    completed_cleanly: false,
                };

                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> Result<(String, Option<PathBuf>), String> {
                        if cancel_flag.load(Ordering::Relaxed) {
                            return Err("Edit canceled.".to_string());
                        }

                        let needs_post_transform = rescale_dims.is_some()
                            || target_fps.is_some()
                            || bitrate_kbps.is_some()
                            || remove_all_audio
                            || !audio_mix.is_unmodified_full_mix();

                        if !splits_ms.is_empty() {
                            let mut active_splits: Vec<u64> = splits_ms
                                .iter()
                                .copied()
                                .filter(|&pt| pt > start_ms.saturating_add(100) && pt + 100 < end_ms)
                                .collect();
                            active_splits.sort_unstable();
                            active_splits.dedup();

                            if !active_splits.is_empty() {
                                let mut boundaries = Vec::with_capacity(active_splits.len() + 2);
                                boundaries.push(start_ms);
                                boundaries.extend(active_splits);
                                boundaries.push(end_ms);

                                let total = boundaries.len() - 1;
                                let mut created_paths: Vec<PathBuf> = Vec::with_capacity(total);

                                for idx in 0..total {
                                    if cancel_flag.load(Ordering::Relaxed) {
                                        for p in &created_paths {
                                            let _ = std::fs::remove_file(p);
                                        }
                                        return Err("Edit canceled.".to_string());
                                    }
                                    let seg_start = boundaries[idx];
                                    let seg_end = boundaries[idx + 1];
                                    let _ = tx.send(UiAsyncMessage::EditorJobProgress(format!(
                                        "Saving split part {}/{}...",
                                        idx + 1,
                                        total
                                    )));
                                    ctx_clone.request_repaint();

                                    let final_part_out = default_edited_output_path(
                                        &input_path,
                                        &format!("part {}", idx + 1),
                                    );
                                    if needs_post_transform {
                                        let job = EditJob {
                                            input_path: input_path.clone(),
                                            output_path: Some(final_part_out),
                                            trim_start_ms: Some(seg_start),
                                            trim_end_ms: Some(seg_end),
                                            cut_mode,
                                            rescale: rescale_dims,
                                            target_fps,
                                            video_bitrate_kbps: bitrate_kbps,
                                            audio_mix: Some(audio_mix.clone()),
                                            mute_all_audio: remove_all_audio,
                                            prefer_hardware: true,
                                        };
                                        let r = bridge.run_edit_job(job)?;
                                        created_paths.push(r.output_path);
                                    } else {
                                        let r = bridge.cut_clip(
                                            &input_path,
                                            seg_start,
                                            seg_end,
                                            cut_mode,
                                            Some(final_part_out),
                                        )?;
                                        created_paths.push(r.output_path);
                                    }
                                }

                                let first_out = created_paths.first().cloned();
                                return Ok((
                                    format!("Saved {} split parts", created_paths.len()),
                                    first_out,
                                ));
                            }
                        }

                        if trim_mode == TrimActionMode::Remove {
                            let _ = tx.send(UiAsyncMessage::EditorJobProgress(
                                "Removing selected middle section...".to_string(),
                            ));
                            ctx_clone.request_repaint();
                            let final_out = default_edited_output_path(&input_path, "edited");
                            if !needs_post_transform {
                                let res = bridge.remove_middle_segment(
                                    &input_path,
                                    start_ms,
                                    end_ms,
                                    Some(final_out),
                                )?;
                                let fname = res
                                    .output_path
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("edited.mp4");
                                return Ok((format!("Saved copy: {fname}"), Some(res.output_path)));
                            }
                            let temp_spliced = input_path.with_extension("splicing_tmp.mp4");
                            let _ = bridge.remove_middle_segment(
                                &input_path,
                                start_ms,
                                end_ms,
                                Some(temp_spliced.clone()),
                            )?;
                            if cancel_flag.load(Ordering::Relaxed) {
                                let _ = std::fs::remove_file(&temp_spliced);
                                return Err("Edit canceled.".to_string());
                            }
                            let _ = tx.send(UiAsyncMessage::EditorJobProgress(
                                "Applying video & audio stem settings...".to_string(),
                            ));
                            ctx_clone.request_repaint();
                            let job = EditJob {
                                input_path: temp_spliced.clone(),
                                output_path: Some(final_out),
                                trim_start_ms: None,
                                trim_end_ms: None,
                                cut_mode,
                                rescale: rescale_dims,
                                target_fps,
                                video_bitrate_kbps: bitrate_kbps,
                                audio_mix: Some(audio_mix),
                                mute_all_audio: remove_all_audio,
                                prefer_hardware: true,
                            };
                            let res = bridge.run_edit_job(job);
                            let _ = std::fs::remove_file(&temp_spliced);
                            let res = res?;
                            let fname = res
                                .output_path
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("edited.mp4");
                            return Ok((format!("Saved copy: {fname}"), Some(res.output_path)));
                        }

                        let _ = tx.send(UiAsyncMessage::EditorJobProgress(
                            "Saving edited copy...".to_string(),
                        ));
                        ctx_clone.request_repaint();
                        let final_out = default_edited_output_path(&input_path, "edited");
                        let job = EditJob {
                            input_path,
                            output_path: Some(final_out),
                            trim_start_ms: Some(start_ms),
                            trim_end_ms: Some(end_ms),
                            cut_mode,
                            rescale: rescale_dims,
                            target_fps,
                            video_bitrate_kbps: bitrate_kbps,
                            audio_mix: Some(audio_mix),
                            mute_all_audio: remove_all_audio,
                            prefer_hardware: true,
                        };
                        let res = bridge.run_edit_job(job)?;
                        if cancel_flag.load(Ordering::Relaxed) {
                            let _ = std::fs::remove_file(&res.output_path);
                            return Err("Edit canceled.".to_string());
                        }
                        let fname = res
                            .output_path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("edited.mp4");
                        Ok((format!("Saved copy: {fname}"), Some(res.output_path)))
                    },
                ));

                guard.completed_cleanly = true;
                match outcome {
                    Ok(Ok((msg, path))) => {
                        let _ = tx.send(UiAsyncMessage::EditorJobFinished {
                            message: msg,
                            reveal_path: path,
                        });
                    }
                    Ok(Err(err_msg)) => {
                        let _ = tx.send(UiAsyncMessage::EditorJobFinished {
                            message: err_msg,
                            reveal_path: None,
                        });
                    }
                    Err(_) => {
                        let _ = tx.send(UiAsyncMessage::EditorJobFinished {
                            message: "Editor worker panicked unexpectedly.".to_string(),
                            reveal_path: None,
                        });
                    }
                }
                ctx_clone.request_repaint();
            });
    }

    fn trigger_editor_export_stems(&mut self, ctx: &egui::Context) {
        let Some(ref mut ed) = self.editor_state else {
            return;
        };
        if ed.busy {
            return;
        }
        let Some(clip) = self.clips.iter().find(|c| c.id == ed.clip_id).cloned() else {
            self.show_toast("Source clip file was not found on disk.", None);
            return;
        };
        if !clip.file_path.exists() {
            self.show_toast("Source clip file does not exist on disk.", None);
            return;
        }
        if !clip.has_separate_stems {
            self.show_toast("This clip was recorded with a single audio track.", None);
            return;
        }

        let input_path = clip.file_path.clone();
        let bridge = self.bridge.clone();
        let tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();

        ed.busy = true;
        ed.status_line = "Exporting separated audio tracks...".to_string();

        let _ = std::thread::Builder::new()
            .name("clotch-export-stems".to_string())
            .spawn(move || {
                let mut guard = EditorBusyResetGuard {
                    tx: tx.clone(),
                    ctx: ctx_clone.clone(),
                    completed_cleanly: false,
                };
                let res = bridge.export_separated_stems(&input_path, None);
                guard.completed_cleanly = true;
                match res {
                    Ok(stems_res) => {
                        let first_stem = stems_res.stems.first().map(|s| s.m4a_path.clone());
                        let _ = tx.send(UiAsyncMessage::EditorJobFinished {
                            message: format!(
                                "Exported {} audio stem files (.m4a)",
                                stems_res.stems.len()
                            ),
                            reveal_path: first_stem,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiAsyncMessage::EditorJobFinished {
                            message: format!("Stem export failed: {e}"),
                            reveal_path: None,
                        });
                    }
                }
                ctx_clone.request_repaint();
            });
    }

    fn trigger_clip_discord_compress(&mut self, clip_id: u64, ctx: &egui::Context) {
        if self.compressing_clips.contains(&clip_id) {
            return;
        }
        let Some(clip) = self.clips.iter().find(|c| c.id == clip_id).cloned() else {
            return;
        };
        if !clip.file_path.exists() {
            self.show_toast("Clip file was not found on disk.", None);
            return;
        }

        self.compressing_clips.insert(clip_id);
        self.show_toast(
            format!("Compressing '{}' for Discord...", clip.display_title),
            None,
        );

        let path = clip.file_path;
        let bridge = self.bridge.clone();
        let tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();

        let _ = std::thread::Builder::new()
            .name("clotch-discord-compress".to_string())
            .spawn(move || {
                match bridge.compress_for_discord_blocking(path, Some(7.5)) {
                    Ok(res) => {
                        let mb = res.output_bytes as f64 / (1024.0 * 1024.0);
                        let _ = tx.send(UiAsyncMessage::DiscordCompressFinished {
                            clip_id,
                            message: format!("Discord ready ({mb:.2} MB)"),
                            reveal_path: Some(res.output_path),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiAsyncMessage::DiscordCompressFinished {
                            clip_id,
                            message: format!("Discord compression failed: {e}"),
                            reveal_path: None,
                        });
                    }
                }
                ctx_clone.request_repaint();
            });
    }

    fn render_header(&mut self, ui: &mut egui::Ui, c: ThemeColors) {
        let avail_w = ui.available_width();
        let (_, rect) = ui.allocate_space(Vec2::new(avail_w, 42.0));
        let painter = ui.painter();

        let brand_galley = painter.layout_no_wrap(
            "Clotch".to_string(),
            FontId::proportional(20.0),
            c.text,
        );
        let brand_pos = Pos2::new(rect.min.x, rect.min.y + 4.0);
        painter.galley(brand_pos, brand_galley.clone(), c.text);

        let dot_x = brand_pos.x + brand_galley.size().x + 20.0;
        let dot_y = rect.min.y + 15.0;
        let dot_color = if self.is_recording_active {
            c.accent
        } else {
            c.text_3
        };
        painter.circle_filled(Pos2::new(dot_x, dot_y), 3.5, dot_color);
        painter.text(
            Pos2::new(dot_x + 10.0, dot_y),
            egui::Align2::LEFT_CENTER,
            &self.status_text,
            FontId::proportional(13.0),
            c.text_2,
        );

        let nav_rect = Rect::from_min_max(
            Pos2::new((rect.max.x - 180.0).max(dot_x + 120.0), rect.min.y),
            Pos2::new(rect.max.x, rect.min.y + 30.0),
        );
        let mut nav_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(nav_rect)
                .layout(Layout::right_to_left(Align::Center)),
        );
        nav_ui.spacing_mut().item_spacing.x = 22.0;

        let is_settings = self.view == ViewTab::Settings;
        let settings_resp = nav_ui.add(
            egui::Label::new(
                RichText::new("Settings")
                    .size(14.0)
                    .color(if is_settings { c.text } else { c.text_2 }),
            )
            .sense(Sense::click()),
        );
        if settings_resp.hovered() {
            nav_ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if settings_resp.clicked() {
            if self.recording_keybind_slot.is_some() {
                self.recording_keybind_slot = None;
                self.bridge.set_hotkeys_paused(false);
            }
            self.close_editor_session();
            self.view = ViewTab::Settings;
        }

        let is_clips = self.view == ViewTab::Clips || self.view == ViewTab::Editor;
        let clips_resp = nav_ui.add(
            egui::Label::new(
                RichText::new("Clips")
                    .size(14.0)
                    .color(if is_clips { c.text } else { c.text_2 }),
            )
            .sense(Sense::click()),
        );
        if clips_resp.hovered() {
            nav_ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if clips_resp.clicked() {
            if self.recording_keybind_slot.is_some() {
                self.recording_keybind_slot = None;
                self.bridge.set_hotkeys_paused(false);
            }
            self.close_editor_session();
            self.view = ViewTab::Clips;
        }

        let rule_rect = Rect::from_min_max(
            Pos2::new(rect.min.x, rect.max.y - 2.0),
            Pos2::new(rect.max.x, rect.max.y),
        );
        ui.painter().rect_filled(rule_rect, Rounding::ZERO, c.accent);
    }

    fn render_clips_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, c: ThemeColors) {
        self.rebuild_filters_and_games_if_needed();

        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            ui.spacing_mut().item_spacing.y = 8.0;

            let mut filter_tabs = Vec::with_capacity(self.available_games.len() + 2);
            filter_tabs.push("All".to_string());
            for g in &self.available_games {
                filter_tabs.push(g.clone());
            }
            if self.clips.iter().any(|clip| clip.favorite) {
                filter_tabs.push("Favorites".to_string());
            }

            for f_name in filter_tabs {
                let active = self.filter.eq_ignore_ascii_case(&f_name);
                let resp = ui.add(
                    egui::Label::new(
                        RichText::new(&f_name)
                            .size(14.0)
                            .color(if active { c.text } else { c.text_2 }),
                    )
                    .sense(Sense::click()),
                );
                if resp.hovered() {
                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                }
                if resp.clicked() && self.filter != f_name {
                    self.filter = f_name;
                    self.filters_dirty = true;
                }
            }

            let rem_w = ui.available_width();
            if rem_w >= 180.0 {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.render_search_box(ui, c);
                });
            }
        });

        if ui.available_width() < 560.0 {
            ui.add_space(6.0);
            self.render_search_box(ui, c);
        }

        ui.add_space(10.0);
        let active_dir = self.active_clips_dir.clone();
        let active_dir_str = active_dir.to_string_lossy().to_string();
        let mut open_clips_folder = false;
        let mut trigger_rescan = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(
                RichText::new(format!("Folder: {active_dir_str}"))
                    .font(FontId::monospace(11.5))
                    .color(c.text_3),
            );
            if text_action_link(ui, "Open folder", c.accent, c.text).clicked() {
                open_clips_folder = true;
            }
            let scan_label = if self.scan_in_progress {
                "Scanning..."
            } else {
                "Refresh"
            };
            if text_action_link(ui, scan_label, c.text_2, c.text).clicked()
                && !self.scan_in_progress
            {
                trigger_rescan = true;
            }
        });
        if open_clips_folder {
            let _ = std::fs::create_dir_all(&active_dir);
            reveal_file_in_explorer(&active_dir);
        }
        if trigger_rescan {
            self.spawn_background_library_scan(ctx);
        }

        ui.add_space(14.0);

        if self.clips.is_empty() {
            let replay_key = self.bridge.recorder_config().keybinds.replay_clip.label;
            ui.add_space(48.0);
            ui.label(
                RichText::new(format!(
                    "No clips yet in {active_dir_str}. Press {replay_key} in a game to save one."
                ))
                .size(14.0)
                .color(c.text_3),
            );
            return;
        }

        self.rebuild_filters_and_games_if_needed();
        if self.filtered_indices.is_empty() {
            ui.add_space(48.0);
            ui.label(
                RichText::new("No clips match your search or filter. Try searching by game, tag (ace, clutch, 5k, hs), or map (Ascent, Mirage).")
                    .size(14.0)
                    .color(c.text_3),
            );
            return;
        }

        let avail_w = ui.available_width();
        let cols = if avail_w >= 1520.0 {
            6usize
        } else if avail_w >= 1260.0 {
            5usize
        } else if avail_w >= 980.0 {
            4usize
        } else if avail_w >= 720.0 {
            3usize
        } else if avail_w >= 480.0 {
            2usize
        } else {
            1usize
        };
        let gap_x = 24.0f32;
        let gap_y = 28.0f32;
        let card_w = ((avail_w - gap_x * ((cols - 1) as f32)) / (cols as f32)).max(200.0);
        let thumb_h = (card_w * 9.0 / 16.0).round();
        let row_h = thumb_h + 76.0;
        let total_items = self.filtered_indices.len();
        let total_rows = (total_items + cols - 1) / cols;

        let mut open_editor_id: Option<u64> = None;
        let mut toggle_fav_id: Option<u64> = None;
        let mut start_rename_id: Option<(u64, String)> = None;
        let mut finish_rename_id: Option<u64> = None;
        let mut cancel_rename = false;
        let mut compress_discord_id: Option<u64> = None;
        let mut ask_delete_id: Option<u64> = None;
        let mut confirm_delete_id: Option<u64> = None;
        let mut cancel_delete = false;
        let mut thumbs_to_request: Vec<ClipItem> = Vec::new();

        ScrollArea::vertical()
            .id_salt("clips_grid_scroll")
            .auto_shrink([false, false])
            .show_rows(ui, row_h + gap_y, total_rows, |ui, row_range| {
                for row_idx in row_range {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap_x;
                        for col_idx in 0..cols {
                            let flat_idx = row_idx * cols + col_idx;
                            if flat_idx >= total_items {
                                break;
                            }
                            let clip_idx = self.filtered_indices[flat_idx];
                            let Some(clip_ref) = self.clips.get(clip_idx) else {
                                continue;
                            };
                            let cid = clip_ref.id;
                            let display_title = clip_ref.display_title.clone();
                            let game = clip_ref.game.clone();
                            let tag = clip_ref.tag.clone();
                            let map_name = clip_ref.map.clone();
                            let date_label = clip_ref.date_label.clone();
                            let duration_label = clip_ref.duration_label.clone();
                            let is_fav = clip_ref.favorite;

                            if !self.thumb_cache.map.contains_key(&cid)
                                && !self.thumb_inflight.contains(&cid)
                            {
                                thumbs_to_request.push(clip_ref.clone());
                            }

                            ui.allocate_ui_with_layout(
                                Vec2::new(card_w, row_h),
                                Layout::top_down(Align::Min),
                                |ui| {
                                    let (thumb_rect, thumb_resp) = ui.allocate_exact_size(
                                        Vec2::new(card_w, thumb_h),
                                        Sense::click(),
                                    );
                                    if thumb_resp.hovered() {
                                        ctx.set_cursor_icon(CursorIcon::PointingHand);
                                    }

                                    let bg_col = if thumb_resp.hovered() {
                                        c.border
                                    } else {
                                        c.surface_2
                                    };
                                    ui.painter().rect(
                                        thumb_rect,
                                        Rounding::same(6 as f32),
                                        bg_col,
                                        Stroke::new(1.0_f32, c.border),
                                    );

                                    if let Some(tex) = self.thumb_cache.get(cid) {
                                        ui.painter().image(
                                            tex.id(),
                                            thumb_rect.shrink(1.0),
                                            Rect::from_min_max(
                                                Pos2::new(0.0, 0.0),
                                                Pos2::new(1.0, 1.0),
                                            ),
                                            Color32::WHITE,
                                        );
                                    }

                                    let dur_galley = ui.painter().layout_no_wrap(
                                        duration_label,
                                        FontId::monospace(11.0),
                                        Color32::from_rgb(245, 245, 243),
                                    );
                                    let badge_w = dur_galley.size().x + 12.0;
                                    let badge_h = 20.0;
                                    let badge_rect = Rect::from_min_size(
                                        Pos2::new(
                                            thumb_rect.max.x - badge_w - 8.0,
                                            thumb_rect.max.y - badge_h - 8.0,
                                        ),
                                        Vec2::new(badge_w, badge_h),
                                    );
                                    ui.painter().rect_filled(
                                        badge_rect,
                                        Rounding::same(4 as f32),
                                        Color32::from_rgba_unmultiplied(15, 16, 18, 200),
                                    );
                                    ui.painter().galley(
                                        Pos2::new(badge_rect.min.x + 6.0, badge_rect.min.y + 3.0),
                                        dur_galley,
                                        Color32::from_rgb(245, 245, 243),
                                    );

                                    let star_rect = Rect::from_min_size(
                                        Pos2::new(thumb_rect.max.x - 32.0, thumb_rect.min.y + 8.0),
                                        Vec2::new(24.0, 24.0),
                                    );
                                    let star_resp = ui.interact(
                                        star_rect,
                                        Id::new(("clip_fav", cid)),
                                        Sense::click(),
                                    );
                                    if star_resp.hovered() {
                                        ctx.set_cursor_icon(CursorIcon::PointingHand);
                                    }
                                    if is_fav || thumb_resp.hovered() || star_resp.hovered() {
                                        ui.painter().rect_filled(
                                            star_rect,
                                            Rounding::same(4 as f32),
                                            Color32::from_rgba_unmultiplied(15, 16, 18, 175),
                                        );
                                        let star_col = if is_fav {
                                            c.accent
                                        } else {
                                            Color32::from_rgb(230, 230, 228)
                                        };
                                        ui.painter().text(
                                            star_rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            if is_fav { "★" } else { "☆" },
                                            FontId::proportional(13.0),
                                            star_col,
                                        );
                                    }

                                    if star_resp.clicked() {
                                        toggle_fav_id = Some(cid);
                                    } else if thumb_resp.clicked() {
                                        open_editor_id = Some(cid);
                                    }

                                    ui.add_space(10.0);

                                    if self.renaming_clip_id == Some(cid) {
                                        let edit_resp = ui.add_sized(
                                            [card_w, 22.0],
                                            egui::TextEdit::singleline(&mut self.rename_buffer)
                                                .font(FontId::proportional(14.0)),
                                        );
                                        if self.rename_focus_requested {
                                            edit_resp.request_focus();
                                            self.rename_focus_requested = false;
                                        }
                                        let enter_pressed =
                                            ui.input(|i| i.key_pressed(Key::Enter));
                                        let esc_pressed = ui.input(|i| i.key_pressed(Key::Escape));
                                        if esc_pressed {
                                            cancel_rename = true;
                                        } else if enter_pressed || edit_resp.lost_focus() {
                                            finish_rename_id = Some(cid);
                                        }
                                    } else {
                                        let title_resp = ui.add(
                                            egui::Label::new(
                                                RichText::new(&display_title)
                                                    .size(14.0)
                                                    .color(c.text),
                                            )
                                            .truncate()
                                            .sense(Sense::click()),
                                        );
                                        if title_resp.hovered() {
                                            ctx.set_cursor_icon(CursorIcon::PointingHand);
                                        }
                                        if title_resp.clicked() {
                                            open_editor_id = Some(cid);
                                        }
                                    }

                                    ui.add_space(3.0);

                                    let card_hovered = ui.rect_contains_pointer(
                                        Rect::from_min_size(
                                            thumb_rect.min,
                                            Vec2::new(card_w, row_h),
                                        ),
                                    );

                                    if self.confirm_delete_clip_id == Some(cid) {
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 10.0;
                                            ui.label(
                                                RichText::new("Delete?")
                                                    .size(12.0)
                                                    .color(c.danger),
                                            );
                                            if text_action_link(ui, "Yes", c.danger, c.danger)
                                                .clicked()
                                            {
                                                confirm_delete_id = Some(cid);
                                            }
                                            if text_action_link(ui, "No", c.text_2, c.text)
                                                .clicked()
                                            {
                                                cancel_delete = true;
                                            }
                                        });
                                    } else if !card_hovered {
                                        let sub = if map_name.is_empty() {
                                            format!("{game} · {tag} · {date_label}")
                                        } else {
                                            format!("{game} · {tag} · {map_name} · {date_label}")
                                        };
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(sub).size(12.0).color(c.text_2),
                                            )
                                            .truncate(),
                                        );
                                    } else {
                                        let is_compressing =
                                            self.compressing_clips.contains(&cid);
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 14.0;
                                            if text_action_link(ui, "Edit", c.text_2, c.text)
                                                .clicked()
                                            {
                                                open_editor_id = Some(cid);
                                            }
                                            if text_action_link(ui, "Rename", c.text_2, c.text)
                                                .clicked()
                                            {
                                                start_rename_id =
                                                    Some((cid, display_title.clone()));
                                            }
                                            let disc_label = if is_compressing {
                                                "Compressing..."
                                            } else {
                                                "Discord"
                                            };
                                            if text_action_link(
                                                ui,
                                                disc_label,
                                                if is_compressing { c.accent } else { c.text_2 },
                                                c.text,
                                            )
                                            .clicked()
                                                && !is_compressing
                                            {
                                                compress_discord_id = Some(cid);
                                            }
                                            if text_action_link(
                                                ui,
                                                "Delete",
                                                c.text_2,
                                                c.danger,
                                            )
                                            .clicked()
                                            {
                                                ask_delete_id = Some(cid);
                                            }
                                        });
                                    }
                                },
                            );
                        }
                    });
                    ui.add_space(gap_y);
                }
            });

        for clip in thumbs_to_request {
            self.request_thumbnail_if_needed(&clip, ctx);
        }

        if let Some(cid) = toggle_fav_id {
            self.toggle_clip_favorite(cid);
        }
        if let Some((cid, current_title)) = start_rename_id {
            self.renaming_clip_id = Some(cid);
            self.rename_buffer = current_title;
            self.rename_focus_requested = true;
        }
        if cancel_rename {
            self.renaming_clip_id = None;
        }
        if let Some(cid) = finish_rename_id {
            self.save_clip_rename(cid);
        }
        if let Some(cid) = compress_discord_id {
            self.trigger_clip_discord_compress(cid, ctx);
        }
        if let Some(cid) = ask_delete_id {
            self.confirm_delete_clip_id = Some(cid);
        }
        if cancel_delete {
            self.confirm_delete_clip_id = None;
        }
        if let Some(cid) = confirm_delete_id {
            self.delete_clip_to_recycle_bin(cid);
        }
        if let Some(cid) = open_editor_id {
            if let Some(clip) = self.clips.iter().find(|c| c.id == cid).cloned() {
                self.close_editor_session();
                let dur = clip.duration_secs.max(1.0);
                let mut default_kf = Vec::new();
                let mut t = 0.0;
                while t <= dur {
                    default_kf.push((t * 100.0).round() / 100.0);
                    t += 2.0;
                }
                self.editor_state = Some(EditorViewState::from_clip(&clip, default_kf));
                self.view = ViewTab::Editor;
                self.request_editor_keyframes(cid, clip.file_path.clone(), ctx);
                self.request_editor_preview_frame(cid, clip.file_path, 0, ctx);
            }
        }
    }

    fn render_search_box(&mut self, ui: &mut egui::Ui, c: ThemeColors) {
        let search_w = 240.0f32.min(ui.available_width().max(120.0));
        let (rect, _) = ui.allocate_exact_size(Vec2::new(search_w, 26.0), Sense::hover());
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        let prev = self.search.clone();
        let resp = child.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text(RichText::new("Search (e.g. ace, 5k, Ascent, val)").size(13.0).color(c.text_3))
                .font(FontId::proportional(13.0))
                .frame(false)
                .desired_width(search_w),
        );
        if self.search != prev {
            self.filters_dirty = true;
        }
        let line_col = if resp.has_focus() {
            c.border_strong
        } else {
            c.border
        };
        ui.painter().line_segment(
            [
                Pos2::new(rect.min.x, rect.max.y),
                Pos2::new(rect.max.x, rect.max.y),
            ],
            Stroke::new(1.0_f32, line_col),
        );
    }

    fn render_editor_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, c: ThemeColors) {
        let Some(clip_id) = self.editor_state.as_ref().map(|e| e.clip_id) else {
            self.view = ViewTab::Clips;
            return;
        };
        let Some(clip) = self.clips.iter().find(|cl| cl.id == clip_id).cloned() else {
            self.view = ViewTab::Clips;
            return;
        };

        let dur = clip.duration_secs.max(1.0);
        let mut dispatch_preview_ms: Option<u64> = None;
        let mut close_to_clips = false;

        if !ctx.wants_keyboard_input() && self.recording_keybind_slot.is_none() {
            let space_pressed = ctx.input(|i| i.key_pressed(Key::Space));
            let key_i = ctx.input(|i| i.key_pressed(Key::I));
            let key_o = ctx.input(|i| i.key_pressed(Key::O));
            let key_s = ctx.input(|i| i.key_pressed(Key::S) && !i.modifiers.ctrl && !i.modifiers.command);
            let key_f = ctx.input(|i| i.key_pressed(Key::F) || i.key_pressed(Key::F11));
            let key_esc = ctx.input(|i| i.key_pressed(Key::Escape));
            let key_left = ctx.input(|i| i.key_pressed(Key::ArrowLeft));
            let key_right = ctx.input(|i| i.key_pressed(Key::ArrowRight));
            let shift_held = ctx.input(|i| i.modifiers.shift);

            if let Some(ref mut ed) = self.editor_state {
                if space_pressed {
                    ed.is_playing = !ed.is_playing;
                    if ed.is_playing {
                        if ed.playhead_secs >= ed.trim_end_secs - 0.05 {
                            ed.playhead_secs = ed.trim_start_secs;
                        }
                        ed.last_play_tick = Some(Instant::now());
                    } else {
                        ed.last_play_tick = None;
                    }
                }
                if key_i {
                    let snapped = ed.snap_to_keyframe(ed.playhead_secs, dur);
                    ed.trim_start_secs = snapped.min(ed.trim_end_secs - 0.25).max(0.0);
                }
                if key_o {
                    let snapped = ed.snap_to_keyframe(ed.playhead_secs, dur);
                    ed.trim_end_secs = snapped.max(ed.trim_start_secs + 0.25).min(dur);
                }
                if key_s {
                    let snapped = ed.snap_to_keyframe(ed.playhead_secs, dur);
                    if snapped > 0.15 && snapped < dur - 0.15 {
                        if let Some(existing) = ed
                            .splits_secs
                            .iter()
                            .position(|&x| (x - snapped).abs() < 0.25)
                        {
                            ed.splits_secs.remove(existing);
                        } else if ed.splits_secs.len() < 12 {
                            ed.splits_secs.push(snapped);
                            ed.splits_secs
                                .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                        }
                    }
                }
                if key_f {
                    ed.fullscreen_preview = !ed.fullscreen_preview;
                }
                if key_esc && ed.fullscreen_preview {
                    ed.fullscreen_preview = false;
                }
                if key_left || key_right {
                    ed.is_playing = false;
                    ed.last_play_tick = None;
                    let step = if shift_held { 0.1 } else { 1.0 };
                    let delta = if key_left { -step } else { step };
                    ed.playhead_secs = (ed.playhead_secs + delta).clamp(0.0, dur);
                    let req_ms = (ed.playhead_secs * 1000.0).round() as u64;
                    ed.pending_preview_ms = Some((req_ms, Instant::now()));
                }
            }
        }

        if let Some(ref mut ed) = self.editor_state {
            if ed.is_playing {
                let now = Instant::now();
                let dt = ed
                    .last_play_tick
                    .map(|t| now.saturating_duration_since(t).as_secs_f64().min(0.1))
                    .unwrap_or(0.033);
                ed.last_play_tick = Some(now);
                ed.playhead_secs += dt;

                let stop_bound = if ed.trim_mode == TrimActionMode::Keep && ed.trim_end_secs > ed.trim_start_secs + 0.2 {
                    ed.trim_end_secs
                } else {
                    dur
                };

                if ed.playhead_secs >= stop_bound {
                    ed.playhead_secs = if ed.trim_mode == TrimActionMode::Keep {
                        ed.trim_start_secs
                    } else {
                        0.0
                    };
                    ed.is_playing = false;
                    ed.last_play_tick = None;
                }

                let target_ms = (ed.playhead_secs * 1000.0).round() as u64;
                if ed
                    .last_preview_req_ms
                    .map(|last| last.abs_diff(target_ms) >= 30)
                    .unwrap_or(true)
                {
                    ed.last_preview_req_ms = Some(target_ms);
                    dispatch_preview_ms = Some(target_ms);
                }
                ctx.request_repaint_after(Duration::from_millis(33));
            } else if let Some((target_ms, queued_at)) = ed.pending_preview_ms {
                if queued_at.elapsed() >= Duration::from_millis(35) {
                    ed.pending_preview_ms = None;
                    if ed.last_preview_req_ms != Some(target_ms) {
                        ed.last_preview_req_ms = Some(target_ms);
                        dispatch_preview_ms = Some(target_ms);
                    }
                } else {
                    ctx.request_repaint_after(Duration::from_millis(20));
                }
            }
        }

        if let Some(ms) = dispatch_preview_ms {
            self.request_editor_preview_frame(clip_id, clip.file_path.clone(), ms, ctx);
        }

        let is_fullscreen = self
            .editor_state
            .as_ref()
            .map(|e| e.fullscreen_preview)
            .unwrap_or(false);

        if is_fullscreen {
            egui::Area::new(Id::new("clotch_fullscreen_preview"))
                .fixed_pos(Pos2::ZERO)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    let screen_rect = ctx.screen_rect();
                    ui.painter()
                        .rect_filled(screen_rect, Rounding::ZERO, Color32::from_rgb(8, 9, 10));
                    let pad = 28.0;
                    let avail_w = (screen_rect.width() - pad * 2.0).max(320.0);
                    let avail_h = (screen_rect.height() - 120.0).max(200.0);
                    let mut stage_w = avail_w;
                    let mut stage_h = (stage_w * 9.0 / 16.0).round();
                    if stage_h > avail_h {
                        stage_h = avail_h;
                        stage_w = (stage_h * 16.0 / 9.0).round();
                    }
                    let x0 = screen_rect.center().x - stage_w * 0.5;
                    let y0 = pad;
                    let stage_rect = Rect::from_min_size(Pos2::new(x0, y0), Vec2::new(stage_w, stage_h));
                    let stage_resp = ui.interact(
                        stage_rect,
                        Id::new(("fs_stage", clip.id)),
                        Sense::click(),
                    );
                    if let Some(ref mut ed) = self.editor_state {
                        if let Some((_, ref tex)) = ed.preview_texture {
                            ui.painter().image(
                                tex.id(),
                                stage_rect,
                                Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        } else if let Some(tex) = self.thumb_cache.get(clip.id) {
                            ui.painter().image(
                                tex.id(),
                                stage_rect,
                                Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                        if stage_resp.clicked() {
                            ed.is_playing = !ed.is_playing;
                            ed.last_play_tick = if ed.is_playing {
                                Some(Instant::now())
                            } else {
                                None
                            };
                        }
                        let bar_rect = Rect::from_min_size(
                            Pos2::new(stage_rect.min.x, stage_rect.max.y + 14.0),
                            Vec2::new(stage_w, 40.0),
                        );
                        let mut bar_ui = ui.new_child(
                            UiBuilder::new()
                                .max_rect(bar_rect)
                                .layout(Layout::left_to_right(Align::Center)),
                        );
                        bar_ui.spacing_mut().item_spacing.x = 12.0;
                        let play_label = if ed.is_playing { "⏸ Pause" } else { "▶ Play" };
                        if draw_ghost_button(&mut bar_ui, play_label, true, c).clicked() {
                            ed.is_playing = !ed.is_playing;
                            ed.last_play_tick = if ed.is_playing {
                                Some(Instant::now())
                            } else {
                                None
                            };
                        }
                        bar_ui.label(
                            RichText::new(format!(
                                "{} / {}",
                                format_mm_ss(ed.playhead_secs),
                                clip.duration_label
                            ))
                            .font(FontId::monospace(12.0))
                            .color(c.text),
                        );
                        if draw_ghost_button(&mut bar_ui, "Exit Fullscreen (Esc)", true, c)
                            .clicked()
                        {
                            ed.fullscreen_preview = false;
                        }
                    }
                });
            return;
        }

        ScrollArea::vertical()
            .id_salt("editor_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                let back_resp = ui.add(
                    egui::Label::new(
                        RichText::new("← All clips").size(13.0).color(c.text_2),
                    )
                    .sense(Sense::click()),
                );
                if back_resp.hovered() {
                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                }
                if back_resp.clicked() {
                    close_to_clips = true;
                }

                ui.add_space(20.0);

                let avail_w = ui.available_width();
                if avail_w >= 860.0 {
                    let side_w = 360.0;
                    let gap = 40.0;
                    let left_w = (avail_w - side_w - gap).max(360.0);
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        ui.allocate_ui_with_layout(
                            Vec2::new(left_w, 0.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                self.render_editor_left_column(ui, ctx, &clip, left_w, c);
                            },
                        );
                        ui.allocate_ui_with_layout(
                            Vec2::new(side_w, 0.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                self.render_editor_right_column(ui, ctx, &clip, side_w, c);
                            },
                        );
                    });
                } else {
                    self.render_editor_left_column(ui, ctx, &clip, avail_w, c);
                    ui.add_space(28.0);
                    self.render_editor_right_column(ui, ctx, &clip, avail_w, c);
                }
                ui.add_space(36.0);
            });

        if close_to_clips {
            self.close_editor_session();
            self.view = ViewTab::Clips;
        }
    }

    fn render_editor_left_column(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        clip: &ClipItem,
        width: f32,
        c: ThemeColors,
    ) {
        let Some(ref mut ed) = self.editor_state else {
            return;
        };
        let dur = clip.duration_secs.max(1.0);
        let stage_h = (width * 9.0 / 16.0).round();
        let (stage_rect, stage_resp) =
            ui.allocate_exact_size(Vec2::new(width, stage_h), Sense::click());

        ui.painter().rect(
            stage_rect,
            Rounding::same(6 as f32),
            c.surface_2,
            Stroke::new(1.0_f32, c.border),
        );

        if let Some((_, ref tex)) = ed.preview_texture {
            ui.painter().image(
                tex.id(),
                stage_rect.shrink(1.0),
                Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        } else if let Some(tex) = self.thumb_cache.get(clip.id) {
            ui.painter().image(
                tex.id(),
                stage_rect.shrink(1.0),
                Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        if stage_resp.hovered() {
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        }

        if !ed.is_playing {
            let center_play_rect = Rect::from_center_size(stage_rect.center(), Vec2::new(48.0, 48.0));
            ui.painter().rect_filled(
                center_play_rect,
                Rounding::same(24.0),
                Color32::from_rgba_unmultiplied(15, 16, 18, 180),
            );
            ui.painter().text(
                center_play_rect.center(),
                egui::Align2::CENTER_CENTER,
                "▶",
                FontId::proportional(18.0),
                Color32::from_rgb(245, 245, 243),
            );
        }

        let badge_rect = Rect::from_min_size(
            Pos2::new(stage_rect.min.x + 12.0, stage_rect.max.y - 32.0),
            Vec2::new(164.0, 22.0),
        );
        ui.painter().rect_filled(
            badge_rect,
            Rounding::same(4 as f32),
            Color32::from_rgba_unmultiplied(15, 16, 18, 195),
        );
        ui.painter().text(
            Pos2::new(badge_rect.min.x + 8.0, badge_rect.center().y),
            egui::Align2::LEFT_CENTER,
            format!(
                "{} {} / {}",
                if ed.is_playing { "▶" } else { "⏸" },
                format_mm_ss(ed.playhead_secs),
                clip.duration_label
            ),
            FontId::monospace(11.0),
            Color32::from_rgb(240, 240, 238),
        );

        let open_btn_rect = Rect::from_min_size(
            Pos2::new(stage_rect.max.x - 122.0, stage_rect.max.y - 32.0),
            Vec2::new(110.0, 22.0),
        );
        let open_resp = ui.interact(
            open_btn_rect,
            Id::new(("open_in_player", clip.id)),
            Sense::click(),
        );
        if open_resp.hovered() {
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        }
        ui.painter().rect_filled(
            open_btn_rect,
            Rounding::same(4 as f32),
            if open_resp.hovered() {
                c.accent
            } else {
                Color32::from_rgba_unmultiplied(15, 16, 18, 195)
            },
        );
        ui.painter().text(
            open_btn_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Open in player",
            FontId::proportional(11.5),
            if open_resp.hovered() {
                c.accent_text
            } else {
                Color32::from_rgb(240, 240, 238)
            },
        );
        if open_resp.clicked() {
            ed.is_playing = false;
            ed.last_play_tick = None;
            open_file_in_default_player(&clip.file_path);
        } else if stage_resp.double_clicked() {
            ed.fullscreen_preview = !ed.fullscreen_preview;
        } else if stage_resp.clicked() {
            ed.is_playing = !ed.is_playing;
            if ed.is_playing {
                if ed.playhead_secs >= ed.trim_end_secs - 0.05 {
                    ed.playhead_secs = ed.trim_start_secs;
                }
                ed.last_play_tick = Some(Instant::now());
                ctx.request_repaint();
            } else {
                ed.last_play_tick = None;
            }
        }

        ui.add_space(12.0);

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.spacing_mut().item_spacing.y = 6.0;

            let play_label = if ed.is_playing {
                "⏸ Pause (Space)"
            } else {
                "▶ Play (Space)"
            };
            if draw_ghost_button(ui, play_label, true, c).clicked() {
                ed.is_playing = !ed.is_playing;
                if ed.is_playing {
                    if ed.playhead_secs >= ed.trim_end_secs - 0.05 {
                        ed.playhead_secs = ed.trim_start_secs;
                    }
                    ed.last_play_tick = Some(Instant::now());
                    ctx.request_repaint();
                } else {
                    ed.last_play_tick = None;
                }
            }

            if draw_ghost_button(ui, "[ Set Start (I)", true, c).clicked() {
                let snapped = ed.snap_to_keyframe(ed.playhead_secs, dur);
                ed.trim_start_secs = snapped.min(ed.trim_end_secs - 0.25).max(0.0);
            }

            if draw_ghost_button(ui, "] Set End (O)", true, c).clicked() {
                let snapped = ed.snap_to_keyframe(ed.playhead_secs, dur);
                ed.trim_end_secs = snapped.max(ed.trim_start_secs + 0.25).min(dur);
            }

            if draw_ghost_button(ui, "✂ Split Here (S)", true, c).clicked() {
                let snapped = ed.snap_to_keyframe(ed.playhead_secs, dur);
                if snapped > 0.15 && snapped < dur - 0.15 {
                    if let Some(existing) = ed
                        .splits_secs
                        .iter()
                        .position(|&x| (x - snapped).abs() < 0.25)
                    {
                        ed.splits_secs.remove(existing);
                    } else if ed.splits_secs.len() < 12 {
                        ed.splits_secs.push(snapped);
                        ed.splits_secs
                            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    }
                }
            }

            if draw_ghost_button(ui, "⛶ Fullscreen (F)", true, c).clicked() {
                ed.fullscreen_preview = !ed.fullscreen_preview;
            }
        });

        ui.add_space(12.0);

        ui.horizontal_wrapped(|ui| {
            let kept = match ed.trim_mode {
                TrimActionMode::Keep => (ed.trim_end_secs - ed.trim_start_secs).max(0.0),
                TrimActionMode::Remove => {
                    (dur - (ed.trim_end_secs - ed.trim_start_secs)).max(0.0)
                }
            };
            ui.label(RichText::new(&clip.display_title).size(15.0).color(c.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut readout = format!(
                    "{} – {}  ({} kept)",
                    format_mm_ss(ed.trim_start_secs),
                    format_mm_ss(ed.trim_end_secs),
                    format_mm_ss(kept)
                );
                if !ed.splits_secs.is_empty() {
                    readout.push_str(&format!(" · {} parts", ed.splits_secs.len() + 1));
                }
                ui.label(RichText::new(readout).font(FontId::monospace(12.0)).color(c.text_2));
            });
        });

        ui.add_space(12.0);

        let (track_rect, track_resp) =
            ui.allocate_exact_size(Vec2::new(width, 44.0), Sense::click_and_drag());
        let painter = ui.painter();
        painter.rect(
            track_rect,
            Rounding::same(6 as f32),
            c.surface_2,
            Stroke::new(1.0_f32, c.border),
        );

        let track_w = track_rect.width().max(1.0);
        for &kf in &ed.keyframes_secs {
            let kx = track_rect.min.x + ((kf / dur) as f32).clamp(0.0, 1.0) * track_w;
            painter.line_segment(
                [
                    Pos2::new(kx, track_rect.max.y - 7.0),
                    Pos2::new(kx, track_rect.max.y),
                ],
                Stroke::new(1.0_f32, c.border_strong),
            );
        }

        let start_x =
            track_rect.min.x + ((ed.trim_start_secs / dur) as f32).clamp(0.0, 1.0) * track_w;
        let end_x =
            track_rect.min.x + ((ed.trim_end_secs / dur) as f32).clamp(0.0, 1.0) * track_w;

        let sel_rect = Rect::from_min_max(
            Pos2::new(start_x, track_rect.min.y),
            Pos2::new(end_x.max(start_x + 2.0), track_rect.max.y),
        );
        let (sel_fill, sel_stroke) = match ed.trim_mode {
            TrimActionMode::Keep => (c.accent_soft, c.accent),
            TrimActionMode::Remove => (
                Color32::from_rgba_unmultiplied(232, 100, 82, 35),
                c.danger,
            ),
        };
        painter.rect_filled(sel_rect, Rounding::ZERO, sel_fill);
        painter.line_segment(
            [sel_rect.left_top(), sel_rect.right_top()],
            Stroke::new(2.0_f32, sel_stroke),
        );
        painter.line_segment(
            [sel_rect.left_bottom(), sel_rect.right_bottom()],
            Stroke::new(2.0_f32, sel_stroke),
        );

        let handle_l = Rect::from_min_max(
            Pos2::new(start_x, track_rect.min.y),
            Pos2::new(start_x + 8.0, track_rect.max.y),
        );
        let handle_r = Rect::from_min_max(
            Pos2::new(end_x - 8.0, track_rect.min.y),
            Pos2::new(end_x, track_rect.max.y),
        );
        painter.rect_filled(handle_l, Rounding::same(2 as f32), sel_stroke);
        painter.rect_filled(handle_r, Rounding::same(2 as f32), sel_stroke);

        for &sp in &ed.splits_secs {
            let sx = track_rect.min.x + ((sp / dur) as f32).clamp(0.0, 1.0) * track_w;
            painter.line_segment(
                [
                    Pos2::new(sx, track_rect.min.y),
                    Pos2::new(sx, track_rect.max.y),
                ],
                Stroke::new(2.0_f32, c.text),
            );
        }

        let play_x =
            track_rect.min.x + ((ed.playhead_secs / dur) as f32).clamp(0.0, 1.0) * track_w;
        painter.line_segment(
            [
                Pos2::new(play_x, track_rect.min.y - 4.0),
                Pos2::new(play_x, track_rect.max.y + 4.0),
            ],
            Stroke::new(2.0_f32, c.text),
        );

        if track_resp.secondary_clicked() {
            if let Some(pos) = track_resp.interact_pointer_pos() {
                if let Some((idx, _)) = ed
                    .splits_secs
                    .iter()
                    .enumerate()
                    .find(|(_, &sp)| {
                        let sx = track_rect.min.x + ((sp / dur) as f32).clamp(0.0, 1.0) * track_w;
                        (pos.x - sx).abs() <= 10.0
                    })
                {
                    ed.splits_secs.remove(idx);
                }
            }
        }

        if let Some(pos) = track_resp.interact_pointer_pos() {
            let rel = ((pos.x - track_rect.min.x) / track_w).clamp(0.0, 1.0) as f64;
            let raw_t = rel * dur;
            if track_resp.drag_started() {
                ed.is_playing = false;
                ed.last_play_tick = None;
                if let Some((idx, _)) = ed
                    .splits_secs
                    .iter()
                    .enumerate()
                    .find(|(_, &sp)| {
                        let sx = track_rect.min.x + ((sp / dur) as f32).clamp(0.0, 1.0) * track_w;
                        (pos.x - sx).abs() <= 9.0
                    })
                {
                    ed.dragging_split_idx = Some(idx);
                    ed.dragging_handle = Some("split");
                } else if (pos.x - start_x).abs() <= 12.0 {
                    ed.dragging_handle = Some("start");
                } else if (pos.x - end_x).abs() <= 12.0 {
                    ed.dragging_handle = Some("end");
                } else {
                    ed.dragging_handle = Some("playhead");
                }
            }
            if track_resp.dragged() || track_resp.clicked() {
                let snapped = ed.snap_to_keyframe(raw_t, dur);
                match ed.dragging_handle {
                    Some("split") => {
                        if let Some(idx) = ed.dragging_split_idx {
                            if idx < ed.splits_secs.len() {
                                ed.splits_secs[idx] = snapped.clamp(0.15, (dur - 0.15).max(0.2));
                                ed.playhead_secs = ed.splits_secs[idx];
                            }
                        }
                    }
                    Some("start") => {
                        ed.trim_start_secs = snapped.min(ed.trim_end_secs - 0.5).max(0.0);
                        ed.playhead_secs = ed.trim_start_secs;
                    }
                    Some("end") => {
                        ed.trim_end_secs = snapped.max(ed.trim_start_secs + 0.5).min(dur);
                        ed.playhead_secs = ed.trim_end_secs;
                    }
                    _ => {
                        ed.is_playing = false;
                        ed.last_play_tick = None;
                        ed.playhead_secs = raw_t.clamp(0.0, dur);
                    }
                }
                let req_ms = (ed.playhead_secs * 1000.0).round() as u64;
                ed.pending_preview_ms = Some((req_ms, Instant::now()));
            }
        }
        if track_resp.drag_stopped() {
            ed.dragging_handle = None;
            if ed.dragging_split_idx.take().is_some() {
                ed.splits_secs
                    .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                ed.splits_secs.dedup_by(|a, b| (*a - *b).abs() < 0.05);
            }
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("0:00").font(FontId::monospace(11.0)).color(c.text_3));
            if !ed.splits_secs.is_empty() {
                ui.add_space(12.0);
                ui.label(
                    RichText::new("Right-click a split line to remove it")
                        .size(11.0)
                        .color(c.text_3),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(&clip.duration_label)
                        .font(FontId::monospace(11.0))
                        .color(c.text_3),
                );
            });
        });
    }

    fn render_editor_right_column(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        clip: &ClipItem,
        width: f32,
        c: ThemeColors,
    ) {
        let mut save_copy_clicked = false;
        let mut export_stems_clicked = false;
        let mut cancel_job_clicked = false;

        let Some(ref mut ed) = self.editor_state else {
            return;
        };

        section_title(ui, "Cut & Split", c, true);

        let mode_hint = match ed.trim_mode {
            TrimActionMode::Keep => "Saves only the selected range",
            TrimActionMode::Remove => "Cuts out the selected range and joins the rest",
        };
        draw_setting_row(ui, width, "Selection", Some(mode_hint), c, |ui| {
            let mut mode_str = match ed.trim_mode {
                TrimActionMode::Keep => "Keep",
                TrimActionMode::Remove => "Remove",
            };
            if draw_segmented_buttons(ui, "ed_trim_mode", &["Keep", "Remove"], &mut mode_str, c) {
                ed.trim_mode = if mode_str == "Remove" {
                    TrimActionMode::Remove
                } else {
                    TrimActionMode::Keep
                };
            }
        });

        let cut_hint = match ed.cut_mode {
            CutMode::KeyframeSnap | CutMode::FastKeyframeCopy => "Instant, zero quality loss",
            CutMode::FrameExact => "Re-encodes around the cut",
        };
        draw_setting_row(ui, width, "Precision", Some(cut_hint), c, |ui| {
            let mut p_str = match ed.cut_mode {
                CutMode::KeyframeSnap | CutMode::FastKeyframeCopy => "Keyframe",
                CutMode::FrameExact => "Exact",
            };
            if draw_segmented_buttons(ui, "ed_cut_mode", &["Keyframe", "Exact"], &mut p_str, c) {
                ed.cut_mode = if p_str == "Exact" {
                    CutMode::FrameExact
                } else {
                    CutMode::KeyframeSnap
                };
            }
        });

        let split_hint = if ed.splits_secs.is_empty() {
            "Slice clip into parts at current time".to_string()
        } else {
            let pts: Vec<String> = ed.splits_secs.iter().map(|&t| format_mm_ss(t)).collect();
            format!("Splits at {}", pts.join(", "))
        };
        draw_setting_row(ui, width, "Split at playhead", Some(&split_hint), c, |ui| {
            if !ed.splits_secs.is_empty()
                && draw_ghost_button(ui, "Clear", false, c).clicked()
            {
                ed.splits_secs.clear();
            }
            if draw_ghost_button(ui, "Add split", false, c).clicked() {
                let dur = clip.duration_secs.max(1.0);
                let t = ed.snap_to_keyframe(ed.playhead_secs, dur);
                if t > 0.5
                    && t < dur - 0.5
                    && ed.splits_secs.iter().all(|&x| (x - t).abs() > 0.4)
                {
                    ed.splits_secs.push(t);
                    ed.splits_secs
                        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                }
            }
        });

        section_title(ui, "Video", c, false);

        draw_setting_row(ui, width, "Resolution", None, c, |ui| {
            egui::ComboBox::from_id_salt("ed_res_combo")
                .selected_text(RichText::new(&ed.resolution).size(13.0).color(c.text))
                .width(120.0)
                .show_ui(ui, |ui| {
                    for opt in ["Original", "1440p", "1080p", "720p"] {
                        ui.selectable_value(&mut ed.resolution, opt.to_string(), opt);
                    }
                });
        });

        draw_setting_row(ui, width, "Frame rate", None, c, |ui| {
            let mut fps_val = ed.fps.as_str();
            if draw_segmented_buttons(
                ui,
                "ed_fps_seg",
                &["Original", "60 fps", "30 fps"],
                &mut fps_val,
                c,
            ) {
                ed.fps = fps_val.to_string();
            }
        });

        draw_setting_row(ui, width, "Quality", None, c, |ui| {
            let mut q_val = ed.quality.as_str();
            if draw_segmented_buttons(
                ui,
                "ed_qual_seg",
                &["High", "Medium", "Small file"],
                &mut q_val,
                c,
            ) {
                ed.quality = q_val.to_string();
            }
        });

        section_title(ui, "Audio tracks", c, false);

        let has_stems = clip.has_separate_stems;
        if !has_stems {
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Recorded with a single audio track. Enable 'Separate audio tracks' in Settings for per-app stems.",
                )
                .size(12.0)
                .color(c.text_3),
            );
            ui.add_space(4.0);
        }

        let any_solo = ed.stems.iter().any(|s| s.solo);
        let mut clicked_toggle_idx: Option<usize> = None;
        let mut clicked_solo_idx: Option<usize> = None;

        for idx in 0..ed.stems.len() {
            let label = ed.stems[idx].label;
            let enabled = ed.stems[idx].enabled;
            let solo = ed.stems[idx].solo;
            let muted_row =
                !has_stems || ed.remove_all_audio || !enabled || (any_solo && !solo);
            let mut vol = ed.stems[idx].volume_pct;
            let (tog, sol) = draw_stem_row(
                ui,
                width,
                label,
                enabled,
                solo,
                muted_row,
                has_stems && !ed.remove_all_audio,
                &mut vol,
                c,
            );
            ed.stems[idx].volume_pct = vol;
            if tog {
                clicked_toggle_idx = Some(idx);
            }
            if sol {
                clicked_solo_idx = Some(idx);
            }
        }

        if let Some(i) = clicked_toggle_idx {
            ed.stems[i].enabled = !ed.stems[i].enabled;
        }
        if let Some(i) = clicked_solo_idx {
            let was_solo = ed.stems[i].solo;
            for st in &mut ed.stems {
                st.solo = false;
            }
            ed.stems[i].solo = !was_solo;
        }

        draw_setting_row(ui, width, "Remove all audio", None, c, |ui| {
            let _ = draw_toggle_switch(ui, &mut ed.remove_all_audio, c);
        });

        let keep_tracks_hint = if has_stems {
            "Preserve stems in the saved file"
        } else {
            "Unavailable (single-track recording)"
        };
        draw_setting_row(
            ui,
            width,
            "Keep separate tracks",
            Some(keep_tracks_hint),
            c,
            |ui| {
                ui.add_enabled_ui(has_stems && !ed.remove_all_audio, |ui| {
                    let _ = draw_toggle_switch(ui, &mut ed.keep_separate_tracks, c);
                });
            },
        );

        ui.add_space(18.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let save_label = if ed.busy {
                "Processing..."
            } else if !ed.splits_secs.is_empty() {
                "Save split parts"
            } else {
                "Save copy"
            };
            if draw_primary_button(ui, save_label, !ed.busy, c).clicked() && !ed.busy {
                save_copy_clicked = true;
            }
            if ed.busy {
                if draw_ghost_button(ui, "Cancel", false, c).clicked() {
                    cancel_job_clicked = true;
                }
            } else if has_stems
                && draw_ghost_button(ui, "Export stems (.m4a)", false, c).clicked()
            {
                export_stems_clicked = true;
            }
        });

        if !ed.status_line.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new(&ed.status_line).size(12.0).color(c.text_2));
        }

        if cancel_job_clicked {
            if let Some(ref mut ed_state) = self.editor_state {
                if let Some(ref flag) = ed_state.cancel_flag {
                    flag.store(true, Ordering::Relaxed);
                }
                ed_state.status_line = "Canceling...".to_string();
            }
        }
        if save_copy_clicked {
            self.trigger_editor_save_copy(ctx);
        }
        if export_stems_clicked {
            self.trigger_editor_export_stems(ctx);
        }
    }

    fn render_settings_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, c: ThemeColors) {
        let cfg = self.bridge.recorder_config();

        ScrollArea::vertical()
            .id_salt("settings_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let settings_w = ui.available_width().min(620.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(settings_w, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        section_title(ui, "Basics", c, true);

                        draw_setting_row(ui, settings_w, "Appearance", None, c, |ui| {
                            let mut cur_app = match self.appearance {
                                AppearancePreference::System => "System",
                                AppearancePreference::Light => "Light",
                                AppearancePreference::Dark => "Dark",
                            };
                            if draw_segmented_buttons(
                                ui,
                                "app_theme_seg",
                                &["System", "Light", "Dark"],
                                &mut cur_app,
                                c,
                            ) {
                                self.appearance = match cur_app {
                                    "Light" => AppearancePreference::Light,
                                    "Dark" => AppearancePreference::Dark,
                                    _ => AppearancePreference::System,
                                };
                                let _ = self
                                    .bridge
                                    .set_appearance_mode(self.appearance.as_config_str());
                                let eff_dark = self.resolve_effective_dark(ctx);
                                self.apply_theme(ctx, eff_dark);
                            }
                        });

                        let mic_hint = self.selected_mic_missing.as_ref().map(|m| {
                            format!("'{m}' is disconnected — using Default")
                        });
                        draw_setting_row(
                            ui,
                            settings_w,
                            "Microphone",
                            mic_hint.as_deref(),
                            c,
                            |ui| {
                                if draw_ghost_button(ui, "Refresh", false, c).clicked() {
                                    self.microphones = self.bridge.list_microphones();
                                    let latest_cfg = self.bridge.recorder_config();
                                    self.selected_mic_missing =
                                        check_missing_microphone(&latest_cfg, &self.microphones);
                                }
                                let current_mic = cfg
                                    .mic_device
                                    .clone()
                                    .unwrap_or_else(|| "Default".to_string());
                                let mut selected_mic = current_mic.clone();
                                egui::ComboBox::from_id_salt("settings_mic_combo")
                                    .selected_text(
                                        RichText::new(&selected_mic).size(13.0).color(c.text),
                                    )
                                    .width(210.0)
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut selected_mic,
                                            "Default".to_string(),
                                            "Default",
                                        );
                                        for dev in &self.microphones {
                                            ui.selectable_value(
                                                &mut selected_mic,
                                                dev.name.clone(),
                                                &dev.name,
                                            );
                                        }
                                    });
                                if selected_mic != current_mic {
                                    let opt = if selected_mic == "Default" {
                                        None
                                    } else {
                                        Some(selected_mic)
                                    };
                                    let _ = self.bridge.set_mic_device(opt);
                                    let latest_cfg = self.bridge.recorder_config();
                                    self.selected_mic_missing =
                                        check_missing_microphone(&latest_cfg, &self.microphones);
                                }
                            },
                        );

                        draw_setting_row(ui, settings_w, "Record audio from", None, c, |ui| {
                            let current_label = match cfg.audio {
                                AudioMode::Both => "Game + mic",
                                AudioMode::SystemOnly => "Game only",
                                AudioMode::MicOnly => "Mic only",
                                AudioMode::None => "No audio",
                            };
                            let mut selected_label = current_label.to_string();
                            egui::ComboBox::from_id_salt("settings_audio_mode_combo")
                                .selected_text(
                                    RichText::new(&selected_label).size(13.0).color(c.text),
                                )
                                .width(170.0)
                                .show_ui(ui, |ui| {
                                    for opt in
                                        ["Game + mic", "Game only", "Mic only", "No audio"]
                                    {
                                        ui.selectable_value(
                                            &mut selected_label,
                                            opt.to_string(),
                                            opt,
                                        );
                                    }
                                });
                            if selected_label != current_label {
                                let new_mode = match selected_label.as_str() {
                                    "Game only" => AudioMode::SystemOnly,
                                    "Mic only" => AudioMode::MicOnly,
                                    "No audio" => AudioMode::None,
                                    _ => AudioMode::Both,
                                };
                                let _ = self.bridge.update_and_save_config(|c| {
                                    c.audio = new_mode;
                                });
                            }
                        });

                        let is_custom_storage =
                            matches!(cfg.storage.mode, StorageMode::Custom(_));
                        draw_setting_row(
                            ui,
                            settings_w,
                            "Save location",
                            Some("Uses the drive with the most free space"),
                            c,
                            |ui| {
                                let mut mode_str = if is_custom_storage {
                                    "Custom"
                                } else {
                                    "Automatic"
                                };
                                if draw_segmented_buttons(
                                    ui,
                                    "storage_mode_seg",
                                    &["Automatic", "Custom"],
                                    &mut mode_str,
                                    c,
                                ) {
                                    if mode_str == "Automatic" {
                                        let _ = self.bridge.update_and_save_config(|c| {
                                            c.storage.mode = StorageMode::AutoPartition;
                                        });
                                        let auto_p = self.bridge.resolve_active_clips_dir();
                                        self.folder_validation_status = Some(
                                            self.bridge.validate_and_query_folder_free_gb(&auto_p),
                                        );
                                        self.spawn_background_library_scan(ctx);
                                    } else {
                                        let p = PathBuf::from(self.custom_path_buffer.trim());
                                        self.apply_custom_save_folder(p, ctx);
                                    }
                                }
                            },
                        );

                        let path_sub_hint = match &self.folder_validation_status {
                            Some(Ok(free_gb)) => {
                                if is_custom_storage {
                                    format!("Custom folder · {free_gb:.1} GB free")
                                } else {
                                    format!("Selected automatically · {free_gb:.1} GB free")
                                }
                            }
                            Some(Err(err)) => format!("Folder error: {err}"),
                            None => "Selected automatically".to_string(),
                        };

                        draw_setting_row(
                            ui,
                            settings_w,
                            "",
                            Some(&path_sub_hint),
                            c,
                            |ui| {
                                if is_custom_storage {
                                    if draw_ghost_button(ui, "Apply", false, c).clicked() {
                                        let p = PathBuf::from(self.custom_path_buffer.trim());
                                        self.apply_custom_save_folder(p, ctx);
                                    }
                                    if draw_ghost_button(ui, "Browse...", false, c).clicked() {
                                        spawn_native_folder_picker(
                                            self.async_tx.clone(),
                                            ctx.clone(),
                                        );
                                    }
                                    ui.add_sized(
                                        [200.0, 26.0],
                                        egui::TextEdit::singleline(&mut self.custom_path_buffer)
                                            .font(FontId::monospace(12.0)),
                                    );
                                } else {
                                    let auto_dir = self
                                        .bridge
                                        .resolve_active_clips_dir()
                                        .to_string_lossy()
                                        .to_string();
                                    ui.label(
                                        RichText::new(auto_dir)
                                            .font(FontId::monospace(12.0))
                                            .color(c.text_2),
                                    );
                                }
                            },
                        );

                        section_title(ui, "Shortcuts", c, false);

                        draw_setting_row(ui, settings_w, "Keybinds", None, c, |ui| {
                            let mut kb_mode = if self.keybinds_custom_mode {
                                "Custom"
                            } else {
                                "Default"
                            };
                            if draw_segmented_buttons(
                                ui,
                                "kb_mode_seg",
                                &["Default", "Custom"],
                                &mut kb_mode,
                                c,
                            ) {
                                if kb_mode == "Default" {
                                    self.keybinds_custom_mode = false;
                                    if self.recording_keybind_slot.is_some() {
                                        self.recording_keybind_slot = None;
                                        self.bridge.set_hotkeys_paused(false);
                                    }
                                    self.keybind_error_msg = None;
                                    let _ = self.bridge.set_keybinds(KeybindsConfig::default());
                                } else {
                                    self.keybinds_custom_mode = true;
                                }
                            }
                        });

                        let kb_rows: [(&'static str, &str, &str); 4] = [
                            ("replay_clip", "Save replay", &cfg.keybinds.replay_clip.label),
                            (
                                "direct_clip",
                                "Clip last seconds",
                                &cfg.keybinds.direct_clip.label,
                            ),
                            (
                                "discord_clip",
                                "Compress for Discord",
                                &cfg.keybinds.discord_clip.label,
                            ),
                            (
                                "mic_mute",
                                "Mute microphone",
                                &cfg.keybinds.mic_mute.label,
                            ),
                        ];

                        for (slot_id, row_title, key_label) in kb_rows {
                            let is_rec = self.recording_keybind_slot == Some(slot_id);
                            draw_setting_row(ui, settings_w, row_title, None, c, |ui| {
                                let display = if is_rec {
                                    "Press any key... (Esc)"
                                } else {
                                    key_label
                                };
                                let btn_resp = draw_keycap_button(
                                    ui,
                                    display,
                                    is_rec,
                                    self.keybinds_custom_mode,
                                    c,
                                );
                                if btn_resp.clicked() && self.keybinds_custom_mode {
                                    if is_rec {
                                        self.recording_keybind_slot = None;
                                        self.keybind_error_msg = None;
                                        self.bridge.set_hotkeys_paused(false);
                                    } else {
                                        self.recording_keybind_slot = Some(slot_id);
                                        self.keybind_error_msg = None;
                                        self.bridge.set_hotkeys_paused(true);
                                    }
                                }
                            });
                        }

                        if let Some(ref kb_err) = self.keybind_error_msg {
                            ui.add_space(4.0);
                            ui.label(RichText::new(kb_err).size(12.0).color(c.danger));
                        }

                        section_title(ui, "Video", c, false);

                        draw_setting_row(ui, settings_w, "Quality", None, c, |ui| {
                            let cur_q = match cfg.quality {
                                Quality::Low => "Low",
                                Quality::Medium => "Medium",
                                Quality::High => "High",
                                Quality::Lossless => "Lossless",
                            };
                            let mut sel_q = cur_q.to_string();
                            egui::ComboBox::from_id_salt("settings_qual_combo")
                                .selected_text(RichText::new(&sel_q).size(13.0).color(c.text))
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for opt in ["High", "Medium", "Low", "Lossless"] {
                                        ui.selectable_value(&mut sel_q, opt.to_string(), opt);
                                    }
                                });
                            if sel_q != cur_q {
                                if let Some(parsed) = Quality::parse_str(&sel_q) {
                                    let _ = self.bridge.update_and_save_config(|c| {
                                        c.quality = parsed;
                                    });
                                }
                            }
                        });

                        draw_setting_row(ui, settings_w, "Frame rate", None, c, |ui| {
                            let cur_fps = format!("{} fps", cfg.fps.as_u32());
                            let mut sel_fps = cur_fps.clone();
                            egui::ComboBox::from_id_salt("settings_fps_combo")
                                .selected_text(RichText::new(&sel_fps).size(13.0).color(c.text))
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for opt in ["30 fps", "60 fps", "120 fps"] {
                                        ui.selectable_value(&mut sel_fps, opt.to_string(), opt);
                                    }
                                });
                            if sel_fps != cur_fps {
                                let num_str = sel_fps.trim_end_matches(" fps");
                                if let Some(parsed) = Fps::parse_str(num_str) {
                                    let _ = self.bridge.update_and_save_config(|c| {
                                        c.fps = parsed;
                                    });
                                }
                            }
                        });

                        draw_setting_row(ui, settings_w, "Resolution", None, c, |ui| {
                            let cur_res = match (cfg.width, cfg.height) {
                                (2560, 1440) => "1440p",
                                (1280, 720) => "720p",
                                _ => "1080p",
                            };
                            let mut sel_res = cur_res.to_string();
                            egui::ComboBox::from_id_salt("settings_res_combo")
                                .selected_text(RichText::new(&sel_res).size(13.0).color(c.text))
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for opt in ["1080p", "1440p", "720p"] {
                                        ui.selectable_value(&mut sel_res, opt.to_string(), opt);
                                    }
                                });
                            if sel_res != cur_res {
                                let _ = self
                                    .bridge
                                    .apply_setting_by_path("capture -> resolution", &sel_res);
                            }
                        });

                        draw_setting_row(ui, settings_w, "Format", None, c, |ui| {
                            let cur_fmt = cfg.container.extension().to_uppercase();
                            let mut sel_fmt = cur_fmt.clone();
                            egui::ComboBox::from_id_salt("settings_fmt_combo")
                                .selected_text(RichText::new(&sel_fmt).size(13.0).color(c.text))
                                .width(110.0)
                                .show_ui(ui, |ui| {
                                    for opt in ["MP4", "MKV"] {
                                        ui.selectable_value(&mut sel_fmt, opt.to_string(), opt);
                                    }
                                });
                            if sel_fmt != cur_fmt {
                                let new_c = if sel_fmt == "MKV" {
                                    Container::Mkv
                                } else {
                                    Container::Mp4
                                };
                                let _ = self.bridge.update_and_save_config(|c| {
                                    c.container = new_c;
                                });
                            }
                        });

                        section_title(ui, "Replay timing", c, false);

                        draw_setting_row(
                            ui,
                            settings_w,
                            "Replay length",
                            Some("How far back the buffer keeps"),
                            c,
                            |ui| {
                                let cur_b = format!("{} s", cfg.buffer_duration_secs);
                                let mut sel_b = cur_b.clone();
                                egui::ComboBox::from_id_salt("settings_buf_combo")
                                    .selected_text(RichText::new(&sel_b).size(13.0).color(c.text))
                                    .width(110.0)
                                    .show_ui(ui, |ui| {
                                        for opt in ["30 s", "60 s", "90 s", "120 s", "300 s"] {
                                            ui.selectable_value(&mut sel_b, opt.to_string(), opt);
                                        }
                                    });
                                if sel_b != cur_b {
                                    if let Ok(v) = sel_b.trim_end_matches(" s").parse::<u64>() {
                                        let _ = self.bridge.update_and_save_config(|c| {
                                            c.buffer_duration_secs = v;
                                        });
                                    }
                                }
                            },
                        );

                        draw_setting_row(ui, settings_w, "Before highlight", None, c, |ui| {
                            let cur_pre = format!("{} s", cfg.pre_roll_secs);
                            let mut sel_pre = cur_pre.clone();
                            egui::ComboBox::from_id_salt("settings_pre_combo")
                                .selected_text(RichText::new(&sel_pre).size(13.0).color(c.text))
                                .width(110.0)
                                .show_ui(ui, |ui| {
                                    for opt in ["5 s", "10 s", "15 s", "20 s", "30 s"] {
                                        ui.selectable_value(&mut sel_pre, opt.to_string(), opt);
                                    }
                                });
                            if sel_pre != cur_pre {
                                if let Ok(v) = sel_pre.trim_end_matches(" s").parse::<u64>() {
                                    let _ = self.bridge.update_and_save_config(|c| {
                                        c.pre_roll_secs = v;
                                    });
                                }
                            }
                        });

                        draw_setting_row(ui, settings_w, "After highlight", None, c, |ui| {
                            let cur_post = format!("{} s", cfg.post_roll_secs);
                            let mut sel_post = cur_post.clone();
                            egui::ComboBox::from_id_salt("settings_post_combo")
                                .selected_text(RichText::new(&sel_post).size(13.0).color(c.text))
                                .width(110.0)
                                .show_ui(ui, |ui| {
                                    for opt in ["5 s", "8 s", "10 s", "15 s"] {
                                        ui.selectable_value(&mut sel_post, opt.to_string(), opt);
                                    }
                                });
                            if sel_post != cur_post {
                                if let Ok(v) = sel_post.trim_end_matches(" s").parse::<u64>() {
                                    let _ = self.bridge.update_and_save_config(|c| {
                                        c.post_roll_secs = v;
                                    });
                                }
                            }
                        });

                        section_title(ui, "Audio", c, false);

                        draw_setting_row(
                            ui,
                            settings_w,
                            "Separate audio tracks",
                            Some("Game, Discord, mic and browser on their own tracks"),
                            c,
                            |ui| {
                                let mut sep = cfg.separate_audio_tracks;
                                if draw_toggle_switch(ui, &mut sep, c) {
                                    let _ = self.bridge.update_and_save_config(|c| {
                                        c.separate_audio_tracks = sep;
                                    });
                                }
                            },
                        );

                        draw_setting_row(ui, settings_w, "Audio quality", None, c, |ui| {
                            let cur_br = format!("{} kbps", cfg.audio_bitrate_kbps);
                            let mut sel_br = cur_br.clone();
                            egui::ComboBox::from_id_salt("settings_abr_combo")
                                .selected_text(RichText::new(&sel_br).size(13.0).color(c.text))
                                .width(120.0)
                                .show_ui(ui, |ui| {
                                    for opt in ["96 kbps", "128 kbps", "160 kbps", "192 kbps"] {
                                        ui.selectable_value(&mut sel_br, opt.to_string(), opt);
                                    }
                                });
                            if sel_br != cur_br {
                                if let Ok(v) = sel_br.trim_end_matches(" kbps").parse::<u32>() {
                                    let _ = self.bridge.update_and_save_config(|c| {
                                        c.audio_bitrate_kbps = v;
                                    });
                                }
                            }
                        });

                        section_title(ui, "Storage & performance", c, false);

                        draw_setting_row(ui, settings_w, "Stop recording below", None, c, |ui| {
                            let cur_gb = format!("{} GB free", cfg.storage.min_free_gb);
                            let mut sel_gb = cur_gb.clone();
                            egui::ComboBox::from_id_salt("settings_free_gb_combo")
                                .selected_text(RichText::new(&sel_gb).size(13.0).color(c.text))
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for opt in ["5 GB free", "10 GB free", "20 GB free"] {
                                        ui.selectable_value(&mut sel_gb, opt.to_string(), opt);
                                    }
                                });
                            if sel_gb != cur_gb {
                                if let Ok(v) = sel_gb.trim_end_matches(" GB free").parse::<u64>() {
                                    let _ = self.bridge.update_and_save_config(|c| {
                                        c.storage.min_free_gb = v;
                                    });
                                }
                            }
                        });

                        draw_setting_row(
                            ui,
                            settings_w,
                            "Low-spec PC mode",
                            Some("Uses less RAM and encoder power"),
                            c,
                            |ui| {
                                let mut low = cfg.low_end_mode;
                                if draw_toggle_switch(ui, &mut low, c) {
                                    let _ = self.bridge.update_and_save_config(|c| {
                                        c.low_end_mode = low;
                                    });
                                }
                            },
                        );

                        draw_setting_row(
                            ui,
                            settings_w,
                            "Show activity on Discord",
                            None,
                            c,
                            |ui| {
                                let mut rpc = self.bridge.discord_rpc_enabled();
                                if draw_toggle_switch(ui, &mut rpc, c) {
                                    let _ = self.bridge.set_discord_rpc_enabled(rpc);
                                }
                            },
                        );

                        section_title(ui, "Privacy", c, false);

                        draw_setting_row(
                            ui,
                            settings_w,
                            "Share anonymous stats",
                            Some("Settings → Privacy → Share anonymous stats"),
                            c,
                            |ui| {
                                let mut stats = self.bridge.share_anonymous_stats();
                                if draw_toggle_switch(ui, &mut stats, c) {
                                    let _ = self.bridge.set_privacy_share_anonymous_stats(stats);
                                }
                            },
                        );

                        ui.add_space(40.0);
                    },
                );
            });
    }
}

impl eframe::App for ClotchGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_async_messages(ctx);

        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if is_minimized {
            if !self.was_minimized {
                self.was_minimized = true;
                if let Some(ref mut ed) = self.editor_state {
                    ed.is_playing = false;
                    ed.last_play_tick = None;
                }
                self.thumb_cache.clear();
                trim_process_working_set();
            }
            return;
        }
        self.was_minimized = false;

        let effective_dark = self.resolve_effective_dark(ctx);
        if self.last_effective_dark != Some(effective_dark) {
            self.apply_theme(ctx, effective_dark);
        }

        self.handle_keybind_recording(ctx);
        let c = self.colors(ctx);

        let screen_w = ctx.screen_rect().width();
        let pad_x = if screen_w < 640.0 { 16.0 } else { 32.0 };
        let content_w = ((screen_w - pad_x * 2.0) * 0.94).clamp(280.0, 1680.0);
        let side_margin = ((screen_w - content_w) * 0.5).max(pad_x);

        egui::TopBottomPanel::top("clotch_top_header")
            .frame(
                egui::Frame::none()
                    .fill(c.bg)
                    .inner_margin(Margin {
                        left: side_margin as f32,
                        right: side_margin as f32,
                        top: 24.0,
                        bottom: 12.0,
                    }),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                self.render_header(ui, c);
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(c.bg)
                    .inner_margin(Margin {
                        left: side_margin as f32,
                        right: side_margin as f32,
                        top: 12.0,
                        bottom: 16.0,
                    }),
            )
            .show(ctx, |ui| {
                match self.view {
                    ViewTab::Clips => self.render_clips_view(ui, ctx, c),
                    ViewTab::Editor => self.render_editor_view(ui, ctx, c),
                    ViewTab::Settings => self.render_settings_view(ui, ctx, c),
                }
            });

        let mut clear_toast = false;
        let mut reveal_target: Option<PathBuf> = None;
        if let Some((ref msg, ref opt_path, ts)) = self.toast {
            let max_dur = if opt_path.is_some() {
                Duration::from_secs(6)
            } else {
                Duration::from_millis(2800)
            };
            if ts.elapsed() < max_dur {
                egui::Area::new(Id::new("clotch_toast_area"))
                    .anchor(egui::Align2::RIGHT_BOTTOM, Vec2::new(-24.0, -24.0))
                    .show(ctx, |ui| {
                        egui::Frame::none()
                            .fill(c.surface)
                            .stroke(Stroke::new(1.0_f32, c.border_strong))
                            .rounding(Rounding::same(6 as f32))
                            .inner_margin(Margin::symmetric(14.0, 10.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 12.0;
                                    ui.label(RichText::new(msg).size(12.5).color(c.text));
                                    if let Some(ref p) = opt_path {
                                        if text_action_link(ui, "Show in folder", c.accent, c.text)
                                            .clicked()
                                        {
                                            reveal_target = Some(p.clone());
                                            clear_toast = true;
                                        }
                                    }
                                });
                            });
                    });
                ctx.request_repaint_after(Duration::from_millis(250));
            } else {
                clear_toast = true;
            }
        }
        if let Some(p) = reveal_target {
            reveal_file_in_explorer(&p);
        }
        if clear_toast {
            self.toast = None;
        }
    }
}

fn section_title(ui: &mut egui::Ui, text: &str, c: ThemeColors, first: bool) {
    if !first {
        ui.add_space(28.0);
    }
    ui.label(RichText::new(text).size(12.0).color(c.text_3));
    ui.add_space(8.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter()
        .line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0_f32, c.border));
}

fn draw_setting_row<R>(
    ui: &mut egui::Ui,
    width: f32,
    label: &str,
    hint: Option<&str>,
    c: ThemeColors,
    add_right: impl FnOnce(&mut egui::Ui) -> R,
) {
    let row_h = if hint.is_some() { 54.0 } else { 44.0 };
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, row_h), Sense::hover());

    let left_w = (width * 0.52).max(120.0);
    let left_rect = Rect::from_min_max(
        rect.min,
        Pos2::new((rect.min.x + left_w).min(rect.max.x), rect.max.y),
    );
    let right_rect = Rect::from_min_max(
        Pos2::new((rect.min.x + left_w).min(rect.max.x), rect.min.y),
        rect.max,
    );

    let mut left_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(left_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    left_ui.vertical(|ui| {
        if hint.is_some() {
            ui.add_space(8.0);
        } else {
            ui.add_space(12.0);
        }
        if !label.is_empty() {
            ui.add(
                egui::Label::new(RichText::new(label).size(13.5).color(c.text)).truncate(),
            );
        }
        if let Some(h) = hint {
            ui.add(egui::Label::new(RichText::new(h).size(12.0).color(c.text_3)).truncate());
        }
    });

    let mut right_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(right_rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    right_ui.spacing_mut().item_spacing.x = 8.0;
    add_right(&mut right_ui);

    ui.painter().line_segment(
        [
            Pos2::new(rect.min.x, rect.max.y),
            Pos2::new(rect.max.x, rect.max.y),
        ],
        Stroke::new(1.0_f32, c.border),
    );
}

fn draw_stem_row(
    ui: &mut egui::Ui,
    width: f32,
    label: &str,
    enabled: bool,
    solo: bool,
    muted_visual: bool,
    interactive: bool,
    volume_pct: &mut u32,
    c: ThemeColors,
) -> (bool, bool) {
    let mut toggle_clicked = false;
    let mut solo_clicked = false;

    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 44.0), Sense::hover());
    let left_w = (width - 210.0).max(90.0);
    let left_rect = Rect::from_min_max(
        rect.min,
        Pos2::new((rect.min.x + left_w).min(rect.max.x), rect.max.y),
    );
    let right_rect = Rect::from_min_max(
        Pos2::new((rect.min.x + left_w).min(rect.max.x), rect.min.y),
        rect.max,
    );

    let mut left_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(left_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let label_col = if muted_visual { c.text_3 } else { c.text };
    left_ui.add(egui::Label::new(RichText::new(label).size(13.5).color(label_col)).truncate());

    let mut right_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(right_rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    right_ui.spacing_mut().item_spacing.x = 8.0;

    right_ui.add_enabled_ui(interactive, |ui| {
        let mut enabled_copy = enabled;
        if draw_toggle_switch(ui, &mut enabled_copy, c) {
            toggle_clicked = true;
        }
        let solo_resp = draw_mini_pill(ui, "Solo", solo, c);
        if solo_resp.clicked() {
            solo_clicked = true;
        }
        ui.label(
            RichText::new(format!("{volume_pct}%"))
                .font(FontId::monospace(11.0))
                .color(c.text_2),
        );
        let mut v_f32 = *volume_pct as f32;
        let slider = egui::Slider::new(&mut v_f32, 0.0..=150.0)
            .show_value(false)
            .trailing_fill(true);
        if ui.add_sized([74.0, 18.0], slider).changed() {
            *volume_pct = v_f32.round() as u32;
        }
    });

    ui.painter().line_segment(
        [
            Pos2::new(rect.min.x, rect.max.y),
            Pos2::new(rect.max.x, rect.max.y),
        ],
        Stroke::new(1.0_f32, c.border),
    );

    (toggle_clicked, solo_clicked)
}

fn draw_toggle_switch(ui: &mut egui::Ui, on: &mut bool, c: ThemeColors) -> bool {
    let desired = Vec2::new(34.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    if resp.hovered() && ui.is_enabled() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let mut changed = false;
    if resp.clicked() && ui.is_enabled() {
        *on = !*on;
        changed = true;
    }
    let bg = if *on { c.accent } else { c.border_strong };
    ui.painter().rect_filled(rect, Rounding::same(10 as f32), bg);
    let knob_x = if *on {
        rect.max.x - 10.0
    } else {
        rect.min.x + 10.0
    };
    let knob_col = if *on {
        c.accent_text
    } else {
        Color32::from_rgb(250, 250, 248)
    };
    ui.painter()
        .circle_filled(Pos2::new(knob_x, rect.center().y), 7.0, knob_col);
    changed
}

fn draw_segmented_buttons(
    ui: &mut egui::Ui,
    id_salt: &str,
    options: &[&'static str],
    current: &mut &str,
    c: ThemeColors,
) -> bool {
    let mut changed = false;
    egui::Frame::none()
        .fill(c.surface)
        .stroke(Stroke::new(1.0_f32, c.border))
        .rounding(Rounding::same(6 as f32))
        .inner_margin(Margin::same(2.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for &opt in options.iter() {
                let active = *current == opt;
                let galley = ui.painter().layout_no_wrap(
                    opt.to_string(),
                    FontId::proportional(12.5),
                    if active { c.accent_text } else { c.text_2 },
                );
                let btn_size = Vec2::new(galley.size().x + 20.0, 24.0);
                let (rect, resp) = ui.allocate_exact_size(btn_size, Sense::click());
                let _ = id_salt;
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                if active {
                    ui.painter()
                        .rect_filled(rect, Rounding::same(4 as f32), c.accent);
                } else if resp.hovered() {
                    ui.painter()
                        .rect_filled(rect, Rounding::same(4 as f32), c.surface_2);
                }
                ui.painter().galley(
                    Pos2::new(
                        rect.center().x - galley.size().x * 0.5,
                        rect.center().y - galley.size().y * 0.5,
                    ),
                    galley,
                    if active { c.accent_text } else { c.text_2 },
                );
                if resp.clicked() && !active {
                    *current = opt;
                    changed = true;
                }
            }
        });
    changed
}

fn draw_primary_button(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    c: ThemeColors,
) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        FontId::proportional(13.0),
        c.accent_text,
    );
    let size = Vec2::new(galley.size().x + 28.0, 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() && enabled {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let fill = if enabled { c.accent } else { c.border_strong };
    ui.painter().rect_filled(rect, Rounding::same(6 as f32), fill);
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        c.accent_text,
    );
    resp
}

fn draw_ghost_button(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    c: ThemeColors,
) -> egui::Response {
    let text_col = if active { c.accent } else { c.text };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::proportional(12.5), text_col);
    let size = Vec2::new(galley.size().x + 22.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let stroke_col = if active {
        c.accent
    } else if resp.hovered() {
        c.border_strong
    } else {
        c.border
    };
    ui.painter().rect(
        rect,
        Rounding::same(6 as f32),
        c.surface,
        Stroke::new(1.0_f32, stroke_col),
    );
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        text_col,
    );
    resp
}

fn draw_mini_pill(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    c: ThemeColors,
) -> egui::Response {
    let text_col = if active { c.accent } else { c.text_2 };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::proportional(11.0), text_col);
    let size = Vec2::new(galley.size().x + 14.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() && ui.is_enabled() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let fill = if active { c.accent_soft } else { Color32::TRANSPARENT };
    let stroke_col = if active {
        c.accent
    } else if resp.hovered() {
        c.border_strong
    } else {
        c.border
    };
    ui.painter().rect(
        rect,
        Rounding::same(4 as f32),
        fill,
        Stroke::new(1.0_f32, stroke_col),
    );
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        text_col,
    );
    resp
}

fn draw_keycap_button(
    ui: &mut egui::Ui,
    label: &str,
    recording: bool,
    custom_enabled: bool,
    c: ThemeColors,
) -> egui::Response {
    let text_col = if recording {
        c.accent
    } else if custom_enabled {
        c.text
    } else {
        c.text_2
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::monospace(12.0), text_col);
    let size = Vec2::new((galley.size().x + 22.0).max(50.0), 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() && custom_enabled {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let fill = if recording { c.accent_soft } else { c.surface };
    let stroke_col = if recording {
        c.accent
    } else if resp.hovered() && custom_enabled {
        c.border_strong
    } else {
        c.border
    };
    ui.painter().rect(
        rect,
        Rounding::same(5 as f32),
        fill,
        Stroke::new(1.0_f32, stroke_col),
    );
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        text_col,
    );
    resp
}

fn text_action_link(
    ui: &mut egui::Ui,
    label: &str,
    normal_col: Color32,
    hover_col: Color32,
) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::proportional(12.0), normal_col);
    let (rect, resp) = ui.allocate_exact_size(galley.size(), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let col = if resp.hovered() { hover_col } else { normal_col };
    ui.painter().text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        col,
    );
    resp
}

fn setup_unicode_system_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let candidate_paths = [
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\tahoma.ttf",
        r"C:\Windows\Fonts\arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for p in candidate_paths {
        let path = Path::new(p);
        if path.is_file() {
            if let Ok(bytes) = std::fs::read(path) {
                fonts.font_data.insert(
                    "system_unicode_fallback".to_string(),
                    FontData::from_owned(bytes),
                );
                if let Some(fam) = fonts.families.get_mut(&FontFamily::Proportional) {
                    fam.push("system_unicode_fallback".to_string());
                }
                if let Some(fam) = fonts.families.get_mut(&FontFamily::Monospace) {
                    fam.push("system_unicode_fallback".to_string());
                }
                ctx.set_fonts(fonts);
                break;
            }
        }
    }
}

fn check_missing_microphone(
    cfg: &crate::recorder::config::RecorderConfig,
    mics: &[AudioInputDeviceInfo],
) -> Option<String> {
    let saved = cfg.mic_device.as_deref()?.trim();
    if saved.is_empty()
        || saved.eq_ignore_ascii_case("default")
        || saved.eq_ignore_ascii_case("auto")
    {
        return None;
    }
    let exists = mics
        .iter()
        .any(|d| d.name.eq_ignore_ascii_case(saved) || d.id.eq_ignore_ascii_case(saved));
    if exists {
        None
    } else {
        Some(saved.to_string())
    }
}

fn resolve_app_cache_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data).join("Clotch");
        let _ = std::fs::create_dir_all(&p);
        return p;
    }
    let fallback = PathBuf::from("ClotchCache");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

fn resolve_thumbs_cache_dir() -> PathBuf {
    let p = resolve_app_cache_dir().join("thumbs");
    let _ = std::fs::create_dir_all(&p);
    p
}

fn resolve_clips_index_path() -> PathBuf {
    resolve_app_cache_dir().join("clips_index.json")
}

fn compute_stable_clip_id(path: &Path) -> u64 {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let norm = canonical.to_string_lossy().to_lowercase();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    norm.hash(&mut hasher);
    hasher.finish()
}

fn trim_process_working_set() {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn SetProcessWorkingSetSize(
                h_process: *mut std::ffi::c_void,
                dw_minimum_working_set_size: usize,
                dw_maximum_working_set_size: usize,
            ) -> i32;
        }
        unsafe {
            let h = GetCurrentProcess();
            let _ = SetProcessWorkingSetSize(h, usize::MAX, usize::MAX);
        }
    }
}

fn load_cached_clips_from_index() -> Vec<ClipItem> {
    let index_path = resolve_clips_index_path();
    let Ok(raw) = std::fs::read_to_string(&index_path) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<CachedClipIndexEntry>>(&raw) else {
        return Vec::new();
    };
    let thumbs_dir = resolve_thumbs_cache_dir();
    let mut items = Vec::with_capacity(entries.len());
    let mut seen_ids = HashSet::with_capacity(entries.len());

    for e in entries {
        let p = PathBuf::from(&e.path_str);
        if !p.exists() {
            continue;
        }
        let cid = compute_stable_clip_id(&p);
        if !seen_ids.insert(cid) {
            continue;
        }
        let thumb_path = thumbs_dir.join(format!("{cid:016x}.rgba"));
        let dur_secs = e.duration_secs.max(1.0);
        items.push(ClipItem {
            id: cid,
            display_title: e.display_title,
            game: e.game,
            tag: e.tag,
            map: e.map,
            mode: e.mode,
            weapon: e.weapon,
            kills: e.kills,
            date_label: format_relative_mtime(e.mtime_secs),
            duration_secs: dur_secs,
            duration_label: format_mm_ss(dur_secs),
            favorite: e.favorite,
            has_separate_stems: e.has_separate_stems,
            file_path: p,
            thumb_path,
            mtime_secs: e.mtime_secs,
        });
    }
    items.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    items
}

fn persist_clip_sidecar(clip: &ClipItem) {
    let sidecar_path = clip.file_path.with_extension("json");
    let mut meta = std::fs::read_to_string(&sidecar_path)
        .ok()
        .and_then(|s| serde_json::from_str::<ClipSidecarMeta>(&s).ok())
        .unwrap_or_default();
    meta.display_title = clip.display_title.clone();
    meta.game = clip.game.clone();
    meta.tag = clip.tag.clone();
    if !clip.map.is_empty() {
        meta.map = clip.map.clone();
    }
    if !clip.mode.is_empty() {
        meta.mode = clip.mode.clone();
    }
    if !clip.weapon.is_empty() {
        meta.weapon = clip.weapon.clone();
    }
    if clip.kills.is_some() {
        meta.kills = clip.kills;
    }
    meta.favorite = clip.favorite;
    meta.duration_secs = Some(clip.duration_secs);
    meta.has_separate_stems = Some(clip.has_separate_stems);
    if meta.created_epoch_secs == 0 {
        meta.created_epoch_secs = clip.mtime_secs;
    }
    if let Ok(serialized) = serde_json::to_string_pretty(&meta) {
        let _ = std::fs::write(&sidecar_path, serialized);
    }
}

fn scan_clips_directory_worker(clips_dir: &Path, bridge: &UiBridge) -> Vec<ClipItem> {
    let index_path = resolve_clips_index_path();
    let mut cached_index: HashMap<String, CachedClipIndexEntry> =
        std::fs::read_to_string(&index_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<CachedClipIndexEntry>>(&s).ok())
            .map(|list| list.into_iter().map(|e| (e.path_str.clone(), e)).collect())
            .unwrap_or_default();

    let mut search_roots: Vec<PathBuf> = Vec::new();
    search_roots.push(clips_dir.to_path_buf());
    if let Some(parent) = clips_dir.parent() {
        if parent
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("ClotchClips"))
            .unwrap_or(false)
            && !search_roots.iter().any(|r| r == parent)
        {
            search_roots.push(parent.to_path_buf());
        }
    }

    let mut raw_paths: Vec<PathBuf> = Vec::new();
    for root in &search_roots {
        collect_video_files_recursive(root, 0, &mut raw_paths);
    }
    for key in cached_index.keys() {
        let p = PathBuf::from(key);
        if p.exists() {
            raw_paths.push(p);
        }
    }

    let thumbs_dir = resolve_thumbs_cache_dir();
    let mut discovered: Vec<ClipItem> = Vec::with_capacity(raw_paths.len());
    let mut updated_index: HashMap<String, CachedClipIndexEntry> =
        HashMap::with_capacity(raw_paths.len());
    let mut seen_ids: HashSet<u64> = HashSet::with_capacity(raw_paths.len());

    for path in raw_paths {
        let cid = compute_stable_clip_id(&path);
        if !seen_ids.insert(cid) {
            continue;
        }
        if let Some(item) =
            build_clip_item_from_path(&path, bridge, &thumbs_dir, &mut cached_index)
        {
            let key = path.to_string_lossy().to_string();
            if let Some(entry) = cached_index.get(&key) {
                updated_index.insert(key, entry.clone());
            }
            discovered.push(item);
        }
    }

    let entries_vec: Vec<CachedClipIndexEntry> = updated_index.into_values().collect();
    if let Ok(json_str) = serde_json::to_string(&entries_vec) {
        let _ = std::fs::write(&index_path, json_str);
    }

    discovered.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    discovered
}

fn build_clip_item_from_path(
    path: &Path,
    bridge: &UiBridge,
    thumbs_dir: &Path,
    index_map: &mut HashMap<String, CachedClipIndexEntry>,
) -> Option<ClipItem> {
    let meta = std::fs::metadata(path).ok()?;
    let file_size_bytes = meta.len();
    if file_size_bytes == 0 {
        return None;
    }
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let clip_id = compute_stable_clip_id(path);
    let thumb_path = thumbs_dir.join(format!("{clip_id:016x}.rgba"));
    let sidecar_path = path.with_extension("json");
    let sidecar_opt: Option<ClipSidecarMeta> = std::fs::read_to_string(&sidecar_path)
        .ok()
        .and_then(|s| serde_json::from_str::<ClipSidecarMeta>(&s).ok());

    let path_key = path.to_string_lossy().to_string();
    let cached_hit = index_map.get(&path_key).filter(|e| {
        e.file_size_bytes == file_size_bytes && e.mtime_secs == mtime_secs && e.duration_secs > 0.1
    });

    let (dur_secs, has_stems) = if let Some(hit) = cached_hit {
        (hit.duration_secs, hit.has_separate_stems)
    } else if let Some(ref sc) = sidecar_opt {
        if let (Some(d), Some(st)) = (sc.duration_secs, sc.has_separate_stems) {
            (d.max(1.0), st)
        } else {
            probe_duration_and_stems(path, bridge)
        }
    } else {
        probe_duration_and_stems(path, bridge)
    };

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Clip");
    let parent_folder = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let (parsed_game, parsed_tag) = parse_legacy_filename_tokens(stem, parent_folder);
    let date_label = format_relative_mtime(mtime_secs);
    let clock_label = format_local_date_time(mtime_secs);

    let (display_title, game, tag, map, mode, weapon, kills, favorite) =
        if let Some(sc) = sidecar_opt {
            let g = if sc.game.trim().is_empty() {
                parsed_game
            } else {
                sc.game
            };
            let t = if sc.tag.trim().is_empty() {
                parsed_tag
            } else {
                sc.tag
            };
            let m = if sc.map.trim().is_empty() {
                detect_known_map_in_text(&sc.display_title)
                    .or_else(|| detect_known_map_in_text(stem))
                    .unwrap_or("")
                    .to_string()
            } else {
                sc.map
            };
            let title = if sc.display_title.trim().is_empty() {
                if m.is_empty() {
                    format!("{g} {t}, {clock_label}")
                } else {
                    format!("{g} {t} on {m}, {clock_label}")
                }
            } else {
                sc.display_title
            };
            (
                title,
                g,
                t,
                m,
                sc.mode,
                sc.weapon,
                sc.kills,
                sc.favorite,
            )
        } else {
            let m = detect_known_map_in_text(stem).unwrap_or("").to_string();
            let default_title = if m.is_empty() {
                format!("{parsed_game} {parsed_tag}, {clock_label}")
            } else {
                format!("{parsed_game} {parsed_tag} on {m}, {clock_label}")
            };
            (
                default_title,
                parsed_game,
                parsed_tag,
                m,
                String::new(),
                String::new(),
                None,
                false,
            )
        };

    index_map.insert(
        path_key.clone(),
        CachedClipIndexEntry {
            path_str: path_key,
            file_size_bytes,
            mtime_secs,
            duration_secs: dur_secs,
            has_separate_stems: has_stems,
            display_title: display_title.clone(),
            game: game.clone(),
            tag: tag.clone(),
            map: map.clone(),
            mode: mode.clone(),
            weapon: weapon.clone(),
            kills,
            favorite,
        },
    );

    let item = ClipItem {
        id: clip_id,
        display_title,
        game,
        tag,
        map,
        mode,
        weapon,
        kills,
        date_label,
        duration_secs: dur_secs,
        duration_label: format_mm_ss(dur_secs),
        favorite,
        has_separate_stems: has_stems,
        file_path: path.to_path_buf(),
        thumb_path,
        mtime_secs,
    };

    if !sidecar_path.exists() {
        persist_clip_sidecar(&item);
    }

    Some(item)
}

fn probe_duration_and_stems(path: &Path, bridge: &UiBridge) -> (f64, bool) {
    match bridge.probe_clip_header_fast(path) {
        Ok((dur_secs, has_stems)) => (dur_secs.max(1.0), has_stems),
        Err(_) => (15.0, false),
    }
}

fn collect_video_files_recursive(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 2 || !dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_video_files_recursive(&p, depth + 1, out);
        } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            let fname = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !fname.ends_with(".splicing_tmp.mp4")
                && (ext.eq_ignore_ascii_case("mp4") || ext.eq_ignore_ascii_case("mkv"))
            {
                out.push(p);
            }
        }
    }
}

fn detect_known_map_in_text(text: &str) -> Option<&'static str> {
    const KNOWN_MAPS: &[(&str, &str)] = &[
        ("ascent", "Ascent"),
        ("bind", "Bind"),
        ("haven", "Haven"),
        ("split", "Split"),
        ("lotus", "Lotus"),
        ("sunset", "Sunset"),
        ("icebox", "Icebox"),
        ("breeze", "Breeze"),
        ("fracture", "Fracture"),
        ("pearl", "Pearl"),
        ("abyss", "Abyss"),
        ("corrode", "Corrode"),
        ("mirage", "Mirage"),
        ("dust2", "Dust2"),
        ("de_dust2", "Dust2"),
        ("inferno", "Inferno"),
        ("nuke", "Nuke"),
        ("ancient", "Ancient"),
        ("anubis", "Anubis"),
        ("vertigo", "Vertigo"),
        ("overpass", "Overpass"),
        ("train", "Train"),
        ("cache", "Cache"),
        ("kingsrow", "Kings Row"),
        ("ilios", "Ilios"),
        ("lijang", "Lijiang Tower"),
        ("dorado", "Dorado"),
        ("eichenwalde", "Eichenwalde"),
        ("worldsedge", "Worlds Edge"),
        ("olympus", "Olympus"),
        ("kings_canyon", "Kings Canyon"),
        ("erangel", "Erangel"),
        ("miramar", "Miramar"),
        ("sanhok", "Sanhok"),
        ("clubhouse", "Clubhouse"),
        ("oregon", "Oregon"),
        ("chalet", "Chalet"),
        ("kafe", "Kafe"),
        ("border", "Border"),
    ];
    for tok in text.split(|c: char| !c.is_alphanumeric()) {
        if tok.is_empty() {
            continue;
        }
        let lower = tok.to_lowercase();
        for &(needle, canonical) in KNOWN_MAPS {
            if lower == needle {
                return Some(canonical);
            }
        }
    }
    None
}

fn compute_clip_search_score(clip: &ClipItem, raw_query: &str) -> Option<u32> {
    let tokens: Vec<String> = raw_query
        .split_whitespace()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if tokens.is_empty() {
        return Some(1);
    }

    let title_l = clip.display_title.to_lowercase();
    let game_l = clip.game.to_lowercase();
    let tag_l = clip.tag.to_lowercase();
    let map_l = clip.map.to_lowercase();
    let mode_l = clip.mode.to_lowercase();
    let weapon_l = clip.weapon.to_lowercase();
    let date_l = clip.date_label.to_lowercase();
    let stem_l = clip
        .file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    let mut total_score = 0u32;

    for tok in &tokens {
        let mut tok_score = 0u32;

        if tag_l == *tok || map_l == *tok || game_l == *tok {
            tok_score = tok_score.max(100);
        } else if tag_l.contains(tok) || (!map_l.is_empty() && map_l.contains(tok)) {
            tok_score = tok_score.max(85);
        }

        if title_l.split(|c: char| !c.is_alphanumeric()).any(|w| w == tok) {
            tok_score = tok_score.max(90);
        } else if title_l.contains(tok) {
            tok_score = tok_score.max(60);
        }

        if game_l.contains(tok)
            || (!mode_l.is_empty() && mode_l.contains(tok))
            || (!weapon_l.is_empty() && weapon_l.contains(tok))
            || date_l.contains(tok)
            || stem_l.contains(tok)
        {
            tok_score = tok_score.max(55);
        }

        if let Some(canon_game) = canonical_game_from_token(tok) {
            if clip.game.eq_ignore_ascii_case(canon_game) {
                tok_score = tok_score.max(90);
            }
        }

        if let Some(canon_tag) = canonical_tag_from_token(tok) {
            if clip.tag.eq_ignore_ascii_case(canon_tag) {
                tok_score = tok_score.max(92);
            }
        }

        if let Some(canon_map) = detect_known_map_in_text(tok) {
            if clip.map.eq_ignore_ascii_case(canon_map)
                || title_l.contains(&canon_map.to_lowercase())
                || stem_l.contains(&canon_map.to_lowercase())
            {
                tok_score = tok_score.max(95);
            }
        }

        match tok.as_str() {
            "ace" | "5k" | "penta" | "pentakill" | "teamwipe" => {
                if clip.tag.eq_ignore_ascii_case("Ace")
                    || clip.kills.unwrap_or(0) >= 5
                    || title_l.contains("ace")
                    || title_l.contains("5k")
                    || title_l.contains("penta")
                {
                    tok_score = tok_score.max(98);
                }
            }
            "4k" | "quad" | "quadra" | "quadrakill" => {
                if clip.kills == Some(4)
                    || title_l.contains("4k")
                    || title_l.contains("quad")
                    || (clip.tag.eq_ignore_ascii_case("Multikill") && clip.kills.is_none())
                {
                    tok_score = tok_score.max(90);
                }
            }
            "3k" | "triple" | "triplekill" => {
                if clip.kills == Some(3)
                    || title_l.contains("3k")
                    || title_l.contains("triple")
                    || (clip.tag.eq_ignore_ascii_case("Multikill") && clip.kills.is_none())
                {
                    tok_score = tok_score.max(88);
                }
            }
            "2k" | "double" | "doublekill" => {
                if clip.kills == Some(2)
                    || title_l.contains("2k")
                    || title_l.contains("double")
                {
                    tok_score = tok_score.max(85);
                }
            }
            "multikill" | "multi" => {
                if clip.tag.eq_ignore_ascii_case("Multikill")
                    || clip.tag.eq_ignore_ascii_case("Ace")
                    || clip.kills.unwrap_or(0) >= 2
                {
                    tok_score = tok_score.max(90);
                }
            }
            "clutch" | "1v1" | "1v2" | "1v3" | "1v4" | "1v5" => {
                if clip.tag.eq_ignore_ascii_case("Clutch")
                    || title_l.contains("clutch")
                    || title_l.contains("1v")
                {
                    tok_score = tok_score.max(94);
                }
            }
            "hs" | "headshot" | "onetap" | "1tap" => {
                if clip.tag.eq_ignore_ascii_case("Headshot")
                    || title_l.contains("headshot")
                    || title_l.contains("hs")
                {
                    tok_score = tok_score.max(92);
                }
            }
            "kill" | "kills" | "frag" | "elim" => {
                if matches!(
                    clip.tag.as_str(),
                    "Kill" | "Headshot" | "Multikill" | "Ace" | "Clutch"
                ) || clip.kills.unwrap_or(0) >= 1
                {
                    tok_score = tok_score.max(75);
                }
            }
            "map" | "maps" => {
                if !clip.map.is_empty() || detect_known_map_in_text(&clip.display_title).is_some() {
                    tok_score = tok_score.max(80);
                }
            }
            "fav" | "favorite" | "favorites" | "star" | "starred" => {
                if clip.favorite {
                    tok_score = tok_score.max(95);
                }
            }
            "stem" | "stems" | "tracks" | "multitrack" => {
                if clip.has_separate_stems {
                    tok_score = tok_score.max(85);
                }
            }
            _ => {}
        }

        if tok_score == 0 {
            return None;
        }
        total_score += tok_score;
    }

    Some(total_score)
}

fn canonical_game_from_token(token: &str) -> Option<&'static str> {
    match token.trim().to_lowercase().as_str() {
        "cs" | "cs2" | "csgo" | "counterstrike" | "counter-strike" => Some("CS2"),
        "val" | "valo" | "valorant" => Some("Valorant"),
        "apex" | "apexlegends" => Some("Apex"),
        "rocketleague" | "rocket" | "rl" => Some("Rocket League"),
        "overwatch" | "overwatch2" | "ow" | "ow2" => Some("Overwatch 2"),
        "dota2" | "dota" => Some("Dota 2"),
        "lol" | "league" | "leagueoflegends" => Some("League of Legends"),
        "fn" | "fortnite" => Some("Fortnite"),
        "wz" | "warzone" | "cod" => Some("Warzone"),
        "pubg" => Some("PUBG"),
        "r6" | "siege" | "rainbowsix" => Some("Rainbow Six"),
        "mc" | "minecraft" => Some("Minecraft"),
        "gta" | "gtav" | "fivem" => Some("GTA V"),
        "rust" => Some("Rust"),
        "desktop" | "app" | "screen" => Some("Desktop"),
        _ => None,
    }
}

fn canonical_tag_from_token(token: &str) -> Option<&'static str> {
    match token.trim().to_lowercase().as_str() {
        "ace" => Some("Ace"),
        "clutch" => Some("Clutch"),
        "multikill" | "quadra" | "penta" | "triple" | "3k" | "4k" | "5k" => Some("Multikill"),
        "headshot" | "hs" => Some("Headshot"),
        "kill" | "knockdown" => Some("Kill"),
        "goal" => Some("Goal"),
        "save" | "epicsave" => Some("Save"),
        "direct" => Some("Direct"),
        "discord" => Some("Discord"),
        "edited" | "edit" | "cut" | "spliced" => Some("Edited"),
        "replay" | "manual" => Some("Replay"),
        _ => None,
    }
}

fn parse_legacy_filename_tokens(stem: &str, parent_folder: &str) -> (String, String) {
    let tokens: Vec<&str> = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();

    let mut game: Option<&'static str> = canonical_game_from_token(parent_folder);
    if game.is_none() {
        for &tok in &tokens {
            if let Some(g) = canonical_game_from_token(tok) {
                game = Some(g);
                break;
            }
        }
    }

    let mut tag: Option<&'static str> = None;
    for &tok in &tokens {
        if let Some(t) = canonical_tag_from_token(tok) {
            tag = Some(t);
            break;
        }
    }

    (
        game.unwrap_or("Desktop").to_string(),
        tag.unwrap_or("Clip").to_string(),
    )
}

fn format_mm_ss(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let m = total / 60;
    let s = total % 60;
    format!("{m}:{s:02}")
}

fn format_local_date_time(mtime_secs: u64) -> String {
    if mtime_secs == 0 {
        return chrono::Local::now().format("%b %d %H:%M").to_string();
    }
    use chrono::TimeZone;
    if let chrono::LocalResult::Single(dt) = chrono::Local.timestamp_opt(mtime_secs as i64, 0) {
        dt.format("%b %d %H:%M").to_string()
    } else {
        chrono::Local::now().format("%b %d %H:%M").to_string()
    }
}

fn format_relative_mtime(mtime_secs: u64) -> String {
    if mtime_secs == 0 {
        return "Today".to_string();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(mtime_secs);
    let diff = now.saturating_sub(mtime_secs);
    if diff < 86_400 {
        "Today".to_string()
    } else if diff < 172_800 {
        "Yesterday".to_string()
    } else {
        format!("{}d ago", diff / 86_400)
    }
}

fn format_key_combo(mods: Modifiers, key_token: &str) -> String {
    let mut parts = Vec::new();
    if mods.ctrl {
        parts.push("Ctrl");
    }
    if mods.alt {
        parts.push("Alt");
    }
    if mods.shift {
        parts.push("Shift");
    }
    parts.push(key_token);
    parts.join("+")
}

fn egui_key_to_config_token(key: Key) -> Option<String> {
    let tok = match key {
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        Key::F13 => "F13",
        Key::F14 => "F14",
        Key::F15 => "F15",
        Key::F16 => "F16",
        Key::F17 => "F17",
        Key::F18 => "F18",
        Key::F19 => "F19",
        Key::F20 => "F20",
        Key::F21 => "F21",
        Key::F22 => "F22",
        Key::F23 => "F23",
        Key::F24 => "F24",
        Key::A => "A",
        Key::B => "B",
        Key::C => "C",
        Key::D => "D",
        Key::E => "E",
        Key::F => "F",
        Key::G => "G",
        Key::H => "H",
        Key::I => "I",
        Key::J => "J",
        Key::K => "K",
        Key::L => "L",
        Key::M => "M",
        Key::N => "N",
        Key::O => "O",
        Key::P => "P",
        Key::Q => "Q",
        Key::R => "R",
        Key::S => "S",
        Key::T => "T",
        Key::U => "U",
        Key::V => "V",
        Key::W => "W",
        Key::X => "X",
        Key::Y => "Y",
        Key::Z => "Z",
        Key::Num0 => "0",
        Key::Num1 => "1",
        Key::Num2 => "2",
        Key::Num3 => "3",
        Key::Num4 => "4",
        Key::Num5 => "5",
        Key::Num6 => "6",
        Key::Num7 => "7",
        Key::Num8 => "8",
        Key::Num9 => "9",
        Key::Space => "Space",
        Key::Tab => "Tab",
        Key::Backspace => "Backspace",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        _ => return None,
    };
    Some(tok.to_string())
}

fn spawn_native_folder_picker(tx: Sender<UiAsyncMessage>, ctx: egui::Context) {
    let _ = std::thread::Builder::new()
        .name("clotch-folder-picker".to_string())
        .spawn(move || {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let ps_script = "Add-Type -AssemblyName System.Windows.Forms; $f = New-Object System.Windows.Forms.FolderBrowserDialog; $f.Description = 'Select Clotch Clips Folder'; if ($f.ShowDialog() -eq 'OK') { Write-Output $f.SelectedPath }";
                let mut cmd = std::process::Command::new("powershell");
                cmd.args([
                    "-NoProfile",
                    "-Sta",
                    "-WindowStyle",
                    "Hidden",
                    "-Command",
                    ps_script,
                ])
                .creation_flags(0x08000000);
                if let Ok(output) = cmd.output() {
                    let chosen = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !chosen.is_empty() {
                        let _ = tx.send(UiAsyncMessage::CustomSaveFolderPicked(PathBuf::from(
                            chosen,
                        )));
                        ctx.request_repaint();
                    }
                }
            }
            #[cfg(not(windows))]
            {
                let _ = (tx, ctx);
            }
        });
}

fn open_file_in_default_player(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &path.to_string_lossy()])
            .creation_flags(0x08000000)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

fn reveal_file_in_explorer(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

pub fn run_gui(bridge: UiBridge, event_rx: broadcast::Receiver<Event>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Clotch")
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([520.0, 480.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Clotch",
        options,
        Box::new(move |cc| Ok(Box::new(ClotchGuiApp::new(cc, bridge, event_rx)))),
    )
    .map_err(|e| format!("Failed to start Clotch GUI: {e}"))
}