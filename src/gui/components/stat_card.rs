#![allow(dead_code, unused_imports)]
use crate::stats::MatchStatsRecord;
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Rounding, Stroke, Vec2};
use crate::stats::MonthlyStatsSummary;
use crate::gui::theme::ThemeColors;

pub fn draw_hit_silhouette_vector(
    p: &egui::Painter,
    center: Pos2,
    hs_pct: f64,
    bs_pct: f64,
    ls_pct: f64,
    c: ThemeColors,
) {
    let head_alpha = ((hs_pct as f32 / 50.0).clamp(0.25, 1.0) * 255.0) as u8;
    let body_alpha = ((bs_pct as f32 / 75.0).clamp(0.25, 1.0) * 255.0) as u8;
    let leg_alpha = ((ls_pct as f32 / 40.0).clamp(0.20, 1.0) * 255.0) as u8;

    let head_col = Color32::from_rgba_unmultiplied(c.accent.r(), c.accent.g(), c.accent.b(), head_alpha);
    let body_col = Color32::from_rgba_unmultiplied(56, 189, 248, body_alpha);
    let leg_col = Color32::from_rgba_unmultiplied(148, 163, 184, leg_alpha);

    p.circle_filled(Pos2::new(center.x, center.y - 38.0), 10.5, head_col);
    p.circle_stroke(
        Pos2::new(center.x, center.y - 38.0),
        10.5,
        Stroke::new(1.2_f32, Color32::WHITE),
    );

    let torso = Rect::from_center_size(Pos2::new(center.x, center.y - 6.0), Vec2::new(26.0, 34.0));
    p.rect_filled(torso, Rounding::same(5.0), body_col);
    p.rect_stroke(torso, Rounding::same(5.0), Stroke::new(1.0_f32, Color32::from_rgb(148, 163, 184)));

    let left_arm = Rect::from_min_size(Pos2::new(torso.min.x - 7.0, torso.min.y + 2.0), Vec2::new(5.5, 28.0));
    let right_arm = Rect::from_min_size(Pos2::new(torso.max.x + 1.5, torso.min.y + 2.0), Vec2::new(5.5, 28.0));
    p.rect_filled(left_arm, Rounding::same(2.5), body_col);
    p.rect_filled(right_arm, Rounding::same(2.5), body_col);

    let left_leg = Rect::from_min_size(Pos2::new(center.x - 11.0, torso.max.y + 3.0), Vec2::new(9.5, 32.0));
    let right_leg = Rect::from_min_size(Pos2::new(center.x + 1.5, torso.max.y + 3.0), Vec2::new(9.5, 32.0));
    p.rect_filled(left_leg, Rounding::same(3.0), leg_col);
    p.rect_filled(right_leg, Rounding::same(3.0), leg_col);
}


pub fn compute_tracker_rating(sum: &MonthlyStatsSummary) -> (u32, &'static str, Color32) {
    let kd_score = (sum.overall_kd * 260.0).clamp(80.0, 420.0);
    let wr_score = if sum.wins + sum.losses > 0 {
        (sum.win_rate_pct * 3.2).clamp(40.0, 260.0)
    } else {
        150.0
    };
    let hs_score = (sum.headshot_pct * 3.5).clamp(0.0, 160.0);
    let impact_score = ((sum.total_clutches * 18 + sum.total_aces * 35 + sum.total_multikills * 8) as f64)
        .clamp(0.0, 160.0);
    let raw = (kd_score + wr_score + hs_score + impact_score).round() as u32;
    let score = raw.clamp(120, 999);
    if score >= 820 {
        (score, "S+", Color32::from_rgb(56, 189, 248))
    } else if score >= 680 {
        (score, "S", Color32::from_rgb(34, 197, 94))
    } else if score >= 520 {
        (score, "A", Color32::from_rgb(59, 130, 246))
    } else if score >= 380 {
        (score, "B", Color32::from_rgb(245, 158, 11))
    } else {
        (score, "C", Color32::from_rgb(156, 163, 175))
    }
}

pub fn draw_kd_sparkline_chart(
    p: &egui::Painter,
    rect: Rect,
    matches: &[MatchStatsRecord],
    c: ThemeColors,
) {
    let plot_rect = Rect::from_min_max(
        Pos2::new(rect.min.x + 16.0, rect.min.y + 44.0),
        Pos2::new(rect.max.x - 16.0, rect.max.y - 20.0),
    );
    let mid_y = plot_rect.center().y;
    p.line_segment(
        [
            Pos2::new(plot_rect.min.x, mid_y),
            Pos2::new(plot_rect.max.x, mid_y),
        ],
        Stroke::new(1.0_f32, c.border_strong),
    );
    p.text(
        Pos2::new(plot_rect.min.x, mid_y - 8.0),
        egui::Align2::LEFT_CENTER,
        "1.00 K/D",
        FontId::proportional(9.5),
        c.text_3,
    );

    let pts_src: Vec<&MatchStatsRecord> = matches.iter().take(12).rev().collect();
    if pts_src.is_empty() {
        return;
    }
    let n = pts_src.len();
    let mut points = Vec::with_capacity(n);
    for (i, m) in pts_src.iter().enumerate() {
        let t = if n <= 1 {
            0.5
        } else {
            (i as f32) / ((n - 1) as f32)
        };
        let x = plot_rect.min.x + t * plot_rect.width();
        let norm_kd = ((m.kd_ratio as f32) / 2.5).clamp(0.05, 1.0);
        let y = plot_rect.max.y - norm_kd * plot_rect.height();
        points.push((Pos2::new(x, y), m.won, m.kd_ratio));
    }

    for win in points.windows(2) {
        p.line_segment([win[0].0, win[1].0], Stroke::new(2.0_f32, c.accent));
    }
    for (pt, won, _) in &points {
        let dot_col = match won {
            Some(true) => c.success,
            Some(false) => c.danger,
            None => c.accent,
        };
        p.circle_filled(*pt, 3.8, dot_col);
    }
}
