#![allow(dead_code, unused_imports)]
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveGameStateSnapshot {
    pub active_game: String,
    pub phase: String,
    pub map: String,
    pub mode: String,
    pub hero_or_agent: String,
    pub weapon_or_loadout: String,
    pub player_identity: String,
    pub score_for: Option<i64>,
    pub score_against: Option<i64>,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub headshots: u32,
    pub bodyshots: u32,
    pub legshots: u32,
    pub damage_dealt: u32,
    pub combat_score: u32,
    pub goals: u32,
    pub saves: u32,
    pub updated_at_ms: u64,
}

pub fn resolve_live_game_state_path() -> PathBuf {
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let dir = PathBuf::from(local_appdata).join("Clotch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("live_game_state.json");
    }
    PathBuf::from("live_game_state.json")
}

pub fn persist_live_game_state_snapshot(snapshot: &LiveGameStateSnapshot) {
    let path = resolve_live_game_state_path();
    if let Ok(body) = serde_json::to_string(snapshot) {
        let _ = std::fs::write(path, body);
    }
}

pub fn load_live_game_state_snapshot() -> Option<LiveGameStateSnapshot> {
    let path = resolve_live_game_state_path();
    let raw = std::fs::read_to_string(path).ok()?;
    let snap: LiveGameStateSnapshot = serde_json::from_str(&raw).ok()?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    if now_ms.saturating_sub(snap.updated_at_ms) <= 180_000 && !snap.active_game.is_empty() {
        Some(snap)
    } else {
        None
    }
}
