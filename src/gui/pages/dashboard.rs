#![allow(dead_code, unused_imports)]
use crate::gui::pages::tracker_page::{check_riot_lockfile_exists, MatchRecordModalState};
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
    pub(crate) fn refresh_stats_and_wrapped(&mut self) {
        let g_filter = if self.stats_game_filter == "All" || self.stats_game_filter.is_empty() {
            None
        } else {
            Some(self.stats_game_filter.as_str())
        };
        self.stats_summary = self
            .bridge
            .query_local_stats_summary(self.stats_window_days, g_filter);
        let yr_opt = if self.wrapped_year_filter <= 0 {
            None
        } else {
            Some(self.wrapped_year_filter)
        };
        self.year_wrapped = self.bridge.query_year_wrapped(yr_opt, g_filter);
        self.local_identities = self.bridge.discover_local_identities();
        self.live_game_state = self.bridge.query_live_game_state();
        self.riot_lockfile_online = check_riot_lockfile_exists();
    }


    pub(crate) fn render_stats_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, c: ThemeColors) {
        if self.last_live_state_poll.elapsed() >= Duration::from_secs(4) {
            self.last_live_state_poll = Instant::now();
            self.live_game_state = self.bridge.query_live_game_state();
            let now_lockfile = check_riot_lockfile_exists();
            if now_lockfile && !self.riot_lockfile_online {
                let _ = self.bridge.sync_game_files_now();
                self.refresh_stats_and_wrapped();
            }
            self.riot_lockfile_online = now_lockfile;
        }

        let mut trigger_refresh = false;
        let mut trigger_sync_logs = false;
        let mut trigger_export_wrapped_png = false;
        let mut trigger_copy_wrapped_png = false;
        let mut delete_match_id: Option<String> = None;
        let mut edit_match_rec: Option<MatchStatsRecord> = None;
        let mut toggle_expand_match: Option<String> = None;
        let mut open_clip_id_from_match: Option<u64> = None;

        ScrollArea::vertical()
            .id_salt("tracker_telemetry_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(if self.stats_subview_wrapped {
                            "Season Wrapped"
                        } else {
                            "Tracker & Match Telemetry"
                        })
                        .size(19.5)
                        .strong()
                        .color(c.text),
                    );
                    ui.add_space(10.0);

                    let mut mode_tab = if self.stats_subview_wrapped {
                        "Year Wrapped"
                    } else {
                        "Match Telemetry"
                    };
                    if draw_segmented_buttons(
                        ui,
                        "stats_mode_seg",
                        &["Match Telemetry", "Year Wrapped"],
                        &mut mode_tab,
                        c,
                    ) {
                        self.stats_subview_wrapped = mode_tab == "Year Wrapped";
                        trigger_refresh = true;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if !self.stats_subview_wrapped {
                            if draw_ghost_button(ui, "+ Log Match", self.match_modal.open, c).clicked()
                            {
                                self.match_modal = MatchRecordModalState {
                                    open: true,
                                    game: if self.stats_game_filter != "All" {
                                        self.stats_game_filter.clone()
                                    } else {
                                        String::new()
                                    },
                                    ..Default::default()
                                };
                            }
                            if draw_ghost_button(ui, "Sync Game Files", false, c).clicked() {
                                trigger_sync_logs = true;
                            }
                            let mut period_label = match self.stats_window_days {
                                1 => "Today",
                                7 => "7 Days",
                                30 => "30 Days",
                                _ => "All Time",
                            };
                            if draw_segmented_buttons(
                                ui,
                                "stats_period_seg",
                                &["Today", "7 Days", "30 Days", "All Time"],
                                &mut period_label,
                                c,
                            ) {
                                self.stats_window_days = match period_label {
                                    "Today" => 1,
                                    "7 Days" => 7,
                                    "30 Days" => 30,
                                    _ => 36500,
                                };
                                trigger_refresh = true;
                            }
                        } else {
                            let mut yr_label = if self.wrapped_year_filter == 2026 {
                                "2026 Wrapped"
                            } else {
                                "All-Time Wrapped"
                            };
                            if draw_segmented_buttons(
                                ui,
                                "wrapped_yr_seg",
                                &["2026 Wrapped", "All-Time Wrapped"],
                                &mut yr_label,
                                c,
                            ) {
                                self.wrapped_year_filter = if yr_label == "2026 Wrapped" {
                                    2026
                                } else {
                                    0
                                };
                                trigger_refresh = true;
                            }
                        }
                    });
                });

                ui.add_space(12.0);

                egui::Frame::none()
                    .fill(c.surface)
                    .stroke(Stroke::new(1.0_f32, c.border))
                    .rounding(Rounding::same(6.0))
                    .inner_margin(Margin::symmetric(14.0, 9.0))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 12.0;
                            if let Some(ref live) = self.live_game_state {
                                let (dot_r, _) =
                                    ui.allocate_exact_size(Vec2::new(8.0, 14.0), Sense::hover());
                                ui.painter().circle_filled(dot_r.center(), 4.0, c.success);
                                let score_txt = match (live.score_for, live.score_against) {
                                    (Some(sf), Some(sa)) => format!(" · Score {sf}:{sa}"),
                                    _ => String::new(),
                                };
                                let hero_txt = if live.hero_or_agent.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · {}", live.hero_or_agent)
                                };
                                let weap_txt = if live.weapon_or_loadout.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({})", live.weapon_or_loadout)
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "LIVE MATCH: {} · {}{}{}{} · K/D/A {}/{}/{}",
                                        pretty_game_title(&live.active_game),
                                        if live.map.is_empty() { "In Progress" } else { &live.map },
                                        hero_txt,
                                        weap_txt,
                                        score_txt,
                                        live.kills,
                                        live.deaths,
                                        live.assists
                                    ))
                                    .size(12.5)
                                    .strong()
                                    .color(c.text),
                                );
                            } else {
                                ui.label(
                                    RichText::new("Local Launcher Profiles:")
                                        .size(12.0)
                                        .color(c.text_3),
                                );
                            }

                            for id_info in &self.local_identities {
                                let shaped_name = shape_arabic_for_egui(&id_info.display_id);
                                egui::Frame::none()
                                    .fill(c.surface_2)
                                    .stroke(Stroke::new(1.0_f32, c.border_strong))
                                    .rounding(Rounding::same(5.0))
                                    .inner_margin(Margin::symmetric(9.0, 3.0))
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(format!(
                                                "{}: {}",
                                                id_info.launcher, shaped_name
                                            ))
                                            .size(11.5)
                                            .color(c.text),
                                        );
                                    });
                            }

                            let has_riot = self
                                .local_identities
                                .iter()
                                .any(|x| x.launcher.contains("Riot"));
                            if !has_riot {
                                ui.label(
                                    RichText::new(if self.riot_lockfile_online {
                                        "Riot Client: Active (Click 'Sync Game Files' to pull match details)"
                                    } else {
                                        "Riot Client: Offline (Open Riot Client to auto-sync Valorant Agents & Headshot %)"
                                    })
                                    .size(11.5)
                                    .color(c.text_3),
                                );
                            }
                        });
                    });

                ui.add_space(12.0);

                let mut played_games: Vec<String> = self
                    .stats_summary
                    .per_game
                    .iter()
                    .filter(|(_, st)| st.matches_played > 0)
                    .map(|(k, _)| k.clone())
                    .collect();
                played_games.sort_by(|a, b| pretty_game_title(a).cmp(&pretty_game_title(b)));

                if !played_games.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let all_active = self.stats_game_filter == "All";
                        if draw_ghost_button(ui, "All Played Games", all_active, c).clicked() {
                            self.stats_game_filter = "All".to_string();
                            trigger_refresh = true;
                        }
                        for g in &played_games {
                            let pretty = pretty_game_title(g);
                            let active = self.stats_game_filter.eq_ignore_ascii_case(g)
                                || pretty_game_title(&self.stats_game_filter)
                                    .eq_ignore_ascii_case(&pretty);
                            if draw_ghost_button(ui, &pretty, active, c).clicked() {
                                self.stats_game_filter = g.clone();
                                trigger_refresh = true;
                            }
                        }
                    });
                    ui.add_space(14.0);
                }

                if self.match_modal.open {
                    self.render_match_record_modal(ui, c, &mut trigger_refresh);
                    ui.add_space(14.0);
                }

                if self.stats_subview_wrapped {
                    self.render_year_wrapped_card_view(
                        ui,
                        c,
                        &mut trigger_export_wrapped_png,
                        &mut trigger_copy_wrapped_png,
                    );
                } else {
                    self.render_tracker_telemetry_view(
                        ui,
                        ctx,
                        c,
                        &mut edit_match_rec,
                        &mut delete_match_id,
                        &mut toggle_expand_match,
                        &mut open_clip_id_from_match,
                    );
                }
            });

        if let Some(mid) = toggle_expand_match {
            if self.expanded_match_id.as_deref() == Some(mid.as_str()) {
                self.expanded_match_id = None;
            } else {
                self.expanded_match_id = Some(mid);
            }
        }
        if let Some(cid) = open_clip_id_from_match {
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
        if trigger_sync_logs {
            let added = self.bridge.sync_game_files_now();
            self.refresh_stats_and_wrapped();
            self.show_toast(
                if added > 0 {
                    format!("Synced {added} verified matches from local game telemetry")
                } else if !self.riot_lockfile_online {
                    "Local logs synced. Open Riot Client to pull full Valorant match scoreboards."
                        .to_string()
                } else {
                    "All local game telemetry is up to date".to_string()
                },
                None,
            );
        }
        if let Some(del_id) = delete_match_id {
            if self.bridge.delete_match_stats(&del_id).unwrap_or(false) {
                self.refresh_stats_and_wrapped();
                self.show_toast("Match removed from local history", None);
            }
        }
        if let Some(rec) = edit_match_rec {
            self.match_modal = MatchRecordModalState {
                open: true,
                editing_match_id: Some(rec.match_id.clone()),
                game: pretty_game_title(&rec.game),
                map: rec.map.clone(),
                mode: rec.mode.clone(),
                hero_or_team: rec.hero_or_team.clone(),
                favorite_weapon: rec.favorite_weapon.clone(),
                kills_str: rec.kills.to_string(),
                deaths_str: rec.deaths.to_string(),
                assists_str: rec.assists.to_string(),
                headshots_str: rec.headshots.to_string(),
                bodyshots_str: rec.bodyshots.to_string(),
                legshots_str: rec.legshots.to_string(),
                aces_str: rec.aces.to_string(),
                clutches_str: rec.clutches.to_string(),
                multikills_str: rec.multikills.to_string(),
                goals_str: rec.goals.to_string(),
                saves_str: rec.saves.to_string(),
                score_for_str: rec.score_for.map(|v| v.to_string()).unwrap_or_default(),
                score_against_str: rec.score_against.map(|v| v.to_string()).unwrap_or_default(),
                won_choice: match rec.won {
                    Some(true) => 1,
                    Some(false) => 2,
                    None => 0,
                },
            };
        }
        if trigger_refresh {
            self.refresh_stats_and_wrapped();
        }
        if trigger_export_wrapped_png || trigger_copy_wrapped_png {
            match export_wrapped_card_png(&self.year_wrapped, &self.active_clips_dir) {
                Ok(saved_path) => {
                    if trigger_copy_wrapped_png {
                        let _ = self.bridge.copy_file_to_clipboard(&saved_path);
                        self.show_toast(
                            "Wrapped card copied to clipboard & saved as PNG",
                            Some(saved_path),
                        );
                    } else {
                        self.show_toast(
                            format!("Wrapped card saved: {}", saved_path.display()),
                            Some(saved_path),
                        );
                    }
                }
                Err(e) => {
                    self.show_toast(format!("Failed to export Wrapped PNG: {e}"), None);
                }
            }
        }
    }


    pub(crate) fn render_year_wrapped_card_view(
        &self,
        ui: &mut egui::Ui,
        c: ThemeColors,
        trigger_export_png: &mut bool,
        trigger_copy_png: &mut bool,
    ) {
        let w = &self.year_wrapped;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            if draw_primary_button(ui, "Save Wrapped Card as PNG", true, c).clicked() {
                *trigger_export_png = true;
            }
            if draw_ghost_button(ui, "Copy Wrapped Card to Clipboard", false, c).clicked() {
                *trigger_copy_png = true;
            }
            ui.label(
                RichText::new(
                    "100% Local Year-in-Review Card — Select any played game above to generate its custom card.",
                )
                .size(12.0)
                .color(c.text_3),
            );
        });
        ui.add_space(14.0);

        let card_w = ui.available_width().clamp(440.0, 680.0);
        let card_h = 540.0f32;
        let (card_rect, _) = ui.allocate_exact_size(Vec2::new(card_w, card_h), Sense::hover());
        let p = ui.painter();

        p.rect_filled(card_rect, Rounding::same(12.0), Color32::from_rgb(10, 12, 18));
        p.rect_filled(
            Rect::from_min_max(
                card_rect.min,
                Pos2::new(card_rect.max.x, card_rect.min.y + 135.0),
            ),
            Rounding {
                nw: 12.0,
                ne: 12.0,
                sw: 0.0,
                se: 0.0,
            },
            Color32::from_rgb(16, 24, 44),
        );
        p.line_segment(
            [
                Pos2::new(card_rect.min.x + 24.0, card_rect.min.y + 135.0),
                Pos2::new(card_rect.max.x - 24.0, card_rect.min.y + 135.0),
            ],
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(59, 130, 246, 90)),
        );
        p.rect_stroke(
            card_rect,
            Rounding::same(12.0),
            Stroke::new(1.5_f32, c.accent),
        );

        let yr_str = if w.year > 0 {
            format!("CLOTCH WRAPPED · {}", w.year)
        } else {
            "CLOTCH WRAPPED · ALL TIME".to_string()
        };
        p.text(
            Pos2::new(card_rect.min.x + 26.0, card_rect.min.y + 28.0),
            egui::Align2::LEFT_CENTER,
            yr_str,
            FontId::proportional(12.0),
            c.accent,
        );

        let game_title = if w.game_filter.eq_ignore_ascii_case("All Games")
            || w.game_filter.eq_ignore_ascii_case("all")
        {
            "ALL PLAYED GAMES".to_string()
        } else {
            pretty_game_title(&w.game_filter).to_uppercase()
        };
        p.text(
            Pos2::new(card_rect.max.x - 26.0, card_rect.min.y + 28.0),
            egui::Align2::RIGHT_CENTER,
            &game_title,
            FontId::proportional(12.0),
            Color32::from_rgb(148, 163, 184),
        );

        let shaped_identity = shape_arabic_for_egui(&w.player_identity);
        p.text(
            Pos2::new(card_rect.min.x + 26.0, card_rect.min.y + 62.0),
            egui::Align2::LEFT_CENTER,
            &shaped_identity,
            FontId::proportional(24.0),
            Color32::WHITE,
        );
        p.text(
            Pos2::new(card_rect.min.x + 26.0, card_rect.min.y + 92.0),
            egui::Align2::LEFT_CENTER,
            &w.playstyle_title,
            FontId::proportional(14.5),
            Color32::from_rgb(96, 165, 250),
        );
        p.text(
            Pos2::new(card_rect.min.x + 26.0, card_rect.min.y + 114.0),
            egui::Align2::LEFT_CENTER,
            &w.playstyle_subtitle,
            FontId::proportional(11.5),
            Color32::from_rgb(161, 161, 170),
        );

        let is_sports = w.total_goals > w.total_kills && w.total_goals > 0;
        let char_display = if w.top_character_name.is_empty()
            || w.top_character_name == "Flex Specialist"
            || w.top_character_name == "Unassigned"
        {
            "—".to_string()
        } else {
            w.top_character_name.clone()
        };
        let top_metrics = [
            (
                if is_sports { "TOTAL GOALS" } else { "TOTAL KILLS" },
                if is_sports {
                    format!("{}", w.total_goals)
                } else {
                    format!("{}", w.total_kills)
                },
                if is_sports {
                    format!("{} Saves · {} Assists", w.total_saves, w.total_assists)
                } else {
                    format!("K/D {:.2} · Peak {} in 1 match", w.kd_ratio, w.peak_kills_in_match)
                },
            ),
            (
                "WIN RATE & MATCHES",
                format!("{:.1}%", w.win_rate_pct),
                format!("{} Matches · {:.1} Hours", w.total_matches, w.total_hours),
            ),
            (
                w.top_character_label.as_str(),
                char_display,
                format!("Map: {}", w.favorite_map),
            ),
        ];

        let m_gap = 12.0f32;
        let m_w = (card_w - 52.0 - m_gap * 2.0) / 3.0;
        for (idx, (m_lbl, m_val, m_sub)) in top_metrics.iter().enumerate() {
            let mx = card_rect.min.x + 26.0 + (idx as f32) * (m_w + m_gap);
            let m_rect = Rect::from_min_size(Pos2::new(mx, card_rect.min.y + 154.0), Vec2::new(m_w, 94.0));
            p.rect(
                m_rect,
                Rounding::same(8.0),
                Color32::from_rgb(17, 20, 28),
                Stroke::new(1.0_f32, Color32::from_rgb(34, 40, 54)),
            );
            p.text(
                Pos2::new(m_rect.min.x + 12.0, m_rect.min.y + 20.0),
                egui::Align2::LEFT_CENTER,
                *m_lbl,
                FontId::proportional(10.5),
                Color32::from_rgb(148, 163, 184),
            );
            p.text(
                Pos2::new(m_rect.min.x + 12.0, m_rect.min.y + 48.0),
                egui::Align2::LEFT_CENTER,
                m_val,
                FontId::proportional(20.0),
                Color32::WHITE,
            );
            p.text(
                Pos2::new(m_rect.min.x + 12.0, m_rect.min.y + 74.0),
                egui::Align2::LEFT_CENTER,
                m_sub,
                FontId::proportional(10.5),
                Color32::from_rgb(161, 161, 170),
            );
        }

        let mid_rect = Rect::from_min_size(
            Pos2::new(card_rect.min.x + 26.0, card_rect.min.y + 264.0),
            Vec2::new(card_w - 52.0, 160.0),
        );
        p.rect(
            mid_rect,
            Rounding::same(8.0),
            Color32::from_rgb(17, 20, 28),
            Stroke::new(1.0_f32, Color32::from_rgb(34, 40, 54)),
        );
        p.text(
            Pos2::new(mid_rect.min.x + 16.0, mid_rect.min.y + 22.0),
            egui::Align2::LEFT_CENTER,
            "HIT DISTRIBUTION & ACCURACY BREAKDOWN",
            FontId::proportional(11.0),
            Color32::from_rgb(148, 163, 184),
        );

        draw_hit_silhouette_vector(
            p,
            Pos2::new(mid_rect.min.x + 62.0, mid_rect.min.y + 96.0),
            w.headshot_pct,
            w.bodyshot_pct,
            w.legshot_pct,
            c,
        );

        let tot_w_hits = w.total_headshots + w.total_bodyshots + w.total_legshots;
        let bx = mid_rect.min.x + 124.0;
        let bw = (mid_rect.max.x - bx - 20.0).max(100.0);
        for (idx, (lbl, pct, cnt, col)) in [
            (
                "Headshot Accuracy",
                w.headshot_pct,
                w.total_headshots,
                c.accent,
            ),
            (
                "Bodyshot Ratio",
                w.bodyshot_pct,
                w.total_bodyshots,
                Color32::from_rgb(56, 189, 248),
            ),
            (
                "Lower / Leg Ratio",
                w.legshot_pct,
                w.total_legshots,
                Color32::from_rgb(113, 113, 122),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let by = mid_rect.min.y + 52.0 + (idx as f32) * 36.0;
            p.text(
                Pos2::new(bx, by),
                egui::Align2::LEFT_CENTER,
                format!("{lbl} ({cnt})"),
                FontId::proportional(12.0),
                Color32::from_rgb(212, 212, 216),
            );
            p.text(
                Pos2::new(bx + bw, by),
                egui::Align2::RIGHT_CENTER,
                if tot_w_hits > 0 {
                    format!("{pct:.1}%")
                } else {
                    "—".to_string()
                },
                FontId::proportional(13.0),
                Color32::WHITE,
            );
            let bg_r = Rect::from_min_size(Pos2::new(bx, by + 11.0), Vec2::new(bw, 7.0));
            p.rect_filled(bg_r, Rounding::same(3.5), Color32::from_rgb(30, 35, 48));
            if tot_w_hits > 0 {
                let fw = (bw * ((pct as f32) / 100.0).clamp(0.0, 1.0)).max(2.0);
                p.rect_filled(
                    Rect::from_min_size(bg_r.min, Vec2::new(fw, 7.0)),
                    Rounding::same(3.5),
                    col,
                );
            }
        }

        let bot_rect = Rect::from_min_size(
            Pos2::new(card_rect.min.x + 26.0, card_rect.min.y + 440.0),
            Vec2::new(card_w - 52.0, 74.0),
        );
        p.rect(
            bot_rect,
            Rounding::same(8.0),
            Color32::from_rgb(17, 20, 28),
            Stroke::new(1.0_f32, Color32::from_rgb(34, 40, 54)),
        );
        let peak_items = [
            ("ACES", format!("{}", w.total_aces)),
            ("CLUTCHES", format!("{}", w.total_clutches)),
            ("MULTIKILLS", format!("{}", w.total_multikills)),
            ("MVPS", format!("{}", w.total_mvps)),
            ("ASSISTS", format!("{}", w.total_assists)),
        ];
        let col_step = bot_rect.width() / (peak_items.len() as f32);
        for (i, (p_lbl, p_val)) in peak_items.iter().enumerate() {
            let cx = bot_rect.min.x + ((i as f32) + 0.5) * col_step;
            p.text(
                Pos2::new(cx, bot_rect.min.y + 24.0),
                egui::Align2::CENTER_CENTER,
                *p_lbl,
                FontId::proportional(10.0),
                Color32::from_rgb(148, 163, 184),
            );
            p.text(
                Pos2::new(cx, bot_rect.min.y + 50.0),
                egui::Align2::CENTER_CENTER,
                p_val,
                FontId::proportional(18.0),
                Color32::WHITE,
            );
        }
    }


}

pub(crate) fn export_wrapped_card_png(
    w: &YearWrappedSummary,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    let _ = std::fs::create_dir_all(output_dir);
    let width = 800u32;
    let height = 1000u32;
    let mut img = image::RgbaImage::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let t = y as f32 / height as f32;
            let r = (10.0 * (1.0 - t) + 14.0 * t) as u8;
            let g = (14.0 * (1.0 - t) + 20.0 * t) as u8;
            let b = (24.0 * (1.0 - t) + 36.0 * t) as u8;
            img.put_pixel(x, y, image::Rgba([r, g, b, 255]));
        }
    }

    let fill_rect = |img: &mut image::RgbaImage, x0: u32, y0: u32, rw: u32, rh: u32, col: [u8; 4]| {
        for yy in y0..(y0 + rh).min(height) {
            for xx in x0..(x0 + rw).min(width) {
                img.put_pixel(xx, yy, image::Rgba(col));
            }
        }
    };

    fill_rect(&mut img, 0, 0, width, 8, [59, 130, 246, 255]);
    fill_rect(&mut img, 32, 40, width - 64, 180, [18, 25, 42, 255]);
    fill_rect(&mut img, 32, 244, width - 64, 220, [16, 20, 30, 255]);
    fill_rect(&mut img, 32, 488, width - 64, 240, [16, 20, 30, 255]);
    fill_rect(&mut img, 32, 752, width - 64, 190, [16, 20, 30, 255]);

    let hs_w = (((width - 140) as f32) * ((w.headshot_pct as f32) / 100.0).clamp(0.0, 1.0)) as u32;
    let bs_w = (((width - 140) as f32) * ((w.bodyshot_pct as f32) / 100.0).clamp(0.0, 1.0)) as u32;
    let ls_w = (((width - 140) as f32) * ((w.legshot_pct as f32) / 100.0).clamp(0.0, 1.0)) as u32;

    fill_rect(&mut img, 64, 540, width - 140, 22, [28, 34, 48, 255]);
    fill_rect(&mut img, 64, 540, hs_w.max(6), 22, [59, 130, 246, 255]);

    fill_rect(&mut img, 64, 600, width - 140, 22, [28, 34, 48, 255]);
    fill_rect(&mut img, 64, 600, bs_w.max(6), 22, [56, 189, 248, 255]);

    fill_rect(&mut img, 64, 660, width - 140, 22, [28, 34, 48, 255]);
    fill_rect(&mut img, 64, 660, ls_w.max(6), 22, [113, 113, 122, 255]);

    let clean_game = w
        .game_filter
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let file_name = format!("Clotch_Wrapped_{}_{}.png", w.year.max(2026), clean_game);
    let out_path = output_dir.join(file_name);
    img.save(&out_path)
        .map_err(|e| format!("Failed to write PNG: {e}"))?;
    Ok(out_path)
}

