#![allow(dead_code, unused_imports)]
use std::path::Path;
use eframe::egui::{
    self, Align, Color32, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Key, Layout,
    Modifiers, Pos2, Rect, RichText, Margin, Rounding, Sense, Stroke, UiBuilder, Vec2,
};

#[derive(Clone, Copy, Debug)]
pub struct ThemeColors {
    pub bg: Color32,
    pub surface: Color32,
    pub surface_2: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text_2: Color32,
    pub text_3: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub accent_soft: Color32,
    pub success: Color32,
    pub danger: Color32,
}

impl ThemeColors {
    pub fn light() -> Self {
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
            success: Color32::from_rgb(22, 163, 74),
            danger: Color32::from_rgb(194, 59, 42),
        }
    }

    pub fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(11, 11, 13),
            surface: Color32::from_rgb(15, 16, 19),
            surface_2: Color32::from_rgb(24, 26, 31),
            border: Color32::from_rgb(30, 32, 38),
            border_strong: Color32::from_rgb(46, 50, 60),
            text: Color32::from_rgb(242, 243, 245),
            text_2: Color32::from_rgb(148, 153, 163),
            text_3: Color32::from_rgb(94, 99, 110),
            accent: Color32::from_rgb(59, 130, 246),
            accent_text: Color32::from_rgb(255, 255, 255),
            accent_soft: Color32::from_rgba_unmultiplied(59, 130, 246, 32),
            success: Color32::from_rgb(34, 197, 94),
            danger: Color32::from_rgb(228, 84, 72),
        }
    }
}


pub fn draw_sidebar_vector_icon(painter: &egui::Painter, center: Pos2, kind: &str, col: Color32) {
    let st = Stroke::new(1.4_f32, col);
    match kind {
        "grid" => {
            for dy in [-3.5f32, 2.0f32] {
                for dx in [-3.5f32, 2.0f32] {
                    let r = Rect::from_min_size(
                        Pos2::new(center.x + dx - 1.0, center.y + dy - 1.0),
                        Vec2::new(4.2, 4.2),
                    );
                    painter.rect_filled(r, Rounding::same(1.0), col);
                }
            }
        }
        "play" => {
            let p1 = Pos2::new(center.x - 3.5, center.y - 4.8);
            let p2 = Pos2::new(center.x - 3.5, center.y + 4.8);
            let p3 = Pos2::new(center.x + 4.8, center.y);
            painter.line_segment([p1, p2], st);
            painter.line_segment([p2, p3], st);
            painter.line_segment([p3, p1], st);
        }
        "clock" => {
            painter.circle_stroke(center, 5.5, st);
            painter.line_segment(
                [center, Pos2::new(center.x, center.y - 3.2)],
                st,
            );
            painter.line_segment(
                [center, Pos2::new(center.x + 2.5, center.y + 1.2)],
                st,
            );
        }
        "star" => {
            let r_top = Pos2::new(center.x, center.y - 5.2);
            let r_br = Pos2::new(center.x + 4.2, center.y + 4.5);
            let r_bl = Pos2::new(center.x - 4.2, center.y + 4.5);
            let r_l = Pos2::new(center.x - 5.0, center.y - 1.5);
            let r_r = Pos2::new(center.x + 5.0, center.y - 1.5);
            painter.line_segment([r_top, r_br], st);
            painter.line_segment([r_br, r_l], st);
            painter.line_segment([r_l, r_r], st);
            painter.line_segment([r_r, r_bl], st);
            painter.line_segment([r_bl, r_top], st);
        }
        "stats" => {
            painter.line_segment(
                [Pos2::new(center.x - 4.0, center.y + 4.5), Pos2::new(center.x - 4.0, center.y + 0.5)],
                Stroke::new(1.8_f32, col),
            );
            painter.line_segment(
                [Pos2::new(center.x, center.y + 4.5), Pos2::new(center.x, center.y - 4.5)],
                Stroke::new(1.8_f32, col),
            );
            painter.line_segment(
                [Pos2::new(center.x + 4.0, center.y + 4.5), Pos2::new(center.x + 4.0, center.y - 2.0)],
                Stroke::new(1.8_f32, col),
            );
        }
        "settings" => {
            painter.circle_stroke(center, 3.2, st);
            for &(dx, dy) in &[
                (0.0f32, -5.2f32),
                (0.0, 5.2),
                (-5.2, 0.0),
                (5.2, 0.0),
                (-3.6, -3.6),
                (3.6, -3.6),
                (-3.6, 3.6),
                (3.6, 3.6),
            ] {
                painter.circle_filled(Pos2::new(center.x + dx, center.y + dy), 1.0, col);
            }
        }
        _ => {}
    }
}

pub fn draw_sidebar_nav_row(
    ui: &mut egui::Ui,
    icon_kind: &str,
    label: &str,
    badge: Option<usize>,
    active: bool,
    dot_color: Option<Color32>,
    c: ThemeColors,
) -> egui::Response {
    let row_w = (ui.available_width() - 20.0).max(150.0);
    let mut row_resp: Option<egui::Response> = None;
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(row_w, 32.0), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if active {
            ui.painter()
                .rect_filled(rect, Rounding::same(6.0), c.accent_soft);
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(rect, Rounding::same(6.0), c.surface_2);
        }

        let active_fg = Color32::from_rgb(96, 165, 250);
        let fg_col = if active {
            active_fg
        } else if resp.hovered() {
            c.text
        } else {
            c.text_2
        };

        let mut text_x = rect.min.x + 12.0;
        if let Some(d_col) = dot_color {
            ui.painter().circle_filled(
                Pos2::new(rect.min.x + 15.0, rect.center().y),
                3.8,
                d_col,
            );
            text_x = rect.min.x + 28.0;
        } else if !icon_kind.is_empty() {
            draw_sidebar_vector_icon(
                ui.painter(),
                Pos2::new(rect.min.x + 16.0, rect.center().y),
                icon_kind,
                fg_col,
            );
            text_x = rect.min.x + 31.0;
        }

        ui.painter().text(
            Pos2::new(text_x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            fg_col,
        );

        if let Some(cnt) = badge {
            let badge_str = cnt.to_string();
            let badge_text_col = if active { active_fg } else { c.text_2 };
            let galley = ui.painter().layout_no_wrap(
                badge_str,
                FontId::monospace(10.5),
                badge_text_col,
            );
            let bw = (galley.size().x + 12.0).max(22.0);
            let bh = 18.0;
            let b_rect = Rect::from_min_size(
                Pos2::new(rect.max.x - bw - 8.0, rect.center().y - bh * 0.5),
                Vec2::new(bw, bh),
            );
            let b_fill = if active {
                Color32::from_rgba_unmultiplied(37, 99, 235, 55)
            } else {
                c.surface_2
            };
            ui.painter()
                .rect_filled(b_rect, Rounding::same(9.0), b_fill);
            ui.painter().galley(
                Pos2::new(
                    b_rect.center().x - galley.size().x * 0.5,
                    b_rect.center().y - galley.size().y * 0.5,
                ),
                galley,
                badge_text_col,
            );
        }
        row_resp = Some(resp);
    });
    row_resp.unwrap()
}

pub fn draw_danger_button(ui: &mut egui::Ui, label: &str, c: ThemeColors) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        FontId::proportional(12.5),
        c.danger,
    );
    let size = Vec2::new(galley.size().x + 22.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let fill = if resp.hovered() {
        Color32::from_rgba_unmultiplied(228, 84, 72, 36)
    } else {
        c.surface
    };
    ui.painter().rect(
        rect,
        Rounding::same(6.0),
        fill,
        Stroke::new(1.0_f32, c.danger),
    );
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        c.danger,
    );
    resp
}

pub fn section_title(ui: &mut egui::Ui, text: &str, c: ThemeColors, first: bool) {
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

pub fn draw_setting_row<R>(
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

pub fn draw_stem_row(
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

pub fn draw_toggle_switch(ui: &mut egui::Ui, on: &mut bool, c: ThemeColors) -> bool {
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

pub fn draw_segmented_buttons(
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

pub fn draw_primary_button(
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

pub fn draw_ghost_button(
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

pub fn draw_mini_pill(
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

pub fn draw_keycap_button(
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

pub fn text_action_link(
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

pub fn setup_unicode_system_fonts(ctx: &egui::Context) {
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


pub fn shape_arabic_for_egui(input: &str) -> String {
    let has_arabic = input.chars().any(|ch| ('\u{0600}'..='\u{06FF}').contains(&ch));
    if !has_arabic {
        return input.to_string();
    }
    fn map_forms(ch: char) -> Option<(char, char, char, char, bool)> {
        match ch {
            '\u{0627}' => Some(('\u{FE8D}', '\u{FE8E}', '\u{FE8D}', '\u{FE8E}', false)),
            '\u{0628}' => Some(('\u{FE8F}', '\u{FE90}', '\u{FE91}', '\u{FE92}', true)),
            '\u{062A}' => Some(('\u{FE95}', '\u{FE96}', '\u{FE97}', '\u{FE98}', true)),
            '\u{062B}' => Some(('\u{FE99}', '\u{FE9A}', '\u{FE9B}', '\u{FE9C}', true)),
            '\u{062C}' => Some(('\u{FE9D}', '\u{FE9E}', '\u{FE9F}', '\u{FEA0}', true)),
            '\u{062D}' => Some(('\u{FEA1}', '\u{FEA2}', '\u{FEA3}', '\u{FEA4}', true)),
            '\u{062E}' => Some(('\u{FEA5}', '\u{FEA6}', '\u{FEA7}', '\u{FEA8}', true)),
            '\u{062F}' => Some(('\u{FEA9}', '\u{FEAA}', '\u{FEA9}', '\u{FEAA}', false)),
            '\u{0630}' => Some(('\u{FEAB}', '\u{FEAC}', '\u{FEAB}', '\u{FEAC}', false)),
            '\u{0631}' => Some(('\u{FEAD}', '\u{FEAE}', '\u{FEAD}', '\u{FEAE}', false)),
            '\u{0632}' => Some(('\u{FEAF}', '\u{FEB0}', '\u{FEAF}', '\u{FEB0}', false)),
            '\u{0633}' => Some(('\u{FEB1}', '\u{FEB2}', '\u{FEB3}', '\u{FEB4}', true)),
            '\u{0634}' => Some(('\u{FEB5}', '\u{FEB6}', '\u{FEB7}', '\u{FEB8}', true)),
            '\u{0635}' => Some(('\u{FEB9}', '\u{FEBA}', '\u{FEBB}', '\u{FEBC}', true)),
            '\u{0636}' => Some(('\u{FEBD}', '\u{FEBE}', '\u{FEBF}', '\u{FEC0}', true)),
            '\u{0637}' => Some(('\u{FEC1}', '\u{FEC2}', '\u{FEC3}', '\u{FEC4}', true)),
            '\u{0638}' => Some(('\u{FEC5}', '\u{FEC6}', '\u{FEC7}', '\u{FEC8}', true)),
            '\u{0639}' => Some(('\u{FEC9}', '\u{FECA}', '\u{FECB}', '\u{FECC}', true)),
            '\u{063A}' => Some(('\u{FECD}', '\u{FECE}', '\u{FECF}', '\u{FED0}', true)),
            '\u{0641}' => Some(('\u{FED1}', '\u{FED2}', '\u{FED3}', '\u{FED4}', true)),
            '\u{0642}' => Some(('\u{FED5}', '\u{FED6}', '\u{FED7}', '\u{FED8}', true)),
            '\u{0643}' => Some(('\u{FED9}', '\u{FEDA}', '\u{FEDB}', '\u{FEDC}', true)),
            '\u{0644}' => Some(('\u{FEDD}', '\u{FEDE}', '\u{FEDF}', '\u{FEE0}', true)),
            '\u{0645}' => Some(('\u{FEE1}', '\u{FEE2}', '\u{FEE3}', '\u{FEE4}', true)),
            '\u{0646}' => Some(('\u{FEE5}', '\u{FEE6}', '\u{FEE7}', '\u{FEE8}', true)),
            '\u{0647}' => Some(('\u{FEE9}', '\u{FEEA}', '\u{FEEB}', '\u{FEEC}', true)),
            '\u{0648}' => Some(('\u{FEED}', '\u{FEEE}', '\u{FEED}', '\u{FEEE}', false)),
            '\u{0649}' => Some(('\u{FEEF}', '\u{FEF0}', '\u{FEEF}', '\u{FEF0}', false)),
            '\u{064A}' => Some(('\u{FEF1}', '\u{FEF2}', '\u{FEF3}', '\u{FEF4}', true)),
            _ => None,
        }
    }
    let chars: Vec<char> = input.chars().collect();
    let mut shaped = Vec::with_capacity(chars.len());
    for i in 0..chars.len() {
        let c = chars[i];
        let Some((iso, fin, ini, med, _)) = map_forms(c) else {
            shaped.push(c);
            continue;
        };
        let prev_connects = if i > 0 {
            map_forms(chars[i - 1]).map_or(false, |(_, _, _, _, conn)| conn)
        } else {
            false
        };
        let next_is_ar = if i + 1 < chars.len() {
            map_forms(chars[i + 1]).is_some()
        } else {
            false
        };
        let g = match (prev_connects, next_is_ar) {
            (false, false) => iso,
            (true, false) => fin,
            (false, true) => ini,
            (true, true) => med,
        };
        shaped.push(g);
    }
    shaped.reverse();
    shaped.into_iter().collect()
}