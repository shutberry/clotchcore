#![allow(dead_code, unused_imports)]
use eframe::egui::Color32;

pub fn pretty_game_title(raw: &str) -> String {
    let trimmed = raw.trim();
    let low = trimmed.to_lowercase();
    if low == "lol" || low == "league_of_legends" || low.contains("league of legends") {
        "League of Legends".to_string()
    } else if low == "valorant" || low == "val" {
        "Valorant".to_string()
    } else if low == "cs2" || low.contains("counter-strike") || low.contains("counter_strike") {
        "CS2".to_string()
    } else if low == "rocket_league" || low == "rl" || low.contains("rocket") {
        "Rocket League".to_string()
    } else if low == "fc26" || low == "fc27" || low == "ea_sports_fc" || low.contains("ea sports fc") || low.contains("fifa") {
        "EA Sports FC".to_string()
    } else if low == "efootball" {
        "eFootball".to_string()
    } else if low == "overwatch2" || low == "ow2" || low.contains("overwatch") {
        "Overwatch 2".to_string()
    } else if low == "fortnite" || low == "fn" {
        "Fortnite".to_string()
    } else if low == "apex_legends" || low == "apex" || low.contains("apex") {
        "Apex Legends".to_string()
    } else if low == "dota2" || low.contains("dota") {
        "Dota 2".to_string()
    } else if low == "rainbow_six_siege" || low == "r6" || low.contains("rainbow") {
        "Rainbow Six Siege".to_string()
    } else if low == "cod" || low == "warzone" || low.contains("call_of_duty") {
        "Call of Duty".to_string()
    } else if low == "marvel_rivals" || low.contains("marvel") {
        "Marvel Rivals".to_string()
    } else if low == "the_finals" || low.contains("finals") {
        "The Finals".to_string()
    } else if trimmed.is_empty() {
        "Desktop".to_string()
    } else {
        trimmed.replace('_', " ")
    }
}

pub fn game_indicator_color(game: &str) -> Color32 {
    let low = game.trim().to_lowercase();
    if low.contains("league") || low == "lol" {
        Color32::from_rgb(56, 189, 248)
    } else if low.contains("valorant") || low == "val" {
        Color32::from_rgb(244, 63, 94)
    } else if low == "cs2" || low.contains("counter") {
        Color32::from_rgb(132, 204, 22)
    } else if low.contains("rocket") || low == "rl" {
        Color32::from_rgb(249, 115, 22)
    } else if low.contains("fc") || low.contains("fifa") || low.contains("efootball") {
        Color32::from_rgb(16, 185, 129)
    } else if low.contains("overwatch") || low == "ow2" {
        Color32::from_rgb(245, 158, 11)
    } else if low.contains("fortnite") {
        Color32::from_rgb(168, 85, 247)
    } else if low.contains("apex") {
        Color32::from_rgb(239, 68, 68)
    } else {
        Color32::from_rgb(59, 130, 246)
    }
}

