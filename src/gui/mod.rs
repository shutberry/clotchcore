#![allow(dead_code, unused_imports)]
pub mod components;
pub mod pages;
pub mod theme;

pub use components::*;
pub use pages::*;
pub use theme::*;

pub use crate::clips::*;

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

use crate::audio::AudioInputDeviceInfo;
use crate::bridge::{
    role_column_label_for_game, LiveGameStateSnapshot, LocalLauncherIdentity, UiBridge,
    YearWrappedSummary,
};
use crate::editor::{
    default_edited_output_path, AudioStemMixConfig, CutMode, EditJob,
};
use crate::stats::{GamePeriodStats, MatchStatsRecord, MonthlyStatsSummary};
use crate::tracker::Event;
use crate::recorder::config::{
    AudioMode, Container, Fps, Keybind, KeybindsConfig, Quality, StorageMode,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewTab {
    Clips,
    Editor,
    Stats,
    Settings,
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

pub struct ClotchGuiApp {
    pub(crate) bridge: UiBridge,
    pub(crate) async_tx: Sender<UiAsyncMessage>,
    pub(crate) async_rx: Receiver<UiAsyncMessage>,
    pub(crate) appearance: AppearancePreference,
    pub(crate) last_effective_dark: Option<bool>,
    pub(crate) view: ViewTab,
    pub(crate) filter: String,
    pub(crate) search: String,
    pub(crate) status_text: String,
    pub(crate) is_recording_active: bool,
    pub(crate) clips: Vec<ClipItem>,
    pub(crate) filtered_indices: Vec<usize>,
    pub(crate) available_games: Vec<String>,
    pub(crate) filters_dirty: bool,
    pub(crate) scan_in_progress: bool,
    pub(crate) thumb_cache: ThumbnailLruCache,
    pub(crate) thumb_inflight: HashSet<u64>,
    pub(crate) compressing_clips: HashSet<u64>,
    pub(crate) renaming_clip_id: Option<u64>,
    pub(crate) rename_buffer: String,
    pub(crate) rename_focus_requested: bool,
    pub(crate) confirm_delete_clip_id: Option<u64>,
    pub(crate) editor_state: Option<EditorViewState>,
    pub(crate) recording_keybind_slot: Option<&'static str>,
    pub(crate) keybind_error_msg: Option<String>,
    pub(crate) keybinds_custom_mode: bool,
    pub(crate) custom_path_buffer: String,
    pub(crate) folder_validation_status: Option<Result<f64, String>>,
    pub(crate) microphones: Vec<AudioInputDeviceInfo>,
    pub(crate) selected_mic_missing: Option<String>,
    pub(crate) toast: Option<(String, Option<PathBuf>, Instant)>,
    pub(crate) was_minimized: bool,
    pub(crate) active_clips_dir: PathBuf,
    pub(crate) stats_window_days: u32,
    pub(crate) stats_game_filter: String,
    pub(crate) stats_subview_wrapped: bool,
    pub(crate) wrapped_year_filter: i32,
    pub(crate) stats_summary: MonthlyStatsSummary,
    pub(crate) year_wrapped: YearWrappedSummary,
    pub(crate) local_identities: Vec<LocalLauncherIdentity>,
    pub(crate) live_game_state: Option<LiveGameStateSnapshot>,
    pub(crate) last_live_state_poll: Instant,
    pub(crate) match_modal: MatchRecordModalState,
    pub(crate) expanded_match_id: Option<String>,
    pub(crate) riot_lockfile_online: bool,
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
            stats_window_days: 30,
            stats_game_filter: "All".to_string(),
            stats_subview_wrapped: false,
            wrapped_year_filter: 2026,
            stats_summary: MonthlyStatsSummary::default(),
            year_wrapped: YearWrappedSummary::default(),
            local_identities: Vec::new(),
            live_game_state: None,
            last_live_state_poll: Instant::now(),
            match_modal: MatchRecordModalState::default(),
            expanded_match_id: None,
            riot_lockfile_online: false,
        };

        app.clips = load_cached_clips_from_index();
        app.refresh_stats_and_wrapped();
        app.filters_dirty = true;
        app.rebuild_filters_and_games_if_needed();
        app.spawn_background_library_scan(&cc.egui_ctx);
        let effective_dark = app.resolve_effective_dark(&cc.egui_ctx);
        app.apply_theme(&cc.egui_ctx, effective_dark);
        app
    }

    pub(crate) fn resolve_effective_dark(&self, ctx: &egui::Context) -> bool {
        match self.appearance {
            AppearancePreference::Dark => true,
            AppearancePreference::Light => false,
            AppearancePreference::System => match ctx.system_theme() {
                Some(egui::Theme::Light) => false,
                Some(egui::Theme::Dark) | None => true,
            },
        }
    }

    pub(crate) fn colors(&self, ctx: &egui::Context) -> ThemeColors {
        if self.resolve_effective_dark(ctx) {
            ThemeColors::dark()
        } else {
            ThemeColors::light()
        }
    }

    pub(crate) fn apply_theme(&mut self, ctx: &egui::Context, dark: bool) {
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

    pub(crate) fn show_toast(&mut self, msg: impl Into<String>, reveal_path: Option<PathBuf>) {
        self.toast = Some((msg.into(), reveal_path, Instant::now()));
    }

    pub(crate) fn spawn_background_library_scan(&mut self, ctx: &egui::Context) {
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
                    scan_clips_directory_worker(&clips_dir)
                }))
                .unwrap_or_default();
                let _ = tx.send(UiAsyncMessage::LibraryScanned(items));
                ctx_clone.request_repaint();
            });
    }

    pub(crate) fn spawn_single_clip_probe(&self, path: PathBuf, ctx: &egui::Context) {
        let bridge = self.bridge.clone();
        let tx = self.async_tx.clone();
        let ctx_clone = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("clotch-clip-probe".to_string())
            .spawn(move || {
                let thumbs_dir = resolve_thumbs_cache_dir();
                let mut empty_index = HashMap::new();
                if let Some(item) =
                    build_clip_item_from_path(&path, &thumbs_dir, &mut empty_index)
                {
                    let _ = tx.send(UiAsyncMessage::SingleClipDiscovered(item));
                    ctx_clone.request_repaint();
                }
            });
    }

    pub(crate) fn request_thumbnail_if_needed(&mut self, clip: &ClipItem, ctx: &egui::Context) {
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

    pub(crate) fn request_editor_keyframes(&self, clip_id: u64, video_path: PathBuf, ctx: &egui::Context) {
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

    pub(crate) fn ensure_editor_preview_worker(
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
                    let mut latest_decode: Option<(u64, u32, u32)> = None;
                    let mut apply_cmd = |c: PreviewWorkerCmd, sess: &mut Option<crate::editor::VideoPreviewSession>| -> bool {
                        match c {
                            PreviewWorkerCmd::Shutdown => return true,
                            PreviewWorkerCmd::DecodeAt {
                                target_ms,
                                width,
                                height,
                            } => {
                                latest_decode = Some((target_ms, width, height));
                            }
                            PreviewWorkerCmd::SetPlayState {
                                is_playing,
                                position_ms,
                            } => {
                                if let Some(ref s) = sess {
                                    s.set_preview_playing(is_playing, position_ms);
                                }
                            }
                            PreviewWorkerCmd::SeekAudio { target_ms } => {
                                if let Some(ref s) = sess {
                                    s.seek_preview_audio(target_ms);
                                }
                            }
                            PreviewWorkerCmd::UpdateAudioMix(mix) => {
                                if let Some(ref s) = sess {
                                    s.set_preview_audio_mix(mix);
                                }
                            }
                            PreviewWorkerCmd::SetPlaybackRate(rate) => {
                                if let Some(ref s) = sess {
                                    s.set_preview_playback_rate(rate);
                                }
                            }
                        }
                        false
                    };

                    if apply_cmd(cmd, &mut session_opt) {
                        break;
                    }
                    while let Ok(newer) = cmd_rx.try_recv() {
                        if apply_cmd(newer, &mut session_opt) {
                            return;
                        }
                    }

                    if let Some((target_ms, w, h)) = latest_decode {
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
                }
            });

        Some(cmd_tx)
    }

    pub(crate) fn request_editor_preview_frame(
        &mut self,
        clip_id: u64,
        video_path: PathBuf,
        target_ms: u64,
        ctx: &egui::Context,
    ) {
        let is_fs = self
            .editor_state
            .as_ref()
            .map(|e| e.fullscreen_preview)
            .unwrap_or(false);
        let (w, h) = if is_fs { (1920, 1080) } else { (1280, 720) };
        if let Some(cmd_tx) = self.ensure_editor_preview_worker(clip_id, video_path, ctx) {
            let _ = cmd_tx.send(PreviewWorkerCmd::DecodeAt {
                target_ms,
                width: w,
                height: h,
            });
        }
    }

    pub(crate) fn close_editor_session(&mut self) {
        if let Some(mut ed) = self.editor_state.take() {
            ed.is_playing = false;
            if let Some(tx) = ed.preview_worker_tx.take() {
                let _ = tx.send(PreviewWorkerCmd::Shutdown);
            }
        }
        trim_process_working_set();
    }

    pub(crate) fn rebuild_filters_and_games_if_needed(&mut self) {
        if !self.filters_dirty {
            return;
        }
        self.filters_dirty = false;

        let mut games_set: Vec<String> = Vec::new();
        for c in &self.clips {
            let g_clean = c.game.trim();
            if !g_clean.is_empty()
                && !games_set
                    .iter()
                    .any(|g| pretty_game_title(g).eq_ignore_ascii_case(&pretty_game_title(g_clean)))
            {
                games_set.push(g_clean.to_string());
            }
        }
        self.available_games = games_set;

        if self.filter != "All"
            && self.filter != "Highlights"
            && self.filter != "Replays"
            && self.filter != "Favorites"
            && !self.available_games.iter().any(|g| {
                g.eq_ignore_ascii_case(&self.filter)
                    || pretty_game_title(g).eq_ignore_ascii_case(&pretty_game_title(&self.filter))
            })
        {
            self.filter = "All".to_string();
        }

        if self.stats_game_filter != "All"
            && !self.stats_summary.per_game.keys().any(|g| {
                g.eq_ignore_ascii_case(&self.stats_game_filter)
                    || pretty_game_title(g)
                        .eq_ignore_ascii_case(&pretty_game_title(&self.stats_game_filter))
            })
            && !self.available_games.iter().any(|g| {
                g.eq_ignore_ascii_case(&self.stats_game_filter)
                    || pretty_game_title(g)
                        .eq_ignore_ascii_case(&pretty_game_title(&self.stats_game_filter))
            })
        {
            self.stats_game_filter = "All".to_string();
        }

        let q = self.search.trim();
        let mut scored_indices: Vec<(u32, u64, usize)> = Vec::with_capacity(self.clips.len());
        for (idx, clip) in self.clips.iter().enumerate() {
            let match_filter = if self.filter == "All" {
                true
            } else if self.filter == "Highlights" {
                is_highlight_clip(clip)
            } else if self.filter == "Replays" {
                !is_highlight_clip(clip)
            } else if self.filter == "Favorites" {
                clip.favorite
            } else {
                clip.game.eq_ignore_ascii_case(&self.filter)
                    || pretty_game_title(&clip.game)
                        .eq_ignore_ascii_case(&pretty_game_title(&self.filter))
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

    pub(crate) fn process_async_messages(&mut self, ctx: &egui::Context) {
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
                    "stats.match_recorded"
                    | "match.official_details"
                    | "match.end"
                    | "stats.add_custom_match"
                    | "stats.delete_match" => {
                        self.refresh_stats_and_wrapped();
                    }
                    "presence.update"
                    | "player_stats"
                    | "player.kill"
                    | "player.death"
                    | "player.assist"
                    | "match.start" => {
                        self.live_game_state = self.bridge.query_live_game_state();
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

    pub(crate) fn apply_custom_save_folder(&mut self, path: PathBuf, ctx: &egui::Context) {
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

    pub(crate) fn handle_keybind_recording(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn save_clip_rename(&mut self, clip_id: u64) {
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

    pub(crate) fn toggle_clip_favorite(&mut self, clip_id: u64) {
        if let Some(clip) = self.clips.iter_mut().find(|c| c.id == clip_id) {
            clip.favorite = !clip.favorite;
            persist_clip_sidecar(clip);
            self.filters_dirty = true;
        }
    }

    pub(crate) fn delete_clip_to_recycle_bin(&mut self, clip_id: u64) {
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

    pub(crate) fn trigger_editor_save_copy(&mut self, ctx: &egui::Context) {
        self.trigger_editor_save_internal(ctx, false);
    }

    pub(crate) fn trigger_editor_save_split_parts(&mut self, ctx: &egui::Context) {
        self.trigger_editor_save_internal(ctx, true);
    }

    pub(crate) fn trigger_editor_save_internal(&mut self, ctx: &egui::Context, export_separate_parts: bool) {
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

        let dur = clip.duration_secs.max(1.0);
        let kept_segments = ed.kept_segments_ms(dur);
        if kept_segments.is_empty() {
            self.show_toast("All parts are deleted. Restore at least one part to save.", None);
            return;
        }

        let cut_mode = ed.cut_mode;
        let rescale_dims: Option<(u32, u32)> = match ed.resolution.as_str() {
            "1440p" => Some((2560, 1440)),
            "1080p" => Some((1920, 1080)),
            "720p" => Some((1280, 720)),
            "9:16 Vertical" => Some((1080, 1920)),
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
        ed.status_line = "Saving edited clip...".to_string();

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

                        let template_job = EditJob {
                            input_path: input_path.clone(),
                            output_path: None,
                            trim_start_ms: None,
                            trim_end_ms: None,
                            cut_mode,
                            rescale: rescale_dims,
                            target_fps,
                            video_bitrate_kbps: bitrate_kbps,
                            audio_mix: Some(audio_mix.clone()),
                            mute_all_audio: remove_all_audio,
                            prefer_hardware: true,
                        };

                        if export_separate_parts && kept_segments.len() > 1 {
                            let total = kept_segments.len();
                            let mut created_paths = Vec::with_capacity(total);
                            for (idx, &(seg_start, seg_end)) in kept_segments.iter().enumerate() {
                                if cancel_flag.load(Ordering::Relaxed) {
                                    for p in &created_paths {
                                        let _ = std::fs::remove_file(p);
                                    }
                                    return Err("Edit canceled.".to_string());
                                }
                                let _ = tx.send(UiAsyncMessage::EditorJobProgress(format!(
                                    "Saving part {}/{}...",
                                    idx + 1,
                                    total
                                )));
                                ctx_clone.request_repaint();
                                let part_out = default_edited_output_path(
                                    &input_path,
                                    &format!("part {}", idx + 1),
                                );
                                let mut job = template_job.clone();
                                job.output_path = Some(part_out);
                                job.trim_start_ms = Some(seg_start);
                                job.trim_end_ms = Some(seg_end);
                                let r = bridge.run_edit_job(job)?;
                                created_paths.push(r.output_path);
                            }
                            let first_out = created_paths.first().cloned();
                            return Ok((
                                format!("Saved {} separate parts", created_paths.len()),
                                first_out,
                            ));
                        }

                        let final_out = default_edited_output_path(&input_path, "edited");
                        let res = bridge.splice_kept_segments(
                            &input_path,
                            &kept_segments,
                            &template_job,
                            Some(final_out),
                        )?;
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

    pub(crate) fn trigger_editor_export_stems(&mut self, ctx: &egui::Context) {
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

    pub(crate) fn trigger_clip_discord_compress(&mut self, clip_id: u64, ctx: &egui::Context) {
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

        egui::SidePanel::right("clotch_right_sidebar")
            .exact_width(228.0)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(c.surface)
                    .stroke(Stroke::new(1.0_f32, c.border))
                    .inner_margin(Margin::symmetric(0.0, 0.0)),
            )
            .show(ctx, |ui| {
                self.render_right_sidebar(ui, ctx, c);
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(c.bg)
                    .inner_margin(Margin {
                        left: 28.0,
                        right: 28.0,
                        top: 22.0,
                        bottom: 18.0,
                    }),
            )
            .show(ctx, |ui| {
                match self.view {
                    ViewTab::Clips => self.render_clips_view(ui, ctx, c),
                    ViewTab::Editor => self.render_editor_view(ui, ctx, c),
                    ViewTab::Stats => self.render_stats_view(ui, ctx, c),
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
                    .anchor(egui::Align2::LEFT_BOTTOM, Vec2::new(28.0, -24.0))
                    .show(ctx, |ui| {
                        egui::Frame::none()
                            .fill(c.surface)
                            .stroke(Stroke::new(1.0_f32, c.border_strong))
                            .rounding(Rounding::same(6.0))
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


pub(crate) fn open_file_in_default_player(path: &Path) {
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

pub(crate) fn reveal_file_in_explorer(path: &Path) {
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

