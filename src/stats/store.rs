#![allow(dead_code, unused_imports)]
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::core::identity::{
    discover_all_local_identities, discover_riot_local_identity_sync,
    persist_cached_launcher_identity, LocalLauncherIdentity,
};
use crate::stats::aggregation::{compute_hit_percentages, compute_kd, compute_win_rate};
use crate::stats::models::{GamePeriodStats, MatchStatsRecord, MonthlyStatsSummary};
use crate::stats::reports::{compute_year_wrapped, YearWrappedSummary};
use crate::tracker::{
    load_live_game_state_snapshot, persist_live_game_state_snapshot, ActiveMatchAccumulator,
    Confidence, Event, LiveGameStateSnapshot,
};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn resolve_local_match_stats_path() -> PathBuf {
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let dir = PathBuf::from(local_appdata).join("Clotch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("local_match_stats.json");
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let dir = PathBuf::from(appdata).join("Clotch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("local_match_stats.json");
    }
    PathBuf::from("local_match_stats.json")
}

pub struct LocalMatchStatsStore {
    active: HashMap<String, ActiveMatchAccumulator>,
    history: Vec<MatchStatsRecord>,
    storage_path: Option<PathBuf>,
    seq: u64,
}

impl Default for LocalMatchStatsStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalMatchStatsStore {
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
            history: Vec::new(),
            storage_path: None,
            seq: 0,
        }
    }

    pub fn load_or_default(path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        let raw_history = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<MatchStatsRecord>>(&s).ok())
            .unwrap_or_default();
        let orig_len = raw_history.len();
        let history: Vec<MatchStatsRecord> = raw_history
            .into_iter()
            .filter(|r| {
                let has_real_score = r.kills > 0
                    || r.deaths > 0
                    || r.assists > 0
                    || r.goals > 0
                    || r.saves > 0;
                let is_old_pixel_corrupted = r.game.eq_ignore_ascii_case("valorant")
                    && r.deaths == 0
                    && r.assists == 0
                    && r.headshots == 0
                    && r.bodyshots == 0;
                has_real_score
                    && !is_old_pixel_corrupted
                    && r.kills <= 60
                    && r.multikills <= 20
                    && r.aces <= 5
                    && r.clutches <= 10
                    && !r.match_id.starts_with("custom_")
            })
            .collect();
        let seq = history.len() as u64;
        let mut store = Self {
            active: HashMap::new(),
            history,
            storage_path: Some(p),
            seq,
        };
        let added_from_logs = store.sync_historical_game_logs();
        if store.history.len() != orig_len || added_from_logs > 0 {
            let _ = store.save_to_disk();
        }
        store
    }

    pub fn sync_historical_game_logs(&mut self) -> usize {
        let mut added = 0usize;
        let (log_shard, _, log_puuid, _) = crate::games::detection::extract_valorant_log_metadata();

        if let Some((port, password)) = crate::games::detection::read_riot_lockfile_credentials() {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let fetched = tokio::task::block_in_place(|| {
                    rt.block_on(async {
                        let Ok(client) = reqwest::Client::builder()
                            .danger_accept_invalid_certs(true)
                            .no_proxy()
                            .timeout(Duration::from_millis(2500))
                            .build()
                        else {
                            return Vec::new();
                        };
                        let (puuid_opt, rid_opt) =
                            crate::games::adapters::fetch_riot_local_session_identity(&client, port, &password).await;
                        let eff_puuid = puuid_opt.or(log_puuid.clone());
                        if let Some(my_puuid) = eff_puuid {
                            crate::games::adapters::fetch_valorant_recent_match_details_local(
                                &client,
                                port,
                                &password,
                                &my_puuid,
                                rid_opt.as_deref(),
                            )
                            .await
                        } else {
                            Vec::new()
                        }
                    })
                });
                for match_val in fetched {
                    let ev = Event {
                        seq: 1,
                        dedup_key: None,
                        at_ms: now_ms(),
                        game: Arc::from("valorant"),
                        source: Arc::from("riot_official_api"),
                        kind: "match.official_details".to_string(),
                        game_time: None,
                        confidence: Confidence::Confirmed,
                        data: match_val,
                        pid: None,
                    };
                    if self.ingest_event_internal(&ev, false).is_some() {
                        added += 1;
                    }
                }
            }
        }

        if discover_riot_local_identity_sync().is_none() {
            if let (Some(my_puuid), false) = (log_puuid, log_shard.is_empty()) {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    tokio::task::block_in_place(|| {
                        rt.block_on(async {
                            if let Ok(client) = reqwest::Client::builder()
                                .danger_accept_invalid_certs(true)
                                .no_proxy()
                                .timeout(Duration::from_millis(2000))
                                .build()
                            {
                                let ns_url =
                                    format!("https://pd.{log_shard}.a.pvp.net/name-service/v2/players");
                                if let Ok(resp) = client
                                    .put(&ns_url)
                                    .json(&serde_json::json!([my_puuid]))
                                    .send()
                                    .await
                                {
                                    if let Ok(arr) = resp.json::<Vec<Value>>().await {
                                        if let Some(first) = arr.first() {
                                            let gn = first
                                                .get("GameName")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .trim();
                                            let tl = first
                                                .get("TagLine")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .trim();
                                            if !gn.is_empty() {
                                                let full_id = if tl.is_empty() {
                                                    gn.to_string()
                                                } else {
                                                    format!("{gn}#{tl}")
                                                };
                                                persist_cached_launcher_identity(
                                                    LocalLauncherIdentity {
                                                        launcher: "Riot Games".to_string(),
                                                        display_id: full_id,
                                                        account_id: my_puuid.clone(),
                                                        region_or_tag: log_shard.to_uppercase(),
                                                        online_status: "Verified Riot NameService"
                                                            .to_string(),
                                                    },
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    });
                }
            }
        }

        for r6_dir in crate::games::detection::locate_r6_match_replay_dirs() {
            let Ok(rd) = std::fs::read_dir(&r6_dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let folder_name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    let det_id = format!("r6_replay_{folder_name}");
                    if self.history.iter().any(|r| r.match_id == det_id) {
                        continue;
                    }
                    let mut r6_tracker = crate::games::event_mapping::RainbowSixTracker::new();
                    let Ok(sub_rd) = std::fs::read_dir(&path) else {
                        continue;
                    };
                    let mut rec_files: Vec<PathBuf> = sub_rd
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rec"))
                        .collect();
                    rec_files.sort();
                    let mut mtime_ms = now_ms();
                    let mut had_rounds = false;
                    for rf in rec_files {
                        if let Ok(meta) = std::fs::metadata(&rf) {
                            if let Ok(mod_t) = meta.modified() {
                                if let Ok(dur) = mod_t.duration_since(std::time::UNIX_EPOCH) {
                                    mtime_ms = dur.as_millis() as u64;
                                }
                            }
                        }
                        if let Ok(bytes) = std::fs::read(&rf) {
                            let rname = rf.file_stem().and_then(|s| s.to_str()).unwrap_or("round");
                            for ev in r6_tracker.ingest_replay_round(&folder_name, rname, &bytes) {
                                had_rounds = true;
                                let synthetic = Event {
                                    seq: 1,
                                    dedup_key: ev.dedup_key,
                                    at_ms: mtime_ms,
                                    game: Arc::from("rainbow_six_siege"),
                                    source: Arc::from("r6_replay"),
                                    kind: ev.kind,
                                    game_time: None,
                                    confidence: ev.confidence,
                                    data: ev.data,
                                    pid: None,
                                };
                                let _ = self.ingest_event_internal(&synthetic, false);
                            }
                        }
                    }
                    if had_rounds {
                        if let Some(mut rec) = self.finalize_game_internal("rainbow_six_siege", mtime_ms, None, false) {
                            rec.match_id = det_id;
                            if let Some(last) = self.history.last_mut() {
                                *last = rec;
                            }
                            added += 1;
                        }
                    }
                }
            }
        }

        added
    }

    pub fn save_to_disk(&self) -> io::Result<()> {
        let Some(ref p) = self.storage_path else {
            return Ok(());
        };
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let body = serde_json::to_string_pretty(&self.history)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, p)?;
        Ok(())
    }

    pub fn history(&self) -> &[MatchStatsRecord] {
        &self.history
    }

    pub fn matches_for_game(&self, game: &str, limit: usize) -> Vec<MatchStatsRecord> {
        let g_low = game.trim().to_lowercase();
        self.history
            .iter()
            .rev()
            .filter(|r| r.game.eq_ignore_ascii_case(&g_low))
            .take(limit.max(1))
            .cloned()
            .collect()
    }

    pub fn active_match_snapshot(&self, game: &str) -> Option<MatchStatsRecord> {
        let g_low = game.trim().to_lowercase();
        let acc = self.active.get(&g_low)?;
        let cur_ms = now_ms().max(acc.started_at_ms);
        let (hs, bs, ls, _, _, _) =
            compute_hit_percentages(acc.headshots, acc.bodyshots, acc.legshots, acc.kills);
        Some(MatchStatsRecord {
            match_id: format!("live_{}", acc.game),
            game: acc.game.clone(),
            map: if acc.map.is_empty() { "Unknown".into() } else { acc.map.clone() },
            mode: if acc.mode.is_empty() { "Standard".into() } else { acc.mode.clone() },
            hero_or_team: acc.hero_or_team.clone(),
            started_at_ms: acc.started_at_ms,
            ended_at_ms: cur_ms,
            duration_secs: cur_ms.saturating_sub(acc.started_at_ms) / 1000,
            kills: acc.kills,
            deaths: acc.deaths,
            assists: acc.assists,
            headshots: hs,
            bodyshots: bs,
            legshots: ls,
            damage_dealt: acc.damage_dealt,
            combat_score: acc.combat_score,
            shots_fired: acc.shots_fired,
            favorite_weapon: acc.favorite_weapon.clone(),
            player_identity: acc.player_identity.clone(),
            rank_or_tier: acc.rank_or_tier.clone(),
            multikills: acc.multikills,
            aces: acc.aces,
            clutches: acc.clutches,
            mvps: acc.mvps,
            goals: acc.goals,
            goals_conceded: acc.goals_conceded,
            saves: acc.saves,
            objectives: acc.objectives,
            won: acc.won,
            score_for: acc.score_for,
            score_against: acc.score_against,
            kd_ratio: compute_kd(acc.kills, acc.deaths),
        })
    }

    pub fn ingest_event(&mut self, ev: &Event) -> Option<MatchStatsRecord> {
        self.ingest_event_internal(ev, true)
    }

    fn ingest_event_internal(&mut self, ev: &Event, persist: bool) -> Option<MatchStatsRecord> {
        let game = ev.game.trim().to_lowercase();
        if game.is_empty() || game == "system" {
            return None;
        }
        if ev.source.as_ref() == "valorant_hud"
            || ev.data.get("via").and_then(Value::as_str) == Some("hud_pixel")
        {
            return None;
        }

        let kind = ev.kind.as_str();
        let at_ms = if ev.at_ms > 0 { ev.at_ms } else { now_ms() };

        if kind == "match.official_details" {
            let match_id = ev
                .data
                .get("match_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if match_id.is_empty() {
                return None;
            }
            let kills = ev.data.get("kills").and_then(Value::as_u64).unwrap_or(0) as u32;
            let deaths = ev.data.get("deaths").and_then(Value::as_u64).unwrap_or(0) as u32;
            let assists = ev.data.get("assists").and_then(Value::as_u64).unwrap_or(0) as u32;
            let raw_hs = ev.data.get("headshots").and_then(Value::as_u64).unwrap_or(0) as u32;
            let raw_bs = ev.data.get("bodyshots").and_then(Value::as_u64).unwrap_or(0) as u32;
            let raw_ls = ev.data.get("legshots").and_then(Value::as_u64).unwrap_or(0) as u32;
            let (hs, bs, ls, _, _, _) = compute_hit_percentages(raw_hs, raw_bs, raw_ls, kills);
            let record = MatchStatsRecord {
                match_id: match_id.clone(),
                game: game.clone(),
                map: ev
                    .data
                    .get("map")
                    .and_then(Value::as_str)
                    .unwrap_or("Standard")
                    .to_string(),
                mode: ev
                    .data
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("Competitive")
                    .to_string(),
                hero_or_team: crate::games::event_mapping::clean_hero_display_name(
                    &game,
                    ev.data.get("hero").and_then(Value::as_str).unwrap_or(""),
                ),
                started_at_ms: ev
                    .data
                    .get("started_at_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(at_ms),
                ended_at_ms: ev
                    .data
                    .get("ended_at_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(at_ms),
                duration_secs: ev
                    .data
                    .get("duration_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(1800),
                kills,
                deaths,
                assists,
                headshots: hs,
                bodyshots: bs,
                legshots: ls,
                damage_dealt: ev.data.get("damage_dealt").and_then(Value::as_u64).unwrap_or(0) as u32,
                combat_score: ev.data.get("combat_score").and_then(Value::as_u64).unwrap_or(0) as u32,
                shots_fired: hs + bs + ls,
                favorite_weapon: crate::games::event_mapping::clean_weapon_display_name(
                    ev.data.get("favorite_weapon").and_then(Value::as_str).unwrap_or(""),
                ),
                player_identity: ev
                    .data
                    .get("player_identity")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                rank_or_tier: ev
                    .data
                    .get("rank_or_tier")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                multikills: ev.data.get("multikills").and_then(Value::as_u64).unwrap_or(0) as u32,
                aces: ev.data.get("aces").and_then(Value::as_u64).unwrap_or(0) as u32,
                clutches: ev.data.get("clutches").and_then(Value::as_u64).unwrap_or(0) as u32,
                mvps: ev.data.get("mvps").and_then(Value::as_u64).unwrap_or(0) as u32,
                goals: 0,
                goals_conceded: 0,
                saves: 0,
                objectives: 0,
                won: ev.data.get("won").and_then(Value::as_bool),
                score_for: ev.data.get("ally_score").and_then(Value::as_i64),
                score_against: ev.data.get("enemy_score").and_then(Value::as_i64),
                kd_ratio: compute_kd(kills, deaths),
            };
            if let Some(pos) = self.history.iter().position(|r| r.match_id == match_id) {
                self.history[pos] = record.clone();
            } else {
                self.history.push(record.clone());
                self.history.sort_by_key(|r| r.ended_at_ms);
            }
            if persist {
                let _ = self.save_to_disk();
            }
            return Some(record);
        }

        if kind == "match.start" {
            let mut prev_finished = None;
            if let Some(existing) = self.active.get(&game) {
                if existing.has_activity {
                    prev_finished = self.finalize_game_internal(&game, at_ms, None, persist);
                }
            }
            let mut acc = ActiveMatchAccumulator {
                game: game.clone(),
                started_at_ms: at_ms,
                ..Default::default()
            };
            self.apply_metadata(&mut acc, &ev.data);
            self.active.insert(game.clone(), acc);
            if persist {
                if let Some(snap) = self.active.get(&game) {
                    Self::publish_live_snapshot(snap, "INGAME");
                }
            }
            return prev_finished;
        }

        if matches!(kind, "match.end" | "match.leave" | "game.stopped") {
            if self.active.contains_key(&game) {
                if let Some(acc) = self.active.get_mut(&game) {
                    Self::apply_metadata_static(acc, &ev.data);
                    if let Some(tk) = ev.data.get("total_kills").and_then(Value::as_u64) {
                        acc.kills = acc.kills.max(tk as u32);
                        acc.has_activity = true;
                    }
                    if let Some(gf) = ev.data.get("goals_for").and_then(Value::as_u64) {
                        acc.goals = acc.goals.max(gf as u32);
                        acc.score_for = Some(gf as i64);
                        acc.has_activity = true;
                    }
                    if let Some(ga) = ev.data.get("goals_against").and_then(Value::as_u64) {
                        acc.goals_conceded = acc.goals_conceded.max(ga as u32);
                        acc.score_against = Some(ga as i64);
                        acc.has_activity = true;
                    }
                }
                let won_opt = ev.data.get("won").and_then(Value::as_bool);
                return self.finalize_game_internal(&game, at_ms, won_opt, persist);
            }
            return None;
        }

        let acc = self.active.entry(game.clone()).or_insert_with(|| ActiveMatchAccumulator {
            game: game.clone(),
            started_at_ms: at_ms,
            ..Default::default()
        });
        Self::apply_metadata_static(acc, &ev.data);

        match kind {
            "presence.update" | "player_stats" | "match.map_load" | "update_state" => {
                if let Some(k) = ev.data.get("kills").and_then(Value::as_u64) {
                    acc.kills = acc.kills.max(k as u32);
                    if k > 0 {
                        acc.has_activity = true;
                    }
                }
                if let Some(d) = ev.data.get("deaths").and_then(Value::as_u64) {
                    acc.deaths = acc.deaths.max(d as u32);
                    if d > 0 {
                        acc.has_activity = true;
                    }
                }
                if let Some(a) = ev.data.get("assists").and_then(Value::as_u64) {
                    acc.assists = acc.assists.max(a as u32);
                    if a > 0 {
                        acc.has_activity = true;
                    }
                }
                if let Some(m) = ev.data.get("mvps").and_then(Value::as_u64) {
                    acc.mvps = acc.mvps.max(m as u32);
                }
                if let Some(cs) = ev
                    .data
                    .get("cs")
                    .or_else(|| ev.data.get("score"))
                    .and_then(Value::as_u64)
                {
                    acc.combat_score = acc.combat_score.max(cs as u32);
                }
            }
            "player.kill" => {
                let count = ev
                    .data
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as u32;
                if let Some(tot) = ev.data.get("total").and_then(Value::as_u64) {
                    acc.kills = acc.kills.max(tot as u32);
                } else {
                    acc.kills = acc.kills.saturating_add(count);
                }
                if ev.data.get("headshot").and_then(Value::as_bool) == Some(true) {
                    acc.headshots = acc.headshots.saturating_add(1);
                } else if let Some(hs_batch) = ev.data.get("headshots_in_batch").and_then(Value::as_u64) {
                    acc.headshots = acc.headshots.saturating_add(hs_batch as u32);
                    let rem = count.saturating_sub(hs_batch as u32);
                    acc.bodyshots = acc.bodyshots.saturating_add(rem);
                } else if ev.data.get("legshot").and_then(Value::as_bool) == Some(true) {
                    acc.legshots = acc.legshots.saturating_add(count);
                } else {
                    acc.bodyshots = acc.bodyshots.saturating_add(count);
                }
                if let Some(dmg) = ev
                    .data
                    .get("damage")
                    .or_else(|| ev.data.get("round_dmg"))
                    .and_then(Value::as_u64)
                {
                    acc.damage_dealt = acc.damage_dealt.max(dmg as u32);
                }
                acc.has_activity = true;
            }
            "player.death" => {
                let count = ev
                    .data
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as u32;
                if let Some(tot) = ev.data.get("total").and_then(Value::as_u64) {
                    acc.deaths = acc.deaths.max(tot as u32);
                } else {
                    acc.deaths = acc.deaths.saturating_add(count);
                }
                acc.has_activity = true;
            }
            "player.assist" => {
                let count = ev
                    .data
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as u32;
                if let Some(tot) = ev.data.get("total").and_then(Value::as_u64) {
                    acc.assists = acc.assists.max(tot as u32);
                } else {
                    acc.assists = acc.assists.saturating_add(count);
                }
                acc.has_activity = true;
            }
            "player.headshot" => {
                acc.headshots = acc.headshots.saturating_add(1);
                acc.has_activity = true;
            }
            "player.multikill" | "player.round_multikill_win" => {
                acc.multikills = acc.multikills.saturating_add(1);
                acc.has_activity = true;
            }
            "player.ace" | "sport.hat_trick" => {
                acc.aces = acc.aces.saturating_add(1);
                acc.has_activity = true;
            }
            "player.clutch" => {
                acc.clutches = acc.clutches.saturating_add(1);
                acc.has_activity = true;
            }
            "player.mvp" | "match.mvp" | "statfeed.mvp" => {
                acc.mvps = acc.mvps.saturating_add(1);
                acc.has_activity = true;
            }
            "sport.goal" | "goal_scored" => {
                let count = ev
                    .data
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as u32;
                if let Some(tot) = ev.data.get("goals").and_then(Value::as_u64) {
                    acc.goals = acc.goals.max(tot as u32);
                } else {
                    acc.goals = acc.goals.saturating_add(count);
                }
                acc.shots_fired = acc.shots_fired.max(acc.goals);
                acc.has_activity = true;
            }
            "sport.goal_conceded" => {
                let count = ev
                    .data
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as u32;
                if let Some(tot) = ev.data.get("goals_against").and_then(Value::as_u64) {
                    acc.goals_conceded = acc.goals_conceded.max(tot as u32);
                } else {
                    acc.goals_conceded = acc.goals_conceded.saturating_add(count);
                }
                acc.has_activity = true;
            }
            "sport.epic_save" => {
                let count = ev
                    .data
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as u32;
                if let Some(tot) = ev.data.get("saves").and_then(Value::as_u64) {
                    acc.saves = acc.saves.max(tot as u32);
                } else {
                    acc.saves = acc.saves.saturating_add(count);
                }
                acc.has_activity = true;
            }
            "objective.secured" | "objective.first_blood" | "bomb.planted" | "bomb.defused" => {
                acc.objectives = acc.objectives.saturating_add(1);
                acc.has_activity = true;
            }
            _ => {}
        }

        if persist {
            if let Some(snap) = self.active.get(&game) {
                Self::publish_live_snapshot(snap, "LIVE");
            }
        }

        None
    }

    fn publish_live_snapshot(acc: &ActiveMatchAccumulator, phase: &str) {
        let (hs, bs, ls, _, _, _) =
            compute_hit_percentages(acc.headshots, acc.bodyshots, acc.legshots, acc.kills);
        let snap = LiveGameStateSnapshot {
            active_game: acc.game.clone(),
            phase: phase.to_string(),
            map: acc.map.clone(),
            mode: acc.mode.clone(),
            hero_or_agent: acc.hero_or_team.clone(),
            weapon_or_loadout: acc.favorite_weapon.clone(),
            player_identity: acc.player_identity.clone(),
            score_for: acc.score_for,
            score_against: acc.score_against,
            kills: acc.kills,
            deaths: acc.deaths,
            assists: acc.assists,
            headshots: hs,
            bodyshots: bs,
            legshots: ls,
            damage_dealt: acc.damage_dealt,
            combat_score: acc.combat_score,
            goals: acc.goals,
            saves: acc.saves,
            updated_at_ms: now_ms(),
        };
        persist_live_game_state_snapshot(&snap);
    }

    fn apply_metadata(&self, acc: &mut ActiveMatchAccumulator, data: &Value) {
        Self::apply_metadata_static(acc, data);
    }

    fn apply_metadata_static(acc: &mut ActiveMatchAccumulator, data: &Value) {
        if let Some(m) = data
            .get("map")
            .or_else(|| data.get("arena"))
            .or_else(|| data.get("stadium"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            acc.map = m.to_string();
        }
        if let Some(md) = data
            .get("mode")
            .or_else(|| data.get("queue_id"))
            .or_else(|| data.get("playlist"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            acc.mode = md.to_string();
        }
        if let Some(ht) = data
            .get("hero")
            .or_else(|| data.get("champion"))
            .or_else(|| data.get("operator"))
            .or_else(|| data.get("character"))
            .or_else(|| data.get("agent"))
            .or_else(|| data.get("car"))
            .or_else(|| data.get("team"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let cleaned = crate::games::event_mapping::clean_hero_display_name(&acc.game, ht);
            if !cleaned.is_empty() {
                acc.hero_or_team = cleaned;
            }
        }
        if let Some(wp) = data
            .get("weapon")
            .or_else(|| data.get("weapon_0"))
            .or_else(|| data.get("favorite_weapon"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let cleaned_wp = crate::games::event_mapping::clean_weapon_display_name(wp);
            if !cleaned_wp.is_empty() {
                *acc.weapon_counts.entry(cleaned_wp.clone()).or_insert(0) += 1;
                if let Some((best_w, _)) = acc.weapon_counts.iter().max_by_key(|(_, c)| *c) {
                    acc.favorite_weapon = best_w.clone();
                } else {
                    acc.favorite_weapon = cleaned_wp;
                }
            }
        }
        if let Some(pid_str) = data
            .get("player_identity")
            .or_else(|| data.get("player_name"))
            .or_else(|| data.get("riot_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            acc.player_identity = pid_str.to_string();
        }
        if let Some(w) = data.get("won").and_then(Value::as_bool) {
            acc.won = Some(w);
        }
        if let (Some(ct), Some(t)) = (
            data.get("ct").and_then(Value::as_i64),
            data.get("t").and_then(Value::as_i64),
        ) {
            if acc.hero_or_team.eq_ignore_ascii_case("T") {
                acc.score_for = Some(t);
                acc.score_against = Some(ct);
            } else {
                acc.score_for = Some(ct);
                acc.score_against = Some(t);
            }
        }
        if let (Some(ally), Some(enemy)) = (
            data.get("ally_score").and_then(Value::as_i64),
            data.get("enemy_score").and_then(Value::as_i64),
        ) {
            acc.score_for = Some(ally);
            acc.score_against = Some(enemy);
        }
    }

    pub fn finalize_game(
        &mut self,
        game: &str,
        ended_at_ms: u64,
        explicit_won: Option<bool>,
    ) -> Option<MatchStatsRecord> {
        self.finalize_game_internal(game, ended_at_ms, explicit_won, true)
    }

    fn finalize_game_internal(
        &mut self,
        game: &str,
        ended_at_ms: u64,
        explicit_won: Option<bool>,
        persist: bool,
    ) -> Option<MatchStatsRecord> {
        let acc = self.active.remove(game)?;
        let total_events = acc.kills
            + acc.deaths
            + acc.assists
            + acc.goals
            + acc.goals_conceded
            + acc.saves;
        if total_events == 0 {
            return None;
        }
        self.seq = self.seq.wrapping_add(1);
        let end_ms = ended_at_ms.max(acc.started_at_ms);
        let duration_secs = (end_ms.saturating_sub(acc.started_at_ms) / 1000).max(1);
        let won = explicit_won.or(acc.won).or_else(|| {
            match (acc.score_for, acc.score_against) {
                (Some(sf), Some(sa)) if sf != sa => Some(sf > sa),
                _ => None,
            }
        });
        let effective_kills = if acc.kills > 0 { acc.kills } else { acc.goals };
        let effective_deaths = if acc.deaths > 0 {
            acc.deaths
        } else {
            acc.goals_conceded
        };
        let (hs, bs, ls, _, _, _) =
            compute_hit_percentages(acc.headshots, acc.bodyshots, acc.legshots, effective_kills);

        let det_key = format!(
            "{}_{}_{}_{}_{}",
            acc.game,
            acc.map,
            end_ms / 60_000,
            effective_kills,
            effective_deaths
        );
        if !persist && self.history.iter().any(|r| r.match_id == det_key) {
            return None;
        }

        let record = MatchStatsRecord {
            match_id: if persist {
                format!("{}_{}_{}", acc.game, end_ms, self.seq)
            } else {
                det_key
            },
            game: acc.game,
            map: if acc.map.is_empty() {
                "Standard".into()
            } else {
                acc.map
            },
            mode: if acc.mode.is_empty() {
                "Standard".into()
            } else {
                acc.mode
            },
            hero_or_team: acc.hero_or_team,
            started_at_ms: acc.started_at_ms,
            ended_at_ms: end_ms,
            duration_secs,
            kills: effective_kills,
            deaths: effective_deaths,
            assists: acc.assists,
            headshots: hs,
            bodyshots: bs,
            legshots: ls,
            damage_dealt: acc.damage_dealt,
            combat_score: acc.combat_score,
            shots_fired: if acc.shots_fired > 0 { acc.shots_fired } else { hs + bs + ls },
            favorite_weapon: acc.favorite_weapon,
            player_identity: acc.player_identity,
            rank_or_tier: acc.rank_or_tier,
            multikills: acc.multikills,
            aces: acc.aces,
            clutches: acc.clutches,
            mvps: acc.mvps,
            goals: acc.goals,
            goals_conceded: acc.goals_conceded,
            saves: acc.saves,
            objectives: acc.objectives,
            won,
            score_for: acc.score_for,
            score_against: acc.score_against,
            kd_ratio: compute_kd(effective_kills, effective_deaths),
        };

        self.history.push(record.clone());
        if self.history.len() > 1000 {
            let excess = self.history.len() - 1000;
            self.history.drain(..excess);
        }
        if persist {
            let _ = self.save_to_disk();
        }
        Some(record)
    }

    pub fn summary_for_days(&self, days: u32, reference_now_ms: u64) -> MonthlyStatsSummary {
        self.summary_for_days_filtered(days, None, reference_now_ms)
    }

    pub fn monthly_summary(&self) -> MonthlyStatsSummary {
        self.summary_for_days(30, now_ms())
    }

    pub fn add_or_update_custom_match(&mut self, mut record: MatchStatsRecord) -> MatchStatsRecord {
        let cur_ms = now_ms();
        if record.ended_at_ms == 0 {
            record.ended_at_ms = cur_ms;
        }
        if record.started_at_ms == 0 {
            let dur_ms = record.duration_secs.max(600) * 1000;
            record.started_at_ms = record.ended_at_ms.saturating_sub(dur_ms);
        }
        if record.duration_secs == 0 && record.ended_at_ms > record.started_at_ms {
            record.duration_secs = (record.ended_at_ms - record.started_at_ms) / 1000;
        }
        if record.game.trim().is_empty() {
            record.game = "valorant".to_string();
        } else {
            record.game = record.game.trim().to_lowercase();
        }
        if record.map.trim().is_empty() {
            record.map = "Standard".to_string();
        }
        if record.mode.trim().is_empty() {
            record.mode = "Competitive".to_string();
        }
        let eff_kills = if record.kills > 0 { record.kills } else { record.goals };
        let eff_deaths = if record.deaths > 0 {
            record.deaths
        } else {
            record.goals_conceded
        };
        let (hs, bs, ls, _, _, _) =
            compute_hit_percentages(record.headshots, record.bodyshots, record.legshots, eff_kills);
        record.headshots = hs;
        record.bodyshots = bs;
        record.legshots = ls;
        record.kd_ratio = compute_kd(eff_kills, eff_deaths);
        if record.won.is_none() {
            if let (Some(sf), Some(sa)) = (record.score_for, record.score_against) {
                if sf != sa {
                    record.won = Some(sf > sa);
                }
            }
        }
        if record.match_id.trim().is_empty() {
            self.seq = self.seq.wrapping_add(1);
            record.match_id = format!("{}_{}_{}", record.game, record.ended_at_ms, self.seq);
        }
        if let Some(pos) = self.history.iter().position(|r| r.match_id == record.match_id) {
            self.history[pos] = record.clone();
        } else {
            self.history.push(record.clone());
            self.history.sort_by_key(|r| r.ended_at_ms);
            if self.history.len() > 1000 {
                let excess = self.history.len() - 1000;
                self.history.drain(..excess);
            }
        }
        let _ = self.save_to_disk();
        record
    }

    pub fn delete_match(&mut self, match_id: &str) -> bool {
        let before = self.history.len();
        self.history.retain(|r| r.match_id != match_id);
        let removed = self.history.len() < before;
        if removed {
            let _ = self.save_to_disk();
        }
        removed
    }

    pub fn summary_for_days_filtered(
        &self,
        days: u32,
        game_filter: Option<&str>,
        reference_now_ms: u64,
    ) -> MonthlyStatsSummary {
        let window_days = days.max(1);
        let window_ms = (window_days as u64).saturating_mul(86_400_000);
        let cutoff_ms = reference_now_ms.saturating_sub(window_ms);
        let g_filter = game_filter
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all") && !s.eq_ignore_ascii_case("all games"))
            .map(str::to_lowercase);

        let mut summary = MonthlyStatsSummary {
            window_days,
            generated_at_ms: reference_now_ms,
            ..Default::default()
        };

        let mut overall_hero_counts: HashMap<String, u32> = HashMap::new();
        let mut overall_weapon_counts: HashMap<String, u32> = HashMap::new();
        let mut overall_map_counts: HashMap<String, u32> = HashMap::new();
        let mut combat_score_sum = 0u64;

        for rec in self.history.iter().filter(|r| r.ended_at_ms >= cutoff_ms) {
            let (rec_hs, rec_bs, rec_ls, _, _, _) =
                compute_hit_percentages(rec.headshots, rec.bodyshots, rec.legshots, rec.kills);

            let g_entry = summary
                .per_game
                .entry(rec.game.clone())
                .or_insert_with(|| GamePeriodStats {
                    game: rec.game.clone(),
                    ..Default::default()
                });
            g_entry.matches_played += 1;
            g_entry.total_duration_secs += rec.duration_secs;
            match rec.won {
                Some(true) => g_entry.wins += 1,
                Some(false) => g_entry.losses += 1,
                None => {}
            }
            g_entry.kills += rec.kills;
            g_entry.deaths += rec.deaths;
            g_entry.assists += rec.assists;
            g_entry.clutches += rec.clutches;
            g_entry.aces += rec.aces;
            g_entry.headshots += rec_hs;
            g_entry.bodyshots += rec_bs;
            g_entry.legshots += rec_ls;
            g_entry.damage_dealt += rec.damage_dealt;
            g_entry.multikills += rec.multikills;
            g_entry.mvps += rec.mvps;
            g_entry.goals += rec.goals;
            g_entry.goals_conceded += rec.goals_conceded;
            g_entry.saves += rec.saves;
            g_entry.shots_fired += rec.shots_fired;
            g_entry.objectives += rec.objectives;

            if !rec.map.is_empty() && rec.map != "Unknown" {
                *g_entry.map_counts.entry(rec.map.clone()).or_insert(0) += 1;
            }
            if !rec.hero_or_team.is_empty() {
                *g_entry.hero_counts.entry(rec.hero_or_team.clone()).or_insert(0) += 1;
            }
            if !rec.favorite_weapon.is_empty() {
                *g_entry.weapon_counts.entry(rec.favorite_weapon.clone()).or_insert(0) += 1;
            }

            if let Some(ref gf) = g_filter {
                if !rec.game.eq_ignore_ascii_case(gf) {
                    continue;
                }
            }

            summary.total_matches += 1;
            summary.total_duration_secs += rec.duration_secs;
            match rec.won {
                Some(true) => summary.wins += 1,
                Some(false) => summary.losses += 1,
                None => {}
            }
            summary.total_kills += rec.kills;
            summary.total_deaths += rec.deaths;
            summary.total_assists += rec.assists;
            summary.total_clutches += rec.clutches;
            summary.total_aces += rec.aces;
            summary.total_headshots += rec_hs;
            summary.total_bodyshots += rec_bs;
            summary.total_legshots += rec_ls;
            summary.total_damage_dealt += rec.damage_dealt;
            combat_score_sum += rec.combat_score as u64;
            summary.total_multikills += rec.multikills;
            summary.total_mvps += rec.mvps;
            summary.total_goals += rec.goals;
            summary.total_goals_conceded += rec.goals_conceded;
            summary.total_saves += rec.saves;
            summary.total_shots_fired += rec.shots_fired;
            summary.total_objectives += rec.objectives;

            if !rec.map.is_empty() && rec.map != "Unknown" {
                *overall_map_counts.entry(rec.map.clone()).or_insert(0) += 1;
            }
            if !rec.hero_or_team.is_empty() {
                *overall_hero_counts.entry(rec.hero_or_team.clone()).or_insert(0) += 1;
            }
            if !rec.favorite_weapon.is_empty() {
                *overall_weapon_counts.entry(rec.favorite_weapon.clone()).or_insert(0) += 1;
            }
        }

        summary.win_rate_pct = compute_win_rate(summary.wins, summary.losses);
        summary.overall_kd = compute_kd(summary.total_kills, summary.total_deaths);
        let (_, _, _, hs_p, bs_p, ls_p) = compute_hit_percentages(
            summary.total_headshots,
            summary.total_bodyshots,
            summary.total_legshots,
            summary.total_kills,
        );
        summary.headshot_pct = hs_p;
        summary.bodyshot_pct = bs_p;
        summary.legshot_pct = ls_p;
        if summary.total_matches > 0 {
            summary.avg_combat_score =
                ((combat_score_sum as f64 / summary.total_matches as f64) * 10.0).round() / 10.0;
        }
        if let Some((best_h, _)) = overall_hero_counts.iter().max_by_key(|(_, c)| *c) {
            summary.favorite_hero_or_team = best_h.clone();
        }
        if let Some((best_w, _)) = overall_weapon_counts.iter().max_by_key(|(_, c)| *c) {
            summary.favorite_weapon = best_w.clone();
        }
        if let Some((best_m, _)) = overall_map_counts.iter().max_by_key(|(_, c)| *c) {
            summary.favorite_map = best_m.clone();
        }

        for g_stats in summary.per_game.values_mut() {
            g_stats.win_rate_pct = compute_win_rate(g_stats.wins, g_stats.losses);
            g_stats.kd_ratio = compute_kd(g_stats.kills, g_stats.deaths);
            let (_, _, _, g_hs_p, g_bs_p, g_ls_p) = compute_hit_percentages(
                g_stats.headshots,
                g_stats.bodyshots,
                g_stats.legshots,
                g_stats.kills,
            );
            g_stats.headshot_pct = g_hs_p;
            g_stats.bodyshot_pct = g_bs_p;
            g_stats.legshot_pct = g_ls_p;
            if let Some((best_map, _)) = g_stats.map_counts.iter().max_by_key(|(_, c)| *c) {
                g_stats.favorite_map = best_map.clone();
            }
            if let Some((best_hero, _)) = g_stats.hero_counts.iter().max_by_key(|(_, c)| *c) {
                g_stats.favorite_hero_or_team = best_hero.clone();
            }
            if let Some((best_weap, _)) = g_stats.weapon_counts.iter().max_by_key(|(_, c)| *c) {
                g_stats.favorite_weapon = best_weap.clone();
            }
        }

        summary.recent_matches = self
            .history
            .iter()
            .rev()
            .filter(|r| {
                r.ended_at_ms >= cutoff_ms
                    && g_filter
                        .as_ref()
                        .map(|gf| r.game.eq_ignore_ascii_case(gf))
                        .unwrap_or(true)
            })
            .take(100)
            .cloned()
            .collect();

        summary
    }

    pub fn compute_year_wrapped(
        &self,
        year_filter: Option<i32>,
        game_filter: Option<&str>,
    ) -> YearWrappedSummary {
        compute_year_wrapped(&self.history, year_filter, game_filter)
    }

    pub fn discover_local_identities_value() -> Value {
        serde_json::to_value(discover_all_local_identities()).unwrap_or(Value::Null)
    }

    pub fn live_game_state_value() -> Value {
        serde_json::to_value(load_live_game_state_snapshot()).unwrap_or(Value::Null)
    }

    pub fn compute_year_wrapped_value(
        &self,
        year_filter: Option<i32>,
        game_filter: Option<&str>,
    ) -> Value {
        serde_json::to_value(self.compute_year_wrapped(year_filter, game_filter))
            .unwrap_or(Value::Null)
    }
}
