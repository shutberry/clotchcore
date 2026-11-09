#![allow(dead_code, unused_imports)]
use crate::clips::ClipItem;
use std::time::Instant;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimActionMode {
    Keep,
    Remove,
}
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use crossbeam_channel::Sender;
use eframe::egui::TextureHandle;

use crate::editor::{AudioStemMixConfig, CutMode, ResolutionPreset, VideoPreviewSession};
use crate::gui::theme::ThemeColors;
use crate::gui::UiAsyncMessage;

pub enum PreviewWorkerCmd {
    DecodeAt {
        target_ms: u64,
        width: u32,
        height: u32,
    },
    SetPlayState {
        is_playing: bool,
        position_ms: Option<u64>,
    },
    SeekAudio {
        target_ms: u64,
    },
    UpdateAudioMix(AudioStemMixConfig),
    SetPlaybackRate(f32),
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
    pub playback_speed: f32,
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
    pub selected_segment_idx: Option<usize>,
    pub deleted_segments: HashSet<usize>,
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
            cut_mode: CutMode::FrameExact,
            splits_secs: Vec::new(),
            keyframes_secs,
            resolution: "Original".to_string(),
            fps: "Original".to_string(),
            quality: "High".to_string(),
            playback_speed: 1.0,
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
                    volume_pct: 100,
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
            selected_segment_idx: Some(0),
            deleted_segments: HashSet::new(),
            preview_worker_tx: None,
        }
    }

    pub fn compute_segments(&self, duration: f64) -> Vec<(usize, f64, f64, bool)> {
        let dur = duration.max(0.5);
        let s0 = self.trim_start_secs.clamp(0.0, dur);
        let e0 = self.trim_end_secs.clamp((s0 + 0.1).min(dur), dur);
        let mut pts = Vec::with_capacity(self.splits_secs.len() + 2);
        pts.push(s0);
        for &sp in &self.splits_secs {
            if sp > s0 + 0.08 && sp < e0 - 0.08 {
                pts.push(sp);
            }
        }
        pts.push(e0);
        pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        pts.dedup_by(|a, b| (*a - *b).abs() < 0.05);
        let mut out = Vec::with_capacity(pts.len().saturating_sub(1));
        for idx in 0..pts.len().saturating_sub(1) {
            let is_del = self.deleted_segments.contains(&idx);
            out.push((idx, pts[idx], pts[idx + 1], is_del));
        }
        out
    }

    pub fn kept_segments_ms(&self, duration: f64) -> Vec<(u64, u64)> {
        let segs = self.compute_segments(duration);
        let mut out = Vec::new();
        for (_idx, s, e, is_del) in segs {
            if !is_del && e > s + 0.08 {
                let sm = (s * 1000.0).round() as u64;
                let em = (e * 1000.0).round() as u64;
                if em > sm + 80 {
                    out.push((sm, em));
                }
            }
        }
        out
    }

    pub fn split_at_time(&mut self, raw_t: f64, duration: f64) -> bool {
        let t = self.snap_to_keyframe(raw_t, duration);
        let s0 = self.trim_start_secs;
        let e0 = self.trim_end_secs;
        if t <= s0 + 0.12 || t >= e0 - 0.12 {
            return false;
        }
        if self.splits_secs.iter().any(|&x| (x - t).abs() < 0.12) {
            return false;
        }
        if self.splits_secs.len() >= 24 {
            return false;
        }
        let insert_idx = self
            .splits_secs
            .iter()
            .position(|&x| x > t)
            .unwrap_or(self.splits_secs.len());
        self.splits_secs.insert(insert_idx, t);

        let mut updated_del = HashSet::new();
        for &old_seg in &self.deleted_segments {
            if old_seg <= insert_idx {
                updated_del.insert(old_seg);
            } else {
                updated_del.insert(old_seg + 1);
            }
        }
        self.deleted_segments = updated_del;
        self.selected_segment_idx = Some(insert_idx + 1);
        true
    }

    pub fn snap_to_keyframe(&self, t: f64, duration: f64) -> f64 {
        let clamped = t.clamp(0.0, duration);
        if self.cut_mode == CutMode::FrameExact || self.keyframes_secs.is_empty() {
            return (clamped * 1000.0).round() / 1000.0;
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
        if best_dist <= 0.22 {
            best.clamp(0.0, duration)
        } else {
            (clamped * 1000.0).round() / 1000.0
        }
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

pub struct EditorBusyResetGuard {
    pub(crate) tx: Sender<UiAsyncMessage>,
    pub(crate) ctx: eframe::egui::Context,
    pub(crate) completed_cleanly: bool,
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
