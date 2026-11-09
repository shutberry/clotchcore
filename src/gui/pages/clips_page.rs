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
    pub(crate) fn render_right_sidebar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, c: ThemeColors) {
        self.rebuild_filters_and_games_if_needed();

        let total_clips = self.clips.len();
        let highlights_count = self.clips.iter().filter(|cl| is_highlight_clip(cl)).count();
        let replays_count = total_clips.saturating_sub(highlights_count);
        let fav_count = self.clips.iter().filter(|cl| cl.favorite).count();

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            let (logo_rect, _) = ui.allocate_exact_size(Vec2::new(24.0, 24.0), Sense::hover());
            ui.painter()
                .rect_filled(logo_rect, Rounding::same(6.0), c.accent);
            let lc = logo_rect.center();
            ui.painter().line_segment(
                [Pos2::new(lc.x - 4.5, lc.y - 2.0), Pos2::new(lc.x, lc.y + 3.0)],
                Stroke::new(2.0_f32, Color32::WHITE),
            );
            ui.painter().line_segment(
                [Pos2::new(lc.x, lc.y + 3.0), Pos2::new(lc.x + 4.5, lc.y - 2.0)],
                Stroke::new(2.0_f32, Color32::WHITE),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("Clotch")
                    .size(16.5)
                    .strong()
                    .color(c.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(16.0);
                let (dot_r, _) = ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(dot_r.center(), 3.2, c.accent);
            });
        });

        ui.add_space(22.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(RichText::new("Library").size(11.5).color(c.text_3));
        });
        ui.add_space(6.0);

        let nav_items: [(&str, &str, &str, Option<usize>, bool); 6] = [
            (
                "grid",
                "All clips",
                "All",
                Some(total_clips),
                (self.view == ViewTab::Clips || self.view == ViewTab::Editor)
                    && self.filter == "All",
            ),
            (
                "play",
                "Highlights",
                "Highlights",
                Some(highlights_count),
                (self.view == ViewTab::Clips || self.view == ViewTab::Editor)
                    && self.filter == "Highlights",
            ),
            (
                "clock",
                "Replays",
                "Replays",
                Some(replays_count),
                (self.view == ViewTab::Clips || self.view == ViewTab::Editor)
                    && self.filter == "Replays",
            ),
            (
                "star",
                "Favorites",
                "Favorites",
                if fav_count > 0 { Some(fav_count) } else { None },
                (self.view == ViewTab::Clips || self.view == ViewTab::Editor)
                    && self.filter == "Favorites",
            ),
            (
                "stats",
                "Stats & Wrapped",
                "__STATS__",
                if self.stats_summary.total_matches > 0 {
                    Some(self.stats_summary.total_matches as usize)
                } else {
                    None
                },
                self.view == ViewTab::Stats,
            ),
            (
                "settings",
                "Settings",
                "__SETTINGS__",
                None,
                self.view == ViewTab::Settings,
            ),
        ];

        for (icon_kind, label, filter_key, badge_opt, is_active) in nav_items {
            if draw_sidebar_nav_row(ui, icon_kind, label, badge_opt, is_active, None, c).clicked() {
                if self.recording_keybind_slot.is_some() {
                    self.recording_keybind_slot = None;
                    self.bridge.set_hotkeys_paused(false);
                }
                match filter_key {
                    "__STATS__" => {
                        self.close_editor_session();
                        self.refresh_stats_and_wrapped();
                        self.view = ViewTab::Stats;
                    }
                    "__SETTINGS__" => {
                        self.close_editor_session();
                        self.view = ViewTab::Settings;
                    }
                    f_val => {
                        self.close_editor_session();
                        self.view = ViewTab::Clips;
                        self.filter = f_val.to_string();
                        self.filters_dirty = true;
                    }
                }
            }
            ui.add_space(2.0);
        }

        let mut game_rows: Vec<(String, String, usize, Color32)> = Vec::new();
        if self.view == ViewTab::Stats {
            for (g_key, g_stat) in &self.stats_summary.per_game {
                if g_stat.matches_played == 0 {
                    continue;
                }
                let pretty = pretty_game_title(g_key);
                if game_rows.iter().any(|r| r.1.eq_ignore_ascii_case(&pretty)) {
                    continue;
                }
                let dot = game_indicator_color(&pretty);
                game_rows.push((g_key.clone(), pretty, g_stat.matches_played as usize, dot));
            }
        } else {
            for g in &self.available_games {
                let pretty = pretty_game_title(g);
                if game_rows.iter().any(|r| r.1.eq_ignore_ascii_case(&pretty)) {
                    continue;
                }
                let cnt = self
                    .clips
                    .iter()
                    .filter(|cl| {
                        cl.game.eq_ignore_ascii_case(g)
                            || pretty_game_title(&cl.game).eq_ignore_ascii_case(&pretty)
                    })
                    .count();
                if cnt == 0 {
                    continue;
                }
                let dot = game_indicator_color(&pretty);
                game_rows.push((g.clone(), pretty, cnt, dot));
            }
        }
        game_rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));

        if !game_rows.is_empty() {
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.label(RichText::new("Games").size(11.5).color(c.text_3));
            });
            ui.add_space(6.0);

            let avail_h = (ui.available_height() - 64.0).max(60.0);
            ScrollArea::vertical()
                .id_salt("right_sidebar_games_scroll")
                .max_height(avail_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (raw_g, pretty_g, cnt, dot_col) in game_rows {
                        let is_active = if self.view == ViewTab::Stats {
                            self.stats_game_filter.eq_ignore_ascii_case(&raw_g)
                                || pretty_game_title(&self.stats_game_filter)
                                    .eq_ignore_ascii_case(&pretty_g)
                        } else {
                            (self.view == ViewTab::Clips || self.view == ViewTab::Editor)
                                && (self.filter.eq_ignore_ascii_case(&raw_g)
                                    || pretty_game_title(&self.filter)
                                        .eq_ignore_ascii_case(&pretty_g))
                        };
                        if draw_sidebar_nav_row(
                            ui,
                            "",
                            &pretty_g,
                            Some(cnt),
                            is_active,
                            Some(dot_col),
                            c,
                        )
                        .clicked()
                        {
                            if self.view == ViewTab::Stats {
                                self.stats_game_filter = raw_g;
                                self.refresh_stats_and_wrapped();
                            } else {
                                self.close_editor_session();
                                self.view = ViewTab::Clips;
                                self.filter = raw_g;
                                self.filters_dirty = true;
                            }
                        }
                        ui.add_space(2.0);
                    }
                });
        }

        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            ui.add_space(14.0);
            let pill_w = (ui.available_width() - 24.0).max(150.0);
            let (pill_rect, pill_resp) =
                ui.allocate_exact_size(Vec2::new(pill_w, 34.0), Sense::click());
            if pill_resp.hovered() {
                ctx.set_cursor_icon(CursorIcon::PointingHand);
            }
            let pill_fill = if pill_resp.hovered() {
                Color32::from_rgb(37, 99, 235)
            } else {
                c.accent
            };
            ui.painter()
                .rect_filled(pill_rect, Rounding::same(7.0), pill_fill);
            let dot_center = Pos2::new(pill_rect.min.x + 16.0, pill_rect.center().y);
            ui.painter()
                .circle_filled(dot_center, 3.5, Color32::WHITE);
            let status_label = if self.is_recording_active {
                self.status_text.clone()
            } else {
                "Recording active".to_string()
            };
            ui.painter().text(
                Pos2::new(pill_rect.min.x + 28.0, pill_rect.center().y),
                egui::Align2::LEFT_CENTER,
                status_label,
                FontId::proportional(12.5),
                Color32::WHITE,
            );
            if pill_resp.clicked() {
                self.bridge.trigger_save_replay();
                self.show_toast("Saving instant replay...", None);
            }
        });
    }


    pub(crate) fn render_clips_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, c: ThemeColors) {
        self.rebuild_filters_and_games_if_needed();

        let active_dir = self.active_clips_dir.clone();
        let active_dir_str = active_dir.to_string_lossy().to_string();
        let mut open_clips_folder = false;
        let mut trigger_rescan = false;

        ui.horizontal_wrapped(|ui| {
            let header_title = match self.filter.as_str() {
                "All" => "All clips".to_string(),
                "Highlights" => "Highlights".to_string(),
                "Replays" => "Replays".to_string(),
                "Favorites" => "Favorites".to_string(),
                other => pretty_game_title(other),
            };
            ui.label(
                RichText::new(header_title)
                    .size(19.0)
                    .strong()
                    .color(c.text),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("{}", self.filtered_indices.len()))
                    .size(13.0)
                    .color(c.text_3),
            );

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
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
                if text_action_link(ui, "Open folder", c.text_2, c.text).clicked() {
                    open_clips_folder = true;
                }
                self.render_search_box(ui, c);
            });
        });

        if open_clips_folder {
            let _ = std::fs::create_dir_all(&active_dir);
            reveal_file_in_explorer(&active_dir);
        }
        if trigger_rescan {
            self.spawn_background_library_scan(ctx);
        }

        ui.add_space(16.0);

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
                RichText::new("No clips match your search or filter.")
                    .size(14.0)
                    .color(c.text_3),
            );
            return;
        }

        let avail_w = ui.available_width();
        let cols = if avail_w >= 1440.0 {
            5usize
        } else if avail_w >= 1100.0 {
            4usize
        } else if avail_w >= 760.0 {
            3usize
        } else if avail_w >= 480.0 {
            2usize
        } else {
            1usize
        };
        let gap_x = 22.0f32;
        let gap_y = 26.0f32;
        let card_w = ((avail_w - gap_x * ((cols - 1) as f32)) / (cols as f32)).max(200.0);
        let thumb_h = (card_w * 9.0 / 16.0).round();
        let row_h = thumb_h + 72.0;
        let total_items = self.filtered_indices.len();
        let total_rows = (total_items + cols - 1) / cols;

        let mut open_editor_id: Option<u64> = None;
        let mut toggle_fav_id: Option<u64> = None;
        let mut start_rename_id: Option<(u64, String)> = None;
        let mut finish_rename_id: Option<u64> = None;
        let mut cancel_rename = false;
        let mut compress_discord_id: Option<u64> = None;
        let mut copy_clip_id: Option<u64> = None;
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
                            let game = pretty_game_title(&clip_ref.game);
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

                                    let border_col = if thumb_resp.hovered() {
                                        c.border_strong
                                    } else {
                                        c.border
                                    };
                                    ui.painter().rect(
                                        thumb_rect,
                                        Rounding::same(6.0),
                                        c.surface,
                                        Stroke::new(1.0_f32, border_col),
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
                                        Rounding::same(4.0),
                                        Color32::from_rgba_unmultiplied(11, 11, 13, 205),
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
                                            Rounding::same(4.0),
                                            Color32::from_rgba_unmultiplied(11, 11, 13, 180),
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

                                    ui.add_space(9.0);

                                    if self.renaming_clip_id == Some(cid) {
                                        let edit_resp = ui.add_sized(
                                            [card_w, 22.0],
                                            egui::TextEdit::singleline(&mut self.rename_buffer)
                                                .font(FontId::proportional(13.5)),
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
                                                    .size(13.5)
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
                                        let sub = if map_name.is_empty() || map_name == "Unknown" {
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
                                            ui.spacing_mut().item_spacing.x = 13.0;
                                            if text_action_link(ui, "Edit", c.text_2, c.text)
                                                .clicked()
                                            {
                                                open_editor_id = Some(cid);
                                            }
                                            if text_action_link(ui, "Copy", c.text_2, c.text)
                                                .clicked()
                                            {
                                                copy_clip_id = Some(cid);
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
        if let Some(cid) = copy_clip_id {
            if let Some(clip) = self.clips.iter().find(|c| c.id == cid) {
                match self.bridge.copy_file_to_clipboard(&clip.file_path) {
                    Ok(()) => self.show_toast("Copied clip to clipboard (Ctrl+V in Discord)", None),
                    Err(e) => self.show_toast(format!("Copy failed: {e}"), None),
                }
            }
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


    pub(crate) fn render_search_box(&mut self, ui: &mut egui::Ui, c: ThemeColors) {
        let search_w = 220.0f32.min(ui.available_width().max(120.0));
        let (rect, _) = ui.allocate_exact_size(Vec2::new(search_w, 26.0), Sense::hover());
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        let prev = self.search.clone();
        let resp = child.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text(RichText::new("Search clips...").size(13.0).color(c.text_3))
                .font(FontId::proportional(13.0))
                .frame(false)
                .desired_width(search_w),
        );
        if self.search != prev {
            self.filters_dirty = true;
        }
        let line_col = if resp.has_focus() {
            c.accent
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


}