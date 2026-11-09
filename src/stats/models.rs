#![allow(dead_code, unused_imports)]
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use super::aggregation::{GameSession, KeybindTelemetrySummary, TelemetryPayload};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchStatsRecord {
    pub match_id: String,
    pub game: String,
    pub map: String,
    pub mode: String,
    pub hero_or_team: String,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub duration_secs: u64,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub headshots: u32,
    #[serde(default)]
    pub bodyshots: u32,
    #[serde(default)]
    pub legshots: u32,
    #[serde(default)]
    pub damage_dealt: u32,
    #[serde(default)]
    pub combat_score: u32,
    #[serde(default)]
    pub shots_fired: u32,
    #[serde(default)]
    pub favorite_weapon: String,
    #[serde(default)]
    pub player_identity: String,
    #[serde(default)]
    pub rank_or_tier: String,
    pub multikills: u32,
    pub aces: u32,
    pub clutches: u32,
    pub mvps: u32,
    pub goals: u32,
    pub goals_conceded: u32,
    pub saves: u32,
    pub objectives: u32,
    pub won: Option<bool>,
    pub score_for: Option<i64>,
    pub score_against: Option<i64>,
    pub kd_ratio: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GamePeriodStats {
    pub game: String,
    pub matches_played: u32,
    pub total_duration_secs: u64,
    pub wins: u32,
    pub losses: u32,
    pub win_rate_pct: f64,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub kd_ratio: f64,
    pub clutches: u32,
    pub aces: u32,
    pub headshots: u32,
    pub bodyshots: u32,
    pub legshots: u32,
    pub headshot_pct: f64,
    pub bodyshot_pct: f64,
    pub legshot_pct: f64,
    pub damage_dealt: u32,
    pub avg_combat_score: f64,
    pub multikills: u32,
    pub mvps: u32,
    pub goals: u32,
    pub goals_conceded: u32,
    pub saves: u32,
    pub shots_fired: u32,
    pub objectives: u32,
    pub favorite_map: String,
    pub favorite_hero_or_team: String,
    pub favorite_weapon: String,
    pub hero_counts: HashMap<String, u32>,
    pub weapon_counts: HashMap<String, u32>,
    pub map_counts: HashMap<String, u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MonthlyStatsSummary {
    pub window_days: u32,
    pub generated_at_ms: u64,
    pub total_matches: u32,
    pub total_duration_secs: u64,
    pub wins: u32,
    pub losses: u32,
    pub win_rate_pct: f64,
    pub total_kills: u32,
    pub total_deaths: u32,
    pub total_assists: u32,
    pub overall_kd: f64,
    pub total_clutches: u32,
    pub total_aces: u32,
    pub total_headshots: u32,
    pub total_bodyshots: u32,
    pub total_legshots: u32,
    pub headshot_pct: f64,
    pub bodyshot_pct: f64,
    pub legshot_pct: f64,
    pub total_damage_dealt: u32,
    pub avg_combat_score: f64,
    pub total_multikills: u32,
    pub total_mvps: u32,
    pub total_goals: u32,
    pub total_goals_conceded: u32,
    pub total_saves: u32,
    pub total_shots_fired: u32,
    pub total_objectives: u32,
    pub favorite_hero_or_team: String,
    pub favorite_weapon: String,
    pub favorite_map: String,
    pub per_game: HashMap<String, GamePeriodStats>,
    pub recent_matches: Vec<MatchStatsRecord>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GameAggregateStats {
    pub total_duration_minutes: u64,
    pub total_sessions: u64,
    pub first_seen_timestamp: u64,
    pub last_seen_timestamp: u64,
    pub total_kills: u64,
    pub total_deaths: u64,
    pub total_assists: u64,
    pub total_mvps: u64,
    pub total_goals: u64,
    pub total_saves: u64,
}

pub fn compute_kd(kills: u32, deaths: u32) -> f64 {
    let k = kills as f64;
    let d = (deaths.max(1)) as f64;
    ((k / d) * 100.0).round() / 100.0
}

pub fn compute_win_rate(wins: u32, losses: u32) -> f64 {
    let decided = wins + losses;
    if decided == 0 {
        0.0
    } else {
        (((wins as f64) / (decided as f64)) * 1000.0).round() / 10.0
    }
}

pub fn compute_hit_percentages(
    headshots: u32,
    bodyshots: u32,
    legshots: u32,
    fallback_kills: u32,
) -> (u32, u32, u32, f64, f64, f64) {
    let mut hs = headshots;
    let mut bs = bodyshots;
    let ls = legshots;
    if hs > 0 && bs == 0 && ls == 0 && fallback_kills > hs {
        bs = fallback_kills.saturating_sub(hs);
    }
    if hs > fallback_kills && bs == 0 && ls == 0 && fallback_kills > 0 {
        hs = fallback_kills;
    }
    let total_hits = hs + bs + ls;
    if total_hits == 0 {
        return (0, 0, 0, 0.0, 0.0, 0.0);
    }
    let hs_pct = (((hs as f64) / (total_hits as f64)) * 1000.0).round() / 10.0;
    let ls_pct = (((ls as f64) / (total_hits as f64)) * 1000.0).round() / 10.0;
    let bs_pct = (100.0 - hs_pct - ls_pct).clamp(0.0, 100.0);
    let bs_pct_round = (bs_pct * 10.0).round() / 10.0;
    (hs, bs, ls, hs_pct, bs_pct_round, ls_pct)
}

pub fn clean_weapon_display_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let low = trimmed.to_lowercase();
    let stripped = low.strip_prefix("weapon_").unwrap_or(&low);
    match stripped {
        "ak47" => "AK-47".to_string(),
        "m4a1_silencer" => "M4A1-S".to_string(),
        "m4a1" => "M4A4".to_string(),
        "awp" => "AWP".to_string(),
        "deagle" => "Desert Eagle".to_string(),
        "usp_silencer" => "USP-S".to_string(),
        "glock" => "Glock-18".to_string(),
        "mp9" => "MP9".to_string(),
        "mac10" => "MAC-10".to_string(),
        "galilar" => "Galil AR".to_string(),
        "famas" => "FAMAS".to_string(),
        "ssg08" => "SSG 08".to_string(),
        "p250" => "P250".to_string(),
        "fiveseven" => "Five-SeveN".to_string(),
        "tec9" => "Tec-9".to_string(),
        "aug" => "AUG".to_string(),
        "sg556" => "SG 553".to_string(),
        _ => trimmed.to_string(),
    }
}

pub fn clean_hero_display_name(game: &str, raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(rest) = trimmed.strip_prefix("npc_dota_hero_") {
        let mut out = String::new();
        for part in rest.split('_') {
            if !part.is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                let mut chars = part.chars();
                if let Some(first) = chars.next() {
                    out.extend(first.to_uppercase());
                    out.push_str(chars.as_str());
                }
            }
        }
        return out;
    }
    trimmed.to_string()
}

pub fn role_column_label_for_game(game: &str) -> &'static str {
    match game.trim().to_lowercase().as_str() {
        "valorant" => "TOP AGENT",
        "lol" | "league_of_legends" | "league of legends" => "TOP CHAMPION",
        "dota2" | "dota 2" | "overwatch2" | "overwatch 2" | "marvel_rivals" | "marvel rivals" => "TOP HERO",
        "apex_legends" | "apex legends" => "TOP LEGEND",
        "rainbow_six_siege" | "rainbow six siege" => "TOP OPERATOR",
        "cs2" | "counter-strike 2" => "PRIMARY SIDE / ROLE",
        "rocket_league" | "rocket league" => "SIGNATURE CAR",
        "fc26" | "fc27" | "efootball" | "ea sports fc" => "FAVORITE CLUB",
        _ => "PRIMARY AGENT / HERO",
    }
}
