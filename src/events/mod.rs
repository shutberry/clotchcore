#![allow(dead_code, unused_imports)]
mod bus;
mod sources;
mod game_sources;
pub mod presets;

#[allow(unused_imports)]
pub use bus::{
    Confidence, Event, EventReader, EventSink, EventSource,
    Registration, SourceError,
};
#[allow(unused_imports)]
pub use sources::{TailRule, TailSource, UnsupportedOfficialLocalSource};
#[allow(unused_imports)]
pub use game_sources::{
    ApexLegendsSource, ApexTracker, ArcRaidersSource, ArcRaidersTracker, BattlefieldSource,
    CallOfDutySource, CallOfDutyTracker, Dota2GsiSource, EFootballSource, EaSportsFc26Source,
    EaSportsFc27Source, FootballTitle, FootballTracker, FortniteSource, FortniteTracker,
    GamePeriodStats, GsiSource, HudGameProfile, LeagueLiveClient, LocalMatchStatsStore,
    MarvelRivalsSource, MarvelRivalsTracker, MatchStatsRecord, MonthlyStatsSummary,
    Overwatch2Source, RainbowSixSiegeSource, RainbowSixTracker, RocketLeagueStats,
    TheFinalsSource, TheFinalsTracker, ValorantSource, ValorantTracker,
    enable_rocket_league_stats_api, enable_rocket_league_stats_api_with_web_port,
    install_apex_liveapi_config, locate_apex_cfg_dir, locate_arc_raiders_log_path,
    locate_cod_log_path, locate_cs2_cfg_dir, locate_dota2_cfg_dir, locate_efootball_log_path,
    locate_fc26_log_path, locate_fc27_log_path, locate_fortnite_log_path,
    locate_marvel_rivals_log_path, locate_overwatch2_log_dir, locate_overwatch2_workshop_dir,
    locate_r6_match_replay_dirs, locate_rocket_league_config_dir, locate_the_finals_log_path,
    locate_valorant_log_path, publish_nv12_hud_sample, resolve_local_match_stats_path,
    APEX_LIVEAPI_PORT, COD_EXECUTABLES, EFOOTBALL_EXECUTABLES, FC26_EXECUTABLES,
    FC27_EXECUTABLES,
};

pub(crate) use game_sources::validate_token;

#[cfg(test)]
pub(crate) use bus::{now_ms, reconcile, Slot};
#[cfg(test)]
pub(crate) use game_sources::{drain_json_stream, DotaTracker, EmittedEvent, RlTracker, Snap, Tracker, upsert_ini};

pub fn run_all_games_self_test() -> Result<Vec<(String, Vec<String>)>, String> {
    use game_sources::{
        CombatHighlightTracker, DotaTracker, RlTracker, Snap, Tracker,
        read_shared_nv12_hud_metrics,
    };
    use serde_json::{json, Map};
    use std::time::Instant;

    let mut report: Vec<(String, Vec<String>)> = Vec::new();

    {
        let mut t = Tracker::default();
        let base = json!({
            "provider": { "steamid": "76561198000000001" },
            "auth": { "token": "test" },
            "map": { "name": "de_mirage", "mode": "competitive", "phase": "live", "round": 1,
                     "team_ct": { "score": 0 }, "team_t": { "score": 0 } },
            "round": { "phase": "live", "bomb": "" },
            "player": { "steamid": "76561198000000001", "team": "CT", "activity": "playing",
                "state": { "health": 100, "flashed": 0, "round_kills": 0, "round_killhs": 0 },
                "match_stats": { "kills": 0, "deaths": 0, "assists": 0, "mvps": 0 },
                "weapons": { "weapon_0": { "name": "weapon_ak47", "state": "active" } } }
        });
        let mut kinds: Vec<String> = t
            .ingest(Snap::from_payload(&base))
            .into_iter()
            .map(|e| e.kind)
            .collect();
        for k in 1..=5 {
            let mut step = base.clone();
            step["player"]["state"]["round_kills"] = json!(k);
            step["player"]["state"]["round_killhs"] = json!(k);
            step["player"]["match_stats"]["kills"] = json!(k);
            if k == 3 {
                step["round"]["bomb"] = json!("planted");
            }
            if k == 5 {
                step["round"]["bomb"] = json!("defused");
                step["round"]["phase"] = json!("over");
                step["round"]["win_team"] = json!("CT");
                step["player"]["state"]["health"] = json!(20);
            }
            kinds.extend(
                t.ingest(Snap::from_payload(&step))
                    .into_iter()
                    .map(|e| e.kind),
            );
        }
        for req in [
            "match.start",
            "player.kill",
            "player.multikill",
            "player.ace",
            "player.clutch",
            "bomb.planted",
            "bomb.defused",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!("CS2 self-test missing expected event '{req}'"));
            }
        }
        report.push(("Counter-Strike 2 (GSI)".into(), kinds));
    }

    {
        let mut dt = DotaTracker::default();
        let mut kinds = Vec::new();
        let s1 = json!({
            "map": { "name": "dota", "game_state": "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS", "clock_time": 100.0 },
            "player": { "kills": 0, "deaths": 0, "assists": 0, "team_name": "radiant" },
            "hero": { "name": "npc_dota_hero_juggernaut", "level": 6 }
        });
        let s2 = json!({
            "map": { "name": "dota", "game_state": "DOTA_GAMERULES_STATE_POST_GAME", "win_team": "radiant", "clock_time": 110.0 },
            "player": { "kills": 5, "deaths": 0, "assists": 2, "team_name": "radiant" },
            "hero": { "name": "npc_dota_hero_juggernaut", "level": 7 }
        });
        kinds.extend(dt.ingest(&s1).into_iter().map(|e| e.kind));
        kinds.extend(dt.ingest(&s2).into_iter().map(|e| e.kind));
        for req in [
            "match.start",
            "player.kill",
            "player.multikill",
            "player.ace",
            "hero.level_up",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!("Dota 2 self-test missing expected event '{req}'"));
            }
        }
        report.push(("Dota 2 (GSI)".into(), kinds));
    }

    {
        let reader = EventReader::new(64);
        let mut rx = reader.subscribe();
        let sink = reader.sink("lol", "live_client");
        for (id, ev_name, streak) in [
            (1, "GameStart", 0),
            (2, "ChampionKill", 1),
            (3, "Multikill", 5),
            (4, "Ace", 5),
            (5, "BaronKill", 0),
            (6, "GameEnd", 0),
        ] {
            let mut m = Map::new();
            m.insert("EventID".into(), json!(id));
            m.insert("EventName".into(), json!(ev_name));
            m.insert("EventTime".into(), json!(id as f64 * 10.0));
            m.insert("KillerName".into(), json!("PlayerOne#EUW"));
            m.insert("Acer".into(), json!("PlayerOne#EUW"));
            if streak > 0 {
                m.insert("KillStreak".into(), json!(streak));
            }
            LeagueLiveClient::emit_event(&sink, &m, Some("PlayerOne"), &[], false);
        }
        let mut kinds = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            kinds.push(ev.kind);
        }
        for req in [
            "match.start",
            "player.kill",
            "player.multikill",
            "player.ace",
            "player.clutch",
            "objective.secured",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "League of Legends self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("League of Legends (LiveClient API)".into(), kinds));
    }

    {
        let reader = EventReader::new(64);
        let mut rx = reader.subscribe();
        let sink = reader.sink("rocket_league", "stats_api");
        let rl = RocketLeagueStats::new(49123);
        let mut last_state = None;
        let mut tracker = RlTracker::default();
        rl.handle(
            &sink,
            json!({
                "Event": "UpdateState",
                "Data": {
                    "Game": { "Arena": "EuroStadium_P", "TimeSeconds": 30.0, "bOvertime": false, "bHasWinner": false },
                    "Players": [{ "Name": "Pilot", "UniqueId": "u1", "Team": 0, "Goals": 0, "Saves": 0, "Demolishes": 0 }]
                }
            }),
            &mut last_state,
            &mut tracker,
        );
        rl.handle(
            &sink,
            json!({
                "Event": "UpdateState",
                "Data": {
                    "Game": { "Arena": "EuroStadium_P", "TimeSeconds": 60.0, "bOvertime": true, "bHasWinner": true, "Winner": "Blue" },
                    "Players": [{ "Name": "Pilot", "UniqueId": "u1", "Team": 0, "Goals": 2, "Saves": 1, "Demolishes": 1 }]
                }
            }),
            &mut last_state,
            &mut tracker,
        );
        let mut kinds = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            kinds.push(ev.kind);
        }
        for req in [
            "match.start",
            "sport.goal",
            "sport.epic_save",
            "player.kill",
            "match.overtime",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "Rocket League self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("Rocket League (MatchStats API)".into(), kinds));
    }

    {
        let mut vt = ValorantTracker::new();
        let mut kinds = Vec::new();
        for line in [
            "LogGameFlowStateManager: Transitioning from PreGame to InGame",
            "LogMapLoadModel: Map Name: Ascent",
            "LogVO: PlayBark AudBasePawnVOComponent Bark_Kill",
            "LogVO: PlayBark AudBasePawnVOComponent Bark_Headshot",
            "LogShooterGameState: SpikePlanted",
            "LogShooterGameState: SpikeDefused",
            "LogVO: PlayBark AudBasePawnVOComponent Bark_Ace",
            "AShooterGameState::OnRoundEnded for round '1'",
            "LogGameFlowStateManager: Transitioning from InGame to PostGame",
        ] {
            vt.combat.last_kill_at = None;
            kinds.extend(vt.ingest_line(line).into_iter().map(|e| e.kind));
        }
        for req in [
            "match.start",
            "player.kill",
            "player.headshot",
            "player.multikill",
            "player.ace",
            "player.clutch",
            "bomb.planted",
            "bomb.defused",
            "round.end",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "VALORANT self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("VALORANT (ShooterGame Log + Riot Presence)".into(), kinds));
    }

    {
        let mut ft = FortniteTracker::new();
        let mut kinds = Vec::new();
        for line in [
            "LogPlaylistManager: SetCurrentPlaylist Playlist_NoBuildBR_Solo",
            "LogGameState: Match State Changed from WaitingToStart to InProgress",
            "LogFortElimination: ConfirmedElimination LocalPlayerEliminatedEnemy",
            "LogFort: FortPlayerControllerAthena HeadshotElimination bIsHeadshot=true",
            "LogFort: FortPlayerPawnAthena EnemyDowned",
            "LogAthena: AthenaMatchReport PlayVictoryRoyale Placement: 1",
        ] {
            ft.combat.last_kill_at = None;
            kinds.extend(ft.ingest_line(line).into_iter().map(|e| e.kind));
        }
        for req in [
            "presence.update",
            "match.start",
            "player.kill",
            "player.headshot",
            "player.multikill",
            "player.mvp",
            "player.clutch",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "Fortnite self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("Fortnite (FortniteGame.log + HUD)".into(), kinds));
    }

    {
        let mut at = ApexTracker::new();
        let mut kinds = Vec::new();
        let payloads = [
            json!({
                "category": "matchSetup",
                "map": "mp_rr_desertlands_mu4",
                "playlistName": "Ranked Leagues"
            }),
            json!({
                "category": "characterSelected",
                "player": { "name": "HeaviApex", "character": "Wraith" }
            }),
            json!({
                "category": "playerDowned",
                "attacker": { "name": "HeaviApex", "character": "Wraith" },
                "victim": { "name": "Enemy1" },
                "weapon": "R-99"
            }),
            json!({
                "category": "playerKilled",
                "attacker": { "name": "HeaviApex", "character": "Wraith" },
                "victim": { "name": "Enemy1" },
                "weapon": "R-99",
                "headshot": true
            }),
            json!({
                "category": "playerKilled",
                "attacker": { "name": "HeaviApex", "character": "Wraith" },
                "victim": { "name": "Enemy2" },
                "weapon": "Wingman",
                "headshot": true
            }),
            json!({
                "category": "squadEliminated",
                "placement": 1,
                "won": true
            }),
        ];
        for p in &payloads {
            at.combat.last_kill_at = None;
            kinds.extend(at.ingest_json(p).into_iter().map(|e| e.kind));
        }
        for req in [
            "match.start",
            "presence.update",
            "player.kill",
            "player.headshot",
            "player.multikill",
            "player.clutch",
            "player.mvp",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "Apex Legends self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("Apex Legends (LiveAPI JSON + Log + HUD)".into(), kinds));
    }

    {
        let mut r6 = RainbowSixTracker::new();
        let mut kinds = Vec::new();
        kinds.extend(
            r6.ingest_replay_round(
                "Match-2026-11-05_20-00-00-10",
                "Match-2026-11-05_20-00-00-10-R01.rec",
                b"R6Replay Oregon Bomb Ash",
            )
            .into_iter()
            .map(|e| e.kind),
        );
        for line in [
            "DEFUSER_PLANTED",
            "HEADSHOT weapon=R4-C",
            "KILL weapon=R4-C",
            "ACE TeamWipe5K",
            "DEFUSER_DISABLED",
            "ROUND_END round=1 won=true",
        ] {
            r6.combat.last_kill_at = None;
            kinds.extend(r6.ingest_line(line).into_iter().map(|e| e.kind));
        }
        for req in [
            "match.start",
            "round.start",
            "bomb.planted",
            "player.kill",
            "player.headshot",
            "player.multikill",
            "player.ace",
            "bomb.defused",
            "player.clutch",
            "round.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "Rainbow Six Siege self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("Rainbow Six Siege (MatchReplay .rec + Log + HUD)".into(), kinds));
    }

    {
        let mut cod = CallOfDutyTracker::new();
        let mut kinds = Vec::new();
        for line in [
            "MatchStart map=Rebirth_Island mode=Resurgence",
            "RoundStart round=1",
            "BombPlanted",
            "PlayerKill weapon=XM4",
            "HeadshotKill weapon=LR_762",
            "EnemyDowned TeamWipe",
            "FrenzyKill",
            "BombDefused",
            "WarzoneVictory",
        ] {
            cod.combat.last_kill_at = None;
            kinds.extend(cod.ingest_line(line).into_iter().map(|e| e.kind));
        }
        for req in [
            "match.start",
            "round.start",
            "bomb.planted",
            "player.kill",
            "player.headshot",
            "player.multikill",
            "player.ace",
            "player.clutch",
            "bomb.defused",
            "player.mvp",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "Call of Duty self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("Call of Duty / Warzone / BO6 / MW3 (Hybrid)".into(), kinds));
    }

    {
        let mut tf = TheFinalsTracker::new();
        let mut kinds = Vec::new();
        for line in [
            "LoadMap: Skyway_Stadium",
            "LogGameState: Match State Changed from WaitingToStart to InProgress",
            "LogDiscovery: PlayerEliminated CoinScatter",
            "LogDiscovery: HeadshotElimination",
            "LogDiscovery: CashoutStolen",
            "LogDiscovery: TeamWipe",
            "LogDiscovery: TournamentWon",
        ] {
            tf.combat.last_kill_at = None;
            kinds.extend(tf.ingest_line(line).into_iter().map(|e| e.kind));
        }
        for req in [
            "match.start",
            "player.kill",
            "player.headshot",
            "player.multikill",
            "objective.secured",
            "player.clutch",
            "player.mvp",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "THE FINALS self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("THE FINALS (Discovery UE5 Log + HUD)".into(), kinds));
    }

    {
        let mut arc = ArcRaidersTracker::new();
        let mut kinds = Vec::new();
        for line in [
            "RaidStarted zone=Buried_City",
            "RaiderEliminated",
            "ArcDestroyed",
            "QueenDestroyed",
            "ExtractionSuccessful",
        ] {
            arc.combat.last_kill_at = None;
            kinds.extend(arc.ingest_line(line).into_iter().map(|e| e.kind));
        }
        for req in [
            "match.start",
            "player.kill",
            "player.multikill",
            "objective.secured",
            "player.clutch",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "ARC Raiders self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("ARC Raiders (PioneerGame UE5 Log + HUD)".into(), kinds));
    }

    {
        let mut mr = MarvelRivalsTracker::new();
        let mut kinds = Vec::new();
        for line in [
            "HeroSelected=Spider-Man",
            "MatchStart map=Tokyo_2099 mode=Convoy",
            "PlayerKO hero=Spider-Man",
            "HeadshotKO",
            "HexaKO",
            "MatchMVP",
            "MatchEnd won=true",
        ] {
            mr.combat.last_kill_at = None;
            kinds.extend(mr.ingest_line(line).into_iter().map(|e| e.kind));
        }
        for req in [
            "presence.update",
            "match.start",
            "player.kill",
            "player.headshot",
            "player.multikill",
            "player.ace",
            "player.mvp",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "Marvel Rivals self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("Marvel Rivals (Marvel UE5 Log + HUD)".into(), kinds));
    }

    {
        let mut ow_combat = CombatHighlightTracker::default();
        ow_combat.reset_match();
        let mut kinds = vec!["match.start".to_string()];
        for i in 0..5 {
            ow_combat.last_kill_at = None;
            for ev in ow_combat.record_kill(i == 0, "workshop_and_hud", Instant::now()) {
                kinds.push(ev.kind);
            }
        }
        kinds.push("match.end".to_string());
        for req in [
            "match.start",
            "player.kill",
            "player.headshot",
            "player.multikill",
            "player.ace",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "Overwatch 2 self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("Overwatch 2 (Workshop Log + NV12 HUD)".into(), kinds));
    }

    {
        let w = 640usize;
        let h = 360usize;
        let mut nv12 = vec![24u8; (w * h * 3) / 2];
        for y in (h / 3)..((h * 2) / 3) {
            for x in (w / 3)..((w * 2) / 3) {
                nv12[y * w + x] = if (x + y) % 2 == 0 { 235 } else { 80 };
            }
        }
        publish_nv12_hud_sample(&nv12, w as u32, h as u32);
        let sampled = read_shared_nv12_hud_metrics(0.50, 0.745)
            .ok_or_else(|| "Battlefield NV12 HUD ROI sampler returned None".to_string())?;
        if sampled.center_luma <= 50.0 {
            return Err("Battlefield NV12 HUD ROI luma below expected threshold".into());
        }
        let mut bf_combat = CombatHighlightTracker::default();
        let mut kinds = Vec::new();
        for i in 0..4 {
            bf_combat.last_kill_at = None;
            for ev in bf_combat.record_kill(i == 1, "nv12_hud_skull", Instant::now()) {
                kinds.push(ev.kind);
            }
        }
        for req in ["player.kill", "player.headshot", "player.multikill"] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!(
                    "Battlefield self-test missing expected event '{req}'"
                ));
            }
        }
        report.push(("Battlefield (2042 / V / 1 / 4 NV12 HUD)".into(), kinds));
    }

    for (title, label) in [
        (FootballTitle::Fc26, "EA Sports FC 26 (Hybrid Log + HUD)"),
        (FootballTitle::Fc27, "EA Sports FC 27 (Hybrid Log + HUD)"),
        (FootballTitle::EFootball, "eFootball (Hybrid Log + HUD)"),
    ] {
        let mut ft = FootballTracker::new(title);
        let mut kinds = Vec::new();
        for line in [
            "MatchStart stadium=Santiago_Bernabeu mode=Ultimate_Team team=Real_Madrid",
            "GoalScored scorer=Mbappe minute=14",
            "GoalAssist player=Bellingham",
            "GoalConceded minute=38",
            "EpicSave",
            "GoalScored scorer=Mbappe minute=62",
            "GoalScored scorer=Mbappe minute=89",
            "ManOfTheMatch",
            "MatchEnd won=true",
        ] {
            kinds.extend(ft.ingest_line(line).into_iter().map(|e| e.kind));
        }
        for req in [
            "match.start",
            "sport.goal",
            "player.assist",
            "sport.goal_conceded",
            "player.death",
            "sport.epic_save",
            "sport.hat_trick",
            "player.ace",
            "player.clutch",
            "player.mvp",
            "match.end",
        ] {
            if !kinds.iter().any(|k| k == req) {
                return Err(format!("{label} self-test missing expected event '{req}'"));
            }
        }
        report.push((label.into(), kinds));
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::{json, Map, Value};
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::time::{sleep, timeout};
    use tokio_util::sync::CancellationToken;

    fn payload(kills: i64, round_kills: i64, hs: i64) -> Value {
        json!({
            "provider": { "steamid": "1" },
            "auth": { "token": "t" },
            "map": { "name": "de_dust2", "mode": "competitive", "phase": "live", "round": 3,
                     "team_ct": { "score": 2 }, "team_t": { "score": 1 } },
            "round": { "phase": "live" },
            "player": { "steamid": "1", "team": "CT", "activity": "playing",
                "state": { "health": 40, "flashed": 0, "round_kills": round_kills, "round_killhs": hs },
                "match_stats": { "kills": kills, "deaths": 0, "assists": 0, "mvps": 0 },
                "weapons": { "weapon_0": { "name": "weapon_knife", "state": "active" } } }
        })
    }

    #[test]
    fn all_fourteen_games_comprehensive_self_test_passes() {
        let results = run_all_games_self_test().expect("all games self-test must succeed");
        assert_eq!(results.len(), 17);
    }

    #[test]
    fn ace_is_detected_once_per_round_even_with_batched_updates() {
        let mut t = Tracker::default();
        let _ = t.ingest(Snap::from_payload(&payload(0, 0, 0)));
        let mut kinds = Vec::new();
        for k in 1..=5 {
            kinds.extend(
                t.ingest(Snap::from_payload(&payload(k, k, k)))
                    .into_iter()
                    .map(|e| e.kind),
            );
        }
        assert_eq!(kinds.iter().filter(|k| *k == "player.kill").count(), 5);
        assert_eq!(kinds.iter().filter(|k| *k == "player.ace").count(), 1);

        let more = t.ingest(Snap::from_payload(&payload(6, 5, 5)));
        assert!(!more.iter().any(|e| e.kind == "player.ace"));

        let mut t2 = Tracker::default();
        let _ = t2.ingest(Snap::from_payload(&payload(0, 0, 0)));
        let jumped = t2.ingest(Snap::from_payload(&payload(5, 5, 3)));
        assert_eq!(jumped.iter().filter(|e| e.kind == "player.ace").count(), 1);
    }

    #[test]
    fn cs2_clutch_and_round_multikill_win_are_emitted_on_round_victory() {
        let mut t = Tracker::default();
        let _ = t.ingest(Snap::from_payload(&payload(0, 0, 0)));
        let _ = t.ingest(Snap::from_payload(&payload(3, 3, 2)));
        let mut over = payload(3, 3, 2);
        over["player"]["state"]["health"] = json!(25);
        over["round"]["phase"] = json!("over");
        over["round"]["win_team"] = json!("CT");
        let events = t.ingest(Snap::from_payload(&over));
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"player.clutch"));
        assert!(kinds.contains(&"player.round_multikill_win"));
    }

    #[test]
    fn multi_kill_jump_does_not_fabricate_individual_kills_or_weapons() {
        let mut t = Tracker::default();
        let _ = t.ingest(Snap::from_payload(&payload(0, 0, 0)));
        let events = t.ingest(Snap::from_payload(&payload(3, 3, 1)));
        let kills: Vec<&EmittedEvent> = events.iter().filter(|e| e.kind == "player.kill").collect();
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0].data["count"], json!(3));
        assert_eq!(kills[0].data["total"], json!(3));
        assert_eq!(kills[0].data["headshots_in_batch"], json!(1));
        assert!(kills[0].data["headshot"].is_null());
        assert!(kills[0].data["weapon"].is_null());
    }

    #[test]
    fn map_change_and_match_reset_do_not_compare_stale_snapshots() {
        let mut t = Tracker::default();
        let _ = t.ingest(Snap::from_payload(&payload(10, 2, 1)));

        let mut next_map = payload(0, 0, 0);
        next_map["map"]["name"] = json!("de_mirage");
        let events = t.ingest(Snap::from_payload(&next_map));
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"match.leave"));
        assert!(kinds.contains(&"match.start"));
        assert!(!kinds.contains(&"player.kill"));

        let after = t.ingest(Snap::from_payload(&{
            let mut p = next_map.clone();
            p["player"]["match_stats"]["kills"] = json!(1);
            p["player"]["state"]["round_kills"] = json!(1);
            p
        }));
        assert_eq!(after.iter().filter(|e| e.kind == "player.kill").count(), 1);
    }

    #[test]
    fn missing_data_is_distinguished_from_zero() {
        let raw = json!({
            "provider": { "steamid": "1" },
            "map": { "name": "de_dust2", "phase": "gameover" },
            "player": {
                "steamid": "1",
                "team": "CT",
                "state": {}
            }
        });
        let snap = Snap::from_payload(&raw);
        let local = snap.local.as_ref().unwrap();
        assert!(local.kills.is_none());
        assert!(snap.ct.is_none());
        assert!(snap.match_won().is_none());
    }

    #[test]
    fn spectated_player_is_ignored() {
        let mut p = payload(0, 0, 0);
        p["player"]["steamid"] = json!("2");
        assert!(Snap::from_payload(&p).local.is_none());
    }

    #[test]
    fn valorant_tracker_detects_all_modes_kills_multikills_aces_and_clutches() {
        let mut vt = ValorantTracker::new();
        let e1 = vt.ingest_line("LogGameFlowStateManager: Transitioning from PreGame to InGame_CustomGame");
        assert!(e1.iter().any(|e| e.kind == "match.start"));

        let k1 = vt.ingest_line("LogVO: PlayBark AudBasePawnVOComponent Bark_Kill");
        assert!(k1.iter().any(|e| e.kind == "player.kill"));

        vt.combat.last_kill_at = None;
        let k2 = vt.ingest_line("LogVO: PlayBark AudBasePawnVOComponent Bark_Headshot");
        assert!(k2.iter().any(|e| e.kind == "player.kill"));
        assert!(k2.iter().any(|e| e.kind == "player.headshot"));
        assert!(k2.iter().any(|e| e.kind == "player.multikill" && e.data["count"] == json!(2)));

        let r_end = vt.ingest_line("AShooterGameState::OnRoundEnded for round '1'");
        assert!(r_end.iter().any(|e| e.kind == "player.clutch"));
        assert!(r_end.iter().any(|e| e.kind == "round.end"));
    }

    #[test]
    fn fortnite_tracker_detects_zero_build_eliminations_headshots_and_victory_royale() {
        let mut ft = FortniteTracker::new();
        let ev_pl = ft.ingest_line("LogPlaylistManager: SetCurrentPlaylist Playlist_NoBuildBR_Duo");
        assert!(ev_pl.iter().any(|e| e.kind == "presence.update" && e.data["mode"] == json!("Zero Build Duos")));

        let ev_st = ft.ingest_line("LogGameState: Match State Changed from WaitingToStart to InProgress");
        assert!(ev_st.iter().any(|e| e.kind == "match.start"));

        let ev_k1 = ft.ingest_line("LogFort: FortPlayerPawnAthena EnemyDowned");
        assert!(ev_k1.iter().any(|e| e.kind == "player.kill" && e.data["knockdown"] == json!(true)));

        ft.combat.last_kill_at = None;
        let ev_k2 = ft.ingest_line("LogFort: FortPlayerControllerAthena HeadshotElimination bIsHeadshot=true");
        assert!(ev_k2.iter().any(|e| e.kind == "player.kill"));
        assert!(ev_k2.iter().any(|e| e.kind == "player.headshot"));
        assert!(ev_k2.iter().any(|e| e.kind == "player.multikill"));

        let ev_win = ft.ingest_line("LogAthena: AthenaMatchReport PlayVictoryRoyale Placement: 1");
        assert!(ev_win.iter().any(|e| e.kind == "player.mvp"));
        assert!(ev_win.iter().any(|e| e.kind == "player.clutch"));
        assert!(ev_win.iter().any(|e| e.kind == "match.end" && e.data["won"] == json!(true)));
    }

    #[test]
    fn apex_tracker_parses_liveapi_json_and_squad_wipe_clutch() {
        let mut at = ApexTracker::new();
        let setup = json!({
            "category": "matchSetup",
            "map": "mp_rr_olympus",
            "playlistName": "Ranked Leagues"
        });
        let ev_setup = at.ingest_json(&setup);
        assert!(ev_setup.iter().any(|e| e.kind == "match.start" && e.data["map"] == json!("Olympus")));

        let char_sel = json!({
            "category": "characterSelected",
            "player": { "name": "ImperialHal", "character": "Horizon" }
        });
        let _ = at.ingest_json(&char_sel);

        for i in 1..=3 {
            at.combat.last_kill_at = None;
            let kill = json!({
                "category": "playerKilled",
                "attacker": { "name": "ImperialHal", "character": "Horizon" },
                "victim": { "name": format!("Enemy{i}") },
                "weapon": "R-99",
                "headshot": i == 3
            });
            let evs = at.ingest_json(&kill);
            assert!(evs.iter().any(|e| e.kind == "player.kill"));
            if i == 3 {
                assert!(evs.iter().any(|e| e.kind == "player.clutch" && e.data["squad_wipe"] == json!(true)));
            }
        }
    }

    #[test]
    fn rainbow_six_tracker_handles_rec_replay_headers_defuser_and_ace() {
        let mut r6 = RainbowSixTracker::new();
        let ev_r1 = r6.ingest_replay_round(
            "Match-2026-11-05_21-00-00-12",
            "Match-2026-11-05_21-00-00-12-R01.rec",
            b"HDR Clubhouse Bomb Jager",
        );
        assert!(ev_r1.iter().any(|e| e.kind == "match.start" && e.data["map"] == json!("Clubhouse")));
        assert!(ev_r1.iter().any(|e| e.kind == "round.start" && e.data["round"] == json!(1)));

        let ev_plant = r6.ingest_line("DEFUSER_PLANTED");
        assert!(ev_plant.iter().any(|e| e.kind == "bomb.planted"));

        for _ in 0..5 {
            r6.combat.last_kill_at = None;
            let _ = r6.ingest_line("HEADSHOT weapon=416-C");
        }
        assert!(r6.combat.ace_emitted);

        let ev_r2 = r6.ingest_replay_round(
            "Match-2026-11-05_21-00-00-12",
            "Match-2026-11-05_21-00-00-12-R02.rec",
            b"HDR Clubhouse Bomb Ash",
        );
        assert!(ev_r2.iter().any(|e| e.kind == "round.end"));
        assert!(ev_r2.iter().any(|e| e.kind == "player.clutch"));
        assert!(ev_r2.iter().any(|e| e.kind == "round.start" && e.data["round"] == json!(2)));
    }

    #[test]
    fn call_of_duty_tracker_handles_warzone_and_multiplayer_highlights() {
        let mut cod = CallOfDutyTracker::new();
        let ev_start = cod.ingest_line("MatchStart map=Nuketown mode=Hardpoint");
        assert!(ev_start.iter().any(|e| e.kind == "match.start"));

        let ev_hs = cod.ingest_line("HeadshotKill weapon=Jackal_PDW");
        assert!(ev_hs.iter().any(|e| e.kind == "player.kill"));
        assert!(ev_hs.iter().any(|e| e.kind == "player.headshot"));

        let ev_mk = cod.ingest_line("FrenzyKill");
        assert!(ev_mk.iter().any(|e| e.kind == "player.multikill" && e.data["count"] == json!(5)));
        assert!(ev_mk.iter().any(|e| e.kind == "player.ace"));

        let ev_win = cod.ingest_line("WarzoneVictory");
        assert!(ev_win.iter().any(|e| e.kind == "player.clutch"));
        assert!(ev_win.iter().any(|e| e.kind == "player.mvp"));
        assert!(ev_win.iter().any(|e| e.kind == "match.end"));
    }

    #[test]
    fn the_finals_arc_raiders_and_marvel_rivals_trackers_detect_all_highlights() {
        let mut tf = TheFinalsTracker::new();
        assert!(tf.ingest_line("LogGameState: Match State Changed from WaitingToStart to InProgress").iter().any(|e| e.kind == "match.start"));
        assert!(tf.ingest_line("CashoutStolen").iter().any(|e| e.kind == "player.clutch"));
        assert!(tf.ingest_line("TournamentWon").iter().any(|e| e.kind == "player.mvp"));

        let mut arc = ArcRaidersTracker::new();
        assert!(arc.ingest_line("RaidStarted zone=Spaceport").iter().any(|e| e.kind == "match.start"));
        assert!(arc.ingest_line("RaiderEliminated").iter().any(|e| e.kind == "player.kill"));
        assert!(arc.ingest_line("ExtractionSuccessful").iter().any(|e| e.kind == "player.clutch"));

        let mut mr = MarvelRivalsTracker::new();
        assert!(mr.ingest_line("HeroSelected=Moon Knight").iter().any(|e| e.kind == "presence.update"));
        assert!(mr.ingest_line("MatchStart map=Yggsgard mode=Convoy").iter().any(|e| e.kind == "match.start"));
        assert!(mr.ingest_line("HexaKO").iter().any(|e| e.kind == "player.ace"));
        assert!(mr.ingest_line("MatchMVP").iter().any(|e| e.kind == "player.mvp"));
    }

    #[tokio::test]
    async fn league_live_client_emits_highlight_events_for_local_player() {
        let reader = EventReader::new(64);
        let mut rx = reader.subscribe();
        let sink = reader.sink("lol", "live_client");

        let mut ev_map = Map::new();
        ev_map.insert("EventName".into(), json!("ChampionKill"));
        ev_map.insert("EventID".into(), json!(10));
        ev_map.insert("EventTime".into(), json!(120.5));
        ev_map.insert("KillerName".into(), json!("Faker#KR1"));
        ev_map.insert("VictimName".into(), json!("EnemyMid"));

        LeagueLiveClient::emit_event(&sink, &ev_map, Some("Faker"), &[], false);

        let mut kinds = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            kinds.push(ev.kind);
        }
        assert!(kinds.contains(&"champion_kill".to_string()));
        assert!(kinds.contains(&"player.kill".to_string()));
    }

    #[test]
    fn streaming_json_handles_partial_and_corrupt_frames() {
        let mut buf = Vec::new();
        buf.extend_from_slice(br#"{"Event":"GoalScored","Data":{"Player":"A"}}"#);
        buf.extend_from_slice(br#"{"Event":"StatEvent""#);

        let (msgs1, corrupt1) = drain_json_stream(&mut buf);
        assert_eq!(msgs1.len(), 1);
        assert_eq!(corrupt1, 0);
        assert!(!buf.is_empty());

        buf.extend_from_slice(br#","Data":{"Points":10}}"#);
        let (msgs2, corrupt2) = drain_json_stream(&mut buf);
        assert_eq!(msgs2.len(), 1);
        assert_eq!(corrupt2, 0);
        assert!(buf.is_empty());

        buf.extend_from_slice(b"CORRUPT_GARBAGE{\"Event\":\"MatchEnded\"}");
        let (msgs3, corrupt3) = drain_json_stream(&mut buf);
        assert_eq!(msgs3.len(), 1);
        assert!(corrupt3 >= 1);
        assert_eq!(msgs3[0]["Event"], json!("MatchEnded"));
    }

    #[tokio::test]
    async fn rocket_league_extracts_derived_update_state_and_statfeed_events() {
        let reader = EventReader::new(64);
        let mut rx = reader.subscribe();
        let sink = reader.sink("rocket_league", "stats_api");
        let rl = RocketLeagueStats::new(49123);
        let mut last_state = None;
        let mut tracker = RlTracker::default();

        let state1 = json!({
            "Event": "UpdateState",
            "Data": {
                "Game": {
                    "Arena": "Stadium_P",
                    "TimeSeconds": 120.0,
                    "bOvertime": false,
                    "bHasWinner": false,
                    "Teams": [{ "Score": 0 }, { "Score": 0 }]
                },
                "Players": [
                    {
                        "Name": "Striker",
                        "UniqueId": "p1",
                        "Team": 0,
                        "Goals": 0,
                        "Saves": 0,
                        "Speed": 1400.0,
                        "Boost": 80.0,
                        "bDemolished": false,
                        "bSupersonic": false
                    }
                ]
            }
        });
        rl.handle(&sink, state1, &mut last_state, &mut tracker);

        let state2 = json!({
            "Event": "UpdateState",
            "Data": {
                "Game": {
                    "Arena": "Stadium_P",
                    "TimeSeconds": 121.0,
                    "bOvertime": true,
                    "bHasWinner": true,
                    "Winner": "Blue",
                    "Teams": [{ "Score": 1 }, { "Score": 0 }]
                },
                "Players": [
                    {
                        "Name": "Striker",
                        "UniqueId": "p1",
                        "Team": 0,
                        "Goals": 1,
                        "Saves": 1,
                        "Speed": 2250.0,
                        "Boost": 50.0,
                        "bDemolished": true,
                        "bSupersonic": true
                    }
                ]
            }
        });
        rl.handle(&sink, state2, &mut last_state, &mut tracker);

        let statfeed = json!({
            "Event": "StatfeedEvent",
            "Data": {
                "Type": "Demolish",
                "MainTarget": { "Name": "Striker" },
                "SecondaryTarget": { "Name": "Defender" }
            }
        });
        rl.handle(&sink, statfeed, &mut last_state, &mut tracker);

        let mut kinds = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            kinds.push(ev.kind);
        }
        assert!(kinds.contains(&"match.start".to_string()));
        assert!(kinds.contains(&"match.overtime".to_string()));
        assert!(kinds.contains(&"match.end".to_string()));
        assert!(kinds.contains(&"sport.goal".to_string()));
        assert!(kinds.contains(&"sport.epic_save".to_string()));
        assert!(kinds.contains(&"player.demolished".to_string()));
        assert!(kinds.contains(&"player.supersonic".to_string()));
        assert!(kinds.contains(&"statfeed_event".to_string()));
        assert!(kinds.contains(&"statfeed.demolish".to_string()));
        assert!(kinds.contains(&"player.kill".to_string()));
    }

    #[tokio::test]
    async fn tail_source_handles_deletion_recreation_and_rotation() {
        let dir = std::env::temp_dir().join(format!("skirrmish_tail_test_{}", now_ms()));
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("game.log");
        std::fs::write(&file_path, "KILL:Alice\n").unwrap();

        let tail = TailSource::new("test_tail", &file_path)
            .from_start(true)
            .poll_every(Duration::from_millis(50))
            .rule("kill", r"KILL:(?P<who>[A-Za-z]+)")
            .unwrap();

        let reader = EventReader::new(64);
        let mut rx = reader.subscribe();
        let sink = reader.sink("test_game", "test_tail");
        let cancel = CancellationToken::new();
        let c_clone = cancel.clone();
        let handle = tokio::spawn(async move { tail.run(sink, c_clone).await });

        let ev1 = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev1.data["who"], json!("Alice"));

        std::fs::remove_file(&file_path).unwrap();
        sleep(Duration::from_millis(120)).await;
        std::fs::write(&file_path, "KILL:Bob\n").unwrap();

        let ev2 = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev2.data["who"], json!("Bob"));

        cancel.cancel();
        let _ = handle.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct ReconnectMockSource {
        runs: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_parallel: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl EventSource for ReconnectMockSource {
        fn name(&self) -> &str {
            "mock_source"
        }

        async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
            let cur_active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_parallel.fetch_max(cur_active, Ordering::SeqCst);
            let run_idx = self.runs.fetch_add(1, Ordering::SeqCst);
            sink.emit("mock.started", None, json!({ "run": run_idx }));

            let res = if run_idx == 0 {
                Err(SourceError::Other("simulated transient disconnect".into()))
            } else {
                cancel.cancelled().await;
                Ok(())
            };
            self.active.fetch_sub(1, Ordering::SeqCst);
            res
        }
    }

    #[tokio::test]
    async fn supervise_reconnects_and_reconcile_prevents_overlapping_tasks() {
        let runs = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let max_parallel = Arc::new(AtomicUsize::new(0));

        let src = ReconnectMockSource {
            runs: runs.clone(),
            active: active.clone(),
            max_parallel: max_parallel.clone(),
        };

        let reader = EventReader::new(64);
        let mut rx = reader.subscribe();
        let mut slots = vec![Slot {
            reg: Registration::new("mock_game", src).when_running(["game.exe"]),
            task: None,
        }];
        let mut live = HashSet::new();
        let parent = CancellationToken::new();

        let mut procs = HashMap::new();
        procs.insert("game.exe".to_string(), 1234);

        reconcile(&reader.bus, &mut slots, &procs, &mut live, &parent).await;

        let mut seen_runs = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && seen_runs < 2 {
            if let Ok(Ok(ev)) = timeout(Duration::from_millis(800), rx.recv()).await {
                if ev.kind == "mock.started" {
                    seen_runs += 1;
                }
            }
        }
        assert!(seen_runs >= 2);

        procs.clear();
        reconcile(&reader.bus, &mut slots, &procs, &mut live, &parent).await;
        assert_eq!(active.load(Ordering::SeqCst), 0);

        procs.insert("game.exe".to_string(), 1234);
        reconcile(&reader.bus, &mut slots, &procs, &mut live, &parent).await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!(max_parallel.load(Ordering::SeqCst), 1);

        parent.cancel();
        procs.clear();
        reconcile(&reader.bus, &mut slots, &procs, &mut live, &parent).await;
    }

    #[test]
    fn invalid_gsi_token_is_rejected_without_silent_fallback() {
        assert!(GsiSource::try_new(3000, "").is_err());
        assert!(GsiSource::try_new(3000, "bad\"token").is_err());
        assert!(presets::all("").is_err());
        let src = GsiSource::new(3000, "");
        assert!(src.config().is_err());
    }

    #[test]
    fn presets_do_not_use_passive_placeholder() {
        let reader = presets::all("secret_token").unwrap();
        assert_eq!(reader.registrations().len(), 17);
        for reg in reader.registrations() {
            assert_ne!(reg.source_name(), "passive");
            assert!(reg.is_supported());
        }
    }

    #[test]
    fn dota2_tracker_extracts_combat_and_match_events() {
        let mut t = DotaTracker::default();
        let p1 = json!({
            "map": { "name": "start", "game_state": "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS", "clock_time": 10.0 },
            "player": { "kills": 0, "deaths": 0, "assists": 0, "team_name": "radiant" },
            "hero": { "name": "npc_dota_hero_axe", "level": 1 }
        });
        let ev1 = t.ingest(&p1);
        assert!(ev1.iter().any(|e| e.kind == "match.start"));

        let p2 = json!({
            "map": { "name": "start", "game_state": "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS", "clock_time": 45.0 },
            "player": { "kills": 2, "deaths": 0, "assists": 1, "team_name": "radiant" },
            "hero": { "name": "npc_dota_hero_axe", "level": 2 }
        });
        let ev2 = t.ingest(&p2);
        assert!(ev2
            .iter()
            .any(|e| e.kind == "player.kill" && e.data["count"] == json!(2)));
        assert!(ev2
            .iter()
            .any(|e| e.kind == "player.multikill" && e.data["count"] == json!(2)));
        assert!(ev2.iter().any(|e| e.kind == "player.assist"));
        assert!(ev2.iter().any(|e| e.kind == "hero.level_up"));
    }

    #[test]
    fn ini_is_upserted_preserving_comments() {
        let a = upsert_ini("; comment=1\n", "S", "Port", "1");
        assert_eq!(a, "; comment=1\n\n[S]\nPort=1\n");
        let b = upsert_ini(&a, "S", "Port", "2");
        assert_eq!(b, "; comment=1\n\n[S]\nPort=2\n");
        let c = upsert_ini(&b, "S", "Rate", "9");
        assert!(c.contains("Rate=9") && c.contains("Port=2") && c.contains("; comment=1"));
    }

    #[tokio::test]
    async fn reader_forwards_manual_sink_events_and_deduplicates() {
        let reader = EventReader::new(16);
        let mut rx = reader.subscribe();
        let sink = reader.sink("x", "vision");
        sink.emit_detailed(
            "kill",
            Some(1.0),
            Confidence::Confirmed,
            Some("k1".into()),
            json!({ "n": 1 }),
        );
        sink.emit_detailed(
            "kill",
            Some(1.0),
            Confidence::Confirmed,
            Some("k1".into()),
            json!({ "n": 1 }),
        );
        let e = rx.recv().await.unwrap();
        assert_eq!(&*e.game, "x");
        assert_eq!(e.kind, "kill");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn filename_and_window_title_sanitization_prevents_windows_os_error_123_and_leaks() {
        use crate::recorder::{
            extract_clean_window_app_name, format_clip_filename, StorageManager,
        };

        assert_eq!(
            extract_clean_window_app_name("@badr3, eagle, eddie — Discord"),
            "Discord"
        );
        assert_eq!(
            extract_clean_window_app_name("Minecraft* 1.20.4 - Multiplayer (3rd-party Server)"),
            "Minecraft"
        );
        assert_eq!(
            extract_clean_window_app_name("Minecraft 1.21: Java Edition"),
            "Minecraft"
        );
        assert_eq!(
            StorageManager::sanitize_folder_name("Minecraft* 1.20.4: Java."),
            "Minecraft_1_20_4_Java"
        );
        assert_eq!(StorageManager::sanitize_folder_name("CON"), "Game_CON");
        assert_eq!(
            StorageManager::sanitize_folder_name(r#"<>:"/\|?*..."#),
            "UnknownGame"
        );

        let fn1 = format_clip_filename("Minecraft* 1.20: Java", "manual.clip", 123456789);
        assert_eq!(fn1, "MC_Clip_234567.mp4");

        let fn2 = format_clip_filename(r#"<>:"/\|?*"#, "...", 999);
        assert_eq!(fn2, "Game_Clip_999.mp4");
    }

    #[test]
    fn local_match_stats_store_aggregates_matches_and_monthly_summary() {
        let mut store = LocalMatchStatsStore::new();
        let t0 = 1_700_000_000_000u64;

        let ev_start = Event {
            seq: 1,
            dedup_key: None,
            at_ms: t0,
            game: Arc::from("valorant"),
            source: Arc::from("test"),
            kind: "match.start".into(),
            game_time: None,
            confidence: Confidence::Confirmed,
            data: json!({ "map": "Ascent", "mode": "Competitive" }),
            pid: None,
        };
        assert!(store.ingest_event(&ev_start).is_none());

        for (seq, kind, data) in [
            (2, "player.kill", json!({ "count": 3, "total": 3, "headshots_in_batch": 2 })),
            (3, "player.death", json!({ "count": 1, "total": 1 })),
            (4, "player.assist", json!({ "count": 2, "total": 2 })),
            (5, "player.clutch", json!({ "count": 1 })),
            (6, "player.ace", json!({ "count": 5 })),
        ] {
            let ev = Event {
                seq,
                dedup_key: None,
                at_ms: t0 + seq * 1000,
                game: Arc::from("valorant"),
                source: Arc::from("test"),
                kind: kind.into(),
                game_time: None,
                confidence: Confidence::Confirmed,
                data,
                pid: None,
            };
            assert!(store.ingest_event(&ev).is_none());
        }

        let ev_end = Event {
            seq: 7,
            dedup_key: None,
            at_ms: t0 + 600_000,
            game: Arc::from("valorant"),
            source: Arc::from("test"),
            kind: "match.end".into(),
            game_time: None,
            confidence: Confidence::Confirmed,
            data: json!({ "won": true, "ally_score": 13, "enemy_score": 9 }),
            pid: None,
        };
        let rec = store.ingest_event(&ev_end).expect("match record should finalize");
        assert_eq!(rec.game, "valorant");
        assert_eq!(rec.map, "Ascent");
        assert_eq!(rec.kills, 3);
        assert_eq!(rec.deaths, 1);
        assert_eq!(rec.assists, 2);
        assert_eq!(rec.headshots, 2);
        assert_eq!(rec.clutches, 1);
        assert_eq!(rec.aces, 1);
        assert_eq!(rec.won, Some(true));

        let monthly = store.summary_for_days(30, t0 + 605_000);
        assert_eq!(monthly.total_matches, 1);
        assert_eq!(monthly.wins, 1);
        assert_eq!(monthly.total_kills, 3);
        assert_eq!(monthly.total_deaths, 1);
        assert_eq!(monthly.total_clutches, 1);
        assert_eq!(monthly.per_game["valorant"].favorite_map, "Ascent");
    }
}