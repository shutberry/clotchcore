#![allow(dead_code, unused_imports)]
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use serde_json::{json, Map, Value};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use crate::events::Event;

pub const DEFAULT_DISCORD_APP_ID: &str = "1567535923669770360";

static DISCORD_RPC_CONNECTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static DISCORD_RPC_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

pub fn is_discord_rpc_active() -> bool {
    DISCORD_RPC_CONNECTED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn is_discord_rpc_enabled() -> bool {
    DISCORD_RPC_ENABLED.load(std::sync::atomic::Ordering::Acquire)
}

pub fn set_discord_rpc_enabled(enabled: bool) {
    DISCORD_RPC_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
    let _ = persist_discord_rpc_enabled(enabled);
}

pub fn persist_discord_rpc_enabled(enabled: bool) -> Result<PathBuf, String> {
    let target_path = locate_config_file().unwrap_or_else(|| {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return dir.join("clotch_config.toml");
            }
        }
        PathBuf::from("clotch_config.toml")
    });
    let val_str = if enabled { "true" } else { "false" };
    let current = std::fs::read_to_string(&target_path).unwrap_or_default();
    let mut out_lines: Vec<String> = Vec::new();
    let mut updated = false;
    for line in current.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && !trimmed.starts_with(';')
            && !trimmed.starts_with('[')
        {
            if let Some((k, _)) = trimmed.split_once('=') {
                let key_low = k.trim().to_lowercase();
                if key_low == "discord_enabled" || key_low == "rich_presence" {
                    if !updated {
                        out_lines.push(format!("discord_enabled = {val_str}"));
                        updated = true;
                    }
                    continue;
                }
            }
        }
        out_lines.push(line.to_string());
    }
    if !updated {
        out_lines.push(format!("discord_enabled = {val_str}"));
    }
    let mut final_text = out_lines.join("\n");
    if !final_text.ends_with('\n') {
        final_text.push('\n');
    }
    std::fs::write(&target_path, final_text)
        .map_err(|e| format!("failed to write discord_enabled to {}: {e}", target_path.display()))?;
    Ok(target_path)
}

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

#[derive(Clone, Debug)]
pub struct DiscordConfig {
    pub app_id: String,
    pub enabled: bool,
    pub show_idle: bool,
    pub custom_assets: HashMap<String, String>,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self::from_env_and_file()
    }
}

pub fn locate_config_file() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("clotch_config.toml"));
        }
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidates.push(PathBuf::from(appdata).join("Clotch").join("clotch_config.toml"));
    }
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local_appdata).join("Clotch").join("clotch_config.toml"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(xdg).join("clotch/clotch_config.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".config/clotch/clotch_config.toml"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

impl DiscordConfig {
    pub fn from_env_and_file() -> Self {
        let mut app_id = std::env::var("CLOTCH_DISCORD_APP_ID")
            .or_else(|_| std::env::var("DISCORD_CLIENT_ID"))
            .unwrap_or_else(|_| DEFAULT_DISCORD_APP_ID.to_string());
        let mut enabled = true;
        let mut show_idle = true;
        let mut custom_assets = HashMap::new();

        if let Some(cfg_path) = locate_config_file() {
            if let Ok(content) = std::fs::read_to_string(cfg_path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                        continue;
                    }
                    let Some((k, v)) = trimmed.split_once('=') else {
                        continue;
                    };
                    let key = k.trim().to_lowercase();
                    let val = v.trim().trim_matches('"').trim_matches('\'').trim();
                    if val.is_empty() {
                        continue;
                    }
                    match key.as_str() {
                        "discord_app_id" | "discord_client_id" => {
                            app_id = val.to_string();
                        }
                        "discord_enabled" | "rich_presence" => {
                            enabled = !matches!(
                                val.to_lowercase().as_str(),
                                "false" | "0" | "no" | "off"
                            );
                        }
                        "discord_show_idle" => {
                            show_idle = !matches!(
                                val.to_lowercase().as_str(),
                                "false" | "0" | "no" | "off"
                            );
                        }
                        k_asset if k_asset.starts_with("discord_asset_") => {
                            if !val.ends_with("-skir") {
                                let game_key =
                                    k_asset.trim_start_matches("discord_asset_").to_string();
                                custom_assets.insert(game_key, val.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        Self {
            app_id,
            enabled,
            show_idle,
            custom_assets,
        }
    }

    pub fn asset_for_game(&self, game: &str) -> (&str, &str) {
        let g = game.to_lowercase();
        let default_pair = if g == "cs2" || g.contains("counter-strike") {
            ("clotch-cs2", "Counter-Strike 2")
        } else if g == "valorant" {
            ("clotch-valorant", "VALORANT")
        } else if g == "dota2" || g.contains("dota") {
            ("clotch-dota2", "Dota 2")
        } else if g == "lol" || g.contains("league of legends") {
            ("clotch-lol", "League of Legends")
        } else if g == "overwatch2" || g.contains("overwatch") {
            ("clotch-overwatch2", "Overwatch 2")
        } else if g == "rocket_league" || g.contains("rocket") {
            ("clotch-rocket", "Rocket League")
        } else if g == "warzone" || g == "cod" || g == "call_of_duty" || g.contains("call of duty") {
            ("clotch-cod", "Call of Duty")
        } else if g == "battlefield" || g.starts_with("bf") {
            ("clotch-bf", "Battlefield")
        } else if g == "apex_legends" || g == "apex" || g.contains("apex legends") {
            ("clotch-apexlegend", "Apex Legends")
        } else if g == "fortnite" {
            ("clotch-fortnite", "Fortnite")
        } else if g == "rainbow_six_siege" || g == "r6" || g.contains("rainbow six") {
            ("clotch-r6", "Rainbow Six Siege")
        } else if g == "the_finals" || g.contains("the finals") {
            ("clotch-thefinals", "THE FINALS")
        } else if g == "arc_raiders" || g.contains("arc raiders") {
            ("clotch-arc", "ARC Raiders")
        } else if g == "marvel_rivals" || g.contains("marvel rivals") {
            ("clotch-marvel", "Marvel Rivals")
        } else if g == "fc26" || g.contains("fc 26") || g.contains("easportsfc26") {
            ("clotch-fc26", "EA SPORTS FC 26")
        } else if g == "fc27" || g.contains("fc 27") || g.contains("easportsfc27") {
            ("clotch-fc27", "EA SPORTS FC 27")
        } else if g == "efootball" || g.contains("efootball") {
            ("clotch-efootball", "eFootball")
        } else {
            ("clotch-idle", "Clotch")
        };

        if let Some(custom) = self.custom_assets.get(&g) {
            return (custom.as_str(), default_pair.1);
        }
        default_pair
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SessionPresence {
    active_game: Option<String>,
    active_pid: Option<u32>,
    started_epoch_secs: u64,
    in_match: bool,
    loop_state: String,
    party_state: String,
    map: String,
    mode: String,
    hero: String,
    team: String,
    level: i64,
    round: i64,
    my_score: Option<i64>,
    enemy_score: Option<i64>,
    kills: i64,
    deaths: i64,
    assists: i64,
    mvps: i64,
    cs: i64,
    goals: i64,
    saves: i64,
    demolishes: i64,
    best_multikill: i64,
    aces: i64,
    clutches: i64,
    overtime: bool,
    party_id: String,
    party_size: Option<i64>,
    party_max: Option<i64>,
}

impl SessionPresence {
    fn new_idle(now_secs: u64) -> Self {
        Self {
            started_epoch_secs: now_secs,
            ..Default::default()
        }
    }

    fn reset_for_game(&mut self, game: String, pid: Option<u32>, now_secs: u64) {
        *self = Self {
            active_game: Some(game),
            active_pid: pid,
            started_epoch_secs: now_secs,
            in_match: false,
            loop_state: "MENUS".to_string(),
            ..Default::default()
        };
    }

    fn reset_match_stats(&mut self, now_secs: u64) {
        self.started_epoch_secs = now_secs;
        self.round = 0;
        self.my_score = None;
        self.enemy_score = None;
        self.kills = 0;
        self.deaths = 0;
        self.assists = 0;
        self.mvps = 0;
        self.cs = 0;
        self.goals = 0;
        self.saves = 0;
        self.demolishes = 0;
        self.best_multikill = 0;
        self.aces = 0;
        self.clutches = 0;
        self.overtime = false;
    }

    fn ingest_event(&mut self, event: &Event) -> bool {
        let now_secs = unix_secs();
        let ev_game = event.game.as_ref();

        match event.kind.as_str() {
            "game.started" => {
                let pid = event.pid.or_else(|| {
                    event
                        .data
                        .get("pid")
                        .and_then(Value::as_u64)
                        .map(|p| p as u32)
                });
                self.reset_for_game(ev_game.to_string(), pid, now_secs);
                return true;
            }
            "game.pid_updated" => {
                let pid = event.pid.or_else(|| {
                    event
                        .data
                        .get("pid")
                        .and_then(Value::as_u64)
                        .map(|p| p as u32)
                });
                if self.active_game.as_deref() == Some(ev_game) && pid.is_some() && pid != self.active_pid {
                    self.active_pid = pid;
                    return true;
                }
                return false;
            }
            "game.stopped" => {
                if self.active_game.as_deref() == Some(ev_game) {
                    *self = Self::new_idle(now_secs);
                    return true;
                }
                return false;
            }
            _ => {}
        }

        if matches!(
            ev_game,
            "system" | "reader" | "engine" | "hotkey"
        ) || event.kind.starts_with("source.")
            || event.kind.starts_with("recorder.")
            || event.kind.starts_with("reader.")
            || event.kind.starts_with("clip.")
            || event.kind.starts_with("discord.")
            || event.kind.starts_with("manual.")
            || event.kind.starts_with("stats.")
        {
            return false;
        }

        if self.active_game.is_none() {
            self.reset_for_game(ev_game.to_string(), event.pid, now_secs);
        }

        if self.active_game.as_deref() != Some(ev_game) {
            return false;
        }

        if let Some(pid) = event.pid {
            self.active_pid = Some(pid);
        }

        let before = self.clone();
        let d = &event.data;

        if let Some(p_id) = str_field(d, "party_id") {
            if !p_id.is_empty() {
                self.party_id = p_id;
            } else {
                self.party_id.clear();
            }
        }
        if d.get("party_size").is_some() {
            if let Some(ps) = int_field(d, "party_size") {
                self.party_size = if ps > 1 { Some(ps) } else { None };
            } else {
                self.party_size = None;
            }
        }
        if d.get("party_max").is_some() || d.get("max_party_size").is_some() {
            if let Some(pm) = int_field(d, "party_max").or_else(|| int_field(d, "max_party_size")) {
                self.party_max = if pm > 1 { Some(pm) } else { None };
            } else {
                self.party_max = None;
            }
        }

        match event.kind.as_str() {
            "match.start" | "workshop.match_start" => {
                let raw_m = str_field(d, "map")
                    .or_else(|| str_field(d, "arena"))
                    .unwrap_or_default();
                if !raw_m.is_empty() && is_non_gameplay_map(&raw_m) {
                    self.in_match = false;
                    self.loop_state = "MENUS".to_string();
                    self.reset_match_stats(now_secs);
                    self.map.clear();
                    self.hero.clear();
                } else {
                    self.reset_match_stats(now_secs);
                    self.in_match = true;
                    if self.loop_state.eq_ignore_ascii_case("MENUS") || self.loop_state.is_empty() {
                        self.loop_state = "INGAME".to_string();
                    }
                    let clean_m = format_clean_map_name(ev_game, &raw_m);
                    if !clean_m.is_empty() {
                        self.map = clean_m;
                    }
                    if let Some(mode) = str_field(d, "mode").or_else(|| str_field(d, "queue_id")) {
                        if !mode.is_empty() {
                            self.mode = mode;
                        }
                    }
                }
            }
            "match.map_load" => {
                if let Some(m) = str_field(d, "map") {
                    if is_non_gameplay_map(&m) {
                        self.in_match = false;
                        self.loop_state = "MENUS".to_string();
                        self.reset_match_stats(now_secs);
                        self.map.clear();
                        self.hero.clear();
                    } else {
                        let clean_m = format_clean_map_name(ev_game, &m);
                        if !clean_m.is_empty() {
                            self.map = clean_m;
                            self.in_match = true;
                            if self.loop_state.eq_ignore_ascii_case("MENUS") || self.loop_state.is_empty() {
                                self.loop_state = "INGAME".to_string();
                            }
                        }
                    }
                }
            }
            "match.end" | "match.leave" | "match_ended" | "workshop.match_end" => {
                self.reset_match_stats(now_secs);
                self.in_match = false;
                self.loop_state = "MENUS".to_string();
                self.map.clear();
                self.hero.clear();
            }
            "match.reset" => {
                self.reset_match_stats(now_secs);
            }
            "match.overtime" => {
                self.in_match = true;
                self.overtime = true;
            }
            "presence.update" => {
                if let Some(ls) = str_field(d, "session_loop_state") {
                    if !ls.is_empty() {
                        self.loop_state = ls;
                    }
                }
                if let Some(ps) = str_field(d, "party_state") {
                    self.party_state = ps;
                }

                let explicit_menu = str_field(d, "activity")
                    .map(|a| a.eq_ignore_ascii_case("menu"))
                    .unwrap_or(false)
                    || str_field(d, "session_loop_state")
                        .map(|ls| ls.eq_ignore_ascii_case("MENUS"))
                        .unwrap_or(false)
                    || str_field(d, "state")
                        .map(|st| st.eq_ignore_ascii_case("lobby") || st.eq_ignore_ascii_case("menu"))
                        .unwrap_or(false);

                if explicit_menu {
                    self.in_match = false;
                    self.loop_state = "MENUS".to_string();
                    self.reset_match_stats(now_secs);
                    self.map.clear();
                    self.hero.clear();
                } else if let Some(m) = str_field(d, "map") {
                    if m.is_empty() || is_non_gameplay_map(&m) {
                        if matches!(ev_game, "cs2" | "dota2" | "valorant") {
                            self.in_match = false;
                            self.loop_state = "MENUS".to_string();
                            self.reset_match_stats(now_secs);
                            self.map.clear();
                            self.hero.clear();
                        }
                    } else {
                        let clean_m = format_clean_map_name(ev_game, &m);
                        if !clean_m.is_empty() {
                            self.map = clean_m;
                            self.in_match = true;
                            if self.loop_state.eq_ignore_ascii_case("MENUS") || self.loop_state.is_empty() {
                                self.loop_state = "INGAME".to_string();
                            }
                        }
                    }
                }

                if !explicit_menu
                    && (self.loop_state.eq_ignore_ascii_case("INGAME")
                        || self.loop_state.eq_ignore_ascii_case("PREGAME"))
                {
                    self.in_match = true;
                }

                if d.get("mode").is_some()
                    || d.get("queue_id").is_some()
                    || d.get("provisioning_flow").is_some()
                {
                    let next_mode = str_field(d, "mode")
                        .filter(|s| !s.is_empty())
                        .or_else(|| str_field(d, "queue_id").filter(|s| !s.is_empty()))
                        .or_else(|| str_field(d, "provisioning_flow").filter(|s| !s.is_empty()))
                        .unwrap_or_default();
                    if !next_mode.is_empty() || explicit_menu {
                        self.mode = next_mode;
                    }
                }

                if !explicit_menu {
                    if let Some(h) = str_field(d, "hero")
                        .or_else(|| str_field(d, "champion"))
                        .or_else(|| str_field(d, "legend"))
                        .or_else(|| str_field(d, "operator"))
                    {
                        if !h.is_empty() {
                            self.hero = h;
                        }
                    }
                    if let Some(t) = str_field(d, "team") {
                        if !t.is_empty() {
                            self.team = t;
                        }
                    }
                    if let Some(lvl) = int_field(d, "level") {
                        if lvl > 0 {
                            self.level = lvl;
                            self.in_match = true;
                        }
                    }
                    if let Some(r) = int_field(d, "rounds_played").or_else(|| int_field(d, "round")) {
                        if r >= 0 {
                            self.round = r + 1;
                            self.in_match = true;
                        }
                    }
                    if let Some(k) = int_field(d, "kills") {
                        self.kills = k;
                        if k > 0 {
                            self.in_match = true;
                            if self.loop_state.eq_ignore_ascii_case("MENUS") {
                                self.loop_state = "INGAME".to_string();
                            }
                        }
                    }
                    if let Some(de) = int_field(d, "deaths") {
                        self.deaths = de;
                    }
                    if let Some(a) = int_field(d, "assists") {
                        self.assists = a;
                    }
                    if let Some(m) = int_field(d, "mvps") {
                        self.mvps = m;
                    }
                    if let (Some(ct), Some(t)) = (int_field(d, "ct"), int_field(d, "t")) {
                        if self.team.eq_ignore_ascii_case("T") {
                            self.my_score = Some(t);
                            self.enemy_score = Some(ct);
                        } else {
                            self.my_score = Some(ct);
                            self.enemy_score = Some(t);
                        }
                    }
                    if let (Some(ally), Some(enemy)) =
                        (int_field(d, "ally_score"), int_field(d, "enemy_score"))
                    {
                        self.my_score = Some(ally);
                        self.enemy_score = Some(enemy);
                    }
                    if let (Some(rad), Some(dire)) =
                        (int_field(d, "radiant_score"), int_field(d, "dire_score"))
                    {
                        if self.team.eq_ignore_ascii_case("dire") {
                            self.my_score = Some(dire);
                            self.enemy_score = Some(rad);
                        } else {
                            self.my_score = Some(rad);
                            self.enemy_score = Some(dire);
                        }
                    }
                }
            }
            "player_stats" => {
                self.in_match = true;
                if self.loop_state.eq_ignore_ascii_case("MENUS") {
                    self.loop_state = "INGAME".to_string();
                }
                if let Some(c) = str_field(d, "champion") {
                    if !c.is_empty() {
                        self.hero = c;
                    }
                }
                if let Some(t) = str_field(d, "team") {
                    if !t.is_empty() {
                        self.team = t;
                    }
                }
                if let Some(k) = int_field(d, "kills") {
                    self.kills = k;
                }
                if let Some(de) = int_field(d, "deaths") {
                    self.deaths = de;
                }
                if let Some(a) = int_field(d, "assists") {
                    self.assists = a;
                }
                if let Some(cs) = int_field(d, "cs") {
                    self.cs = cs;
                }
                if let Some(lvl) = int_field(d, "level") {
                    self.level = lvl;
                }
            }
            "update_state" => {
                if let Some(arena) = d.pointer("/Game/Arena").and_then(Value::as_str) {
                    if arena.is_empty() || is_non_gameplay_map(arena) {
                        self.in_match = false;
                        self.loop_state = "MENUS".to_string();
                        self.map.clear();
                    } else {
                        let clean_a = format_clean_map_name(ev_game, arena);
                        if !clean_a.is_empty() {
                            self.map = clean_a;
                            self.in_match = true;
                            self.loop_state = "INGAME".to_string();
                        }
                    }
                }
                if let Some(ot) = d.pointer("/Game/bOvertime").and_then(Value::as_bool) {
                    self.overtime = ot;
                }

                let mut local_team: Option<i64> = None;
                let explicit_local_name = d
                    .get("LocalPlayerName")
                    .or_else(|| d.pointer("/Game/LocalPlayerName"))
                    .or_else(|| d.pointer("/Player/Name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let explicit_local_id = d
                    .get("PrimaryPlayer")
                    .or_else(|| d.pointer("/Game/PrimaryPlayer"))
                    .or_else(|| d.pointer("/Player/PlayerID"))
                    .and_then(Value::as_i64);

                if let Some(players) = d.pointer("/Players").and_then(Value::as_array) {
                    for p in players {
                        let p_name = p.get("Name").and_then(Value::as_str).map(str::trim);
                        let p_id = p.get("PlayerID").and_then(Value::as_i64);
                        let is_local_flag = p
                            .get("bLocalPlayer")
                            .or_else(|| p.get("IsLocal"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let is_local = is_local_flag
                            || (explicit_local_id.is_some() && p_id == explicit_local_id)
                            || (explicit_local_name.is_some()
                                && p_name
                                    .zip(explicit_local_name)
                                    .map(|(a, b)| a.eq_ignore_ascii_case(b))
                                    .unwrap_or(false));
                        if is_local {
                            local_team = p.get("Team").and_then(Value::as_i64);
                            if let Some(g) = p.get("Goals").and_then(Value::as_i64) {
                                self.goals = g;
                            }
                            if let Some(sv) = p.get("Saves").and_then(Value::as_i64) {
                                self.saves = sv;
                            }
                            if let Some(dm) = p.get("Demolishes").and_then(Value::as_i64) {
                                self.demolishes = dm;
                            }
                            break;
                        }
                    }
                }

                if let Some(teams) = d.pointer("/Game/Teams").and_then(Value::as_array) {
                    if teams.len() >= 2 {
                        let s0 = teams[0].get("Score").and_then(Value::as_i64);
                        let s1 = teams[1].get("Score").and_then(Value::as_i64);
                        if let (Some(a), Some(b)) = (s0, s1) {
                            if local_team == Some(1) {
                                self.my_score = Some(b);
                                self.enemy_score = Some(a);
                            } else {
                                self.my_score = Some(a);
                                self.enemy_score = Some(b);
                            }
                        }
                    }
                }
            }
            "round.freezetime" | "round.live" | "round.over" | "round.start" | "round.end" => {
                self.in_match = true;
                if self.loop_state.eq_ignore_ascii_case("MENUS") {
                    self.loop_state = "INGAME".to_string();
                }
                if let Some(r) = int_field(d, "rounds_played")
                    .or_else(|| int_field(d, "round_number"))
                    .or_else(|| int_field(d, "round"))
                {
                    self.round = if event.kind.starts_with("round.over") || event.kind == "round.end"
                    {
                        r + 1
                    } else {
                        r.max(1)
                    };
                }
                if let (Some(ct), Some(t)) = (int_field(d, "ct"), int_field(d, "t")) {
                    if self.team.eq_ignore_ascii_case("T") {
                        self.my_score = Some(t);
                        self.enemy_score = Some(ct);
                    } else {
                        self.my_score = Some(ct);
                        self.enemy_score = Some(t);
                    }
                }
            }
            "player.kill" | "workshop.kill" => {
                self.in_match = true;
                if self.loop_state.eq_ignore_ascii_case("MENUS") {
                    self.loop_state = "INGAME".to_string();
                }
                if let Some(tot) = int_field(d, "total") {
                    self.kills = tot;
                } else {
                    let c = int_field(d, "count").unwrap_or(1).max(1);
                    self.kills = self.kills.saturating_add(c);
                }
                if let Some(rk) = int_field(d, "round_kills") {
                    if rk > self.best_multikill {
                        self.best_multikill = rk;
                    }
                }
                if let Some(sk) = int_field(d, "streak_kills") {
                    if sk > self.best_multikill {
                        self.best_multikill = sk;
                    }
                }
                if let Some(h) = str_field(d, "hero")
                    .or_else(|| str_field(d, "legend"))
                    .or_else(|| str_field(d, "operator"))
                {
                    if !h.is_empty() {
                        self.hero = h;
                    }
                }
            }
            "player.death" => {
                if let Some(tot) = int_field(d, "total") {
                    self.deaths = tot;
                } else {
                    let c = int_field(d, "count").unwrap_or(1).max(1);
                    self.deaths = self.deaths.saturating_add(c);
                }
            }
            "player.assist" => {
                if let Some(tot) = int_field(d, "total") {
                    self.assists = tot;
                } else {
                    let c = int_field(d, "count").unwrap_or(1).max(1);
                    self.assists = self.assists.saturating_add(c);
                }
            }
            "player.mvp" | "match.mvp" => {
                self.in_match = true;
                if self.loop_state.eq_ignore_ascii_case("MENUS") {
                    self.loop_state = "INGAME".to_string();
                }
                if let Some(tot) = int_field(d, "total") {
                    self.mvps = tot;
                } else {
                    self.mvps = self.mvps.saturating_add(1);
                }
            }
            "player.multikill" => {
                if let Some(c) = int_field(d, "count") {
                    if c > self.best_multikill {
                        self.best_multikill = c;
                    }
                }
            }
            "player.ace" => {
                self.in_match = true;
                if self.loop_state.eq_ignore_ascii_case("MENUS") {
                    self.loop_state = "INGAME".to_string();
                }
                self.aces = self.aces.saturating_add(1);
                if self.best_multikill < 5 {
                    self.best_multikill = 5;
                }
            }
            "player.clutch" | "player.round_multikill_win" => {
                self.clutches = self.clutches.saturating_add(1);
            }
            "hero.level_up" => {
                if let Some(lvl) = int_field(d, "level") {
                    self.level = lvl;
                }
                if let Some(h) = str_field(d, "hero") {
                    if !h.is_empty() {
                        self.hero = h;
                    }
                }
            }
            "sport.goal" | "goal_scored" => {
                self.in_match = true;
                if self.loop_state.eq_ignore_ascii_case("MENUS") {
                    self.loop_state = "INGAME".to_string();
                }
                if let Some(g) = int_field(d, "goals") {
                    self.goals = g;
                    self.my_score = Some(g);
                } else {
                    let c = int_field(d, "count").unwrap_or(1).max(1);
                    self.goals = self.goals.saturating_add(c);
                }
                if let Some(ga) = int_field(d, "goals_against") {
                    self.enemy_score = Some(ga);
                }
                if let Some(m) = str_field(d, "map").or_else(|| str_field(d, "stadium")) {
                    let clean_m = format_clean_map_name(ev_game, &m);
                    if !clean_m.is_empty() {
                        self.map = clean_m;
                    }
                }
            }
            "sport.goal_conceded" => {
                self.in_match = true;
                if self.loop_state.eq_ignore_ascii_case("MENUS") {
                    self.loop_state = "INGAME".to_string();
                }
                if let Some(gf) = int_field(d, "goals_for") {
                    self.my_score = Some(gf);
                }
                if let Some(ga) = int_field(d, "goals_against") {
                    self.enemy_score = Some(ga);
                } else {
                    let c = int_field(d, "count").unwrap_or(1).max(1);
                    self.enemy_score = Some(self.enemy_score.unwrap_or(0).saturating_add(c));
                }
            }
            "sport.hat_trick" => {
                self.in_match = true;
                if self.loop_state.eq_ignore_ascii_case("MENUS") {
                    self.loop_state = "INGAME".to_string();
                }
                self.aces = self.aces.saturating_add(1);
                if self.goals < 3 {
                    self.goals = 3;
                }
            }
            "sport.epic_save" => {
                if let Some(s) = int_field(d, "saves") {
                    self.saves = s;
                } else {
                    let c = int_field(d, "count").unwrap_or(1).max(1);
                    self.saves = self.saves.saturating_add(c);
                }
            }
            _ => {}
        }

        *self != before
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
}

fn int_field(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn title_case_words(input: &str) -> String {
    input
        .split(|c: char| c == '_' || c == '-' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => {
                    let mut s = first.to_uppercase().collect::<String>();
                    s.push_str(&chars.as_str().to_lowercase());
                    s
                }
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

pub fn is_non_gameplay_map(raw_map: &str) -> bool {
    let trimmed = raw_map.trim();
    if trimmed.is_empty() {
        return true;
    }
    let last_seg = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    let lower = last_seg.to_lowercase();
    if lower.len() < 2 || lower.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if matches!(
        lower.as_str(),
        "init"
            | "entry"
            | "start"
            | "startup"
            | "boot"
            | "preload"
            | "login"
            | "splash"
            | "title"
            | "menu"
            | "mainmenu"
            | "mainmenuv2"
            | "menumap"
            | "lobby"
            | "mp_lobby"
            | "frontend"
            | "front_end"
            | "staging"
            | "transition"
            | "transitionmap"
            | "loading"
            | "pregame"
            | "characterselect"
            | "characterselectpersistentlevel"
            | "charselect"
            | "agentselect"
            | "uilevel"
            | "none"
            | "null"
            | "empty"
            | "default"
            | "unknown"
            | "invalid"
    ) {
        return true;
    }
    lower.starts_with("init")
        || lower.contains("mainmenu")
        || lower.contains("menu_main")
        || lower.contains("menumap")
        || lower.contains("characterselect")
        || lower.contains("charselect")
        || lower.contains("agentselect")
        || lower.contains("frontend")
        || lower.contains("front_end")
        || lower.contains("mp_lobby")
        || lower.contains("_lobby")
        || lower.contains("lobby_")
        || lower.contains("canyonlands_staging")
        || lower.contains("transitionmap")
        || lower.contains("uilevel")
        || lower.contains("bootsequence")
}

pub fn format_clean_map_name(game: &str, raw_map: &str) -> String {
    let trimmed = raw_map.trim();
    if is_non_gameplay_map(trimmed) {
        return String::new();
    }
    let last_seg = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    let lower = last_seg.to_lowercase();

    let g = game.to_lowercase();
    if g == "cs2" || g.contains("counter-strike") {
        let stripped = lower
            .strip_prefix("de_")
            .or_else(|| lower.strip_prefix("cs_"))
            .or_else(|| lower.strip_prefix("ar_"))
            .or_else(|| lower.strip_prefix("dz_"))
            .unwrap_or(&lower);
        return match stripped {
            "dust2" => "Dust II".into(),
            "mirage" => "Mirage".into(),
            "inferno" => "Inferno".into(),
            "nuke" => "Nuke".into(),
            "overpass" => "Overpass".into(),
            "vertigo" => "Vertigo".into(),
            "ancient" => "Ancient".into(),
            "anubis" => "Anubis".into(),
            "train" => "Train".into(),
            "cache" => "Cache".into(),
            "cobblestone" | "cbble" => "Cobblestone".into(),
            "office" => "Office".into(),
            "italy" => "Italy".into(),
            "baggage" => "Baggage".into(),
            "shoots" => "Shoots".into(),
            "pool_day" => "Pool Day".into(),
            "thera" => "Thera".into(),
            "mills" => "Mills".into(),
            "edin" => "Edin".into(),
            "basalt" => "Basalt".into(),
            "palais" => "Palais".into(),
            "whistle" => "Whistle".into(),
            other => title_case_words(other),
        };
    }

    if g == "valorant" {
        let base = lower
            .trim_end_matches("_p")
            .trim_end_matches("_v2")
            .trim_end_matches("v2")
            .trim_end_matches("_art")
            .trim_end_matches("_lighting");
        if base == "ascent" || lower.contains("ascent") {
            return "Ascent".into();
        }
        if base == "triad" || base == "haven" || lower.contains("triad") || lower.contains("haven") {
            return "Haven".into();
        }
        if base == "duality" || base == "bind" || lower.contains("duality") || lower.contains("bind") {
            return "Bind".into();
        }
        if base == "bonsai" || base == "split" || lower.contains("bonsai") || lower.contains("split") {
            return "Split".into();
        }
        if base == "port" || base == "icebox" || lower.contains("icebox") {
            return "Icebox".into();
        }
        if base == "foxtrot" || base == "breeze" || lower.contains("foxtrot") || lower.contains("breeze") {
            return "Breeze".into();
        }
        if base == "canyon" || base == "fracture" || lower.contains("canyon") || lower.contains("fracture") {
            return "Fracture".into();
        }
        if base == "pitt" || base == "pearl" || lower.contains("pitt") || lower.contains("pearl") {
            return "Pearl".into();
        }
        if base == "jam" || base == "lotus" || lower.contains("lotus") {
            return "Lotus".into();
        }
        if base == "juliett" || base == "sunset" || lower.contains("juliett") || lower.contains("sunset") {
            return "Sunset".into();
        }
        if base == "pummit" || base == "summit" {
            return "Summit".into();
        }
        if base == "infinity" || base == "abyss" || lower.contains("infinity") || lower.contains("abyss") {
            return "Abyss".into();
        }
        if base == "rook" || base == "corrode" || lower.contains("corrode") {
            return "Corrode".into();
        }
        if base == "poveglia" || base == "range" || base == "shootingrange" || lower.contains("poveglia") || lower.contains("shootingrange") {
            return "The Range".into();
        }
        if base == "hurm_alley" || base == "district" || lower.contains("hurm_alley") || lower.contains("district") {
            return "District".into();
        }
        if base == "hurm_bowl" || base == "kasbah" || lower.contains("hurm_bowl") || lower.contains("kasbah") {
            return "Kasbah".into();
        }
        if base == "hurm_yard" || base == "piazza" || lower.contains("hurm_yard") || lower.contains("piazza") {
            return "Piazza".into();
        }
        if base == "hurm_helix" || base == "drift" || lower.contains("hurm_helix") || lower.contains("drift") {
            return "Drift".into();
        }
        if base == "hurm_highground"
            || base == "hurm_higherground"
            || base == "hurm_higherdground"
            || base == "glitch"
            || lower.contains("hurm_high")
            || lower.contains("glitch")
        {
            return "Glitch".into();
        }
        if lower.starts_with("skirmish") {
            return "Skirmish".into();
        }
        return String::new();
    }

    if g == "warzone" || g == "cod" || g == "call_of_duty" || g.contains("call of duty") {
        let stripped = lower
            .strip_prefix("mp_")
            .or_else(|| lower.strip_prefix("wz_"))
            .or_else(|| lower.strip_prefix("zm_"))
            .unwrap_or(&lower);
        return match stripped {
            "summit" | "pummit" => "Summit".into(),
            "nuketown" | "nuketown_2025" | "nuketown_holiday" | "nuketown24" => "Nuketown".into(),
            "shipment" => "Shipment".into(),
            "rust" => "Rust".into(),
            "terminal" => "Terminal".into(),
            "highrise" => "Highrise".into(),
            "raid" => "Raid".into(),
            "standoff" => "Standoff".into(),
            "firingrange" | "firing_range" => "Firing Range".into(),
            "hijacked" => "Hijacked".into(),
            "slums" => "Slums".into(),
            "express" => "Express".into(),
            "favela" => "Favela".into(),
            "quarry" => "Quarry".into(),
            "scrapyard" => "Scrapyard".into(),
            "crash" => "Crash".into(),
            "vacant" => "Vacant".into(),
            "urzikstan" => "Urzikstan".into(),
            "rebirth" | "rebirth_island" | "island" => "Rebirth Island".into(),
            "fortunes_keep" | "fortune" => "Fortune's Keep".into(),
            "vondel" => "Vondel".into(),
            "ashika" | "ashika_island" => "Ashika Island".into(),
            "verdansk" => "Verdansk".into(),
            "area99" | "area_99" => "Area 99".into(),
            "avalon" => "Avalon".into(),
            "babylon" => "Babylon".into(),
            "skyline" => "Skyline".into(),
            "scud" => "Scud".into(),
            "rewind" => "Rewind".into(),
            "derelict" => "Derelict".into(),
            "payback" => "Payback".into(),
            "protocol" => "Protocol".into(),
            "redcard" | "red_card" => "Red Card".into(),
            "subsonic" => "Subsonic".into(),
            "vault" => "Vault".into(),
            "vorkuta" => "Vorkuta".into(),
            other => title_case_words(other),
        };
    }

    if g == "rocket_league" || g.contains("rocket") {
        let mut base = lower.strip_suffix("_p").unwrap_or(&lower);
        for suf in [
            "_night",
            "_day",
            "_dusk",
            "_dawn",
            "_rainy",
            "_snow",
            "_snownight",
            "_snowy",
            "_winter",
            "_foggy",
            "_standard",
            "_toon",
            "_hacked",
            "_race_day",
            "_upsidedown",
            "_fireandice",
            "_hw",
            "_s",
        ] {
            if let Some(s) = base.strip_suffix(suf) {
                base = s;
            }
        }
        return match base {
            "stadium" | "dfhstadium" => "DFH Stadium".into(),
            "eurostadium" | "mannfield" => "Mannfield".into(),
            "cs" | "championsfield" => "Champions Field".into(),
            "trainstation" | "urbancentral" => "Urban Central".into(),
            "park" | "beckwithpark" => "Beckwith Park".into(),
            "utopiastadium" | "utopiacoliseum" => "Utopia Coliseum".into(),
            "neotokyo" => "Neo Tokyo".into(),
            "wasteland" => "Wasteland".into(),
            "beach" | "beachsandy" | "saltyshores" => "Salty Shores".into(),
            "farm" | "farmstead" => "Farmstead".into(),
            "chn_stadium" | "forbiddentemple" => "Forbidden Temple".into(),
            "underwater" | "aquadome" => "AquaDome".into(),
            "deadeyecanyon" | "outlaw" => "Deadeye Canyon".into(),
            "sovereignheights" | "street" => "Sovereign Heights".into(),
            "estadiovida" => "Estadio Vida".into(),
            "hoopsstadium" => "Dunk House".into(),
            "throwbackstadium" => "Throwback Stadium".into(),
            "arc_standard" | "arc" => "Starbase ARC".into(),
            "music" => "Neon Fields".into(),
            "bb" => "Champions Field (NFL)".into(),
            other => title_case_words(other),
        };
    }

    if g == "apex_legends" || g == "apex" || g.contains("apex legends") {
        if lower.contains("canyonlands") || lower.contains("kings_canyon") || lower.contains("kings canyon") {
            return "Kings Canyon".into();
        }
        if lower.contains("desertlands") || lower.contains("worlds_edge") || lower.contains("world's edge") {
            return "World's Edge".into();
        }
        if lower.contains("olympus") {
            return "Olympus".into();
        }
        if lower.contains("tropic_island") || lower.contains("storm_point") || lower.contains("storm point") {
            return "Storm Point".into();
        }
        if lower.contains("divided_moon") || lower.contains("broken_moon") || lower.contains("broken moon") {
            return "Broken Moon".into();
        }
        if lower.contains("district") || lower.contains("edistrict") {
            return "E-District".into();
        }
        if lower.contains("firing_range") || lower.contains("rr_arena") || lower.contains("firing range") {
            return "Firing Range".into();
        }
        if lower.contains("freedm") || lower.contains("thunderdome") {
            return "Thunderdome".into();
        }
    }

    if g == "rainbow_six_siege" || g == "r6" || g.contains("rainbow six") {
        return match lower.as_str() {
            "kafe" | "kafe_dostoyevsky" | "kafe dostoyevsky" => "Kafe Dostoyevsky".into(),
            "nighthaven" | "nighthaven_labs" | "nighthaven labs" => "Nighthaven Labs".into(),
            "clubhouse" | "club_house" => "Clubhouse".into(),
            "emerald" | "emerald_plains" | "emerald plains" => "Emerald Plains".into(),
            "themepark" | "theme_park" | "theme park" => "Theme Park".into(),
            other => title_case_words(other),
        };
    }

    if g == "the_finals" || g.contains("the finals") {
        if lower.contains("syshorizon") || lower.contains("sys$horizon") || lower.contains("sys_horizon") {
            return "SYS$HORIZON".into();
        }
        if lower.contains("skyway") {
            return "Skyway Stadium".into();
        }
        if lower.contains("fortune") {
            return "Fortune Stadium".into();
        }
        if lower.contains("vegas") {
            return "Las Vegas".into();
        }
        if lower.contains("monaco") {
            return "Monaco".into();
        }
        if lower.contains("seoul") {
            return "Seoul".into();
        }
        if lower.contains("kyoto") {
            return "Kyoto".into();
        }
        if lower.contains("bernal") {
            return "Bernal".into();
        }
    }

    if g == "arc_raiders" || g.contains("arc raiders") {
        if lower.contains("dam") {
            return "Dam Battlegrounds".into();
        }
        if lower.contains("buried") {
            return "Buried City".into();
        }
        if lower.contains("spaceport") {
            return "The Spaceport".into();
        }
        if lower.contains("blue_gate") || lower.contains("bluegate") || lower.contains("blue gate") {
            return "Blue Gate".into();
        }
    }

    if g == "marvel_rivals" || g.contains("marvel rivals") {
        if lower.contains("tokyo") {
            return "Tokyo 2099".into();
        }
        if lower.contains("yggsgard") || lower.contains("asgard") {
            return "Yggsgard".into();
        }
        if lower.contains("wakanda") || lower.contains("birnin") {
            return "Wakanda".into();
        }
        if lower.contains("klyntar") {
            return "Klyntar".into();
        }
        if lower.contains("hydra") {
            return "Hydra Base".into();
        }
        if lower.contains("midtown") {
            return "Midtown".into();
        }
        if lower.contains("krakoa") {
            return "Krakoa".into();
        }
    }

    if g == "fc26"
        || g == "fc27"
        || g == "efootball"
        || g.contains("fc 26")
        || g.contains("fc 27")
        || g.contains("efootball")
    {
        if lower.contains("bernabeu") {
            return "Santiago Bernabeu".into();
        }
        if lower.contains("allianz") {
            return "Allianz Arena".into();
        }
        if lower.contains("camp_nou") || lower.contains("camp nou") {
            return "Spotify Camp Nou".into();
        }
        if lower.contains("old_trafford") || lower.contains("old trafford") {
            return "Old Trafford".into();
        }
        if lower.contains("anfield") {
            return "Anfield".into();
        }
        if lower.contains("wembley") {
            return "Wembley Stadium".into();
        }
        if lower.contains("san_siro") || lower.contains("san siro") {
            return "San Siro".into();
        }
        if lower.contains("emirates") {
            return "Emirates Stadium".into();
        }
        if lower.contains("etihad") {
            return "Etihad Stadium".into();
        }
        if lower.contains("parc_des_princes") || lower.contains("parc des princes") {
            return "Parc des Princes".into();
        }
        return title_case_words(last_seg);
    }

    if lower == "pummit" || lower == "summit" || lower == "mp_summit" {
        return "Summit".into();
    }

    if trimmed.chars().any(char::is_uppercase)
        && !trimmed.contains('/')
        && !trimmed.contains('\\')
        && !trimmed.contains('_')
    {
        return trimmed.to_string();
    }

    title_case_words(last_seg)
}

pub fn format_clean_mode_name(raw_mode: &str) -> String {
    let trimmed = raw_mode.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_lowercase();
    let stripped = lower.strip_prefix("ingame_").unwrap_or(&lower);
    match stripped {
        "ingame" | "matchmaking" | "invalid" => String::new(),
        "competitive" | "comp" => "Competitive".into(),
        "premier" => "Premier".into(),
        "unrated" | "standard" => "Unrated".into(),
        "swiftplay" => "Swiftplay".into(),
        "spikerush" | "quickbomb" => "Spike Rush".into(),
        "deathmatch" | "dm" => "Deathmatch".into(),
        "hurm" | "teamdeathmatch" | "tdm" => "Team Deathmatch".into(),
        "custom" | "customgame" => "Custom".into(),
        "skirmish" => "Skirmish".into(),
        "shootingrange" | "range" => "The Range".into(),
        "scrimmage" | "wingman" => "Wingman".into(),
        "casual" => "Casual".into(),
        "escalation" | "ggteam" => "Escalation".into(),
        other => title_case_words(other),
    }
}

pub fn format_clean_hero_name(raw_hero: &str) -> String {
    let trimmed = raw_hero.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_lowercase();
    let stripped = lower.strip_prefix("npc_dota_hero_").unwrap_or(&lower);
    match stripped {
        "antimage" => "Anti-Mage".into(),
        "nevermore" => "Shadow Fiend".into(),
        "zuus" => "Zeus".into(),
        "windrunner" => "Windranger".into(),
        "wisp" => "Io".into(),
        "doom_bringer" => "Doom".into(),
        "furion" => "Nature's Prophet".into(),
        "obsidian_destroyer" => "Outworld Destroyer".into(),
        "rattletrap" => "Clockwerk".into(),
        "shredder" => "Timbersaw".into(),
        "magnataur" => "Magnus".into(),
        "skeleton_king" => "Wraith King".into(),
        "abyssal_underlord" => "Underlord".into(),
        "queenofpain" => "Queen of Pain".into(),
        "keeper_of_the_light" => "Keeper of the Light".into(),
        _ if !lower.starts_with("npc_dota_hero_") && trimmed.chars().any(char::is_uppercase) => {
            trimmed.to_string()
        }
        other => title_case_words(other),
    }
}

fn should_flex_shooter_kda(s: &SessionPresence) -> bool {
    if s.kills <= 0 {
        return false;
    }
    if s.aces > 0 || s.clutches > 0 || s.best_multikill >= 3 {
        return true;
    }
    if s.deaths == 0 && s.kills >= 2 {
        return true;
    }
    if s.kills >= 3 && s.kills > s.deaths {
        return true;
    }
    let kd = (s.kills as f64) / (s.deaths.max(1) as f64);
    s.kills >= 6 && kd >= 1.35
}

fn should_flex_moba_kda(s: &SessionPresence) -> bool {
    if s.kills <= 0 && s.assists < 3 {
        return false;
    }
    if s.aces > 0 || s.best_multikill >= 3 {
        return true;
    }
    if s.deaths == 0 && (s.kills >= 2 || s.kills + s.assists >= 4) {
        return true;
    }
    let kda = ((s.kills + s.assists) as f64) / (s.deaths.max(1) as f64);
    (s.kills >= 2 && s.kills >= s.deaths && kda >= 1.75)
        || (s.kills + s.assists >= 6 && kda >= 2.4)
}

fn format_shooter_kda_line(s: &SessionPresence) -> String {
    let base = format!("{} / {} / {}", s.kills, s.deaths, s.assists);
    let kd = (s.kills as f64) / (s.deaths.max(1) as f64);
    if s.aces > 0 {
        format!("{base} · Ace")
    } else if s.clutches > 0 {
        format!("{base} · Clutch")
    } else if kd >= 2.0 && s.kills >= 4 {
        format!("{base} · {kd:.1} KD")
    } else if s.mvps >= 3 {
        format!("{base} · {} MVP", s.mvps)
    } else if s.best_multikill >= 3 {
        format!("{base} · {}K", s.best_multikill)
    } else {
        base
    }
}

fn format_moba_kda_line(s: &SessionPresence, is_dota: bool) -> String {
    let base = format!("{} / {} / {}", s.kills, s.deaths, s.assists);
    let kda = ((s.kills + s.assists) as f64) / (s.deaths.max(1) as f64);
    if s.aces > 0 {
        if is_dota {
            format!("{base} · Rampage")
        } else {
            format!("{base} · Pentakill")
        }
    } else if s.best_multikill == 4 {
        if is_dota {
            format!("{base} · Ultra Kill")
        } else {
            format!("{base} · Quadra Kill")
        }
    } else if kda >= 3.0 && (s.kills >= 3 || s.kills + s.assists >= 6) {
        format!("{base} · {kda:.1} KDA")
    } else {
        base
    }
}

fn build_activity_payload(s: &SessionPresence, cfg: &DiscordConfig) -> Option<Value> {
    let Some(ref game) = s.active_game else {
        if !cfg.show_idle {
            return None;
        }
        let (img, text_label) = cfg.asset_for_game("idle");
        return Some(json!({
            "details": "Idle",
            "state": "Ready",
            "timestamps": {
                "start": s.started_epoch_secs
            },
            "assets": {
                "large_image": img,
                "large_text": text_label
            }
        }));
    };

    let g = game.to_lowercase();
    let (large_image, large_text) = cfg.asset_for_game(&g);
    let clean_map = format_clean_map_name(&g, &s.map);
    let clean_mode = format_clean_mode_name(&s.mode);
    let clean_hero = format_clean_hero_name(&s.hero);
    let has_score = match (s.my_score, s.enemy_score) {
        (Some(a), Some(b)) => a > 0 || b > 0,
        _ => false,
    };

    let is_in_lobby = !s.in_match
        || s.loop_state.eq_ignore_ascii_case("MENUS")
        || (clean_map.is_empty()
            && clean_hero.is_empty()
            && s.kills == 0
            && s.round == 0
            && s.goals == 0
            && s.saves == 0
            && s.level == 0
            && s.cs == 0
            && !has_score
            && !s.loop_state.eq_ignore_ascii_case("INGAME")
            && !s.loop_state.eq_ignore_ascii_case("PREGAME"));

    let (details, state) = if is_in_lobby {
        let ps_upper = s.party_state.to_uppercase();
        let is_queuing = ps_upper.contains("MATCHMAKING") || ps_upper.contains("STARTING");
        let st = if is_queuing {
            if !clean_mode.is_empty() {
                format!("In Queue · {clean_mode}")
            } else {
                "In Queue".to_string()
            }
        } else if !clean_mode.is_empty() {
            format!("Lobby · {clean_mode}")
        } else {
            "In Lobby".to_string()
        };
        (large_text.to_string(), st)
    } else if g == "cs2" || g.contains("counter-strike") {
        let det = if !clean_map.is_empty() && has_score {
            format!(
                "{clean_map} · {} - {}",
                s.my_score.unwrap_or(0),
                s.enemy_score.unwrap_or(0)
            )
        } else if !clean_map.is_empty() && !clean_mode.is_empty() {
            format!("{clean_map} · {clean_mode}")
        } else if !clean_map.is_empty() {
            clean_map
        } else {
            "Counter-Strike 2".to_string()
        };

        let st = if should_flex_shooter_kda(s) {
            format_shooter_kda_line(s)
        } else if s.round > 0 && !clean_mode.is_empty() {
            format!("Round {} · {clean_mode}", s.round)
        } else if s.round > 0 {
            format!("Round {}", s.round)
        } else if !clean_mode.is_empty() {
            clean_mode
        } else if !s.map.is_empty() {
            "In Match".to_string()
        } else {
            "In Lobby".to_string()
        };
        (det, st)
    } else if g == "valorant" {
        if s.loop_state.eq_ignore_ascii_case("PREGAME") {
            let det = if !clean_map.is_empty() && !clean_mode.is_empty() {
                format!("{clean_map} · {clean_mode}")
            } else if !clean_map.is_empty() {
                clean_map
            } else if !clean_mode.is_empty() {
                format!("VALORANT · {clean_mode}")
            } else {
                "VALORANT".to_string()
            };
            (det, "Agent Select".to_string())
        } else {
            let det = if !clean_map.is_empty() && has_score {
                format!(
                    "{clean_map} · {} - {}",
                    s.my_score.unwrap_or(0),
                    s.enemy_score.unwrap_or(0)
                )
            } else if !clean_map.is_empty() && !clean_mode.is_empty() {
                format!("{clean_map} · {clean_mode}")
            } else if !clean_map.is_empty() {
                clean_map
            } else {
                "VALORANT".to_string()
            };

            let st = if s.aces > 0 {
                format!("{} Kills · Ace", s.kills.max(5))
            } else if s.clutches > 0 && s.kills >= 1 {
                format!("{} Kills · Clutch", s.kills)
            } else if s.best_multikill >= 3 && s.kills >= 3 {
                format!("{} Kills · {}K Streak", s.kills, s.best_multikill)
            } else if s.kills >= 2 {
                format!("{} Kills", s.kills)
            } else if s.round > 1 {
                format!("Round {}", s.round)
            } else if !clean_mode.is_empty() {
                clean_mode
            } else {
                "In Match".to_string()
            };
            (det, st)
        }
    } else if g == "dota2" || g.contains("dota") {
        let det = if !clean_hero.is_empty() && s.level > 0 {
            format!("{clean_hero} · Lv {}", s.level)
        } else if !clean_hero.is_empty() {
            clean_hero
        } else {
            "Dota 2".to_string()
        };

        let st = if should_flex_moba_kda(s) {
            format_moba_kda_line(s, true)
        } else if has_score {
            format!(
                "{} - {}",
                s.my_score.unwrap_or(0),
                s.enemy_score.unwrap_or(0)
            )
        } else if !s.hero.is_empty() || !s.map.is_empty() {
            "In Match".to_string()
        } else {
            "In Lobby".to_string()
        };
        (det, st)
    } else if g == "lol" || g.contains("league of legends") {
        let det = if !clean_hero.is_empty() && s.level > 0 {
            format!("{clean_hero} · Lv {}", s.level)
        } else if !clean_hero.is_empty() {
            clean_hero
        } else {
            "Summoner's Rift".to_string()
        };

        let st = if should_flex_moba_kda(s) {
            format_moba_kda_line(s, false)
        } else if s.cs >= 20 {
            format!("{} CS", s.cs)
        } else {
            "In Match".to_string()
        };
        (det, st)
    } else if g == "rocket_league" || g.contains("rocket") {
        let det = if !clean_map.is_empty() && has_score {
            format!(
                "{clean_map} · {} - {}",
                s.my_score.unwrap_or(0),
                s.enemy_score.unwrap_or(0)
            )
        } else if !clean_map.is_empty() {
            clean_map
        } else {
            "Rocket League".to_string()
        };

        let st = if s.goals >= 3 {
            format!("{} Goals · Hat Trick", s.goals)
        } else if s.goals >= 1 && s.saves >= 1 {
            let g_word = if s.goals == 1 { "Goal" } else { "Goals" };
            let s_word = if s.saves == 1 { "Save" } else { "Saves" };
            format!("{} {g_word} · {} {s_word}", s.goals, s.saves)
        } else if s.goals > 1 {
            format!("{} Goals", s.goals)
        } else if s.goals == 1 {
            "1 Goal".to_string()
        } else if s.saves >= 2 {
            format!("{} Saves", s.saves)
        } else if s.overtime {
            "Overtime".to_string()
        } else {
            "In Match".to_string()
        };
        (det, st)
    } else if g == "fc26"
        || g == "fc27"
        || g == "efootball"
        || g.contains("fc 26")
        || g.contains("fc 27")
        || g.contains("efootball")
    {
        let det = if !clean_map.is_empty() && has_score {
            format!(
                "{clean_map} · {} - {}",
                s.my_score.unwrap_or(0),
                s.enemy_score.unwrap_or(0)
            )
        } else if !clean_map.is_empty() && !clean_mode.is_empty() {
            format!("{clean_map} · {clean_mode}")
        } else if !clean_map.is_empty() {
            clean_map
        } else if !clean_mode.is_empty() {
            format!("{large_text} · {clean_mode}")
        } else {
            large_text.to_string()
        };

        let st = if s.goals >= 3 || s.aces > 0 {
            format!("{} Goals · Hat Trick", s.goals.max(3))
        } else if s.clutches > 0 && s.goals >= 1 {
            format!("{} Goals · Clutch Winner", s.goals)
        } else if s.goals >= 1 && s.saves >= 1 {
            let g_word = if s.goals == 1 { "Goal" } else { "Goals" };
            let s_word = if s.saves == 1 { "Save" } else { "Saves" };
            format!("{} {g_word} · {} {s_word}", s.goals, s.saves)
        } else if s.goals > 1 {
            format!("{} Goals", s.goals)
        } else if s.goals == 1 {
            "1 Goal".to_string()
        } else if s.saves >= 2 {
            format!("{} Saves", s.saves)
        } else if s.overtime {
            "Extra Time".to_string()
        } else if !clean_mode.is_empty() {
            clean_mode
        } else {
            "In Match".to_string()
        };
        (det, st)
    } else if g == "apex_legends" || g == "apex" || g.contains("apex legends") {
        let det = if !clean_map.is_empty() && !clean_hero.is_empty() {
            format!("{clean_map} · {clean_hero}")
        } else if !clean_map.is_empty() && !clean_mode.is_empty() {
            format!("{clean_map} · {clean_mode}")
        } else if !clean_map.is_empty() {
            clean_map
        } else if !clean_hero.is_empty() {
            format!("Apex Legends · {clean_hero}")
        } else {
            "Apex Legends".to_string()
        };

        let st = if s.mvps > 0 {
            format!("{} Kills · Apex Champion", s.kills.max(1))
        } else if s.aces > 0 {
            format!("{} Kills · Squad Wipe", s.kills.max(3))
        } else if s.clutches > 0 && s.kills >= 1 {
            format!("{} Kills · Clutch", s.kills)
        } else if s.best_multikill >= 3 && s.kills >= 3 {
            format!("{} Kills · {}K Streak", s.kills, s.best_multikill)
        } else if s.kills >= 2 {
            format!("{} Kills", s.kills)
        } else if !clean_mode.is_empty() {
            clean_mode
        } else {
            "In Match".to_string()
        };
        (det, st)
    } else if g == "fortnite" {
        let det = if !clean_mode.is_empty() {
            format!("Fortnite · {clean_mode}")
        } else if !clean_map.is_empty() {
            clean_map
        } else {
            "Fortnite".to_string()
        };

        let st = if s.mvps > 0 {
            format!("{} Eliminations · Victory Royale", s.kills.max(1))
        } else if s.aces > 0 {
            format!("{} Eliminations · Squad Wipe", s.kills.max(4))
        } else if s.best_multikill >= 3 && s.kills >= 3 {
            format!("{} Eliminations · {}x Streak", s.kills, s.best_multikill)
        } else if s.kills >= 2 {
            format!("{} Eliminations", s.kills)
        } else if s.kills == 1 {
            "1 Elimination".to_string()
        } else {
            "In Match".to_string()
        };
        (det, st)
    } else if g == "rainbow_six_siege" || g == "r6" || g.contains("rainbow six") {
        let det = if !clean_map.is_empty() && has_score {
            format!(
                "{clean_map} · {} - {}",
                s.my_score.unwrap_or(0),
                s.enemy_score.unwrap_or(0)
            )
        } else if !clean_map.is_empty() && !clean_mode.is_empty() {
            format!("{clean_map} · {clean_mode}")
        } else if !clean_map.is_empty() {
            clean_map
        } else {
            "Rainbow Six Siege".to_string()
        };

        let hero_prefix = if !clean_hero.is_empty() {
            format!("{clean_hero} · ")
        } else {
            String::new()
        };

        let st = if s.aces > 0 {
            format!("{hero_prefix}{} Kills · Ace", s.kills.max(5))
        } else if s.clutches > 0 && s.kills >= 1 {
            format!("{hero_prefix}{} Kills · Clutch", s.kills)
        } else if s.kills >= 2 {
            format!("{hero_prefix}{} Kills", s.kills)
        } else if s.round > 0 && !clean_mode.is_empty() {
            format!("{hero_prefix}Round {} · {clean_mode}", s.round)
        } else if s.round > 0 {
            format!("{hero_prefix}Round {}", s.round)
        } else if !clean_hero.is_empty() {
            clean_hero
        } else if !clean_mode.is_empty() {
            clean_mode
        } else {
            "In Match".to_string()
        };
        (det, st)
    } else if g == "the_finals" || g.contains("the finals") {
        let det = if !clean_map.is_empty() && !clean_mode.is_empty() {
            format!("{clean_map} · {clean_mode}")
        } else if !clean_map.is_empty() {
            clean_map
        } else if !clean_mode.is_empty() {
            format!("THE FINALS · {clean_mode}")
        } else {
            "THE FINALS".to_string()
        };

        let st = if s.mvps > 0 {
            format!("{} Eliminations · Tournament Winner", s.kills.max(1))
        } else if s.aces > 0 {
            format!("{} Eliminations · Team Wipe", s.kills.max(3))
        } else if s.best_multikill >= 3 && s.kills >= 3 {
            format!("{} Eliminations · {}x Streak", s.kills, s.best_multikill)
        } else if s.kills >= 2 {
            format!("{} Eliminations", s.kills)
        } else {
            "In Match".to_string()
        };
        (det, st)
    } else if g == "arc_raiders" || g.contains("arc raiders") {
        let det = if !clean_map.is_empty() {
            format!("{clean_map} · Extraction Raid")
        } else {
            "ARC Raiders · Extraction Raid".to_string()
        };

        let st = if s.mvps > 0 {
            format!("{} Takedowns · Extracted", s.kills.max(1))
        } else if s.aces > 0 {
            format!("{} Takedowns · Squad Wipe", s.kills.max(3))
        } else if s.kills >= 2 {
            format!("{} Takedowns", s.kills)
        } else if s.kills == 1 {
            "1 Takedown".to_string()
        } else {
            "In Raid".to_string()
        };
        (det, st)
    } else if g == "marvel_rivals" || g.contains("marvel rivals") {
        let det = if !clean_hero.is_empty() && !clean_map.is_empty() {
            format!("{clean_hero} · {clean_map}")
        } else if !clean_hero.is_empty() {
            format!("Marvel Rivals · {clean_hero}")
        } else if !clean_map.is_empty() {
            clean_map
        } else {
            "Marvel Rivals".to_string()
        };

        let st = if s.aces > 0 {
            format!("{} KOs · Hexa KO!", s.kills.max(6))
        } else if s.mvps > 0 {
            format!("{} KOs · MVP", s.kills.max(1))
        } else if s.best_multikill >= 3 && s.kills >= 3 {
            format!("{} KOs · {} KO Streak", s.kills, s.best_multikill)
        } else if s.kills >= 2 {
            format!("{} KOs", s.kills)
        } else if !clean_mode.is_empty() {
            clean_mode
        } else {
            "In Match".to_string()
        };
        (det, st)
    } else {
        let default_title = large_text;
        let det = if !clean_map.is_empty() && !clean_mode.is_empty() {
            format!("{clean_map} · {clean_mode}")
        } else if !clean_map.is_empty() {
            clean_map
        } else if !clean_mode.is_empty() {
            clean_mode
        } else {
            default_title.to_string()
        };

        let st = if s.aces > 0 {
            if g.contains("overwatch") {
                format!("{} Eliminations · Team Kill", s.kills.max(5))
            } else {
                format!("{} Kills · Team Wipe", s.kills.max(4))
            }
        } else if s.clutches > 0 && s.kills >= 1 {
            format!("{} Kills · Clutch", s.kills)
        } else if s.best_multikill >= 3 && s.kills >= 3 {
            format!("{} Kills · {}K Streak", s.kills, s.best_multikill)
        } else if s.kills >= 2 {
            if g.contains("overwatch") {
                format!("{} Eliminations", s.kills)
            } else {
                format!("{} Kills", s.kills)
            }
        } else {
            "In Match".to_string()
        };
        (det, st)
    };

    let mut activity = Map::new();
    activity.insert("details".into(), json!(details));
    activity.insert("state".into(), json!(state));
    activity.insert(
        "timestamps".into(),
        json!({ "start": s.started_epoch_secs }),
    );
    activity.insert(
        "assets".into(),
        json!({
            "large_image": large_image,
            "large_text": large_text,
        }),
    );

    if let (Some(ps), Some(pm)) = (s.party_size, s.party_max) {
        if ps > 1 && pm >= ps {
            let pid_str = if s.party_id.is_empty() {
                format!("clotch_party_{g}")
            } else {
                s.party_id.clone()
            };
            activity.insert(
                "party".into(),
                json!({
                    "id": pid_str,
                    "size": [ps, pm],
                }),
            );
        }
    }

    Some(Value::Object(activity))
}

fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let _ = CloseHandle(handle);
            return true;
        }
        false
    }
    #[cfg(not(windows))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

fn resolve_effective_pid(s: &SessionPresence) -> u32 {
    if let Some(pid) = s.active_pid {
        if is_pid_alive(pid) {
            return pid;
        }
    }
    std::process::id()
}

struct DiscordIpcSocket {
    #[cfg(windows)]
    pipe: std::fs::File,
    #[cfg(unix)]
    pipe: std::os::unix::net::UnixStream,
    nonce_seq: u64,
}

impl DiscordIpcSocket {
    fn connect(app_id: &str) -> io::Result<Self> {
        #[cfg(windows)]
        {
            let mut last_err = None;
            for idx in 0..10 {
                for prefix in [r"\\.\pipe\discord-ipc-", r"\\?\pipe\discord-ipc-"] {
                    let path = format!("{prefix}{idx}");
                    if let Ok(file) = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                    {
                        let mut sock = Self {
                            pipe: file,
                            nonce_seq: 1,
                        };
                        match sock.handshake(app_id) {
                            Ok(()) => return Ok(sock),
                            Err(e) => last_err = Some(e),
                        }
                    }
                }
            }
            Err(last_err.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Discord IPC pipe not found (make sure Discord desktop app is running)",
                )
            }))
        }
        #[cfg(unix)]
        {
            let mut base_dirs = Vec::new();
            for env_key in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
                if let Some(val) = std::env::var_os(env_key) {
                    let p = PathBuf::from(val);
                    base_dirs.push(p.join("app/com.discordapp.Discord"));
                    base_dirs.push(p.join("snap.discord"));
                    base_dirs.push(p);
                }
            }
            base_dirs.push(PathBuf::from("/tmp"));
            for dir in base_dirs {
                for prefix in ["discord-ipc-", "vesktop-ipc-"] {
                    for idx in 0..10 {
                        let sock_path = dir.join(format!("{prefix}{idx}"));
                        if let Ok(stream) = std::os::unix::net::UnixStream::connect(&sock_path) {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                            let mut sock = Self {
                                pipe: stream,
                                nonce_seq: 1,
                            };
                            if sock.handshake(app_id).is_ok() {
                                return Ok(sock);
                            }
                        }
                    }
                }
            }
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Discord IPC socket not available",
            ))
        }
    }

    fn send_packet(&mut self, opcode: u32, payload: &Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&opcode.to_le_bytes());
        header[4..8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.pipe.write_all(&header)?;
        self.pipe.write_all(&bytes)?;
        self.pipe.flush()?;
        Ok(())
    }

    fn recv_packet(&mut self) -> io::Result<(u32, Value)> {
        let mut header = [0u8; 8];
        self.pipe.read_exact(&mut header)?;
        let opcode = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if len > 256 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Discord IPC response frame too large",
            ));
        }
        let mut body = vec![0u8; len];
        self.pipe.read_exact(&mut body)?;
        let val = serde_json::from_slice(&body).unwrap_or(Value::Null);
        Ok((opcode, val))
    }

    fn handshake(&mut self, app_id: &str) -> io::Result<()> {
        let hs = json!({
            "v": 1,
            "client_id": app_id,
        });
        self.send_packet(OP_HANDSHAKE, &hs)?;
        let (op, data) = self.recv_packet()?;
        if op == OP_CLOSE {
            let msg = data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Discord rejected Client ID during handshake");
            eprintln!("[DISCORD RPC ERROR] Handshake closed by Discord (app_id={app_id}): {msg}");
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, msg));
        }
        let evt = data.get("evt").and_then(Value::as_str);
        if evt == Some("ERROR") || evt != Some("READY") {
            let msg = data
                .pointer("/data/message")
                .or_else(|| data.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unexpected handshake response (expected evt=READY)");
            eprintln!("[DISCORD RPC ERROR] Handshake failed (app_id={app_id}, evt={evt:?}): {msg}");
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
        }
        let user = data
            .pointer("/data/user/username")
            .and_then(Value::as_str)
            .unwrap_or("connected");
        DISCORD_RPC_CONNECTED.store(true, std::sync::atomic::Ordering::Relaxed);
        println!("[DISCORD RPC] Connected to Discord IPC (user: {user}, app_id: {app_id})");
        Ok(())
    }

    fn send_set_activity_cmd(&mut self, pid: u32, activity: &Option<Value>) -> io::Result<Value> {
        let nonce = format!("clotch-rpc-{}", self.nonce_seq);
        self.nonce_seq = self.nonce_seq.wrapping_add(1);
        let cmd = json!({
            "cmd": "SET_ACTIVITY",
            "args": {
                "pid": pid,
                "activity": activity,
            },
            "nonce": nonce,
        });
        self.send_packet(OP_FRAME, &cmd)?;
        loop {
            let (op, data) = self.recv_packet()?;
            if op == OP_PING {
                let _ = self.send_packet(OP_PONG, &data);
                continue;
            }
            if op == OP_CLOSE {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "Discord IPC closed connection",
                ));
            }
            if data.get("cmd").and_then(Value::as_str) == Some("DISPATCH") {
                continue;
            }
            return Ok(data);
        }
    }

    fn set_activity(&mut self, pid: u32, activity: Option<Value>) -> io::Result<Option<String>> {
        let resp = self.send_set_activity_cmd(pid, &activity)?;
        if resp.get("evt").and_then(Value::as_str) == Some("ERROR") {
            if let Some(Value::Object(mut fallback_act)) = activity {
                fallback_act.remove("assets");
                fallback_act.remove("party");
                let retry_clean =
                    self.send_set_activity_cmd(pid, &Some(Value::Object(fallback_act)))?;
                if retry_clean.get("evt").and_then(Value::as_str) != Some("ERROR") {
                    let app_name = retry_clean
                        .pointer("/data/name")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    return Ok(app_name);
                }
            }
            let err_msg = resp
                .pointer("/data/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown SET_ACTIVITY error");
            eprintln!("[DISCORD RPC WARN] Discord rejected SET_ACTIVITY: {err_msg}");
            return Ok(None);
        }
        let app_name = resp
            .pointer("/data/name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(app_name)
    }
}

pub struct DiscordPresenceHandle {
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
}

impl DiscordPresenceHandle {
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.join.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }
    }
}

pub fn start_discord_presence(
    mut rx: broadcast::Receiver<Event>,
    config: DiscordConfig,
) -> DiscordPresenceHandle {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    DISCORD_RPC_ENABLED.store(config.enabled, std::sync::atomic::Ordering::Release);
    let clean_id = config.app_id.trim();
    if clean_id.len() < 17 || !clean_id.chars().all(|c| c.is_ascii_digit()) {
        eprintln!("[DISCORD RPC] Disabled: invalid or missing Discord Application ID");
        return DiscordPresenceHandle { cancel, join: None };
    }

    let (tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let forward_cancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = forward_cancel.cancelled() => break,
                res = rx.recv() => {
                    match res {
                        Ok(ev) => {
                            if tx.send(ev).is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });

    let join = tokio::task::spawn_blocking(move || {
        let mut host_socket: Option<DiscordIpcSocket> = None;
        let mut session = SessionPresence::new_idle(unix_secs());
        let mut dirty = true;
        let mut last_push = Instant::now() - Duration::from_secs(60);
        let mut last_reconnect = Instant::now() - Duration::from_secs(60);
        let mut reassert_deadline: Option<Instant> = None;
        let mut last_sent_activity: Option<Value> = None;
        let mut last_logged_summary = String::new();
        let mut warned_no_pipe = false;
        let self_pid = std::process::id();

        while !cancel_clone.is_cancelled() {
            while let Ok(ev) = worker_rx.try_recv() {
                if matches!(
                    ev.kind.as_str(),
                    "settings.discord.rich_presence"
                        | "discord.set_enabled"
                        | "settings.discord_rpc"
                ) {
                    if let Some(en) = ev
                        .data
                        .get("enabled")
                        .or_else(|| ev.data.get("discord_enabled"))
                        .and_then(Value::as_bool)
                    {
                        set_discord_rpc_enabled(en);
                        if en {
                            dirty = true;
                            last_sent_activity = None;
                        }
                    }
                    continue;
                }
                let is_game_start = matches!(ev.kind.as_str(), "game.started" | "match.start");
                if session.ingest_event(&ev) {
                    dirty = true;
                    if is_game_start {
                        reassert_deadline = Some(Instant::now() + Duration::from_secs(5));
                    }
                }
            }

            if !is_discord_rpc_enabled() {
                if let Some(mut sock) = host_socket.take() {
                    let _ = sock.set_activity(self_pid, None);
                    let _ = sock.send_packet(OP_CLOSE, &json!({}));
                    DISCORD_RPC_CONNECTED.store(false, std::sync::atomic::Ordering::Relaxed);
                    last_sent_activity = None;
                    last_logged_summary.clear();
                    println!("[DISCORD RPC] Rich Presence disabled by user settings.");
                }
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }

            let now = Instant::now();
            if let Some(deadline) = reassert_deadline {
                if now >= deadline {
                    reassert_deadline = None;
                    dirty = true;
                    last_sent_activity = None;
                }
            }

            if host_socket.is_none() && last_reconnect.elapsed() >= Duration::from_secs(5) {
                last_reconnect = now;
                match DiscordIpcSocket::connect(&config.app_id) {
                    Ok(s) => {
                        host_socket = Some(s);
                        dirty = true;
                        last_sent_activity = None;
                        warned_no_pipe = false;
                    }
                    Err(e) => {
                        if !warned_no_pipe {
                            warned_no_pipe = true;
                            eprintln!("[DISCORD RPC NOTICE] Waiting for Discord client: {e}");
                        }
                    }
                }
            }

            if let Some(ref mut sock) = host_socket {
                let min_cooldown = Duration::from_secs(5);
                let keepalive_interval = Duration::from_secs(60);
                let should_check = (dirty && last_push.elapsed() >= min_cooldown)
                    || last_push.elapsed() >= keepalive_interval;

                if should_check {
                    let activity = build_activity_payload(&session, &config);
                    if activity != last_sent_activity || last_push.elapsed() >= keepalive_interval {
                        let summary = activity
                            .as_ref()
                            .map(|a| {
                                let d = a.get("details").and_then(Value::as_str).unwrap_or("");
                                let st = a.get("state").and_then(Value::as_str).unwrap_or("");
                                format!("{d} | {st}")
                            })
                            .unwrap_or_else(|| "Cleared".to_string());

                        let effective_pid = resolve_effective_pid(&session);
                        let res_act = match sock.set_activity(effective_pid, activity.clone()) {
                            Ok(Some(name)) => Ok(Some(name)),
                            Ok(None) if effective_pid != self_pid => {
                                sock.set_activity(self_pid, activity.clone())
                            }
                            other => other,
                        };
                        match res_act {
                            Ok(app_name_opt) => {
                                if summary != last_logged_summary {
                                    let app_label =
                                        app_name_opt.unwrap_or_else(|| "Clotch".to_string());
                                    println!("[DISCORD RPC] Active ({app_label}) -> {summary}");
                                    last_logged_summary = summary;
                                }
                                last_sent_activity = activity;
                                dirty = false;
                                last_push = Instant::now();
                            }
                            Err(e) => {
                                let err_kind = match e.kind() {
                                    io::ErrorKind::ConnectionReset => "connection_reset",
                                    io::ErrorKind::BrokenPipe => "broken_pipe",
                                    io::ErrorKind::TimedOut => "timeout",
                                    _ => "io_error",
                                };
                                eprintln!(
                                    "[DISCORD RPC WARN] IPC disconnected [{err_kind}]: {e}"
                                );
                                DISCORD_RPC_CONNECTED.store(false, std::sync::atomic::Ordering::Relaxed);
                                host_socket = None;
                            }
                        }
                    } else {
                        dirty = false;
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(250));
        }

        DISCORD_RPC_CONNECTED.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(ref mut sock) = host_socket {
            let _ = sock.set_activity(self_pid, None);
        }
    });

    DiscordPresenceHandle {
        cancel,
        join: Some(join),
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::events::Confidence;

    fn dummy_event(game: &str, kind: &str, data: Value) -> Event {
        Event {
            seq: 1,
            dedup_key: None,
            at_ms: 1000,
            game: Arc::from(game),
            source: Arc::from("test"),
            kind: kind.to_string(),
            game_time: None,
            confidence: Confidence::Confirmed,
            data,
            pid: Some(4242),
        }
    }

    #[test]
    fn cs2_presence_shows_clean_map_score_and_ego_boost_kda() {
        let cfg = DiscordConfig::default();
        let mut s = SessionPresence::new_idle(1000);
        s.ingest_event(&dummy_event("cs2", "game.started", json!({ "pid": 7777 })));
        s.ingest_event(&dummy_event(
            "cs2",
            "presence.update",
            json!({
                "map": "de_mirage",
                "mode": "competitive",
                "team": "CT",
                "ct": 11,
                "t": 7,
                "rounds_played": 18,
                "kills": 20,
                "deaths": 5,
                "assists": 4,
                "mvps": 3
            }),
        ));

        let act = build_activity_payload(&s, &cfg).unwrap();
        assert_eq!(act["details"], json!("Mirage · 11 - 7"));
        assert_eq!(act["state"], json!("20 / 5 / 4 · 4.0 KD"));
        assert_eq!(act["assets"]["large_image"], json!("clotch-cs2"));
        assert!(act.get("status_display_type").is_none());
    }

    #[test]
    fn negative_kda_is_hidden_to_protect_player_ego() {
        let cfg = DiscordConfig::default();
        let mut s = SessionPresence::new_idle(1000);
        s.ingest_event(&dummy_event("cs2", "game.started", json!({ "pid": 7777 })));
        s.ingest_event(&dummy_event(
            "cs2",
            "presence.update",
            json!({
                "map": "de_dust2",
                "mode": "competitive",
                "team": "T",
                "ct": 4,
                "t": 2,
                "rounds_played": 6,
                "kills": 1,
                "deaths": 5,
                "assists": 0
            }),
        ));

        let act = build_activity_payload(&s, &cfg).unwrap();
        assert_eq!(act["details"], json!("Dust II · 2 - 4"));
        assert_eq!(act["state"], json!("Round 7 · Competitive"));
    }

    #[test]
    fn valorant_presence_includes_clean_map_party_and_kill_flex() {
        let cfg = DiscordConfig::default();
        let mut s = SessionPresence::new_idle(1000);
        s.ingest_event(&dummy_event("valorant", "game.started", json!({ "pid": 8888 })));
        s.ingest_event(&dummy_event(
            "valorant",
            "presence.update",
            json!({
                "session_loop_state": "INGAME",
                "map": "/Game/Maps/Juliett/Juliett",
                "mode": "competitive",
                "queue_id": "competitive",
                "ally_score": 8,
                "enemy_score": 4,
                "party_id": "val_party_1",
                "party_size": 3,
                "party_max": 5
            }),
        ));
        s.ingest_event(&dummy_event(
            "valorant",
            "player.kill",
            json!({ "total": 14, "round_kills": 5 }),
        ));
        s.ingest_event(&dummy_event("valorant", "player.ace", json!({})));

        let act = build_activity_payload(&s, &cfg).unwrap();
        assert_eq!(act["details"], json!("Sunset · 8 - 4"));
        assert_eq!(act["state"], json!("14 Kills · Ace"));
        assert_eq!(act["assets"]["large_image"], json!("clotch-valorant"));
        assert_eq!(act["party"]["size"], json!([3, 5]));
    }

    #[test]
    fn valorant_solo_lobby_and_queue_accurately_clear_party_and_mode() {
        let cfg = DiscordConfig::default();
        let mut s = SessionPresence::new_idle(1000);
        s.ingest_event(&dummy_event("valorant", "game.started", json!({ "pid": 8888 })));
        s.ingest_event(&dummy_event(
            "valorant",
            "presence.update",
            json!({
                "session_loop_state": "MENUS",
                "party_state": "DEFAULT",
                "mode": "unrated",
                "queue_id": "unrated",
                "party_id": "solo_party",
                "party_size": 1,
                "party_max": 5
            }),
        ));

        let act = build_activity_payload(&s, &cfg).unwrap();
        assert_eq!(act["details"], json!("VALORANT"));
        assert_eq!(act["state"], json!("Lobby · Unrated"));
        assert!(act.get("party").is_none());

        s.ingest_event(&dummy_event(
            "valorant",
            "presence.update",
            json!({
                "session_loop_state": "MENUS",
                "party_state": "MATCHMAKING",
                "mode": "unrated",
                "queue_id": "unrated",
                "party_id": "solo_party",
                "party_size": 1,
                "party_max": 5
            }),
        ));
        let act_q = build_activity_payload(&s, &cfg).unwrap();
        assert_eq!(act_q["details"], json!("VALORANT"));
        assert_eq!(act_q["state"], json!("In Queue · Unrated"));
        assert!(act_q.get("party").is_none());
    }

    #[test]
    fn apex_fortnite_r6_finals_arc_and_marvel_presence_formatting() {
        let cfg = DiscordConfig::default();

        let mut s_apex = SessionPresence::new_idle(1000);
        s_apex.ingest_event(&dummy_event("apex_legends", "game.started", json!({ "pid": 5001 })));
        s_apex.ingest_event(&dummy_event(
            "apex_legends",
            "presence.update",
            json!({
                "map": "mp_rr_desertlands_mu4",
                "mode": "Ranked Leagues",
                "hero": "Wraith",
                "kills": 6
            }),
        ));
        s_apex.ingest_event(&dummy_event("apex_legends", "player.ace", json!({})));
        let act_apex = build_activity_payload(&s_apex, &cfg).unwrap();
        assert_eq!(act_apex["details"], json!("World's Edge · Wraith"));
        assert_eq!(act_apex["state"], json!("6 Kills · Squad Wipe"));
        assert_eq!(act_apex["assets"]["large_image"], json!("clotch-apexlegend"));

        let mut s_fn = SessionPresence::new_idle(1000);
        s_fn.ingest_event(&dummy_event("fortnite", "game.started", json!({ "pid": 5002 })));
        s_fn.ingest_event(&dummy_event(
            "fortnite",
            "presence.update",
            json!({
                "mode": "Zero Build",
                "kills": 9
            }),
        ));
        s_fn.ingest_event(&dummy_event("fortnite", "player.mvp", json!({})));
        let act_fn = build_activity_payload(&s_fn, &cfg).unwrap();
        assert_eq!(act_fn["details"], json!("Fortnite · Zero Build"));
        assert_eq!(act_fn["state"], json!("9 Eliminations · Victory Royale"));

        let mut s_r6 = SessionPresence::new_idle(1000);
        s_r6.ingest_event(&dummy_event("rainbow_six_siege", "game.started", json!({ "pid": 5003 })));
        s_r6.ingest_event(&dummy_event(
            "rainbow_six_siege",
            "presence.update",
            json!({
                "map": "Clubhouse",
                "mode": "Ranked",
                "operator": "Ash",
                "round": 3,
                "kills": 5
            }),
        ));
        s_r6.ingest_event(&dummy_event("rainbow_six_siege", "player.ace", json!({})));
        let act_r6 = build_activity_payload(&s_r6, &cfg).unwrap();
        assert_eq!(act_r6["details"], json!("Clubhouse · Ranked"));
        assert_eq!(act_r6["state"], json!("Ash · 5 Kills · Ace"));

        let mut s_mr = SessionPresence::new_idle(1000);
        s_mr.ingest_event(&dummy_event("marvel_rivals", "game.started", json!({ "pid": 5004 })));
        s_mr.ingest_event(&dummy_event(
            "marvel_rivals",
            "presence.update",
            json!({
                "map": "Tokyo 2099",
                "hero": "Spider-Man",
                "mode": "Competitive",
                "kills": 14
            }),
        ));
        s_mr.ingest_event(&dummy_event("marvel_rivals", "player.ace", json!({})));
        let act_mr = build_activity_payload(&s_mr, &cfg).unwrap();
        assert_eq!(act_mr["details"], json!("Spider-Man · Tokyo 2099"));
        assert_eq!(act_mr["state"], json!("14 KOs · Hexa KO!"));
    }

    #[test]
    fn all_games_default_to_in_lobby_and_reject_init_and_translate_codenames() {
        let cfg = DiscordConfig::default();
        for game in [
            "valorant",
            "cs2",
            "dota2",
            "lol",
            "rocket_league",
            "overwatch2",
            "warzone",
            "battlefield",
            "apex_legends",
            "fortnite",
            "rainbow_six_siege",
            "the_finals",
            "arc_raiders",
            "marvel_rivals",
            "fc26",
            "fc27",
            "efootball",
        ] {
            let mut s = SessionPresence::new_idle(1000);
            s.ingest_event(&dummy_event(game, "game.started", json!({ "pid": 9001 })));
            let act = build_activity_payload(&s, &cfg).unwrap();
            assert_eq!(
                act["state"],
                json!("In Lobby"),
                "game {game} should start in In Lobby"
            );

            s.ingest_event(&dummy_event(
                game,
                "match.map_load",
                json!({ "map": "/Game/Maps/Init/Init" }),
            ));
            let act_init = build_activity_payload(&s, &cfg).unwrap();
            assert_eq!(
                act_init["state"],
                json!("In Lobby"),
                "game {game} must stay In Lobby on Init map"
            );
        }

        assert_eq!(format_clean_map_name("valorant", "Init"), "");
        assert_eq!(format_clean_map_name("valorant", "MainMenuv2"), "");
        assert_eq!(format_clean_map_name("valorant", "Pitt"), "Pearl");
        assert_eq!(format_clean_map_name("valorant", "Juliett_V2"), "Sunset");
        assert_eq!(format_clean_map_name("valorant", "Rook"), "Corrode");
        assert_eq!(format_clean_map_name("valorant", "PovegliaV2"), "The Range");
        assert_eq!(format_clean_map_name("cod", "mp_summit"), "Summit");
        assert_eq!(format_clean_map_name("cod", "pummit"), "Summit");
        assert_eq!(
            format_clean_map_name("rocket_league", "EuroStadium_Night_P"),
            "Mannfield"
        );
    }

    #[test]
    fn fc26_fc27_and_efootball_presence_assets_and_match_scores_work() {
        let cfg = DiscordConfig::default();
        assert_eq!(cfg.asset_for_game("fc26"), ("clotch-fc26", "EA SPORTS FC 26"));
        assert_eq!(cfg.asset_for_game("fc27"), ("clotch-fc27", "EA SPORTS FC 27"));
        assert_eq!(cfg.asset_for_game("efootball"), ("clotch-efootball", "eFootball"));

        let mut s_fc = SessionPresence::new_idle(1000);
        s_fc.ingest_event(&dummy_event("fc26", "game.started", json!({ "pid": 6001 })));
        s_fc.ingest_event(&dummy_event(
            "fc26",
            "match.start",
            json!({
                "map": "Santiago Bernabeu",
                "mode": "Ultimate Team Champions"
            }),
        ));
        s_fc.ingest_event(&dummy_event(
            "fc26",
            "sport.goal",
            json!({
                "goals": 3,
                "goals_against": 1,
                "map": "Santiago Bernabeu"
            }),
        ));
        s_fc.ingest_event(&dummy_event("fc26", "sport.hat_trick", json!({})));
        let act_fc = build_activity_payload(&s_fc, &cfg).unwrap();
        assert_eq!(act_fc["details"], json!("Santiago Bernabeu · 3 - 1"));
        assert_eq!(act_fc["state"], json!("3 Goals · Hat Trick"));
        assert_eq!(act_fc["assets"]["large_image"], json!("clotch-fc26"));
    }
}