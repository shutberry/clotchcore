#![allow(dead_code, unused_imports)]
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

#[derive(Clone, Debug, Default)]
pub(crate) struct MatchRecordModalState {
    pub(crate) open: bool,
    pub(crate) editing_match_id: Option<String>,
    pub(crate) game: String,
    pub(crate) map: String,
    pub(crate) mode: String,
    pub(crate) hero_or_team: String,
    pub(crate) favorite_weapon: String,
    pub(crate) kills_str: String,
    pub(crate) deaths_str: String,
    pub(crate) assists_str: String,
    pub(crate) headshots_str: String,
    pub(crate) bodyshots_str: String,
    pub(crate) legshots_str: String,
    pub(crate) aces_str: String,
    pub(crate) clutches_str: String,
    pub(crate) multikills_str: String,
    pub(crate) goals_str: String,
    pub(crate) saves_str: String,
    pub(crate) score_for_str: String,
    pub(crate) score_against_str: String,
    pub(crate) won_choice: usize,
}



impl ClotchGuiApp {
    pub(crate) fn render_match_record_modal(
        &mut self,
        ui: &mut egui::Ui,
        c: ThemeColors,
        trigger_refresh: &mut bool,
    ) {
        let mut close_modal = false;
        let mut save_modal = false;

        egui::Frame::none()
            .fill(c.surface)
            .stroke(Stroke::new(1.0_f32, c.accent))
            .rounding(Rounding::same(6.0))
            .inner_margin(Margin::symmetric(16.0, 14.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if self.match_modal.editing_match_id.is_some() {
                            "Edit Recorded Match Telemetry"
                        } else {
                            "Log Local Match Telemetry"
                        })
                        .size(14.0)
                        .strong()
                        .color(c.text),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if draw_ghost_button(ui, "Close", false, c).clicked() {
                            close_modal = true;
                        }
                    });
                });
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.spacing_mut().item_spacing.y = 8.0;

                    ui.label(RichText::new("Game:").size(12.0).color(c.text_2));
                    ui.add_sized(
                        [120.0, 24.0],
                        egui::TextEdit::singleline(&mut self.match_modal.game)
                            .hint_text("Valorant / CS2 / RL"),
                    );

                    ui.label(RichText::new("Map:").size(12.0).color(c.text_2));
                    ui.add_sized(
                        [110.0, 24.0],
                        egui::TextEdit::singleline(&mut self.match_modal.map)
                            .hint_text("Ascent / Mirage"),
                    );

                    let role_lbl = role_column_label_for_game(&self.match_modal.game);
                    ui.label(RichText::new(format!("{role_lbl}:")).size(12.0).color(c.text_2));
                    ui.add_sized(
                        [110.0, 24.0],
                        egui::TextEdit::singleline(&mut self.match_modal.hero_or_team)
                            .hint_text("Jett / CT / Octane"),
                    );

                    ui.label(RichText::new("Weapon:").size(12.0).color(c.text_2));
                    ui.add_sized(
                        [110.0, 24.0],
                        egui::TextEdit::singleline(&mut self.match_modal.favorite_weapon)
                            .hint_text("Vandal / AK-47"),
                    );
                });

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    for (lbl, buf) in [
                        ("Kills:", &mut self.match_modal.kills_str),
                        ("Deaths:", &mut self.match_modal.deaths_str),
                        ("Assists:", &mut self.match_modal.assists_str),
                        ("Headshots:", &mut self.match_modal.headshots_str),
                        ("Bodyshots:", &mut self.match_modal.bodyshots_str),
                        ("Legshots:", &mut self.match_modal.legshots_str),
                        ("Clutches:", &mut self.match_modal.clutches_str),
                        ("Aces:", &mut self.match_modal.aces_str),
                        ("Goals:", &mut self.match_modal.goals_str),
                        ("Saves:", &mut self.match_modal.saves_str),
                    ] {
                        ui.label(RichText::new(lbl).size(12.0).color(c.text_2));
                        ui.add_sized([46.0, 24.0], egui::TextEdit::singleline(buf));
                    }
                });

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.label(RichText::new("Score For:").size(12.0).color(c.text_2));
                    ui.add_sized(
                        [46.0, 24.0],
                        egui::TextEdit::singleline(&mut self.match_modal.score_for_str),
                    );
                    ui.label(RichText::new("Against:").size(12.0).color(c.text_2));
                    ui.add_sized(
                        [46.0, 24.0],
                        egui::TextEdit::singleline(&mut self.match_modal.score_against_str),
                    );
                    ui.label(RichText::new("Outcome:").size(12.0).color(c.text_2));
                    for (idx, txt) in [(0usize, "Auto"), (1usize, "Win"), (2usize, "Loss")] {
                        if draw_ghost_button(ui, txt, self.match_modal.won_choice == idx, c)
                            .clicked()
                        {
                            self.match_modal.won_choice = idx;
                        }
                    }
                    if draw_primary_button(ui, "Save Match Telemetry", true, c).clicked() {
                        save_modal = true;
                    }
                });
            });

        if close_modal {
            self.match_modal.open = false;
        }
        if save_modal {
            let game_clean = self.match_modal.game.trim().to_lowercase();
            if game_clean.is_empty() {
                self.show_toast("Please enter the game name", None);
                return;
            }
            let p_u32 = |s: &str| s.trim().parse::<u32>().unwrap_or(0);
            let p_i64 = |s: &str| s.trim().parse::<i64>().ok();
            let rec = MatchStatsRecord {
                match_id: self.match_modal.editing_match_id.clone().unwrap_or_default(),
                game: game_clean,
                map: if self.match_modal.map.trim().is_empty() {
                    "Standard".to_string()
                } else {
                    self.match_modal.map.trim().to_string()
                },
                mode: if self.match_modal.mode.trim().is_empty() {
                    "Competitive".to_string()
                } else {
                    self.match_modal.mode.trim().to_string()
                },
                hero_or_team: self.match_modal.hero_or_team.trim().to_string(),
                started_at_ms: 0,
                ended_at_ms: 0,
                duration_secs: 1800,
                kills: p_u32(&self.match_modal.kills_str),
                deaths: p_u32(&self.match_modal.deaths_str),
                assists: p_u32(&self.match_modal.assists_str),
                headshots: p_u32(&self.match_modal.headshots_str),
                bodyshots: p_u32(&self.match_modal.bodyshots_str),
                legshots: p_u32(&self.match_modal.legshots_str),
                damage_dealt: 0,
                combat_score: 0,
                shots_fired: 0,
                favorite_weapon: self.match_modal.favorite_weapon.trim().to_string(),
                player_identity: String::new(),
                rank_or_tier: String::new(),
                multikills: p_u32(&self.match_modal.multikills_str),
                aces: p_u32(&self.match_modal.aces_str),
                clutches: p_u32(&self.match_modal.clutches_str),
                mvps: 0,
                goals: p_u32(&self.match_modal.goals_str),
                goals_conceded: 0,
                saves: p_u32(&self.match_modal.saves_str),
                objectives: 0,
                won: match self.match_modal.won_choice {
                    1 => Some(true),
                    2 => Some(false),
                    _ => None,
                },
                score_for: p_i64(&self.match_modal.score_for_str),
                score_against: p_i64(&self.match_modal.score_against_str),
                kd_ratio: 0.0,
            };
            if self.bridge.save_custom_match_stats(rec).is_ok() {
                self.match_modal.open = false;
                *trigger_refresh = true;
                self.show_toast("Match telemetry saved locally", None);
            }
        }
    }


    pub(crate) fn render_tracker_telemetry_view(
        &self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        c: ThemeColors,
        edit_match_rec: &mut Option<MatchStatsRecord>,
        delete_match_id: &mut Option<String>,
        toggle_expand_match: &mut Option<String>,
        open_clip_id_from_match: &mut Option<u64>,
    ) {
        let sum = &self.stats_summary;
        if sum.total_matches == 0 {
            egui::Frame::none()
                .fill(c.surface)
                .stroke(Stroke::new(1.0_f32, c.border))
                .rounding(Rounding::same(8.0))
                .inner_margin(Margin::symmetric(22.0, 24.0))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("Waiting for Verified Match Telemetry")
                            .size(16.0)
                            .strong()
                            .color(c.text),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Empty map-load stubs and unverified pixel guesses have been cleaned. To populate your real match history:\n\
                             • For Valorant: Keep Riot Client open and click 'Sync Game Files' above (or finish any match) to import your official Riot match scoreboards (Agent, K/D/A, ACS, Headshot % / Bodyshot % / Legshot %, Weapon, Score).\n\
                             • For CS2, LoL, Dota 2, Rocket League, Apex, R6 Siege: Play a match and Clotch records the official GSI / LiveClientData / Replay scoreboard automatically.",
                        )
                        .size(12.5)
                        .color(c.text_2),
                    );
                });
            return;
        }

        let is_sports_focus = matches!(
            self.stats_game_filter.to_lowercase().as_str(),
            "rocket_league" | "rocket league" | "fc26" | "fc27" | "efootball"
        );
        let role_title = role_column_label_for_game(&self.stats_game_filter);
        let total_verified_hits = sum.total_headshots + sum.total_bodyshots + sum.total_legshots;

        let (trn_score, trn_tier, trn_color) = compute_tracker_rating(sum);
        let banner_w = ui.available_width();
        let (b_rect, _) = ui.allocate_exact_size(Vec2::new(banner_w, 68.0), Sense::hover());
        ui.painter().rect(
            b_rect,
            Rounding::same(8.0),
            c.surface,
            Stroke::new(1.0_f32, c.border_strong),
        );
        ui.painter().rect_filled(
            Rect::from_min_size(b_rect.min, Vec2::new(5.0, b_rect.height())),
            Rounding {
                nw: 8.0,
                sw: 8.0,
                ne: 0.0,
                se: 0.0,
            },
            trn_color,
        );
        let badge_rect = Rect::from_center_size(
            Pos2::new(b_rect.min.x + 42.0, b_rect.center().y),
            Vec2::new(44.0, 44.0),
        );
        ui.painter().rect(
            badge_rect,
            Rounding::same(8.0),
            Color32::from_rgba_unmultiplied(trn_color.r(), trn_color.g(), trn_color.b(), 32),
            Stroke::new(1.5_f32, trn_color),
        );
        ui.painter().text(
            badge_rect.center(),
            egui::Align2::CENTER_CENTER,
            trn_tier,
            FontId::proportional(17.0),
            trn_color,
        );
        ui.painter().text(
            Pos2::new(b_rect.min.x + 76.0, b_rect.min.y + 23.0),
            egui::Align2::LEFT_CENTER,
            format!("TRACKER RATING · {trn_score} / 1000"),
            FontId::proportional(13.5),
            c.text,
        );
        let kpm = (sum.total_kills as f64) / (sum.total_matches.max(1) as f64);
        ui.painter().text(
            Pos2::new(b_rect.min.x + 76.0, b_rect.min.y + 45.0),
            egui::Align2::LEFT_CENTER,
            format!(
                "{} Verified Matches   ·   {:.1} Kills/Match   ·   {} Clutches   ·   {} Aces",
                sum.total_matches, kpm, sum.total_clutches, sum.total_aces
            ),
            FontId::proportional(12.0),
            c.text_2,
        );

        let mut streak_x = b_rect.max.x - 18.0;
        for rec in sum.recent_matches.iter().take(10).rev() {
            let (s_ch, s_col) = match rec.won {
                Some(true) => ("W", c.success),
                Some(false) => ("L", c.danger),
                None => ("M", c.text_3),
            };
            let pill_r = Rect::from_center_size(
                Pos2::new(streak_x - 11.0, b_rect.center().y),
                Vec2::new(22.0, 22.0),
            );
            if pill_r.min.x > b_rect.min.x + 420.0 {
                ui.painter().rect(
                    pill_r,
                    Rounding::same(4.0),
                    Color32::from_rgba_unmultiplied(s_col.r(), s_col.g(), s_col.b(), 30),
                    Stroke::new(1.0_f32, s_col),
                );
                ui.painter().text(
                    pill_r.center(),
                    egui::Align2::CENTER_CENTER,
                    s_ch,
                    FontId::proportional(11.0),
                    s_col,
                );
                streak_x -= 26.0;
            }
        }

        ui.add_space(12.0);

        let mut cards: Vec<(&str, String, String, Color32)> = Vec::new();
        cards.push((
            "MATCHES",
            format!("{}", sum.total_matches),
            format!("{}W - {}L ({:.1}h)", sum.wins, sum.losses, (sum.total_duration_secs as f64) / 3600.0),
            c.accent,
        ));
        cards.push((
            "WIN %",
            if sum.wins + sum.losses > 0 {
                format!("{:.1}%", sum.win_rate_pct)
            } else {
                "—".to_string()
            },
            format!("{} Wins / {} Losses", sum.wins, sum.losses),
            if sum.win_rate_pct >= 50.0 { c.success } else { c.accent },
        ));
        if is_sports_focus {
            cards.push((
                "GOALS",
                format!("{}", sum.total_goals),
                format!("{} Assists", sum.total_assists),
                c.accent,
            ));
            cards.push((
                "SAVES",
                format!("{}", sum.total_saves),
                format!("{} Hat-Tricks", sum.total_aces),
                c.success,
            ));
        } else {
            cards.push((
                "K/D RATIO",
                format!("{:.2}", sum.overall_kd),
                format!("{}K / {}D / {}A", sum.total_kills, sum.total_deaths, sum.total_assists),
                if sum.overall_kd >= 1.0 { c.success } else { c.accent },
            ));
            cards.push((
                "KILLS / MATCH",
                format!("{kpm:.1}"),
                format!("{} Total Kills", sum.total_kills),
                c.accent,
            ));
            cards.push((
                "HEADSHOT %",
                if total_verified_hits > 0 {
                    format!("{:.1}%", sum.headshot_pct)
                } else {
                    "—".to_string()
                },
                format!("{} Headshots", sum.total_headshots),
                Color32::from_rgb(56, 189, 248),
            ));
            cards.push((
                "BODY / LEG %",
                if total_verified_hits > 0 {
                    format!("{:.0}% / {:.0}%", sum.bodyshot_pct, sum.legshot_pct)
                } else {
                    "—".to_string()
                },
                if sum.avg_combat_score > 0.0 {
                    format!("Avg Score: {:.0} ACS", sum.avg_combat_score)
                } else {
                    format!("{} Multikills", sum.total_multikills)
                },
                c.accent,
            ));
        }
        cards.push((
            role_title,
            if sum.favorite_hero_or_team.is_empty() {
                "—".to_string()
            } else {
                sum.favorite_hero_or_team.clone()
            },
            if !sum.favorite_weapon.is_empty() {
                format!("Weapon: {}", sum.favorite_weapon)
            } else if !sum.favorite_map.is_empty() {
                format!("Best Map: {}", sum.favorite_map)
            } else {
                "Verified Local".to_string()
            },
            c.accent,
        ));
        cards.push((
            "CLUTCHES & ACES",
            format!("{}C / {}A", sum.total_clutches, sum.total_aces),
            format!("{} Multikills · {} MVPs", sum.total_multikills, sum.total_mvps),
            Color32::from_rgb(96, 165, 250),
        ));

        let avail_w = ui.available_width();
        let cols = if avail_w >= 1060.0 {
            cards.len().min(8)
        } else if avail_w >= 680.0 {
            4
        } else {
            2
        }
        .max(1);
        let gap = 10.0f32;
        let card_w = ((avail_w - gap * ((cols - 1) as f32)) / (cols as f32)).max(110.0);

        for row in cards.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (label, val, sub, bar_col) in row {
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::new(card_w, 78.0), Sense::hover());
                    ui.painter().rect(
                        rect,
                        Rounding::same(6.0),
                        c.surface,
                        Stroke::new(1.0_f32, c.border),
                    );
                    ui.painter().line_segment(
                        [
                            Pos2::new(rect.min.x + 10.0, rect.max.y - 2.0),
                            Pos2::new(rect.max.x - 10.0, rect.max.y - 2.0),
                        ],
                        Stroke::new(2.0_f32, *bar_col),
                    );
                    ui.painter().text(
                        Pos2::new(rect.min.x + 12.0, rect.min.y + 17.0),
                        egui::Align2::LEFT_CENTER,
                        *label,
                        FontId::proportional(10.5),
                        c.text_3,
                    );
                    ui.painter().text(
                        Pos2::new(rect.min.x + 12.0, rect.min.y + 41.0),
                        egui::Align2::LEFT_CENTER,
                        val,
                        FontId::proportional(18.5),
                        c.text,
                    );
                    ui.painter().text(
                        Pos2::new(rect.min.x + 12.0, rect.min.y + 61.0),
                        egui::Align2::LEFT_CENTER,
                        sub,
                        FontId::proportional(10.8),
                        c.text_2,
                    );
                }
            });
            ui.add_space(10.0);
        }

        ui.add_space(4.0);

        if !is_sports_focus {
            let col3_w = ((avail_w - 20.0) / 3.0).max(240.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;

                let (hit_rect, _) =
                    ui.allocate_exact_size(Vec2::new(col3_w, 174.0), Sense::hover());
                ui.painter().rect(
                    hit_rect,
                    Rounding::same(6.0),
                    c.surface,
                    Stroke::new(1.0_f32, c.border),
                );
                ui.painter().text(
                    Pos2::new(hit_rect.min.x + 14.0, hit_rect.min.y + 20.0),
                    egui::Align2::LEFT_CENTER,
                    "HIT ACCURACY (HEAD / BODY / LEGS)",
                    FontId::proportional(11.5),
                    c.text_2,
                );

                let sil_center = Pos2::new(hit_rect.min.x + 48.0, hit_rect.min.y + 102.0);
                draw_hit_silhouette_vector(
                    ui.painter(),
                    sil_center,
                    sum.headshot_pct,
                    sum.bodyshot_pct,
                    sum.legshot_pct,
                    c,
                );

                let bars_x = hit_rect.min.x + 96.0;
                let bars_w = (hit_rect.max.x - bars_x - 14.0).max(60.0);
                for (idx, (z_name, pct, cnt, col)) in [
                    ("Head", sum.headshot_pct, sum.total_headshots, c.accent),
                    (
                        "Body",
                        sum.bodyshot_pct,
                        sum.total_bodyshots,
                        Color32::from_rgb(56, 189, 248),
                    ),
                    ("Legs", sum.legshot_pct, sum.total_legshots, c.text_3),
                ]
                .into_iter()
                .enumerate()
                {
                    let by = hit_rect.min.y + 54.0 + (idx as f32) * 38.0;
                    ui.painter().text(
                        Pos2::new(bars_x, by),
                        egui::Align2::LEFT_CENTER,
                        format!("{z_name} ({cnt})"),
                        FontId::proportional(11.5),
                        c.text_2,
                    );
                    let pct_str = if total_verified_hits > 0 {
                        format!("{pct:.1}%")
                    } else {
                        "—".to_string()
                    };
                    ui.painter().text(
                        Pos2::new(bars_x + bars_w, by),
                        egui::Align2::RIGHT_CENTER,
                        pct_str,
                        FontId::proportional(12.0),
                        c.text,
                    );
                    let bar_bg = Rect::from_min_size(
                        Pos2::new(bars_x, by + 10.0),
                        Vec2::new(bars_w, 6.0),
                    );
                    ui.painter()
                        .rect_filled(bar_bg, Rounding::same(3.0), c.surface_2);
                    if total_verified_hits > 0 {
                        let fill_w = (bars_w * ((pct as f32) / 100.0).clamp(0.0, 1.0)).max(2.0);
                        ui.painter().rect_filled(
                            Rect::from_min_size(bar_bg.min, Vec2::new(fill_w, 6.0)),
                            Rounding::same(3.0),
                            col,
                        );
                    }
                }

                let (trend_rect, _) =
                    ui.allocate_exact_size(Vec2::new(col3_w, 174.0), Sense::hover());
                ui.painter().rect(
                    trend_rect,
                    Rounding::same(6.0),
                    c.surface,
                    Stroke::new(1.0_f32, c.border),
                );
                ui.painter().text(
                    Pos2::new(trend_rect.min.x + 14.0, trend_rect.min.y + 20.0),
                    egui::Align2::LEFT_CENTER,
                    "RECENT MATCH K/D TREND",
                    FontId::proportional(11.5),
                    c.text_2,
                );
                draw_kd_sparkline_chart(ui.painter(), trend_rect, &sum.recent_matches, c);

                let (role_rect, _) =
                    ui.allocate_exact_size(Vec2::new(col3_w, 174.0), Sense::hover());
                ui.painter().rect(
                    role_rect,
                    Rounding::same(6.0),
                    c.surface,
                    Stroke::new(1.0_f32, c.border),
                );
                ui.painter().text(
                    Pos2::new(role_rect.min.x + 14.0, role_rect.min.y + 20.0),
                    egui::Align2::LEFT_CENTER,
                    format!("{role_title} & MAP MASTERY"),
                    FontId::proportional(11.5),
                    c.text_2,
                );

                let mut merged_heroes: HashMap<String, u32> = HashMap::new();
                let mut merged_maps: HashMap<String, u32> = HashMap::new();
                for (g_k, g_st) in &sum.per_game {
                    if self.stats_game_filter != "All"
                        && !g_k.eq_ignore_ascii_case(&self.stats_game_filter)
                        && !pretty_game_title(g_k)
                            .eq_ignore_ascii_case(&pretty_game_title(&self.stats_game_filter))
                    {
                        continue;
                    }
                    for (h_k, h_c) in &g_st.hero_counts {
                        if !h_k.is_empty() {
                            *merged_heroes.entry(h_k.clone()).or_insert(0) += *h_c;
                        }
                    }
                    for (m_k, m_c) in &g_st.map_counts {
                        if !m_k.is_empty() && m_k != "Unknown" && m_k != "Standard" {
                            *merged_maps.entry(m_k.clone()).or_insert(0) += *m_c;
                        }
                    }
                }
                let mut top_items: Vec<(String, u32)> = if !merged_heroes.is_empty() {
                    merged_heroes.into_iter().collect()
                } else {
                    merged_maps.into_iter().collect()
                };
                top_items.sort_by(|a, b| b.1.cmp(&a.1));

                for (idx, (item_name, item_cnt)) in top_items.iter().take(3).enumerate() {
                    let ry = role_rect.min.y + 54.0 + (idx as f32) * 38.0;
                    let pct = ((*item_cnt as f32 / (sum.total_matches.max(1) as f32)) * 100.0)
                        .clamp(0.0, 100.0);
                    ui.painter().text(
                        Pos2::new(role_rect.min.x + 14.0, ry),
                        egui::Align2::LEFT_CENTER,
                        format!("{item_name} ({item_cnt} matches)"),
                        FontId::proportional(11.8),
                        c.text,
                    );
                    ui.painter().text(
                        Pos2::new(role_rect.max.x - 14.0, ry),
                        egui::Align2::RIGHT_CENTER,
                        format!("{pct:.0}%"),
                        FontId::proportional(11.5),
                        c.text_2,
                    );
                    let b_w = (role_rect.width() - 28.0).max(60.0);
                    let b_rect = Rect::from_min_size(
                        Pos2::new(role_rect.min.x + 14.0, ry + 10.0),
                        Vec2::new(b_w, 5.5),
                    );
                    ui.painter()
                        .rect_filled(b_rect, Rounding::same(2.5), c.surface_2);
                    ui.painter().rect_filled(
                        Rect::from_min_size(
                            b_rect.min,
                            Vec2::new((b_w * (pct / 100.0)).max(2.0), 5.5),
                        ),
                        Rounding::same(2.5),
                        c.accent,
                    );
                }
            });
            ui.add_space(16.0);
        }

        section_title(
            ui,
            "Match History Scoreboard (Click any match to inspect round telemetry & linked video clips)",
            c,
            false,
        );
        ui.add_space(8.0);

        for rec in &sum.recent_matches {
            let is_expanded = self.expanded_match_id.as_deref() == Some(rec.match_id.as_str());
            let (res_txt, res_col) = match rec.won {
                Some(true) => ("WIN", c.success),
                Some(false) => ("LOSS", c.danger),
                None => ("MATCH", c.accent),
            };

            let linked_clips: Vec<&ClipItem> = self
                .clips
                .iter()
                .filter(|cl| {
                    let same_game = cl.game.eq_ignore_ascii_case(&rec.game)
                        || pretty_game_title(&cl.game)
                            .eq_ignore_ascii_case(&pretty_game_title(&rec.game));
                    if !same_game {
                        return false;
                    }
                    let clip_ms = cl.mtime_secs * 1000;
                    let start_win = rec.started_at_ms.saturating_sub(300_000);
                    let end_win = rec.ended_at_ms.saturating_add(600_000);
                    (clip_ms >= start_win && clip_ms <= end_win)
                        || (!rec.map.is_empty()
                            && rec.map != "Standard"
                            && cl.map.eq_ignore_ascii_case(&rec.map))
                })
                .take(6)
                .collect();

            egui::Frame::none()
                .fill(c.surface)
                .stroke(Stroke::new(
                    1.0_f32,
                    if is_expanded { c.accent } else { c.border },
                ))
                .rounding(Rounding::same(6.0))
                .inner_margin(Margin::symmetric(14.0, 10.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;

                        let (bar_r, _) =
                            ui.allocate_exact_size(Vec2::new(4.0, 22.0), Sense::hover());
                        ui.painter()
                            .rect_filled(bar_r, Rounding::same(2.0), res_col);

                        let score_str = match (rec.score_for, rec.score_against) {
                            (Some(sf), Some(sa)) => format!("{res_txt} {sf}:{sa}"),
                            _ => res_txt.to_string(),
                        };
                        ui.label(RichText::new(score_str).size(12.5).strong().color(res_col));

                        let g_pretty = pretty_game_title(&rec.game);
                        let map_click = ui.add(
                            egui::Label::new(
                                RichText::new(format!("{g_pretty} · {}", rec.map))
                                    .size(13.5)
                                    .strong()
                                    .color(c.text),
                            )
                            .sense(Sense::click()),
                        );
                        if map_click.hovered() {
                            ctx.set_cursor_icon(CursorIcon::PointingHand);
                        }
                        if map_click.clicked() {
                            *toggle_expand_match = Some(rec.match_id.clone());
                        }

                        if !rec.hero_or_team.is_empty() && rec.hero_or_team != "Agent" {
                            egui::Frame::none()
                                .fill(c.surface_2)
                                .stroke(Stroke::new(1.0_f32, c.border_strong))
                                .rounding(Rounding::same(4.0))
                                .inner_margin(Margin::symmetric(7.0, 2.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(&rec.hero_or_team)
                                            .size(11.5)
                                            .color(c.accent),
                                    );
                                });
                        }
                        if !rec.favorite_weapon.is_empty() {
                            ui.label(
                                RichText::new(&rec.favorite_weapon)
                                    .size(11.5)
                                    .color(c.text_3),
                            );
                        }
                        if !linked_clips.is_empty() {
                            egui::Frame::none()
                                .fill(c.accent_soft)
                                .rounding(Rounding::same(4.0))
                                .inner_margin(Margin::symmetric(6.0, 2.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(format!("{} clips", linked_clips.len()))
                                            .size(11.0)
                                            .color(c.accent),
                                    );
                                });
                        }

                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if draw_ghost_button(ui, "Delete", false, c).clicked() {
                                *delete_match_id = Some(rec.match_id.clone());
                            }
                            if draw_ghost_button(ui, "Edit", false, c).clicked() {
                                *edit_match_rec = Some(rec.clone());
                            }
                            if draw_ghost_button(
                                ui,
                                if is_expanded { "Hide" } else { "Details" },
                                is_expanded,
                                c,
                            )
                            .clicked()
                            {
                                *toggle_expand_match = Some(rec.match_id.clone());
                            }

                            let tot_hits = rec.headshots + rec.bodyshots + rec.legshots;
                            let hs_str = if tot_hits > 0 {
                                let hs_p = (rec.headshots as f64 / tot_hits as f64) * 100.0;
                                format!("   ·   HS {hs_p:.0}%")
                            } else {
                                String::new()
                            };
                            let acs_str = if rec.combat_score > 0 {
                                format!("   ·   {} ACS", rec.combat_score)
                            } else {
                                String::new()
                            };

                            let metrics_str = if rec.goals > 0 || rec.saves > 0 {
                                format!(
                                    "{} Goals · {} Saves · {} Assists",
                                    rec.goals, rec.saves, rec.assists
                                )
                            } else {
                                format!(
                                    "{} / {} / {} (K/D {:.2}){}{}   ·   {}C / {}A",
                                    rec.kills,
                                    rec.deaths,
                                    rec.assists,
                                    rec.kd_ratio,
                                    acs_str,
                                    hs_str,
                                    rec.clutches,
                                    rec.aces
                                )
                            };
                            ui.label(RichText::new(metrics_str).size(12.0).color(c.text_2));
                        });
                    });

                    if is_expanded {
                        ui.add_space(10.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 18.0;
                            ui.label(
                                RichText::new(format!("Mode: {}", rec.mode))
                                    .size(12.0)
                                    .color(c.text_2),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "Duration: {}m {}s",
                                    rec.duration_secs / 60,
                                    rec.duration_secs % 60
                                ))
                                .size(12.0)
                                .color(c.text_2),
                            );
                            if rec.damage_dealt > 0 {
                                ui.label(
                                    RichText::new(format!("Total Damage: {}", rec.damage_dealt))
                                        .size(12.0)
                                        .color(c.text_2),
                                );
                            }
                            let tot_hits = rec.headshots + rec.bodyshots + rec.legshots;
                            if tot_hits > 0 {
                                ui.label(
                                    RichText::new(format!(
                                        "Hits: {} Head · {} Body · {} Legs",
                                        rec.headshots, rec.bodyshots, rec.legshots
                                    ))
                                    .size(12.0)
                                    .color(c.text),
                                );
                            }
                        });

                        if !linked_clips.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new("Recorded Video Highlights from this Match / Map:")
                                    .size(11.5)
                                    .color(c.text_3),
                            );
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                for cl in linked_clips {
                                    if draw_ghost_button(
                                        ui,
                                        &format!("Play: {} ({})", cl.display_title, cl.duration_label),
                                        false,
                                        c,
                                    )
                                    .clicked()
                                    {
                                        *open_clip_id_from_match = Some(cl.id);
                                    }
                                }
                            });
                        }
                    }
                });
            ui.add_space(6.0);
        }
    }


}

pub(crate) fn check_riot_lockfile_exists() -> bool {
    let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") else {
        return false;
    };
    let p = PathBuf::from(local_appdata).join("Riot Games/Riot Client/Config/lockfile");
    p.is_file()
}

