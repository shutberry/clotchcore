#![allow(dead_code, unused_imports)]
use crate::gui::{open_file_in_default_player, reveal_file_in_explorer};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Sender;
use eframe::egui::{
    self, Align, Color32, ColorImage, CursorIcon, FontId, Id, Key, Layout, Margin, Modifiers, Pos2, Rect,
    RichText, Rounding, ScrollArea, Sense, Stroke, TextureOptions, UiBuilder, Vec2,
};

use crate::audio::AudioInputDeviceInfo;
use crate::bridge::{
    role_column_label_for_game, LiveGameStateSnapshot, LocalLauncherIdentity, UiBridge,
    YearWrappedSummary,
};
use crate::clips::{self, *};
use crate::editor::{self, default_edited_output_path, AudioStemMixConfig, CutMode, EditJob, ResolutionPreset};
use crate::stats::{GamePeriodStats, MatchStatsRecord, MonthlyStatsSummary};
use crate::gui::components::*;
use crate::gui::theme::*;
use crate::gui::{ClotchGuiApp, UiAsyncMessage, ViewTab};
use crate::recorder::config::{
    AudioMode, Container, Fps, Keybind, KeybindsConfig, Quality, StorageMode,
};
impl ClotchGuiApp {
    pub(crate) fn render_editor_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, c: ThemeColors) {
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
            let key_del = ctx.input(|i| i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace));
            let key_f = ctx.input(|i| i.key_pressed(Key::F) || i.key_pressed(Key::F11));
            let key_esc = ctx.input(|i| i.key_pressed(Key::Escape));
            let key_left = ctx.input(|i| i.key_pressed(Key::ArrowLeft));
            let key_right = ctx.input(|i| i.key_pressed(Key::ArrowRight));
            let key_comma = ctx.input(|i| i.key_pressed(Key::Comma));
            let key_period = ctx.input(|i| i.key_pressed(Key::Period));
            let key_copy = ctx.input(|i| (i.modifiers.ctrl || i.modifiers.command) && i.key_pressed(Key::C));
            let shift_held = ctx.input(|i| i.modifiers.shift);

            if key_copy {
                match self.bridge.copy_file_to_clipboard(&clip.file_path) {
                    Ok(()) => self.show_toast("Copied clip to clipboard (Ctrl+V in Discord)", None),
                    Err(e) => self.show_toast(format!("Copy failed: {e}"), None),
                }
            }

            if let Some(ref mut ed) = self.editor_state {
                if space_pressed {
                    ed.is_playing = !ed.is_playing;
                    if ed.is_playing {
                        let kept = ed.kept_segments_ms(dur);
                        let first_start = kept
                            .first()
                            .map(|&(s, _)| s as f64 / 1000.0)
                            .unwrap_or(ed.trim_start_secs);
                        let last_end = kept
                            .last()
                            .map(|&(_, e)| e as f64 / 1000.0)
                            .unwrap_or(ed.trim_end_secs);
                        if ed.playhead_secs >= last_end - 0.05 || ed.playhead_secs < first_start {
                            ed.playhead_secs = first_start;
                        }
                        ed.last_play_tick = Some(Instant::now());
                    } else {
                        ed.last_play_tick = None;
                    }
                    let pos_ms = (ed.playhead_secs * 1000.0).round() as u64;
                    if let Some(ref tx) = ed.preview_worker_tx {
                        let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                            is_playing: ed.is_playing,
                            position_ms: Some(pos_ms),
                        });
                    }
                }
                if key_i {
                    let snapped = ed.snap_to_keyframe(ed.playhead_secs, dur);
                    ed.trim_start_secs = snapped.min(ed.trim_end_secs - 0.10).max(0.0);
                }
                if key_o {
                    let snapped = ed.snap_to_keyframe(ed.playhead_secs, dur);
                    ed.trim_end_secs = snapped.max(ed.trim_start_secs + 0.10).min(dur);
                }
                if key_s {
                    let _ = ed.split_at_time(ed.playhead_secs, dur);
                }
                if key_del {
                    if let Some(sel_idx) = ed.selected_segment_idx {
                        let seg_count = ed.splits_secs.len() + 1;
                        if sel_idx < seg_count && seg_count > 1 {
                            if ed.deleted_segments.contains(&sel_idx) {
                                ed.deleted_segments.remove(&sel_idx);
                            } else {
                                ed.deleted_segments.insert(sel_idx);
                            }
                        }
                    }
                }
                if key_f {
                    ed.fullscreen_preview = !ed.fullscreen_preview;
                }
                if key_esc && ed.fullscreen_preview {
                    ed.fullscreen_preview = false;
                }
                if key_left || key_right || key_comma || key_period {
                    ed.is_playing = false;
                    ed.last_play_tick = None;
                    let step = if key_comma || key_period {
                        1.0 / 60.0
                    } else if shift_held {
                        0.1
                    } else {
                        1.0
                    };
                    let delta = if key_left || key_comma { -step } else { step };
                    ed.playhead_secs = (ed.playhead_secs + delta).clamp(0.0, dur);
                    let req_ms = (ed.playhead_secs * 1000.0).round() as u64;
                    ed.pending_preview_ms = Some((req_ms, Instant::now()));
                    if let Some(ref tx) = ed.preview_worker_tx {
                        let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                            is_playing: false,
                            position_ms: Some(req_ms),
                        });
                    }
                }
            }
        }

        if let Some(ref mut ed) = self.editor_state {
            if ed.is_playing {
                let now = Instant::now();
                let dt = ed
                    .last_play_tick
                    .map(|t| now.saturating_duration_since(t).as_secs_f64().min(0.1))
                    .unwrap_or(0.016);
                ed.last_play_tick = Some(now);
                ed.playhead_secs += dt * (ed.playback_speed as f64);

                let segs = ed.compute_segments(dur);
                let mut jumped_over_deleted = false;
                for (_idx, s_sec, e_sec, is_del) in &segs {
                    if *is_del && ed.playhead_secs >= *s_sec && ed.playhead_secs < *e_sec {
                        if let Some((_, next_s, _, _)) = segs
                            .iter()
                            .find(|(_, ns, _, ndel)| !*ndel && *ns >= *e_sec - 0.02)
                        {
                            ed.playhead_secs = *next_s;
                            jumped_over_deleted = true;
                        } else {
                            ed.playhead_secs = segs
                                .iter()
                                .find(|(_, _, _, ndel)| !*ndel)
                                .map(|(_, fs, _, _)| *fs)
                                .unwrap_or(ed.trim_start_secs);
                            ed.is_playing = false;
                            ed.last_play_tick = None;
                            jumped_over_deleted = true;
                        }
                        break;
                    }
                }

                let stop_bound = segs
                    .iter()
                    .rev()
                    .find(|(_, _, _, is_del)| !*is_del)
                    .map(|(_, _, e_sec, _)| *e_sec)
                    .unwrap_or(ed.trim_end_secs);

                if ed.playhead_secs >= stop_bound {
                    ed.playhead_secs = segs
                        .iter()
                        .find(|(_, _, _, is_del)| !*is_del)
                        .map(|(_, s_sec, _, _)| *s_sec)
                        .unwrap_or(ed.trim_start_secs);
                    ed.is_playing = false;
                    ed.last_play_tick = None;
                    jumped_over_deleted = true;
                }

                if jumped_over_deleted {
                    let sync_ms = (ed.playhead_secs * 1000.0).round() as u64;
                    if let Some(ref tx) = ed.preview_worker_tx {
                        let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                            is_playing: ed.is_playing,
                            position_ms: Some(sync_ms),
                        });
                    }
                }

                let target_ms = (ed.playhead_secs * 1000.0).round() as u64;
                if ed
                    .last_preview_req_ms
                    .map(|last| last.abs_diff(target_ms) >= 15)
                    .unwrap_or(true)
                {
                    ed.last_preview_req_ms = Some(target_ms);
                    dispatch_preview_ms = Some(target_ms);
                }
                ctx.request_repaint_after(Duration::from_millis(16));
            } else if let Some((target_ms, queued_at)) = ed.pending_preview_ms {
                if queued_at.elapsed() >= Duration::from_millis(14) {
                    ed.pending_preview_ms = None;
                    if ed.last_preview_req_ms != Some(target_ms) {
                        ed.last_preview_req_ms = Some(target_ms);
                        dispatch_preview_ms = Some(target_ms);
                    }
                } else {
                    ctx.request_repaint_after(Duration::from_millis(12));
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
                    let stage_rect =
                        Rect::from_min_size(Pos2::new(x0, y0), Vec2::new(stage_w, stage_h));
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
                            let pos_ms = (ed.playhead_secs * 1000.0).round() as u64;
                            if let Some(ref tx) = ed.preview_worker_tx {
                                let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                                    is_playing: ed.is_playing,
                                    position_ms: Some(pos_ms),
                                });
                            }
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
                        let play_label = if ed.is_playing { "Pause" } else { "Play" };
                        if draw_ghost_button(&mut bar_ui, play_label, true, c).clicked() {
                            ed.is_playing = !ed.is_playing;
                            ed.last_play_tick = if ed.is_playing {
                                Some(Instant::now())
                            } else {
                                None
                            };
                            let pos_ms = (ed.playhead_secs * 1000.0).round() as u64;
                            if let Some(ref tx) = ed.preview_worker_tx {
                                let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                                    is_playing: ed.is_playing,
                                    position_ms: Some(pos_ms),
                                });
                            }
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
                        if draw_ghost_button(&mut bar_ui, "Exit fullscreen (Esc)", false, c)
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
                let back_resp = ui.add(
                    egui::Label::new(
                        RichText::new("← Back to clips").size(13.0).color(c.text_2),
                    )
                    .sense(Sense::click()),
                );
                if back_resp.hovered() {
                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                }
                if back_resp.clicked() {
                    close_to_clips = true;
                }

                ui.add_space(16.0);

                let avail_w = ui.available_width();
                if avail_w >= 860.0 {
                    let side_w = 340.0;
                    let gap = 32.0;
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
                    ui.add_space(24.0);
                    self.render_editor_right_column(ui, ctx, &clip, avail_w, c);
                }
                ui.add_space(32.0);
            });

        if close_to_clips {
            self.close_editor_session();
            self.view = ViewTab::Clips;
        }
    }


    pub(crate) fn render_editor_left_column(
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
            Rounding::same(6.0),
            c.surface,
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
            let center_play_rect =
                Rect::from_center_size(stage_rect.center(), Vec2::new(48.0, 48.0));
            ui.painter().rect_filled(
                center_play_rect,
                Rounding::same(24.0),
                Color32::from_rgba_unmultiplied(11, 11, 13, 190),
            );
            ui.painter().text(
                center_play_rect.center(),
                egui::Align2::CENTER_CENTER,
                "▶",
                FontId::proportional(17.0),
                Color32::WHITE,
            );
        }

        let badge_rect = Rect::from_min_size(
            Pos2::new(stage_rect.min.x + 12.0, stage_rect.max.y - 32.0),
            Vec2::new(148.0, 22.0),
        );
        ui.painter().rect_filled(
            badge_rect,
            Rounding::same(4.0),
            Color32::from_rgba_unmultiplied(11, 11, 13, 195),
        );
        ui.painter().text(
            Pos2::new(badge_rect.min.x + 8.0, badge_rect.center().y),
            egui::Align2::LEFT_CENTER,
            format!("{} / {}", format_mm_ss(ed.playhead_secs), clip.duration_label),
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
            Rounding::same(4.0),
            if open_resp.hovered() {
                c.accent
            } else {
                Color32::from_rgba_unmultiplied(11, 11, 13, 195)
            },
        );
        ui.painter().text(
            open_btn_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Open in player",
            FontId::proportional(11.5),
            Color32::WHITE,
        );

        let mut take_screenshot_ms: Option<u64> = None;
        let mut copy_clip_requested = false;

        if open_resp.clicked() {
            ed.is_playing = false;
            ed.last_play_tick = None;
            if let Some(ref tx) = ed.preview_worker_tx {
                let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                    is_playing: false,
                    position_ms: None,
                });
            }
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
            let pos_ms = (ed.playhead_secs * 1000.0).round() as u64;
            if let Some(ref tx) = ed.preview_worker_tx {
                let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                    is_playing: ed.is_playing,
                    position_ms: Some(pos_ms),
                });
            }
        }

        ui.add_space(12.0);

        let segments = ed.compute_segments(dur);
        let seg_count = segments.len();
        if ed.selected_segment_idx.map(|i| i >= seg_count).unwrap_or(true) {
            ed.selected_segment_idx = Some(0);
        }
        let sel_idx = ed.selected_segment_idx.unwrap_or(0);
        let sel_is_deleted = ed.deleted_segments.contains(&sel_idx);

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.spacing_mut().item_spacing.y = 6.0;

            let play_label = if ed.is_playing { "Pause" } else { "Play" };
            if draw_primary_button(ui, play_label, true, c).clicked() {
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
                let pos_ms = (ed.playhead_secs * 1000.0).round() as u64;
                if let Some(ref tx) = ed.preview_worker_tx {
                    let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                        is_playing: ed.is_playing,
                        position_ms: Some(pos_ms),
                    });
                }
            }

            if draw_ghost_button(ui, "Split (S)", false, c).clicked() {
                let _ = ed.split_at_time(ed.playhead_secs, dur);
            }

            if seg_count > 1 {
                if sel_is_deleted {
                    if draw_ghost_button(
                        ui,
                        &format!("Restore part {}", sel_idx + 1),
                        true,
                        c,
                    )
                    .clicked()
                    {
                        ed.deleted_segments.remove(&sel_idx);
                    }
                } else if draw_danger_button(
                    ui,
                    &format!("Delete part {} (Del)", sel_idx + 1),
                    c,
                )
                .clicked()
                {
                    ed.deleted_segments.insert(sel_idx);
                }

                if draw_ghost_button(ui, "Reset cuts", false, c).clicked() {
                    ed.splits_secs.clear();
                    ed.deleted_segments.clear();
                    ed.selected_segment_idx = Some(0);
                    ed.trim_start_secs = 0.0;
                    ed.trim_end_secs = dur;
                }
            }

            let speed_label = match (ed.playback_speed * 100.0).round() as u32 {
                25 => "0.25x",
                50 => "0.5x",
                200 => "2x",
                _ => "1x",
            };
            if draw_ghost_button(ui, speed_label, ed.playback_speed != 1.0, c).clicked() {
                ed.playback_speed = match (ed.playback_speed * 100.0).round() as u32 {
                    100 => 0.5,
                    50 => 0.25,
                    25 => 2.0,
                    _ => 1.0,
                };
                if let Some(ref tx) = ed.preview_worker_tx {
                    let _ = tx.send(PreviewWorkerCmd::SetPlaybackRate(ed.playback_speed));
                }
            }

            if draw_ghost_button(ui, "Screenshot", false, c).clicked() {
                take_screenshot_ms = Some((ed.playhead_secs * 1000.0).round() as u64);
            }

            if draw_ghost_button(ui, "Copy", false, c).clicked() {
                copy_clip_requested = true;
            }

            if draw_ghost_button(ui, "Fullscreen", false, c).clicked() {
                ed.fullscreen_preview = !ed.fullscreen_preview;
            }
        });

        ui.add_space(12.0);

        let segments = ed.compute_segments(dur);
        let kept_dur: f64 = segments
            .iter()
            .filter(|(_, _, _, is_del)| !*is_del)
            .map(|(_, s, e, _)| (e - s).max(0.0))
            .sum();

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&clip.display_title).size(14.5).color(c.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let kept_count = segments.iter().filter(|(_, _, _, d)| !*d).count();
                let readout = if segments.len() > 1 {
                    format!(
                        "{} kept  ({}/{} parts)",
                        format_mm_ss(kept_dur),
                        kept_count,
                        segments.len()
                    )
                } else {
                    format!(
                        "{} – {}  ({} kept)",
                        format_mm_ss(ed.trim_start_secs),
                        format_mm_ss(ed.trim_end_secs),
                        format_mm_ss(kept_dur)
                    )
                };
                ui.label(
                    RichText::new(readout)
                        .font(FontId::monospace(12.0))
                        .color(c.text_2),
                );
            });
        });

        ui.add_space(8.0);

        let (track_rect, track_resp) =
            ui.allocate_exact_size(Vec2::new(width, 46.0), Sense::click_and_drag());
        let painter = ui.painter();
        painter.rect(
            track_rect,
            Rounding::same(6.0),
            c.surface,
            Stroke::new(1.0_f32, c.border),
        );

        let track_w = track_rect.width().max(1.0);
        let start_x =
            track_rect.min.x + ((ed.trim_start_secs / dur) as f32).clamp(0.0, 1.0) * track_w;
        let end_x =
            track_rect.min.x + ((ed.trim_end_secs / dur) as f32).clamp(0.0, 1.0) * track_w;

        for (seg_idx, seg_s, seg_e, is_del) in &segments {
            let sx0 = track_rect.min.x + ((*seg_s / dur) as f32).clamp(0.0, 1.0) * track_w;
            let sx1 = track_rect.min.x + ((*seg_e / dur) as f32).clamp(0.0, 1.0) * track_w;
            let seg_rect = Rect::from_min_max(
                Pos2::new(sx0 + 1.0, track_rect.min.y + 1.0),
                Pos2::new((sx1 - 1.0).max(sx0 + 3.0), track_rect.max.y - 1.0),
            );
            let is_selected = ed.selected_segment_idx == Some(*seg_idx);
            let fill_col = if *is_del {
                Color32::from_rgba_unmultiplied(228, 84, 72, if is_selected { 60 } else { 28 })
            } else if is_selected {
                Color32::from_rgba_unmultiplied(59, 130, 246, 65)
            } else {
                c.accent_soft
            };
            let border_col = if is_selected && segments.len() > 1 {
                if *is_del { c.danger } else { c.accent }
            } else if *is_del {
                Color32::from_rgba_unmultiplied(228, 84, 72, 110)
            } else {
                c.border_strong
            };
            painter.rect(
                seg_rect,
                Rounding::same(4.0),
                fill_col,
                Stroke::new(if is_selected && segments.len() > 1 { 2.0_f32 } else { 1.0_f32 }, border_col),
            );
            if segments.len() > 1 && seg_rect.width() >= 44.0 {
                let seg_label = if *is_del {
                    format!("Part {} (Deleted)", seg_idx + 1)
                } else {
                    format!("Part {}", seg_idx + 1)
                };
                painter.text(
                    seg_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    seg_label,
                    FontId::proportional(11.5),
                    if *is_del { c.danger } else { c.text },
                );
            }
        }

        let handle_l = Rect::from_min_max(
            Pos2::new(start_x, track_rect.min.y),
            Pos2::new(start_x + 7.0, track_rect.max.y),
        );
        let handle_r = Rect::from_min_max(
            Pos2::new(end_x - 7.0, track_rect.min.y),
            Pos2::new(end_x, track_rect.max.y),
        );
        painter.rect_filled(handle_l, Rounding::same(2.0), c.accent);
        painter.rect_filled(handle_r, Rounding::same(2.0), c.accent);

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
            Stroke::new(2.0_f32, Color32::WHITE),
        );

        if track_resp.secondary_clicked() {
            if let Some(pos) = track_resp.interact_pointer_pos() {
                if let Some((idx, _)) = ed.splits_secs.iter().enumerate().find(|(_, &sp)| {
                    let sx = track_rect.min.x + ((sp / dur) as f32).clamp(0.0, 1.0) * track_w;
                    (pos.x - sx).abs() <= 10.0
                }) {
                    ed.splits_secs.remove(idx);
                    ed.deleted_segments.clear();
                    ed.selected_segment_idx = Some(0);
                }
            }
        }

        if let Some(pos) = track_resp.interact_pointer_pos() {
            let rel = ((pos.x - track_rect.min.x) / track_w).clamp(0.0, 1.0) as f64;
            let raw_t = rel * dur;
            if track_resp.clicked() {
                for (seg_idx, seg_s, seg_e, _) in &segments {
                    if raw_t >= *seg_s && raw_t <= *seg_e {
                        ed.selected_segment_idx = Some(*seg_idx);
                        break;
                    }
                }
            }
            if track_resp.drag_started() {
                ed.is_playing = false;
                ed.last_play_tick = None;
                if let Some((idx, _)) = ed.splits_secs.iter().enumerate().find(|(_, &sp)| {
                    let sx = track_rect.min.x + ((sp / dur) as f32).clamp(0.0, 1.0) * track_w;
                    (pos.x - sx).abs() <= 9.0
                }) {
                    ed.dragging_split_idx = Some(idx);
                    ed.dragging_handle = Some("split");
                } else if (pos.x - start_x).abs() <= 12.0 {
                    ed.dragging_handle = Some("start");
                } else if (pos.x - end_x).abs() <= 12.0 {
                    ed.dragging_handle = Some("end");
                } else {
                    ed.dragging_handle = Some("playhead");
                    for (seg_idx, seg_s, seg_e, _) in &segments {
                        if raw_t >= *seg_s && raw_t <= *seg_e {
                            ed.selected_segment_idx = Some(*seg_idx);
                            break;
                        }
                    }
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
                if let Some(ref tx) = ed.preview_worker_tx {
                    let _ = tx.send(PreviewWorkerCmd::SetPlayState {
                        is_playing: false,
                        position_ms: Some(req_ms),
                    });
                }
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
            if segments.len() > 1 {
                ui.add_space(12.0);
                ui.label(
                    RichText::new("Click any part to select it · Press Delete to cut it out")
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

        if copy_clip_requested {
            match self.bridge.copy_file_to_clipboard(&clip.file_path) {
                Ok(()) => self.show_toast("Copied clip to clipboard (Ctrl+V in Discord)", None),
                Err(e) => self.show_toast(format!("Copy failed: {e}"), None),
            }
        }
        if let Some(shot_ms) = take_screenshot_ms {
            let bridge = self.bridge.clone();
            let path = clip.file_path.clone();
            let tx = self.async_tx.clone();
            let ctx_clone = ctx.clone();
            self.show_toast("Saving screenshot...", None);
            let _ = std::thread::Builder::new()
                .name("clotch-screenshot".to_string())
                .spawn(move || {
                    match bridge.save_frame_screenshot(&path, shot_ms) {
                        Ok(out_png) => {
                            let fname = out_png
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("screenshot.png");
                            let _ = tx.send(UiAsyncMessage::EditorJobFinished {
                                message: format!("Screenshot saved & copied: {fname}"),
                                reveal_path: Some(out_png),
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(UiAsyncMessage::EditorJobFinished {
                                message: format!("Screenshot failed: {e}"),
                                reveal_path: None,
                            });
                        }
                    }
                    ctx_clone.request_repaint();
                });
        }
    }


    pub(crate) fn render_editor_right_column(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        clip: &ClipItem,
        width: f32,
        c: ThemeColors,
    ) {
        let mut save_copy_clicked = false;
        let mut save_parts_clicked = false;
        let mut export_stems_clicked = false;
        let mut cancel_job_clicked = false;

        let Some(ref mut ed) = self.editor_state else {
            return;
        };
        let dur = clip.duration_secs.max(1.0);

        section_title(ui, "Cut & Split", c, true);

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

        let segs = ed.compute_segments(dur);
        let sel_idx = ed.selected_segment_idx.unwrap_or(0).min(segs.len().saturating_sub(1));
        let is_sel_del = ed.deleted_segments.contains(&sel_idx);
        let split_hint = if segs.len() <= 1 {
            "Slice clip into parts at current time".to_string()
        } else {
            format!(
                "Part {}/{} selected ({})",
                sel_idx + 1,
                segs.len(),
                if is_sel_del { "Deleted" } else { "Kept" }
            )
        };
        draw_setting_row(ui, width, "Timeline parts", Some(&split_hint), c, |ui| {
            if segs.len() > 1 {
                if is_sel_del {
                    if draw_ghost_button(ui, "Restore part", true, c).clicked() {
                        ed.deleted_segments.remove(&sel_idx);
                    }
                } else if draw_danger_button(ui, "Delete part", c).clicked() {
                    ed.deleted_segments.insert(sel_idx);
                }
            }
            if draw_ghost_button(ui, "Split here", false, c).clicked() {
                let _ = ed.split_at_time(ed.playhead_secs, dur);
            }
        });

        section_title(ui, "Video", c, false);

        draw_setting_row(ui, width, "Resolution", None, c, |ui| {
            egui::ComboBox::from_id_salt("ed_res_combo")
                .selected_text(RichText::new(&ed.resolution).size(13.0).color(c.text))
                .width(120.0)
                .show_ui(ui, |ui| {
                    for opt in ["Original", "1440p", "1080p", "720p", "9:16 Vertical"] {
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
        let mut mix_changed = false;

        for idx in 0..ed.stems.len() {
            let label = ed.stems[idx].label;
            let enabled = ed.stems[idx].enabled;
            let solo = ed.stems[idx].solo;
            let muted_row =
                !has_stems || ed.remove_all_audio || !enabled || (any_solo && !solo);
            let prev_vol = ed.stems[idx].volume_pct;
            let mut vol = prev_vol;
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
            if vol != prev_vol {
                ed.stems[idx].volume_pct = vol;
                mix_changed = true;
            }
            if tog {
                clicked_toggle_idx = Some(idx);
            }
            if sol {
                clicked_solo_idx = Some(idx);
            }
        }

        if let Some(i) = clicked_toggle_idx {
            ed.stems[i].enabled = !ed.stems[i].enabled;
            mix_changed = true;
        }
        if let Some(i) = clicked_solo_idx {
            let was_solo = ed.stems[i].solo;
            for st in &mut ed.stems {
                st.solo = false;
            }
            ed.stems[i].solo = !was_solo;
            mix_changed = true;
        }

        draw_setting_row(ui, width, "Remove all audio", None, c, |ui| {
            if draw_toggle_switch(ui, &mut ed.remove_all_audio, c) {
                mix_changed = true;
            }
        });

        if mix_changed {
            let live_mix = ed.build_audio_mix_config(has_stems);
            if let Some(ref tx) = ed.preview_worker_tx {
                let _ = tx.send(PreviewWorkerCmd::UpdateAudioMix(live_mix));
            }
        }

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
        let kept_count = segs.iter().filter(|(_, _, _, d)| !*d).count();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let save_label = if ed.busy {
                "Processing..."
            } else {
                "Save copy"
            };
            if draw_primary_button(ui, save_label, !ed.busy && kept_count > 0, c).clicked()
                && !ed.busy
                && kept_count > 0
            {
                save_copy_clicked = true;
            }
            if !ed.busy && kept_count > 1 {
                if draw_ghost_button(ui, "Save parts separately", false, c).clicked() {
                    save_parts_clicked = true;
                }
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
        if save_parts_clicked {
            self.trigger_editor_save_split_parts(ctx);
        }
        if export_stems_clicked {
            self.trigger_editor_export_stems(ctx);
        }
    }


}