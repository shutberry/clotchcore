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



impl ClotchGuiApp {
    pub(crate) fn render_settings_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, c: ThemeColors) {
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
                                    for opt in ["10 s", "14 s", "20 s", "30 s"] {
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
                                    for opt in ["8 s", "10 s", "12 s", "15 s"] {
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

pub(crate) fn check_missing_microphone(
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


pub(crate) fn format_key_combo(mods: Modifiers, key_token: &str) -> String {
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

pub(crate) fn egui_key_to_config_token(key: Key) -> Option<String> {
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


pub(crate) fn spawn_native_folder_picker(tx: Sender<UiAsyncMessage>, ctx: egui::Context) {
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

