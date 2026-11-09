#![allow(dead_code, unused_imports)]
use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::identity::{discover_all_local_identities, LocalLauncherIdentity};
use crate::stats::aggregation::{compute_hit_percentages, compute_kd, compute_win_rate};
use crate::stats::models::MatchStatsRecord;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct YearWrappedSummary {
    pub year: i32,
    pub game_filter: String,
    pub player_identity: String,
    pub launcher_name: String,
    pub total_matches: u32,
    pub total_hours: f64,
    pub wins: u32,
    pub losses: u32,
    pub win_rate_pct: f64,
    pub total_kills: u32,
    pub total_deaths: u32,
    pub total_assists: u32,
    pub kd_ratio: f64,
    pub total_headshots: u32,
    pub total_bodyshots: u32,
    pub total_legshots: u32,
    pub headshot_pct: f64,
    pub bodyshot_pct: f64,
    pub legshot_pct: f64,
    pub total_damage: u32,
    pub avg_combat_score: f64,
    pub total_aces: u32,
    pub total_clutches: u32,
    pub total_multikills: u32,
    pub total_mvps: u32,
    pub total_goals: u32,
    pub total_saves: u32,
    pub total_shots: u32,
    pub goal_conversion_pct: f64,
    pub peak_kills_in_match: u32,
    pub top_character_label: String,
    pub top_character_name: String,
    pub top_character_matches: u32,
    pub top_character_win_rate: f64,
    pub top_weapon_name: String,
    pub favorite_map: String,
    pub playstyle_title: String,
    pub playstyle_subtitle: String,
    pub available_games: Vec<String>,
}

pub fn compute_year_wrapped(
    history: &[MatchStatsRecord],
    year_filter: Option<i32>,
    game_filter: Option<&str>,
) -> YearWrappedSummary {
    let target_year = year_filter.unwrap_or(2026);
    let g_filter = game_filter
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all") && !s.eq_ignore_ascii_case("all games"))
        .map(str::to_lowercase);

    let mut available_games_set: Vec<String> = Vec::new();
    for r in history {
        if !r.game.is_empty() && !available_games_set.iter().any(|g| g.eq_ignore_ascii_case(&r.game)) {
            available_games_set.push(r.game.clone());
        }
    }
    available_games_set.sort();

    let year_start_ms = if target_year <= 0 {
        0u64
    } else {
        let yrs_since_epoch = (target_year - 1970).max(0) as u64;
        yrs_since_epoch * 31_556_952_000
    };
    let year_end_ms = if target_year <= 0 {
        u64::MAX
    } else {
        year_start_ms + 36_000_000_000
    };

    let mut total_matches = 0u32;
    let mut total_secs = 0u64;
    let mut wins = 0u32;
    let mut losses = 0u32;
    let mut kills = 0u32;
    let mut deaths = 0u32;
    let mut assists = 0u32;
    let mut hs = 0u32;
    let mut bs = 0u32;
    let mut ls = 0u32;
    let mut dmg = 0u32;
    let mut cs_sum = 0u64;
    let mut aces = 0u32;
    let mut clutches = 0u32;
    let mut multikills = 0u32;
    let mut mvps = 0u32;
    let mut goals = 0u32;
    let mut saves = 0u32;
    let mut shots = 0u32;
    let mut peak_kills = 0u32;
    let mut hero_stats: HashMap<String, (u32, u32, u32)> = HashMap::new();
    let mut weapon_counts: HashMap<String, u32> = HashMap::new();
    let mut map_counts: HashMap<String, u32> = HashMap::new();
    let mut detected_identity = String::new();

    for rec in history {
        if target_year > 0 && (rec.ended_at_ms < year_start_ms || rec.ended_at_ms > year_end_ms) {
            continue;
        }
        if let Some(ref gf) = g_filter {
            if !rec.game.eq_ignore_ascii_case(gf) {
                continue;
            }
        }
        total_matches += 1;
        total_secs += rec.duration_secs.max(600);
        match rec.won {
            Some(true) => wins += 1,
            Some(false) => losses += 1,
            None => {}
        }
        kills += rec.kills;
        deaths += rec.deaths;
        assists += rec.assists;
        let (r_hs, r_bs, r_ls, _, _, _) =
            compute_hit_percentages(rec.headshots, rec.bodyshots, rec.legshots, rec.kills);
        hs += r_hs;
        bs += r_bs;
        ls += r_ls;
        dmg += rec.damage_dealt;
        cs_sum += rec.combat_score as u64;
        aces += rec.aces;
        clutches += rec.clutches;
        multikills += rec.multikills;
        mvps += rec.mvps;
        goals += rec.goals;
        saves += rec.saves;
        shots += rec.shots_fired.max(rec.goals);
        peak_kills = peak_kills.max(rec.kills.max(rec.goals));
        if detected_identity.is_empty() && !rec.player_identity.is_empty() {
            detected_identity = rec.player_identity.clone();
        }
        if !rec.hero_or_team.is_empty() {
            let entry = hero_stats.entry(rec.hero_or_team.clone()).or_insert((0, 0, 0));
            entry.0 += 1;
            if rec.won == Some(true) {
                entry.1 += 1;
            } else if rec.won == Some(false) {
                entry.2 += 1;
            }
        }
        if !rec.favorite_weapon.is_empty() {
            *weapon_counts.entry(rec.favorite_weapon.clone()).or_insert(0) += 1;
        }
        if !rec.map.is_empty() && rec.map != "Unknown" {
            *map_counts.entry(rec.map.clone()).or_insert(0) += 1;
        }
    }

    let identities = discover_all_local_identities();
    let mut launcher_name = "Local Telemetry".to_string();
    if detected_identity.is_empty() {
        if let Some(ref gf) = g_filter {
            if gf == "valorant" || gf == "lol" {
                if let Some(id) = identities.iter().find(|i| i.launcher.contains("Riot")) {
                    detected_identity = id.display_id.clone();
                    launcher_name = id.launcher.clone();
                }
            } else if gf == "cs2" || gf == "dota2" || gf == "apex_legends" || gf == "the_finals" {
                if let Some(id) = identities.iter().find(|i| i.launcher == "Steam") {
                    detected_identity = id.display_id.clone();
                    launcher_name = id.launcher.clone();
                }
            } else if gf == "rocket_league" || gf == "fortnite" {
                if let Some(id) = identities.iter().find(|i| i.launcher.contains("Epic")) {
                    detected_identity = id.display_id.clone();
                    launcher_name = id.launcher.clone();
                }
            }
        }
        if detected_identity.is_empty() {
            if let Some(first_id) = identities.first() {
                detected_identity = first_id.display_id.clone();
                launcher_name = first_id.launcher.clone();
            } else {
                detected_identity = "Local Player".to_string();
            }
        }
    }

    let (_, _, _, hs_pct, bs_pct, ls_pct) = compute_hit_percentages(hs, bs, ls, kills);
    let kd = compute_kd(kills, deaths);
    let win_rate = compute_win_rate(wins, losses);
    let hours = ((total_secs as f64 / 3600.0) * 10.0).round() / 10.0;

    let effective_game_key = g_filter.clone().unwrap_or_else(|| {
        available_games_set
            .first()
            .cloned()
            .unwrap_or_else(|| "all".to_string())
    });
    let top_char_label = crate::games::event_mapping::role_column_label_for_game(&effective_game_key).to_string();
    let (top_char_name, top_char_matches, top_char_wr) = hero_stats
        .into_iter()
        .max_by_key(|(_, (m, _, _))| *m)
        .map(|(name, (m, w, l))| (name, m, compute_win_rate(w, l)))
        .unwrap_or_else(|| ("Unassigned".to_string(), total_matches, win_rate));

    let top_weapon = weapon_counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(w, _)| w)
        .unwrap_or_else(|| {
            match effective_game_key.as_str() {
                "valorant" => "Vandal / Phantom".to_string(),
                "cs2" => "AK-47 / M4A1-S".to_string(),
                "rocket_league" => "Octane".to_string(),
                _ => "Signature Loadout".to_string(),
            }
        });

    let fav_map = map_counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(m, _)| m)
        .unwrap_or_else(|| "All Maps".to_string());

    let (playstyle_title, playstyle_subtitle) = if hs_pct >= 30.0 && kills >= 5 {
        (
            "PRECISION HEADHUNTER".to_string(),
            format!("{hs_pct:.1}% Headshot accuracy across verified engagements"),
        )
    } else if clutches >= 3 || aces >= 1 {
        (
            "CLUTCH ARCHITECT".to_string(),
            format!("{clutches} Clutches & {aces} Aces under high-pressure rounds"),
        )
    } else if goals > kills && goals >= 3 {
        (
            "CLINICAL STRIKER".to_string(),
            format!("{goals} Goals & {saves} Saves recorded on the pitch"),
        )
    } else if kd >= 1.35 && total_matches > 0 {
        (
            "RELENTLESS FRAGGER".to_string(),
            format!("{kd:.2} K/D Ratio with {peak_kills} peak single-match eliminations"),
        )
    } else {
        (
            "TACTICAL SPECIALIST".to_string(),
            format!("{total_matches} Verified local matches tracked across the season"),
        )
    };

    let goal_conv = if shots > 0 {
        (((goals as f64) / (shots as f64)) * 1000.0).round() / 10.0
    } else {
        0.0
    };

    YearWrappedSummary {
        year: target_year,
        game_filter: g_filter.unwrap_or_else(|| "All Games".to_string()),
        player_identity: detected_identity,
        launcher_name,
        total_matches,
        total_hours: hours,
        wins,
        losses,
        win_rate_pct: win_rate,
        total_kills: kills,
        total_deaths: deaths,
        total_assists: assists,
        kd_ratio: kd,
        total_headshots: hs,
        total_bodyshots: bs,
        total_legshots: ls,
        headshot_pct: hs_pct,
        bodyshot_pct: bs_pct,
        legshot_pct: ls_pct,
        total_damage: dmg,
        avg_combat_score: if total_matches > 0 {
            ((cs_sum as f64 / total_matches as f64) * 10.0).round() / 10.0
        } else {
            0.0
        },
        total_aces: aces,
        total_clutches: clutches,
        total_multikills: multikills,
        total_mvps: mvps,
        total_goals: goals,
        total_saves: saves,
        total_shots: shots,
        goal_conversion_pct: goal_conv,
        peak_kills_in_match: peak_kills,
        top_character_label: top_char_label,
        top_character_name: top_char_name,
        top_character_matches: top_char_matches,
        top_character_win_rate: top_char_wr,
        top_weapon_name: top_weapon,
        favorite_map: fav_map,
        playstyle_title,
        playstyle_subtitle,
        available_games: available_games_set,
    }
}
