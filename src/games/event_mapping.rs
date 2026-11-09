#![allow(dead_code, unused_imports)]
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::tracker::event_bus::{now_ms, Confidence, Event, EventSink, SourceError};
use crate::games::definitions::*;

pub(crate) fn scalar(s: &str) -> serde_json::Value {
    serde_json::Value::String(s.to_string())
}

fn compile_re(pat: &str) -> Regex {
    match Regex::new(pat) {
        Ok(r) => r,
        Err(_) => match Regex::new("a^") {
            Ok(f) => f,
            Err(_) => unreachable!(),
        },
    }
}


#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Local {
    pub(crate) player_name: String,
    pub(crate) steam_id: String,
    pub(crate) team: String,
    pub(crate) health: Option<i64>,
    pub(crate) flashed: Option<i64>,
    pub(crate) round_kills: Option<i64>,
    pub(crate) round_hs: Option<i64>,
    pub(crate) round_dmg: Option<i64>,
    pub(crate) kills: Option<i64>,
    pub(crate) deaths: Option<i64>,
    pub(crate) assists: Option<i64>,
    pub(crate) mvps: Option<i64>,
    pub(crate) score: Option<i64>,
    pub(crate) weapon: Option<String>,
}

#[derive(Clone, Default)]
pub(crate) struct Snap {
    pub(crate) map: String,
    pub(crate) mode: String,
    pub(crate) map_phase: String,
    pub(crate) rounds_played: Option<i64>,
    pub(crate) ct: Option<i64>,
    pub(crate) t: Option<i64>,
    pub(crate) round_phase: String,
    pub(crate) bomb: String,
    pub(crate) win_team: String,
    pub(crate) local: Option<Local>,
}

pub(crate) fn text(v: &Value, p: &str) -> String {
    v.pointer(p)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn opt_num(v: &Value, p: &str) -> Option<i64> {
    v.pointer(p).and_then(Value::as_i64)
}

pub(crate) fn opt_f64(v: &Value, p: &str) -> Option<f64> {
    v.pointer(p).and_then(Value::as_f64)
}

pub(crate) fn opt_bool(v: &Value, p: &str) -> Option<bool> {
    match v.pointer(p)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        _ => None,
    }
}

impl Snap {
    pub(crate) fn from_payload(v: &Value) -> Self {
        let own = text(v, "/provider/steamid");
        let player_id = text(v, "/player/steamid");
        let activity = text(v, "/player/activity");
        let is_local = !own.is_empty()
            && own == player_id
            && (activity.is_empty() || activity == "playing")
            && v.pointer("/player/state").is_some();

        let local = is_local.then(|| {
            let weapon = v
                .pointer("/player/weapons")
                .and_then(Value::as_object)
                .and_then(|w| {
                    w.values()
                        .find(|x| x.get("state").and_then(Value::as_str) == Some("active"))
                })
                .and_then(|x| x.get("name").and_then(Value::as_str))
                .map(str::to_owned);

            Local {
                player_name: text(v, "/player/name"),
                steam_id: player_id.clone(),
                team: text(v, "/player/team"),
                health: opt_num(v, "/player/state/health"),
                flashed: opt_num(v, "/player/state/flashed"),
                round_kills: opt_num(v, "/player/state/round_kills"),
                round_hs: opt_num(v, "/player/state/round_killhs"),
                round_dmg: opt_num(v, "/player/state/round_totaldmg"),
                kills: opt_num(v, "/player/match_stats/kills"),
                deaths: opt_num(v, "/player/match_stats/deaths"),
                assists: opt_num(v, "/player/match_stats/assists"),
                mvps: opt_num(v, "/player/match_stats/mvps"),
                score: opt_num(v, "/player/match_stats/score"),
                weapon,
            }
        });

        Self {
            map: text(v, "/map/name"),
            mode: text(v, "/map/mode"),
            map_phase: text(v, "/map/phase"),
            rounds_played: opt_num(v, "/map/round"),
            ct: opt_num(v, "/map/team_ct/score"),
            t: opt_num(v, "/map/team_t/score"),
            round_phase: text(v, "/round/phase"),
            bomb: text(v, "/round/bomb"),
            win_team: text(v, "/round/win_team"),
            local,
        }
    }

    pub(crate) fn match_won(&self) -> Option<bool> {
        let team = &self.local.as_ref()?.team;
        let (ct, t) = (self.ct?, self.t?);
        let (mine, theirs) = match team.as_str() {
            "CT" => (ct, t),
            "T" => (t, ct),
            _ => return None,
        };
        if mine == theirs {
            None
        } else {
            Some(mine > theirs)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EmittedEvent {
    pub kind: String,
    pub confidence: Confidence,
    pub dedup_key: Option<String>,
    pub data: Value,
}

#[derive(Default)]
pub(crate) struct Tracker {
    prev: Option<Snap>,
    prev_local: Option<Local>,
    ace_emitted_round: Option<i64>,
    clutch_emitted_round: Option<i64>,
    last_multikill_round: Option<(i64, i64)>,
    round_first_kill_ms: Option<u64>,
    match_seq: u64,
}

impl Tracker {
    fn reset_match_state(&mut self) {
        self.ace_emitted_round = None;
        self.clutch_emitted_round = None;
        self.last_multikill_round = None;
        self.round_first_kill_ms = None;
        self.match_seq = self.match_seq.wrapping_add(1);
        self.prev_local = None;
    }

    pub(crate) fn ingest(&mut self, cur: Snap) -> Vec<EmittedEvent> {
        let mut out: Vec<EmittedEvent> = Vec::new();
        let Some(prev) = self.prev.take() else {
            if !cur.map.is_empty() {
                out.push(EmittedEvent {
                    kind: "match.start".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("start:{}:{}:{}", self.match_seq, cur.map, cur.mode)),
                    data: json!({ "map": cur.map, "mode": cur.mode }),
                });
            }
            self.prev_local = cur.local.clone();
            self.prev = Some(cur);
            return out;
        };

        let mut push =
            |kind: &str, confidence: Confidence, dedup_key: Option<String>, data: Value| {
                out.push(EmittedEvent {
                    kind: kind.to_owned(),
                    confidence,
                    dedup_key,
                    data,
                });
            };

        let map_changed = prev.map != cur.map;
        let match_restarted = !map_changed
            && !cur.map.is_empty()
            && ((prev.map_phase == "gameover" && cur.map_phase != "gameover")
                || (matches!((prev.rounds_played, cur.rounds_played), (Some(pr), Some(cr)) if cr == 0 && pr > 0)
                    && cur.map_phase == "warmup"));

        let stats_dropped = match (&self.prev_local, &cur.local) {
            (Some(p), Some(c)) => {
                matches!((p.kills, c.kills), (Some(pk), Some(ck)) if ck < pk && ck == 0)
                    && matches!((p.deaths, c.deaths), (Some(pd), Some(cd)) if cd < pd && cd == 0)
            }
            _ => false,
        };

        if map_changed || match_restarted || stats_dropped {
            self.reset_match_state();
            if map_changed {
                if !prev.map.is_empty() {
                    push(
                        "match.leave",
                        Confidence::Confirmed,
                        None,
                        json!({ "map": prev.map }),
                    );
                }
                if !cur.map.is_empty() {
                    push(
                        "match.start",
                        Confidence::Confirmed,
                        Some(format!("start:{}:{}:{}", self.match_seq, cur.map, cur.mode)),
                        json!({ "map": cur.map, "mode": cur.mode }),
                    );
                }
            } else {
                push(
                    "match.reset",
                    Confidence::Confirmed,
                    None,
                    json!({ "map": cur.map, "mode": cur.mode }),
                );
            }
            self.prev_local = cur.local.clone();
            self.prev = Some(cur);
            return out;
        }

        if cur.map.is_empty() {
            self.prev = Some(cur);
            return out;
        }

        if prev.map_phase != cur.map_phase && !cur.map_phase.is_empty() {
            push(
                "match.phase",
                Confidence::Confirmed,
                None,
                json!({ "phase": cur.map_phase, "map": cur.map }),
            );
            if cur.map_phase == "gameover" {
                push(
                    "match.end",
                    Confidence::Confirmed,
                    Some(format!("end:{}:{}:{:?}:{:?}", self.match_seq, cur.map, cur.ct, cur.t)),
                    json!({
                        "map": cur.map,
                        "ct": cur.ct,
                        "t": cur.t,
                        "won": cur.match_won(),
                    }),
                );
            }
        }

        if !cur.round_phase.is_empty() && prev.round_phase != cur.round_phase {
            if (cur.round_phase == "freezetime" || cur.round_phase == "live")
                && prev.round_phase == "over"
            {
                self.last_multikill_round = None;
                self.clutch_emitted_round = None;
                self.round_first_kill_ms = None;
            }
            let mut data = json!({
                "rounds_played": cur.rounds_played,
                "ct": cur.ct,
                "t": cur.t,
            });
            let mut round_won = None;
            if cur.round_phase == "over" && !cur.win_team.is_empty() {
                data["winner"] = json!(cur.win_team);
                let won = cur
                    .local
                    .as_ref()
                    .filter(|l| l.team == "CT" || l.team == "T")
                    .map(|l| l.team == cur.win_team);
                data["won"] = json!(won);
                round_won = won;
            }
            push(
                &format!("round.{}", cur.round_phase),
                Confidence::Confirmed,
                None,
                data,
            );
            if cur.round_phase == "over" && round_won == Some(true) {
                if let Some(ref c) = cur.local {
                    let rk = c.round_kills.unwrap_or(0);
                    let hp = c.health.unwrap_or(100);
                    let round_num = cur.rounds_played.unwrap_or(-1);
                    let seq_ms = self.round_first_kill_ms.unwrap_or_else(now_ms);
                    if rk >= 3 {
                        push(
                            "player.round_multikill_win",
                            Confidence::Confirmed,
                            Some(format!("rmw:{}:{}:{}", self.match_seq, cur.map, round_num)),
                            json!({
                                "count": rk,
                                "round_kills": rk,
                                "health": c.health,
                                "rounds_played": cur.rounds_played,
                                "sequence_start_ms": seq_ms,
                            }),
                        );
                    }
                    if hp > 0
                        && rk >= 2
                        && hp <= 35
                        && self.clutch_emitted_round != Some(round_num)
                    {
                        self.clutch_emitted_round = Some(round_num);
                        push(
                            "player.clutch",
                            Confidence::Inferred,
                            Some(format!("clutch:{}:{}:{}", self.match_seq, cur.map, round_num)),
                            json!({
                                "count": rk,
                                "round_kills": rk,
                                "health": c.health,
                                "rounds_played": cur.rounds_played,
                                "sequence_start_ms": seq_ms,
                            }),
                        );
                    }
                }
            }
        }

        if prev.bomb != cur.bomb && matches!(cur.bomb.as_str(), "planted" | "defused" | "exploded")
        {
            push(
                &format!("bomb.{}", cur.bomb),
                Confidence::Confirmed,
                Some(format!("bomb:{}:{}:{}:{:?}", self.match_seq, cur.map, cur.bomb, cur.rounds_played)),
                json!({ "rounds_played": cur.rounds_played }),
            );
        }

        let map_name = &cur.map;
        let match_seq = self.match_seq;
        if let Some(ref c) = cur.local {
            if let Some(ref p) = self.prev_local {
                if c.round_kills.unwrap_or(0) < p.round_kills.unwrap_or(0) {
                    self.last_multikill_round = None;
                    self.ace_emitted_round = None;
                    self.clutch_emitted_round = None;
                    self.round_first_kill_ms = None;
                }
                let round_num = cur.rounds_played.unwrap_or(-1);

                if let (Some(pk), Some(ck)) = (p.kills, c.kills) {
                    let delta_kills = ck - pk;
                    if delta_kills >= 1 {
                        let cur_ms = now_ms();
                        if self.round_first_kill_ms.is_none() || c.round_kills.unwrap_or(1) <= 1 {
                            self.round_first_kill_ms = Some(cur_ms);
                        }
                    }
                    let seq_ms = self.round_first_kill_ms.unwrap_or_else(now_ms);
                    if delta_kills == 1 {
                        let hs_delta = match (p.round_hs, c.round_hs) {
                            (Some(phs), Some(chs)) if chs >= phs => Some(chs - phs > 0),
                            _ => None,
                        };
                        let active_weapon = c.weapon.as_deref().or(p.weapon.as_deref());
                        let knife = active_weapon
                            .map(|w| w.contains("knife") || w.contains("bayonet"))
                            .unwrap_or(false);
                        push(
                            "player.kill",
                            Confidence::Confirmed,
                            Some(format!("kill:{match_seq}:{map_name}:{ck}")),
                            json!({
                                "count": 1,
                                "total": ck,
                                "round_kills": c.round_kills,
                                "headshot": hs_delta,
                                "bodyshot": hs_delta.map(|hs| !hs),
                                "weapon": active_weapon,
                                "team": c.team,
                                "player_name": c.player_name,
                                "score": c.score,
                                "round_dmg": c.round_dmg,
                                "weapon_confidence": if active_weapon.is_some() { "inferred_active_weapon" } else { "unavailable" },
                                "knife": if active_weapon.is_some() { json!(knife) } else { Value::Null },
                                "health": c.health,
                                "flashed": c.flashed.map(|f| f > 0),
                                "rounds_played": cur.rounds_played,
                                "sequence_start_ms": seq_ms,
                            }),
                        );
                    } else if delta_kills > 1 {
                        let hs_delta = match (p.round_hs, c.round_hs) {
                            (Some(phs), Some(chs)) if chs >= phs => Some(chs - phs),
                            _ => None,
                        };
                        push(
                            "player.kill",
                            Confidence::Confirmed,
                            Some(format!("kill_batch:{match_seq}:{map_name}:{pk}->{ck}")),
                            json!({
                                "count": delta_kills,
                                "total": ck,
                                "previous_total": pk,
                                "round_kills": c.round_kills,
                                "headshots_in_batch": hs_delta,
                                "headshot": Value::Null,
                                "weapon": Value::Null,
                                "knife": Value::Null,
                                "health": c.health,
                                "rounds_played": cur.rounds_played,
                                "sequence_start_ms": seq_ms,
                            }),
                        );
                    }
                }

                if let (Some(pd), Some(cd)) = (p.deaths, c.deaths) {
                    if cd > pd {
                        push(
                            "player.death",
                            Confidence::Confirmed,
                            Some(format!("death:{match_seq}:{map_name}:{cd}")),
                            json!({
                                "count": cd - pd,
                                "total": cd,
                                "rounds_played": cur.rounds_played,
                            }),
                        );
                    }
                }

                if let (Some(pa), Some(ca)) = (p.assists, c.assists) {
                    if ca > pa {
                        push(
                            "player.assist",
                            Confidence::Confirmed,
                            Some(format!("assist:{match_seq}:{map_name}:{ca}")),
                            json!({
                                "count": ca - pa,
                                "total": ca,
                                "rounds_played": cur.rounds_played,
                            }),
                        );
                    }
                }

                if let (Some(pm), Some(cm)) = (p.mvps, c.mvps) {
                    if cm > pm {
                        push(
                            "player.mvp",
                            Confidence::Confirmed,
                            Some(format!("mvp:{match_seq}:{map_name}:{cm}")),
                            json!({ "total": cm, "rounds_played": cur.rounds_played }),
                        );
                    }
                }

                if let (Some(pf), Some(cf)) = (p.flashed, c.flashed) {
                    if pf == 0 && cf > 0 {
                        push(
                            "player.flashed",
                            Confidence::Confirmed,
                            None,
                            json!({ "amount": cf }),
                        );
                    }
                }

                if let Some(crk) = c.round_kills {
                    let prk = p.round_kills.unwrap_or(0);
                    let seq_ms = self.round_first_kill_ms.unwrap_or_else(now_ms);
                    if crk > prk && crk >= 2 {
                        let already_emitted_same = self
                            .last_multikill_round
                            .map_or(false, |(r, k)| r == round_num && k >= crk);
                        if !already_emitted_same {
                            self.last_multikill_round = Some((round_num, crk));
                            push(
                                "player.multikill",
                                Confidence::Confirmed,
                                Some(format!("mk:{match_seq}:{map_name}:{round_num}:{crk}")),
                                json!({
                                    "count": crk,
                                    "round_kills": crk,
                                    "rounds_played": cur.rounds_played,
                                    "sequence_start_ms": seq_ms,
                                }),
                            );
                        }
                    }
                    if crk >= 5 && self.ace_emitted_round != Some(round_num) {
                        self.ace_emitted_round = Some(round_num);
                        push(
                            "player.ace",
                            Confidence::Confirmed,
                            Some(format!("ace:{match_seq}:{map_name}:{round_num}")),
                            json!({
                                "rounds_played": cur.rounds_played,
                                "round_kills": crk,
                                "sequence_start_ms": seq_ms,
                            }),
                        );
                    }
                }
            }
            self.prev_local = Some(c.clone());
        }

        self.prev = Some(cur);
        out
    }
}



#[derive(Default)]
pub(crate) struct DotaTracker {
    prev_map: String,
    prev_state: String,
    prev_kills: Option<i64>,
    prev_deaths: Option<i64>,
    prev_assists: Option<i64>,
    prev_level: Option<i64>,
    prev_win_team: String,
    streak_kills: i64,
    last_kill_clock: Option<f64>,
    match_seq: u64,
}

impl DotaTracker {
    pub(crate) fn ingest(&mut self, v: &Value) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let map_name = text(v, "/map/name");
        let game_state = text(v, "/map/game_state");
        let win_team = text(v, "/map/win_team");
        let clock_time = v.pointer("/map/clock_time").and_then(Value::as_f64);
        let player_team = text(v, "/player/team_name");

        if map_name != self.prev_map {
            if !self.prev_map.is_empty() {
                out.push(EmittedEvent {
                    kind: "match.leave".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({ "map": self.prev_map }),
                });
            }
            if !map_name.is_empty() {
                out.push(EmittedEvent {
                    kind: "match.start".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({ "map": map_name, "state": game_state }),
                });
            }
            self.prev_map = map_name.clone();
            self.prev_kills = None;
            self.prev_deaths = None;
            self.prev_assists = None;
            self.prev_level = None;
            self.prev_win_team.clear();
            self.streak_kills = 0;
            self.last_kill_clock = None;
            self.match_seq = self.match_seq.wrapping_add(1);
        }

        if !game_state.is_empty() && game_state != self.prev_state {
            out.push(EmittedEvent {
                kind: "match.phase".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({ "phase": game_state, "clock_time": clock_time }),
            });
            if game_state == "DOTA_GAMERULES_STATE_POST_GAME"
                && !win_team.is_empty()
                && win_team != "none"
            {
                let won =
                    (!player_team.is_empty()).then(|| player_team.eq_ignore_ascii_case(&win_team));
                out.push(EmittedEvent {
                    kind: "match.end".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("dota_end:{map_name}:{win_team}")),
                    data: json!({ "winner": win_team, "won": won, "clock_time": clock_time }),
                });
                self.prev_win_team = win_team;
            }
            self.prev_state = game_state;
        }

        let kills = opt_num(v, "/player/kills");
        let deaths = opt_num(v, "/player/deaths");
        let assists = opt_num(v, "/player/assists");
        let level = opt_num(v, "/hero/level");
        let hero_name = text(v, "/hero/name");

        if let (Some(pk), Some(ck)) = (self.prev_kills, kills) {
            if ck > pk {
                let delta = ck - pk;
                let within_streak = match (self.last_kill_clock, clock_time) {
                    (Some(prev_c), Some(cur_c)) => (cur_c - prev_c).abs() <= 18.0,
                    _ => delta > 1,
                };
                if within_streak {
                    self.streak_kills += delta;
                } else {
                    self.streak_kills = delta;
                }
                self.last_kill_clock = clock_time;

                let mseq = self.match_seq;
                out.push(EmittedEvent {
                    kind: "player.kill".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("dota_kill:{mseq}:{ck}")),
                    data: json!({
                        "count": delta,
                        "total": ck,
                        "hero": hero_name,
                        "kill_list": v.pointer("/player/kill_list").cloned().unwrap_or(Value::Null),
                    }),
                });

                if self.streak_kills >= 2 {
                    out.push(EmittedEvent {
                        kind: "player.multikill".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: Some(format!("dota_mk:{mseq}:{ck}:{}", self.streak_kills)),
                        data: json!({
                            "count": self.streak_kills,
                            "total": ck,
                            "hero": hero_name,
                        }),
                    });
                }
                if self.streak_kills >= 5 {
                    out.push(EmittedEvent {
                        kind: "player.ace".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: Some(format!("dota_rampage:{mseq}:{ck}")),
                        data: json!({
                            "count": self.streak_kills,
                            "total": ck,
                            "hero": hero_name,
                            "rampage": true,
                        }),
                    });
                }
            }
        }
        let mseq = self.match_seq;
        if let (Some(pd), Some(cd)) = (self.prev_deaths, deaths) {
            if cd > pd {
                self.streak_kills = 0;
                self.last_kill_clock = None;
                out.push(EmittedEvent {
                    kind: "player.death".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("dota_death:{mseq}:{cd}")),
                    data: json!({ "count": cd - pd, "total": cd, "hero": hero_name }),
                });
            }
        }
        if let (Some(pa), Some(ca)) = (self.prev_assists, assists) {
            if ca > pa {
                out.push(EmittedEvent {
                    kind: "player.assist".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("dota_assist:{mseq}:{ca}")),
                    data: json!({ "count": ca - pa, "total": ca, "hero": hero_name }),
                });
            }
        }
        if let (Some(pl), Some(cl)) = (self.prev_level, level) {
            if cl > pl {
                out.push(EmittedEvent {
                    kind: "hero.level_up".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("dota_lvl:{mseq}:{cl}")),
                    data: json!({ "level": cl, "hero": hero_name }),
                });
            }
        }

        self.prev_kills = kills;
        self.prev_deaths = deaths;
        self.prev_assists = assists;
        self.prev_level = level;
        out
    }
}



struct RlPlayerSnap {
    name: String,
    team: Option<i64>,
    score: Option<i64>,
    goals: Option<i64>,
    assists: Option<i64>,
    saves: Option<i64>,
    shots: Option<i64>,
    demolishes: Option<i64>,
    demolished: bool,
    supersonic: bool,
}

#[derive(Default)]
pub(crate) struct RlTracker {
    arena: String,
    overtime: bool,
    has_winner: bool,
    local_name: String,
    local_id: String,
    local_team: Option<i64>,
    players: HashMap<String, RlPlayerSnap>,
}

impl RlTracker {
    pub(crate) fn reset(&mut self) {
        self.arena.clear();
        self.overtime = false;
        self.has_winner = false;
        self.local_name.clear();
        self.local_id.clear();
        self.local_team = None;
        self.players.clear();
    }

    pub(crate) fn is_target_local(&self, target: &Value) -> bool {
        if self.local_name.is_empty() && self.local_id.is_empty() {
            return true;
        }
        let cand_name = target
            .get("Name")
            .or_else(|| target.get("name"))
            .and_then(Value::as_str)
            .or_else(|| target.as_str())
            .unwrap_or("");
        let cand_id = target
            .get("UniqueId")
            .or_else(|| target.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !self.local_id.is_empty() && !cand_id.is_empty() && cand_id == self.local_id {
            return true;
        }
        if !self.local_name.is_empty()
            && !cand_name.is_empty()
            && cand_name.eq_ignore_ascii_case(&self.local_name)
        {
            return true;
        }
        false
    }

    pub(crate) fn ingest_update_state(&mut self, data: &Value) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let Some(game) = data.get("Game") else {
            return out;
        };

        let arena = text(game, "/Arena");
        let is_overtime = opt_bool(game, "/bOvertime").unwrap_or(false);
        let has_winner = opt_bool(game, "/bHasWinner").unwrap_or(false);
        let winner = text(game, "/Winner");

        if !arena.is_empty() && arena != self.arena {
            self.arena = arena.clone();
            self.overtime = false;
            self.has_winner = false;
            self.players.clear();
            out.push(EmittedEvent {
                kind: "match.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "arena": arena,
                    "teams": game.get("Teams").cloned().unwrap_or(Value::Null),
                }),
            });
        }

        if is_overtime && !self.overtime {
            self.overtime = true;
            out.push(EmittedEvent {
                kind: "match.overtime".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({ "arena": self.arena }),
            });
        }

        if has_winner && !self.has_winner {
            self.has_winner = true;
            out.push(EmittedEvent {
                kind: "match.end".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "arena": self.arena,
                    "winner": winner,
                    "teams": game.get("Teams").cloned().unwrap_or(Value::Null),
                }),
            });
        }

        let local_name = text(data, "/LocalPlayerName")
            .chars()
            .collect::<String>();
        let primary_id = text(data, "/PrimaryPlayer/UniqueId");
        if let Some(players) = data.get("Players").and_then(Value::as_array) {
            let single_player_list = players.len() == 1;
            for p in players {
                let name = text(p, "/Name");
                if name.is_empty() {
                    continue;
                }
                let is_local_player = single_player_list
                    || opt_bool(p, "/bLocalPlayer").unwrap_or(false)
                    || (!local_name.is_empty() && name.eq_ignore_ascii_case(&local_name))
                    || (!primary_id.is_empty() && text(p, "/UniqueId") == primary_id);
                let id_key = {
                    let uid = text(p, "/UniqueId");
                    if uid.is_empty() {
                        name.clone()
                    } else {
                        uid
                    }
                };
                let cur = RlPlayerSnap {
                    name: name.clone(),
                    team: opt_num(p, "/Team"),
                    score: opt_num(p, "/Score"),
                    goals: opt_num(p, "/Goals"),
                    assists: opt_num(p, "/Assists"),
                    saves: opt_num(p, "/Saves"),
                    shots: opt_num(p, "/Shots"),
                    demolishes: opt_num(p, "/Demolishes"),
                    demolished: opt_bool(p, "/bDemolished").unwrap_or(false),
                    supersonic: opt_bool(p, "/bSupersonic").unwrap_or(false),
                };

                if is_local_player {
                    self.local_name = name.clone();
                    self.local_id = id_key.clone();
                    self.local_team = cur.team;
                }
                if let Some(prev) = self.players.get(&id_key) {
                    if is_local_player {
                        if let (Some(pg), Some(cg)) = (prev.goals, cur.goals) {
                            if cg > pg {
                                out.push(EmittedEvent {
                                    kind: "sport.goal".into(),
                                    confidence: Confidence::Confirmed,
                                    dedup_key: Some(format!("rl_goal:{}:{id_key}:{cg}", self.arena)),
                                    data: json!({
                                        "player": cur.name,
                                        "team": cur.team,
                                        "goals": cg,
                                        "count": cg - pg,
                                        "by_local": true,
                                    }),
                                });
                            }
                        }
                        if let (Some(ps), Some(cs)) = (prev.saves, cur.saves) {
                            if cs > ps {
                                out.push(EmittedEvent {
                                    kind: "sport.epic_save".into(),
                                    confidence: Confidence::Confirmed,
                                    dedup_key: Some(format!("rl_save:{}:{id_key}:{cs}", self.arena)),
                                    data: json!({
                                        "player": cur.name,
                                        "team": cur.team,
                                        "saves": cs,
                                        "count": cs - ps,
                                        "by_local": true,
                                    }),
                                });
                            }
                        }
                        if let (Some(pa), Some(ca)) = (prev.assists, cur.assists) {
                            if ca > pa {
                                out.push(EmittedEvent {
                                    kind: "player.assist".into(),
                                    confidence: Confidence::Confirmed,
                                    dedup_key: Some(format!("rl_assist:{}:{id_key}:{ca}", self.arena)),
                                    data: json!({
                                        "player": cur.name,
                                        "team": cur.team,
                                        "assists": ca,
                                        "count": ca - pa,
                                        "by_local": true,
                                    }),
                                });
                            }
                        }
                        if let (Some(pd), Some(cd)) = (prev.demolishes, cur.demolishes) {
                            if cd > pd {
                                out.push(EmittedEvent {
                                    kind: "player.kill".into(),
                                    confidence: Confidence::Confirmed,
                                    dedup_key: Some(format!("rl_demo:{}:{id_key}:{cd}", self.arena)),
                                    data: json!({
                                        "player": cur.name,
                                        "team": cur.team,
                                        "demolishes": cd,
                                        "count": cd - pd,
                                        "by_local": true,
                                    }),
                                });
                            }
                        }
                    }
                    if !prev.demolished && cur.demolished {
                        out.push(EmittedEvent {
                            kind: "player.demolished".into(),
                            confidence: Confidence::Confirmed,
                            dedup_key: None,
                            data: json!({
                                "player": cur.name,
                                "team": cur.team,
                                "by_local": is_local_player,
                            }),
                        });
                        if is_local_player {
                            out.push(EmittedEvent {
                                kind: "player.death".into(),
                                confidence: Confidence::Confirmed,
                                dedup_key: None,
                                data: json!({
                                    "player": cur.name,
                                    "team": cur.team,
                                    "count": 1,
                                    "by_local": true,
                                }),
                            });
                        }
                    }
                    if !prev.supersonic && cur.supersonic {
                        out.push(EmittedEvent {
                            kind: "player.supersonic".into(),
                            confidence: Confidence::Confirmed,
                            dedup_key: None,
                            data: json!({
                                "player": cur.name,
                                "team": cur.team,
                                "speed": opt_f64(p, "/Speed"),
                                "boost": opt_f64(p, "/Boost").or_else(|| opt_num(p, "/Boost").map(|b| b as f64)),
                            }),
                        });
                    }
                }
                self.players.insert(id_key, cur);
            }
        }

        out
    }
}



pub(crate) struct CombatHighlightTracker {
    pub(crate) round_kills: u32,
    pub(crate) streak_kills: u32,
    pub(crate) total_kills: u32,
    pub(crate) total_deaths: u32,
    pub(crate) total_assists: u32,
    pub(crate) total_headshots: u32,
    pub(crate) total_multikills: u32,
    pub(crate) total_aces: u32,
    pub(crate) total_clutches: u32,
    pub(crate) current_round: u32,
    pub(crate) in_match: bool,
    pub(crate) current_map: String,
    pub(crate) current_mode: String,
    pub(crate) ace_emitted: bool,
    pub(crate) clutch_emitted: bool,
    pub(crate) last_kill_at: Option<Instant>,
    pub(crate) round_first_kill_epoch_ms: Option<u64>,
    pub(crate) streak_first_kill_epoch_ms: Option<u64>,
    pub(crate) seq: u64,
}

impl Default for CombatHighlightTracker {
    fn default() -> Self {
        Self {
            round_kills: 0,
            streak_kills: 0,
            total_kills: 0,
            total_deaths: 0,
            total_assists: 0,
            total_headshots: 0,
            total_multikills: 0,
            total_aces: 0,
            total_clutches: 0,
            current_round: 1,
            in_match: false,
            current_map: String::new(),
            current_mode: String::new(),
            ace_emitted: false,
            clutch_emitted: false,
            last_kill_at: None,
            round_first_kill_epoch_ms: None,
            streak_first_kill_epoch_ms: None,
            seq: 0,
        }
    }
}

impl CombatHighlightTracker {
    pub(crate) fn reset_round(&mut self) {
        self.round_kills = 0;
        self.streak_kills = 0;
        self.ace_emitted = false;
        self.clutch_emitted = false;
        self.round_first_kill_epoch_ms = None;
        self.streak_first_kill_epoch_ms = None;
    }

    pub(crate) fn reset_match(&mut self) {
        self.reset_round();
        self.total_kills = 0;
        self.total_deaths = 0;
        self.total_assists = 0;
        self.total_headshots = 0;
        self.total_multikills = 0;
        self.total_aces = 0;
        self.total_clutches = 0;
        self.current_round = 1;
    }

    pub(crate) fn record_death(&mut self, via: &str) -> EmittedEvent {
        self.total_deaths = self.total_deaths.saturating_add(1);
        self.streak_kills = 0;
        self.streak_first_kill_epoch_ms = None;
        self.seq = self.seq.wrapping_add(1);
        let conf = if via == "log" {
            Confidence::Confirmed
        } else {
            Confidence::Inferred
        };
        EmittedEvent {
            kind: "player.death".into(),
            confidence: conf,
            dedup_key: Some(format!("cdeath:{}:{}", self.seq, self.total_deaths)),
            data: json!({
                "count": 1,
                "total": self.total_deaths,
                "round": self.current_round,
                "map": self.current_map,
                "mode": self.current_mode,
                "via": via,
            }),
        }
    }

    pub(crate) fn record_assist(&mut self, via: &str) -> EmittedEvent {
        self.total_assists = self.total_assists.saturating_add(1);
        self.seq = self.seq.wrapping_add(1);
        let conf = if via == "log" {
            Confidence::Confirmed
        } else {
            Confidence::Inferred
        };
        EmittedEvent {
            kind: "player.assist".into(),
            confidence: conf,
            dedup_key: Some(format!("cassist:{}:{}", self.seq, self.total_assists)),
            data: json!({
                "count": 1,
                "total": self.total_assists,
                "round": self.current_round,
                "map": self.current_map,
                "mode": self.current_mode,
                "via": via,
            }),
        }
    }

    pub(crate) fn record_kill(
        &mut self,
        headshot: bool,
        via: &str,
        now: Instant,
    ) -> Vec<EmittedEvent> {
        if let Some(last) = self.last_kill_at {
            if via != "log" && now.duration_since(last) < Duration::from_millis(550) {
                return Vec::new();
            }
            if now.duration_since(last) > Duration::from_secs(30) {
                self.streak_kills = 0;
                self.streak_first_kill_epoch_ms = None;
            }
        }
        let cur_epoch_ms = now_ms();
        if self.round_first_kill_epoch_ms.is_none() {
            self.round_first_kill_epoch_ms = Some(cur_epoch_ms);
        }
        if self.streak_first_kill_epoch_ms.is_none() {
            self.streak_first_kill_epoch_ms = Some(cur_epoch_ms);
        }
        let seq_start_ms = self
            .round_first_kill_epoch_ms
            .or(self.streak_first_kill_epoch_ms)
            .unwrap_or(cur_epoch_ms);

        self.last_kill_at = Some(now);
        self.round_kills = self.round_kills.saturating_add(1);
        self.streak_kills = self.streak_kills.saturating_add(1);
        self.total_kills = self.total_kills.saturating_add(1);
        self.seq = self.seq.wrapping_add(1);

        let mut out = Vec::new();
        let conf = if via == "log" {
            Confidence::Confirmed
        } else {
            Confidence::Inferred
        };

        out.push(EmittedEvent {
            kind: "player.kill".into(),
            confidence: conf,
            dedup_key: Some(format!("ckill:{}:{}", self.seq, self.total_kills)),
            data: json!({
                "count": 1,
                "total": self.total_kills,
                "round_kills": self.round_kills,
                "streak_kills": self.streak_kills,
                "headshot": headshot,
                "round": self.current_round,
                "map": self.current_map,
                "mode": self.current_mode,
                "sequence_start_ms": seq_start_ms,
                "via": via,
            }),
        });

        if headshot {
            self.total_headshots = self.total_headshots.saturating_add(1);
            out.push(EmittedEvent {
                kind: "player.headshot".into(),
                confidence: conf,
                dedup_key: Some(format!("chs:{}:{}", self.seq, self.total_kills)),
                data: json!({
                    "count": 1,
                    "round_kills": self.round_kills,
                    "round": self.current_round,
                    "sequence_start_ms": seq_start_ms,
                    "via": via,
                }),
            });
        }

        let multi = self.round_kills.max(self.streak_kills);
        if multi >= 2 {
            self.total_multikills = self.total_multikills.saturating_add(1);
            out.push(EmittedEvent {
                kind: "player.multikill".into(),
                confidence: conf,
                dedup_key: Some(format!("cmk:{}:{}:{}", self.current_round, multi, self.seq)),
                data: json!({
                    "count": multi,
                    "round_kills": self.round_kills,
                    "streak_kills": self.streak_kills,
                    "round": self.current_round,
                    "sequence_start_ms": seq_start_ms,
                    "via": via,
                }),
            });
        }

        if multi >= 5 && !self.ace_emitted {
            self.ace_emitted = true;
            self.total_aces = self.total_aces.saturating_add(1);
            out.push(EmittedEvent {
                kind: "player.ace".into(),
                confidence: conf,
                dedup_key: Some(format!("cace:{}:{}", self.current_round, self.seq)),
                data: json!({
                    "count": multi,
                    "round_kills": self.round_kills,
                    "round": self.current_round,
                    "sequence_start_ms": seq_start_ms,
                    "via": via,
                }),
            });
        }

        out
    }

    pub(crate) fn record_round_end(
        &mut self,
        round_label: Option<&str>,
        round_won: Option<bool>,
    ) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        if let Some(r_str) = round_label {
            if let Ok(r_num) = r_str.parse::<u32>() {
                self.current_round = r_num;
            }
        }
        let rk = self.round_kills;
        let seq_start_ms = self
            .round_first_kill_epoch_ms
            .or(self.streak_first_kill_epoch_ms)
            .unwrap_or_else(now_ms);
        if rk >= 3 && round_won != Some(false) {
            self.seq = self.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.round_multikill_win".into(),
                confidence: Confidence::Inferred,
                dedup_key: Some(format!("crmw:{}:{}", self.current_round, self.seq)),
                data: json!({
                    "count": rk,
                    "round_kills": rk,
                    "round": self.current_round,
                    "won": round_won,
                    "sequence_start_ms": seq_start_ms,
                }),
            });
        }
        if !self.clutch_emitted && ((rk >= 2 && round_won != Some(false)) || (rk >= 1 && round_won == Some(true))) {
            self.clutch_emitted = true;
            self.total_clutches = self.total_clutches.saturating_add(1);
            self.seq = self.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Inferred,
                dedup_key: Some(format!("cclutch:{}:{}", self.current_round, self.seq)),
                data: json!({
                    "count": rk,
                    "round_kills": rk,
                    "round": self.current_round,
                    "won": round_won,
                    "sequence_start_ms": seq_start_ms,
                }),
            });
        }
        out.push(EmittedEvent {
            kind: "round.end".into(),
            confidence: Confidence::Inferred,
            dedup_key: None,
            data: json!({
                "round": round_label.unwrap_or(""),
                "round_number": self.current_round,
                "round_kills": rk,
                "won": round_won,
            }),
        });
        self.current_round = self.current_round.saturating_add(1);
        self.reset_round();
        out
    }
}



pub struct ValorantTracker {
    pub(crate) combat: CombatHighlightTracker,
    pub(crate) current_agent: String,
    pub(crate) current_weapon: String,
    pub(crate) bodyshots: u32,
    pub(crate) legshots: u32,
    flow_re: Regex,
    map_re: Regex,
    agent_re: Regex,
    weapon_re: Regex,
    legshot_re: Regex,
    round_end_re: Regex,
    round_start_re: Regex,
    kill_re: Regex,
    death_re: Regex,
    assist_re: Regex,
    headshot_re: Regex,
    ace_re: Regex,
    clutch_re: Regex,
    plant_re: Regex,
    defuse_re: Regex,
}

impl Default for ValorantTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ValorantTracker {
    pub fn new() -> Self {
        Self {
            combat: CombatHighlightTracker::default(),
            current_agent: String::new(),
            current_weapon: String::new(),
            bodyshots: 0,
            legshots: 0,
            flow_re: compile_re(
                r"LogGameFlowStateManager:.*Transitioning from (?P<from>[A-Za-z0-9_]+) to (?P<to>[A-Za-z0-9_]+)",
            ),
            map_re: compile_re(r"LogMapLoadModel:.*Map Name:\s*(?P<map>[A-Za-z0-9_/-]+)"),
            agent_re: compile_re(
                r"(?i)(?:Characters/|Character_|Default__|Pawn_|PlayBark.*_|VO_)(?P<agent>Wushu|Jett|Vampire|Reyna|Wraith|Omen|Hunter|Sova|Clay|Raze|Pandemic|Viper|Thorne|Sage|Phoenix|Gumshoe|Cypher|Sarge|Brimstone|Breach|Killjoy|Guide|Skye|Stealth|Yoru|Rift|Astra|Grenadier|KAYO|Deadeye|Chamber|Sprinter|Neon|BountyHunter|Fade|Mage|Harbor|AggroBot|Gekko|Cable|Deadlock|Sequoia|Iso|Smonk|Clove|Cashew|Vyse|Terra|Tejo|Glass|Waylay)",
            ),
            weapon_re: compile_re(
                r"(?i)(?:Equippables/Guns/|Weapon_|Gun_|KillFeed.*)(?P<weapon>Vandal|Rifle_AK|Phantom|Rifle_Carbine|Operator|Sniper_Bolt|Sheriff|Luger|Ghost|CorePistol|Spectre|SMG_MP5|Guardian|DMR|Bulldog|Rifle_Burst|Marshal|LeverSniper|Outlaw|DoubleSniper|Judge|AutomaticShotgun|Bucky|PumpShotgun|Odin|HeavyMachineGun|Ares|LightMachineGun|Stinger|Vector|Frenzy|AutoPistol|Shorty|SawedOff|Classic|BasePistol|Melee|Knife)",
            ),
            legshot_re: compile_re(
                r"(?i)(?:HitZone|HitGroup|DamageZone|LimbShot|LegShot).*?(?:Leg|Thigh|Calf|Foot)",
            ),
            round_end_re: compile_re(
                r"AShooterGameState::OnRoundEnded for round '(?P<round>[0-9]+)'",
            ),
            round_start_re: compile_re(
                r"AShooterGameState::(?:OnRoundStarted|TransitionToRoundPhase|BeginRound)",
            ),
            kill_re: compile_re(
                r"(?i)(?:LogShooterCharacter|LogVO|PlayBark|AudBasePawnVOComponent|KillBanner|CombatLog|Telemetry|ShooterGameState|KillFeed).*?(?:EnemyKilled|ConfirmKill|Bark_Kill|VO_Kill|MeKill|PlayKillBanner|KillConfirm|LocalPlayerKill|KillStreak|MultiKill|PlayerKill|PlayKillSound|OnPlayerKilled)",
            ),
            death_re: compile_re(
                r"(?i)(?:LogShooterCharacter|CombatLog|Telemetry|ShooterGameState|KillFeed).*?(?:LocalPlayerDied|OnLocalPlayerDied|PlayerDeath|MeDied|KilledByEnemy)",
            ),
            assist_re: compile_re(
                r"(?i)(?:LogShooterCharacter|LogVO|PlayBark|CombatLog|Telemetry|ShooterGameState).*?(?:LocalPlayerAssist|ConfirmAssist|KillAssist|Bark_Assist|OnPlayerAssist)",
            ),
            headshot_re: compile_re(
                r"(?i)(?:LogShooterCharacter|LogVO|PlayBark|AudBasePawnVOComponent|KillBanner|CombatLog|Telemetry).*?(?:Headshot|Bark_Headshot|VO_Headshot)",
            ),
            ace_re: compile_re(
                r"(?i)(?:LogShooterCharacter|LogVO|PlayBark|AudBasePawnVOComponent|KillBanner|Announcement|ShooterGameState).*?(?:Bark_Ace|VO_Ace|PlayAceBanner|AceBanner|TeamAce|Pentakill|\bAce\b)",
            ),
            clutch_re: compile_re(
                r"(?i)(?:LogShooterCharacter|LogVO|PlayBark|AudBasePawnVOComponent|KillBanner|Announcement|ShooterGameState).*?(?:Bark_Clutch|VO_Clutch|PlayClutchBanner|ClutchBanner|LastPlayerStandingWin|ClutchWin|\bClutch\b)",
            ),
            plant_re: compile_re(
                r"(?i)(?:LogShooterGameState|AShooterGameState|PlayBark|AudBasePawnVOComponent|Bomb).*?(?:BombPlanted|SpikePlanted|OnBombPlanted|Bark_Plant|PlantSpike)",
            ),
            defuse_re: compile_re(
                r"(?i)(?:LogShooterGameState|AShooterGameState|PlayBark|AudBasePawnVOComponent|Bomb).*?(?:BombDefused|SpikeDefused|OnBombDefused|Bark_Defuse|DefuseSpike)",
            ),
        }
    }

    fn is_gameplay_state(s: &str) -> bool {
        if s.starts_with("InGameTo") {
            return false;
        }
        s.starts_with("InGame")
            || s.contains("ShootingRange")
            || s.contains("CustomGame")
            || s.contains("Skirmish")
            || s.contains("Deathmatch")
            || s.contains("Swiftplay")
            || s.contains("SpikeRush")
            || s.contains("HURM")
    }

    pub fn normalize_valorant_map(raw: &str) -> Option<&'static str> {
        let last_seg = raw
            .trim()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(raw)
            .trim();
        let lower = last_seg.to_lowercase();
        if lower.is_empty()
            || lower.starts_with("init")
            || lower.contains("mainmenu")
            || lower.contains("characterselect")
            || lower.contains("transition")
            || lower.contains("uilevel")
            || lower.contains("frontend")
            || lower.contains("lobby")
            || lower == "none"
            || lower == "null"
            || lower == "entry"
            || lower == "startup"
        {
            return None;
        }
        let base = lower
            .trim_end_matches("_p")
            .trim_end_matches("_v2")
            .trim_end_matches("v2")
            .trim_end_matches("_art")
            .trim_end_matches("_lighting");
        if base == "ascent" || lower.contains("ascent") {
            Some("Ascent")
        } else if base == "triad" || base == "haven" || lower.contains("triad") || lower.contains("haven") {
            Some("Haven")
        } else if base == "duality" || base == "bind" || lower.contains("duality") || lower.contains("bind") {
            Some("Bind")
        } else if base == "bonsai" || base == "split" || lower.contains("bonsai") || lower.contains("split") {
            Some("Split")
        } else if base == "port" || base == "icebox" || lower.contains("icebox") {
            Some("Icebox")
        } else if base == "foxtrot" || base == "breeze" || lower.contains("foxtrot") || lower.contains("breeze") {
            Some("Breeze")
        } else if base == "canyon" || base == "fracture" || lower.contains("canyon") || lower.contains("fracture") {
            Some("Fracture")
        } else if base == "pitt" || base == "pearl" || lower.contains("pitt") || lower.contains("pearl") {
            Some("Pearl")
        } else if base == "jam" || base == "lotus" || lower.contains("lotus") {
            Some("Lotus")
        } else if base == "juliett" || base == "sunset" || lower.contains("juliett") || lower.contains("sunset") {
            Some("Sunset")
        } else if base == "infinity" || base == "abyss" || lower.contains("infinity") || lower.contains("abyss") {
            Some("Abyss")
        } else if base == "rook" || base == "corrode" || lower.contains("corrode") {
            Some("Corrode")
        } else if base == "poveglia" || base == "range" || base == "shootingrange" || lower.contains("poveglia") || lower.contains("shootingrange") {
            Some("The Range")
        } else if base == "hurm_alley" || base == "district" || lower.contains("hurm_alley") || lower.contains("district") {
            Some("District")
        } else if base == "hurm_bowl" || base == "kasbah" || lower.contains("hurm_bowl") || lower.contains("kasbah") {
            Some("Kasbah")
        } else if base == "hurm_yard" || base == "piazza" || lower.contains("hurm_yard") || lower.contains("piazza") {
            Some("Piazza")
        } else if base == "hurm_helix" || base == "drift" || lower.contains("hurm_helix") || lower.contains("drift") {
            Some("Drift")
        } else if base == "hurm_highground"
            || base == "hurm_higherground"
            || base == "hurm_higherdground"
            || base == "glitch"
            || lower.contains("hurm_high")
            || lower.contains("glitch")
        {
            Some("Glitch")
        } else if lower.starts_with("skirmish") {
            Some("Skirmish")
        } else {
            None
        }
    }

    pub fn normalize_valorant_agent(raw: &str) -> Option<&'static str> {
        let lower = raw.trim().to_lowercase();
        if lower.is_empty() {
            return None;
        }
        if lower.contains("wushu") || lower.contains("jett") || lower.contains("add6443a-41bd-e414-f6ad-e58d267f4e95") {
            Some("Jett")
        } else if lower.contains("vampire") || lower.contains("reyna") || lower.contains("a3bfb853-43b2-7238-a4f1-ad90e9e46bcc") {
            Some("Reyna")
        } else if lower.contains("wraith") || lower.contains("omen") || lower.contains("8e253930-4c05-31dd-1b6c-968525494517") {
            Some("Omen")
        } else if lower.contains("hunter") && !lower.contains("bounty") || lower.contains("sova") || lower.contains("320b2a48-4d9b-a075-30f1-1f93a9b638fa") {
            Some("Sova")
        } else if lower.contains("clay") || lower.contains("raze") || lower.contains("f94c3b30-42be-e959-889c-5aa313dba261") {
            Some("Raze")
        } else if lower.contains("pandemic") || lower.contains("viper") || lower.contains("707eab51-4836-f488-046a-cda6bf494859") {
            Some("Viper")
        } else if lower.contains("thorne") || lower.contains("sage") || lower.contains("569fdd95-4d10-43ab-ca70-79becc718b46") {
            Some("Sage")
        } else if lower.contains("phoenix") || lower.contains("eb93336a-449b-9c1b-0a54-a891f7921d69") {
            Some("Phoenix")
        } else if lower.contains("gumshoe") || lower.contains("cypher") || lower.contains("117ed9e3-49f3-6512-3ccf-0cada7e3823b") {
            Some("Cypher")
        } else if lower.contains("sarge") || lower.contains("brimstone") || lower.contains("9f0d8ba9-4140-b941-57d3-a7ad57c6b417") {
            Some("Brimstone")
        } else if lower.contains("breach") || lower.contains("5f8d3a7f-467b-97f3-062c-13acf203c006") {
            Some("Breach")
        } else if lower.contains("killjoy") || lower.contains("1e58de9c-4950-5125-93e9-a0aee9f98746") {
            Some("Killjoy")
        } else if lower.contains("guide") || lower.contains("skye") || lower.contains("6f2a04ca-43e0-be17-7f36-b3908627744d") {
            Some("Skye")
        } else if lower.contains("stealth") || lower.contains("yoru") || lower.contains("7f94d92c-4234-0a36-9646-3a87eb8b5c89") {
            Some("Yoru")
        } else if lower.contains("rift") || lower.contains("astra") || lower.contains("41fb69c1-4189-7b37-f117-bcaf1e96f1bf") {
            Some("Astra")
        } else if lower.contains("grenadier") || lower.contains("kayo") || lower.contains("kay/o") || lower.contains("601dbbe7-43ce-be57-2a40-4abd24953621") {
            Some("KAY/O")
        } else if lower.contains("deadeye") || lower.contains("chamber") || lower.contains("22697a3d-45bf-8dd7-4fec-84a9e28c69d7") {
            Some("Chamber")
        } else if lower.contains("sprinter") || lower.contains("neon") || lower.contains("bb2a4828-46eb-8cd1-e765-15848195d751") {
            Some("Neon")
        } else if lower.contains("bountyhunter") || lower.contains("fade") || lower.contains("dade69b4-4f5a-8528-247b-219e5a1facd6") {
            Some("Fade")
        } else if lower.contains("mage") || lower.contains("harbor") || lower.contains("95b78ed7-4637-86d9-7e41-71ba8c293152") {
            Some("Harbor")
        } else if lower.contains("aggrobot") || lower.contains("gekko") || lower.contains("e370fa57-4757-3604-3648-499e1f642d3f") {
            Some("Gekko")
        } else if lower.contains("cable") || lower.contains("deadlock") || lower.contains("cc8b64c8-4b25-4ff9-6e7f-37b4da43d235") {
            Some("Deadlock")
        } else if lower.contains("sequoia") || lower.contains("iso") || lower.contains("0e38b510-41a8-5780-5e8f-568b2a4f2d6c") {
            Some("Iso")
        } else if lower.contains("smonk") || lower.contains("clove") || lower.contains("1dbf2edd-4729-0984-3115-daa5eed44993") {
            Some("Clove")
        } else if lower.contains("cashew") || lower.contains("vyse") || lower.contains("efba5359-4016-a1e5-7626-b1ae76895940") {
            Some("Vyse")
        } else if lower.contains("terra") || lower.contains("tejo") {
            Some("Tejo")
        } else if lower.contains("glass") || lower.contains("waylay") {
            Some("Waylay")
        } else {
            None
        }
    }

    pub fn normalize_valorant_weapon(raw: &str) -> Option<&'static str> {
        let lower = raw.trim().to_lowercase();
        if lower.is_empty() {
            return None;
        }
        if lower.contains("vandal") || lower.contains("rifle_ak") || lower.contains("9c82e19d-4575-0200-1a81-3e800cf872fb") {
            Some("Vandal")
        } else if lower.contains("phantom") || lower.contains("rifle_carbine") || lower.contains("ee8e8d15-496b-07ac-e5f6-8fae5d4c7b1a") {
            Some("Phantom")
        } else if lower.contains("operator") || lower.contains("sniper_bolt") || lower.contains("a03b24d3-4319-996d-0f8c-94bbfba1dfc7") {
            Some("Operator")
        } else if lower.contains("sheriff") || lower.contains("luger") || lower.contains("e336c6b8-418d-9340-d77f-7a9e4cfe0702") {
            Some("Sheriff")
        } else if lower.contains("ghost") || lower.contains("corepistol") || lower.contains("1baa85b4-4c70-1284-64bb-6481dfc3bb4e") {
            Some("Ghost")
        } else if lower.contains("spectre") || lower.contains("smg_mp5") || lower.contains("462080d1-4035-2937-7c09-27aa2a5c27a7") {
            Some("Spectre")
        } else if lower.contains("guardian") || lower.contains("dmr") || lower.contains("4ade7faa-4cf1-8376-95ef-39884480959b") {
            Some("Guardian")
        } else if lower.contains("bulldog") || lower.contains("rifle_burst") || lower.contains("ae3de142-4d85-2547-dd26-4e90bed35cf7") {
            Some("Bulldog")
        } else if lower.contains("marshal") || lower.contains("leversniper") || lower.contains("c4883e50-4494-202c-3ec3-6b8a9284f00b") {
            Some("Marshal")
        } else if lower.contains("outlaw") || lower.contains("doublesniper") || lower.contains("5f0aaf7a-4289-3998-d5ff-eb9a5cf7ef5c") {
            Some("Outlaw")
        } else if lower.contains("judge") || lower.contains("automaticshotgun") || lower.contains("ec845bf4-4f79-ddda-a3da-0db3774b2794") {
            Some("Judge")
        } else if lower.contains("bucky") || lower.contains("pumpshotgun") || lower.contains("910be174-449b-c412-ab22-d0873436b21b") {
            Some("Bucky")
        } else if lower.contains("odin") || lower.contains("heavymachinegun") || lower.contains("63e6c2b6-4a8e-869c-3d4c-e38355226584") {
            Some("Odin")
        } else if lower.contains("ares") || lower.contains("lightmachinegun") || lower.contains("55d8a0f4-4274-ca67-fe2c-06ab45efdf58") {
            Some("Ares")
        } else if lower.contains("stinger") || lower.contains("vector") || lower.contains("f7e1b454-4ad4-1063-ec0a-159e56b58941") {
            Some("Stinger")
        } else if lower.contains("frenzy") || lower.contains("autopistol") || lower.contains("44d4e95c-4157-0037-81b2-17841bf2e8e3") {
            Some("Frenzy")
        } else if lower.contains("shorty") || lower.contains("sawedoff") || lower.contains("42da8ccc-40d5-affc-beec-15aa47b42eda") {
            Some("Shorty")
        } else if lower.contains("classic") || lower.contains("basepistol") || lower.contains("29a0cfab-485b-f5d5-779a-b59f85e204a8") {
            Some("Classic")
        } else if lower.contains("melee") || lower.contains("knife") || lower.contains("2f59173c-4bed-b6c3-2191-dea9b58be9c7") {
            Some("Melee")
        } else {
            None
        }
    }

    pub(crate) fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let now = Instant::now();

        if let Some(caps) = self.agent_re.captures(line) {
            if let Some(ag) = caps.name("agent").and_then(|m| Self::normalize_valorant_agent(m.as_str())) {
                self.current_agent = ag.to_string();
            }
        }
        if let Some(caps) = self.weapon_re.captures(line) {
            if let Some(wp) = caps.name("weapon").and_then(|m| Self::normalize_valorant_weapon(m.as_str())) {
                self.current_weapon = wp.to_string();
            }
        }

        if let Some(caps) = self.flow_re.captures(line) {
            let from = caps.name("from").map_or("", |m| m.as_str());
            let to = caps.name("to").map_or("", |m| m.as_str());
            let from_game = Self::is_gameplay_state(from);
            let to_game = Self::is_gameplay_state(to);
            let to_lower = to.to_lowercase();

            if !from_game && to_game {
                self.combat.reset_match();
                self.combat.in_match = true;
                self.combat.current_mode = to.to_owned();
                out.push(EmittedEvent {
                    kind: "match.start".into(),
                    confidence: Confidence::Inferred,
                    dedup_key: None,
                    data: json!({
                        "from": scalar(from),
                        "to": scalar(to),
                        "map": self.combat.current_map,
                        "mode": to,
                        "line": line,
                    }),
                });
                return out;
            } else if from_game && !to_game {
                self.combat.in_match = false;
                self.combat.current_map.clear();
                self.combat.reset_round();
                out.push(EmittedEvent {
                    kind: "match.end".into(),
                    confidence: Confidence::Inferred,
                    dedup_key: None,
                    data: json!({
                        "from": scalar(from),
                        "to": scalar(to),
                        "line": line,
                    }),
                });
                return out;
            } else if to_lower.contains("mainmenu")
                || to_lower.contains("lobby")
                || to_lower.contains("waitingtoconnect")
            {
                if self.combat.in_match {
                    self.combat.in_match = false;
                    self.combat.current_map.clear();
                    self.combat.reset_round();
                    out.push(EmittedEvent {
                        kind: "match.end".into(),
                        confidence: Confidence::Inferred,
                        dedup_key: None,
                        data: json!({
                            "from": scalar(from),
                            "to": scalar(to),
                            "line": line,
                        }),
                    });
                } else {
                    self.combat.current_map.clear();
                    out.push(EmittedEvent {
                        kind: "presence.update".into(),
                        confidence: Confidence::Inferred,
                        dedup_key: None,
                        data: json!({
                            "session_loop_state": "MENUS",
                            "map": "",
                        }),
                    });
                }
                return out;
            }
        }

        if let Some(caps) = self.map_re.captures(line) {
            let raw_map = caps.name("map").map_or("", |m| m.as_str().trim());
            let lower = raw_map.to_lowercase();
            if lower.contains("mainmenu") || lower.contains("lobby") {
                self.combat.current_map.clear();
                if self.combat.in_match {
                    self.combat.in_match = false;
                    self.combat.reset_round();
                    out.push(EmittedEvent {
                        kind: "match.end".into(),
                        confidence: Confidence::Inferred,
                        dedup_key: None,
                        data: json!({
                            "map": raw_map,
                            "line": line,
                        }),
                    });
                } else {
                    out.push(EmittedEvent {
                        kind: "presence.update".into(),
                        confidence: Confidence::Inferred,
                        dedup_key: None,
                        data: json!({
                            "session_loop_state": "MENUS",
                            "map": "",
                        }),
                    });
                }
                return out;
            }
            if let Some(clean_map) = Self::normalize_valorant_map(raw_map) {
                self.combat.current_map = clean_map.to_owned();
                out.push(EmittedEvent {
                    kind: "match.map_load".into(),
                    confidence: Confidence::Inferred,
                    dedup_key: None,
                    data: json!({
                        "map": clean_map,
                        "raw_map": raw_map,
                        "line": line,
                    }),
                });
                if !self.combat.in_match {
                    self.combat.in_match = true;
                    self.combat.reset_match();
                    self.combat.current_map = clean_map.to_owned();
                    out.push(EmittedEvent {
                        kind: "match.start".into(),
                        confidence: Confidence::Inferred,
                        dedup_key: None,
                        data: json!({
                            "map": clean_map,
                            "raw_map": raw_map,
                            "line": line,
                        }),
                    });
                }
            }
            return out;
        }

        if let Some(caps) = self.round_end_re.captures(line) {
            let round = caps.name("round").map_or("", |m| m.as_str());
            let mut evs = self.combat.record_round_end(Some(round), None);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    obj.insert("round".into(), scalar(round));
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if self.round_start_re.is_match(line) {
            self.combat.reset_round();
            out.push(EmittedEvent {
                kind: "round.start".into(),
                confidence: Confidence::Inferred,
                dedup_key: None,
                data: json!({
                    "round": self.combat.current_round,
                    "line": line,
                }),
            });
            return out;
        }

        if self.headshot_re.is_match(line) {
            let mut evs = self.combat.record_kill(true, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !self.current_agent.is_empty() {
                        obj.insert("hero".into(), json!(self.current_agent));
                    }
                    if !self.current_weapon.is_empty() {
                        obj.insert("weapon".into(), json!(self.current_weapon));
                    }
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if self.kill_re.is_match(line) {
            let is_leg = self.legshot_re.is_match(line);
            if is_leg {
                self.legshots = self.legshots.saturating_add(1);
            } else {
                self.bodyshots = self.bodyshots.saturating_add(1);
            }
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !self.current_agent.is_empty() {
                        obj.insert("hero".into(), json!(self.current_agent));
                    }
                    if !self.current_weapon.is_empty() {
                        obj.insert("weapon".into(), json!(self.current_weapon));
                    }
                    obj.insert("bodyshot".into(), json!(!is_leg));
                    obj.insert("legshot".into(), json!(is_leg));
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if self.ace_re.is_match(line) {
            self.combat.ace_emitted = true;
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.ace".into(),
                confidence: Confidence::Inferred,
                dedup_key: Some(format!("val_ace:{}:{}", self.combat.current_round, self.combat.seq)),
                data: json!({
                    "round": self.combat.current_round,
                    "round_kills": self.combat.round_kills.max(5),
                    "line": line,
                }),
            });
            return out;
        }

        if self.clutch_re.is_match(line) {
            self.combat.clutch_emitted = true;
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Inferred,
                dedup_key: Some(format!("val_clutch:{}:{}", self.combat.current_round, self.combat.seq)),
                data: json!({
                    "round": self.combat.current_round,
                    "round_kills": self.combat.round_kills.max(1),
                    "line": line,
                }),
            });
            return out;
        }

        if self.plant_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "bomb.planted".into(),
                confidence: Confidence::Inferred,
                dedup_key: Some(format!("val_plant:{}:{}", self.combat.current_round, self.combat.seq)),
                data: json!({
                    "round": self.combat.current_round,
                    "line": line,
                }),
            });
            return out;
        }

        if self.defuse_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "bomb.defused".into(),
                confidence: Confidence::Inferred,
                dedup_key: Some(format!("val_defuse:{}:{}", self.combat.current_round, self.combat.seq)),
                data: json!({
                    "round": self.combat.current_round,
                    "line": line,
                }),
            });
            return out;
        }

        if self.death_re.is_match(line) {
            let mut ev = self.combat.record_death("log");
            if let Some(obj) = ev.data.as_object_mut() {
                obj.insert("line".into(), json!(line));
            }
            out.push(ev);
            return out;
        }

        if self.assist_re.is_match(line) {
            let mut ev = self.combat.record_assist("log");
            if let Some(obj) = ev.data.as_object_mut() {
                obj.insert("line".into(), json!(line));
            }
            out.push(ev);
            return out;
        }

        out
    }
}

pub struct CallOfDutyTracker {
    pub(crate) combat: CombatHighlightTracker,
    match_start_re: Regex,
    match_end_re: Regex,
    round_start_re: Regex,
    round_end_re: Regex,
    kill_re: Regex,
    death_re: Regex,
    assist_re: Regex,
    headshot_re: Regex,
    downed_re: Regex,
    multikill_re: Regex,
    clutch_win_re: Regex,
    bomb_plant_re: Regex,
    bomb_defuse_re: Regex,
}

impl Default for CallOfDutyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CallOfDutyTracker {
    pub fn new() -> Self {
        Self {
            combat: CombatHighlightTracker::default(),
            match_start_re: compile_re(
                r"(?i)(?:MatchStart|LoadingMap|GameState\s*[:=]\s*InProgress|dvar\s+ui_mapname|StartMatch)(?:[^\r\n]*?\bmap[=:\s]+(?P<map>[A-Za-z0-9_/-]+))?(?:[^\r\n]*?\b(?:mode|gametype)[=:\s]+(?P<mode>[A-Za-z0-9_-]+))?",
            ),
            match_end_re: compile_re(
                r"(?i)(?:MatchEnd|GameOver|MatchComplete|PostGame|Victory|Defeat|WarzoneVictory)(?:.*?(?:result|outcome|won)[=:\s]+(?P<won>true|false|1|0|win|loss))?",
            ),
            round_start_re: compile_re(
                r"(?i)(?:RoundStart|BeginRound|SD_RoundStart)(?:.*?round[=:\s]+(?P<round>[0-9]+))?",
            ),
            round_end_re: compile_re(
                r"(?i)(?:RoundEnd|EndRound|SD_RoundEnd)(?:.*?round[=:\s]+(?P<round>[0-9]+))?(?:.*?(?:won|winner)[=:\s]+(?P<won>true|false|1|0|allies|axis))?",
            ),
            kill_re: compile_re(
                r"(?i)(?:PlayerKill|EnemyKilled|EliminatedEnemy|ConfirmKill|Ekia|LocalPlayerKill|KillEvent|ScoreEvent_Kill)(?:.*?weapon[=:\s]+(?P<weapon>[A-Za-z0-9_-]+))?",
            ),
            death_re: compile_re(
                r"(?i)(?:LocalPlayerDeath|PlayerDied|KilledByEnemy|OnLocalPlayerDied|DeathEvent)",
            ),
            assist_re: compile_re(
                r"(?i)(?:PlayerAssist|KillAssist|ScoreEvent_Assist|AssistBonus)",
            ),
            headshot_re: compile_re(
                r"(?i)(?:HeadshotKill|Headshot|ScoreEvent_Headshot|HitLoc_Head)(?:.*?weapon[=:\s]+(?P<weapon>[A-Za-z0-9_-]+))?",
            ),
            downed_re: compile_re(
                r"(?i)(?:EnemyDowned|PlayerDowned|DownedEnemy|WhiteArmorBreakKill|TeamWipe)",
            ),
            multikill_re: compile_re(
                r"(?i)(?P<tier>DoubleKill|TripleKill|FuryKill|FrenzyKill|MegaKill|UltraKill|KillChain|MultiKill)",
            ),
            clutch_win_re: compile_re(
                r"(?i)(?:WarzoneVictory|SearchAndDestroyClutch|LastAliveWin|ClutchMedal|NuclearMedal|ViciousMedal)",
            ),
            bomb_plant_re: compile_re(
                r"(?i)(?:BombPlanted|SD_BombPlanted|PlantedBomb)",
            ),
            bomb_defuse_re: compile_re(
                r"(?i)(?:BombDefused|SD_BombDefused|DefusedBomb)",
            ),
        }
    }

    pub fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let now = Instant::now();

        if self.clutch_win_re.is_match(line) {
            self.combat.clutch_emitted = true;
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("cod_clutch:{}", self.combat.seq)),
                data: json!({
                    "round": self.combat.current_round,
                    "round_kills": self.combat.round_kills.max(1),
                    "total_kills": self.combat.total_kills,
                    "line": line,
                }),
            });
            if line.to_lowercase().contains("victory") {
                self.combat.in_match = false;
                out.push(EmittedEvent {
                    kind: "player.mvp".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("cod_victory_mvp:{}", self.combat.seq)),
                    data: json!({ "victory": true, "line": line }),
                });
                out.push(EmittedEvent {
                    kind: "match.end".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({ "won": true, "line": line }),
                });
            }
            return out;
        }

        if let Some(caps) = self.match_start_re.captures(line) {
            let raw_map_opt = caps
                .name("map")
                .map(|m| m.as_str().trim())
                .filter(|s| !s.is_empty());
            if let Some(rm) = raw_map_opt {
                let low = rm.to_lowercase();
                if low.starts_with("init")
                    || low.contains("lobby")
                    || low.contains("frontend")
                    || low.contains("mainmenu")
                    || low == "none"
                {
                    return out;
                }
            } else if line.to_lowercase().contains("loadingmap") {
                return out;
            }
            let map = raw_map_opt.unwrap_or("Urzikstan");
            let mode = caps
                .name("mode")
                .map(|m| m.as_str().trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("Multiplayer");
            self.combat.reset_match();
            self.combat.in_match = true;
            self.combat.current_map = map.to_string();
            self.combat.current_mode = mode.to_string();
            out.push(EmittedEvent {
                kind: "presence.update".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "map": map,
                    "mode": mode,
                    "state": "In Match",
                }),
            });
            out.push(EmittedEvent {
                kind: "match.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "map": map,
                    "mode": mode,
                    "line": line,
                }),
            });
            return out;
        }

        if let Some(caps) = self.round_start_re.captures(line) {
            if let Some(r) = caps.name("round").and_then(|m| m.as_str().parse::<u32>().ok()) {
                self.combat.current_round = r;
            }
            self.combat.reset_round();
            out.push(EmittedEvent {
                kind: "round.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "round": self.combat.current_round,
                    "line": line,
                }),
            });
            return out;
        }

        if let Some(caps) = self.round_end_re.captures(line) {
            let round_str = caps.name("round").map(|m| m.as_str());
            let won = caps.name("won").map(|m| {
                matches!(
                    m.as_str().to_lowercase().as_str(),
                    "true" | "1" | "win" | "allies"
                )
            });
            out.extend(self.combat.record_round_end(round_str, won));
            return out;
        }

        if self.bomb_plant_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "bomb.planted".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("cod_plant:{}", self.combat.seq)),
                data: json!({ "round": self.combat.current_round, "line": line }),
            });
            return out;
        }

        if self.bomb_defuse_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "bomb.defused".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("cod_defuse:{}", self.combat.seq)),
                data: json!({ "round": self.combat.current_round, "line": line }),
            });
            return out;
        }

        if let Some(caps) = self.headshot_re.captures(line) {
            self.combat.last_kill_at = None;
            let weapon = caps.name("weapon").map(|m| m.as_str()).unwrap_or("");
            let mut evs = self.combat.record_kill(true, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !weapon.is_empty() {
                        obj.insert("weapon".into(), json!(weapon));
                    }
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if let Some(caps) = self.kill_re.captures(line) {
            self.combat.last_kill_at = None;
            let weapon = caps.name("weapon").map(|m| m.as_str()).unwrap_or("");
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !weapon.is_empty() {
                        obj.insert("weapon".into(), json!(weapon));
                    }
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if self.downed_re.is_match(line) {
            self.combat.last_kill_at = None;
            let is_team_wipe = line.to_lowercase().contains("teamwipe");
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    obj.insert("downed".into(), json!(true));
                    obj.insert("team_wipe".into(), json!(is_team_wipe));
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            if is_team_wipe && !self.combat.clutch_emitted {
                self.combat.clutch_emitted = true;
                self.combat.seq = self.combat.seq.wrapping_add(1);
                out.push(EmittedEvent {
                    kind: "player.clutch".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("cod_wipe:{}", self.combat.seq)),
                    data: json!({
                        "team_wipe": true,
                        "count": self.combat.round_kills.max(1),
                        "line": line,
                    }),
                });
            }
            return out;
        }

        if let Some(caps) = self.multikill_re.captures(line) {
            let tier = caps.name("tier").map_or("", |m| m.as_str()).to_lowercase();
            let count = match tier.as_str() {
                "doublekill" => 2,
                "triplekill" => 3,
                "furykill" => 4,
                "frenzykill" => 5,
                "megakill" => 6,
                "ultrakill" => 7,
                "killchain" => 8,
                _ => self.combat.streak_kills.max(2),
            };
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.multikill".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("cod_mk:{}:{count}", self.combat.seq)),
                data: json!({
                    "count": count,
                    "tier": tier,
                    "line": line,
                }),
            });
            if count >= 4 && !self.combat.ace_emitted {
                self.combat.ace_emitted = true;
                out.push(EmittedEvent {
                    kind: "player.ace".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("cod_ace:{}", self.combat.seq)),
                    data: json!({
                        "count": count,
                        "tier": tier,
                        "line": line,
                    }),
                });
            }
            return out;
        }

        if self.death_re.is_match(line) {
            let mut ev = self.combat.record_death("log");
            if let Some(obj) = ev.data.as_object_mut() {
                obj.insert("line".into(), json!(line));
            }
            out.push(ev);
            return out;
        }

        if self.assist_re.is_match(line) {
            let mut ev = self.combat.record_assist("log");
            if let Some(obj) = ev.data.as_object_mut() {
                obj.insert("line".into(), json!(line));
            }
            out.push(ev);
            return out;
        }

        if let Some(caps) = self.match_end_re.captures(line) {
            self.combat.in_match = false;
            let won = caps.name("won").map(|m| {
                matches!(m.as_str().to_lowercase().as_str(), "true" | "1" | "win")
            });
            out.push(EmittedEvent {
                kind: "match.end".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "won": won,
                    "total_kills": self.combat.total_kills,
                    "total_deaths": self.combat.total_deaths,
                    "total_assists": self.combat.total_assists,
                    "line": line,
                }),
            });
            return out;
        }

        out
    }
}

pub(crate) fn upsert_ini(content: &str, section: &str, key: &str, value: &str) -> String {
    let line_ending = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let header = format!("[{section}]");
    let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
    let entry = format!("{key}={value}");
    let Some(start) = lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case(&header))
    else {
        if lines.last().map_or(false, |l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(header);
        lines.push(entry);
        return lines.join(line_ending) + line_ending;
    };
    let end = (start + 1..lines.len())
        .find(|&i| {
            let trimmed = lines[i].trim_start();
            trimmed.starts_with('[') && !trimmed.starts_with(';') && !trimmed.starts_with('#')
        })
        .unwrap_or(lines.len());
    let existing = (start + 1..end).find(|&i| {
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with(';') || trimmed.starts_with('#') {
            return false;
        }
        trimmed
            .split_once('=')
            .map_or(false, |(k, _)| k.trim().eq_ignore_ascii_case(key))
    });
    match existing {
        Some(i) => lines[i] = entry,
        None => lines.insert(start + 1, entry),
    }
    lines.join(line_ending) + line_ending
}


pub struct FortniteTracker {
    pub(crate) combat: CombatHighlightTracker,
    state_re: Regex,
    playlist_re: Regex,
    map_re: Regex,
    elim_re: Regex,
    death_re: Regex,
    assist_re: Regex,
    knock_re: Regex,
    headshot_re: Regex,
    win_re: Regex,
}

impl Default for FortniteTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FortniteTracker {
    pub fn new() -> Self {
        Self {
            combat: CombatHighlightTracker::default(),
            state_re: compile_re(
                r"LogGameState:\s*Match State Changed from (?P<from>[A-Za-z0-9_]+) to (?P<to>[A-Za-z0-9_]+)",
            ),
            playlist_re: compile_re(
                r"(?i)(?:LogPlaylistManager|LogFort|LogAthena|Playlist).*?(?P<playlist>Playlist_[A-Za-z0-9_]+|ZeroBuild|NoBuild|Habanero|Reload|Ballistic|Showdown|BattleRoyale)",
            ),
            map_re: compile_re(
                r"(?i)(?:LogNet:\s*Browse:|LogLoad:\s*LoadMap:)\s*(?P<map>[A-Za-z0-9_/-]+)",
            ),
            elim_re: compile_re(
                r"(?i)(?:LogFort|LogAthena|LogFortElimination|LogKillFeed|FortPlayerControllerAthena|FortPlayerStateAthena).*?(?:ClientReceiveKillNotification|OnKilledPlayer|EliminatedPlayer|PlayerEliminated|NotifyPlayerEliminated|PlayEliminationAudio|ConfirmedElimination|LocalPlayerEliminatedEnemy|KillScore)",
            ),
            death_re: compile_re(
                r"(?i)(?:LogFort|LogAthena|FortPlayerPawnAthena|FortPlayerControllerAthena).*?(?:LocalPlayerDied|OnLocalPlayerDied|ClientNotifyPawnDied|EliminatedBy)",
            ),
            assist_re: compile_re(
                r"(?i)(?:LogFort|LogAthena|FortPlayerControllerAthena).*?(?:AssistElimination|KillAssist|AssistScore)",
            ),
            knock_re: compile_re(
                r"(?i)(?:LogFort|LogAthena|FortPlayerPawnAthena|FortPlayerControllerAthena).*?(?:OnEnterDBNO|EnemyDowned|PlayerKnockedDown|ClientNotifyPawnDowned|KnockdownScore)",
            ),
            headshot_re: compile_re(
                r"(?i)(?:LogFort|LogAthena|FortPlayerControllerAthena).*?(?:Headshot|CriticalKill|bIsHeadshot=true|HeadshotElimination)",
            ),
            win_re: compile_re(
                r"(?i)(?:LogFort|LogAthena|AthenaMatchReport|FortPlayerControllerAthena).*?(?:OnMatchWon|VictoryRoyale|PlayVictoryRoyale|ClientNotifyWon|Placement[:=]\s*1\b)",
            ),
        }
    }

    fn normalize_playlist(raw: &str) -> String {
        let lower = raw.to_lowercase();
        if lower.contains("nobuild") || lower.contains("zerobuild") {
            if lower.contains("duo") {
                "Zero Build Duos".into()
            } else if lower.contains("trio") {
                "Zero Build Trios".into()
            } else if lower.contains("squad") {
                "Zero Build Squads".into()
            } else {
                "Zero Build".into()
            }
        } else if lower.contains("habanero") || lower.contains("showdown") {
            "Ranked Battle Royale".into()
        } else if lower.contains("reload") || lower.contains("punchberry") {
            "Fortnite Reload".into()
        } else if lower.contains("ballistic") || lower.contains("feral") {
            "Fortnite Ballistic".into()
        } else if lower.contains("solo") {
            "Battle Royale Solo".into()
        } else if lower.contains("duo") {
            "Battle Royale Duos".into()
        } else if lower.contains("trio") {
            "Battle Royale Trios".into()
        } else if lower.contains("squad") {
            "Battle Royale Squads".into()
        } else if lower.contains("playground") || lower.contains("creative") {
            "Creative".into()
        } else {
            "Battle Royale".into()
        }
    }

    pub fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let now = Instant::now();

        if let Some(caps) = self.playlist_re.captures(line) {
            let raw_pl = caps.name("playlist").map_or("", |m| m.as_str());
            let clean_mode = Self::normalize_playlist(raw_pl);
            self.combat.current_mode = clean_mode.clone();
            out.push(EmittedEvent {
                kind: "presence.update".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "mode": clean_mode,
                    "playlist": raw_pl,
                    "map": if self.combat.current_map.is_empty() { "Battle Royale Island" } else { &self.combat.current_map },
                }),
            });
        }

        if let Some(caps) = self.map_re.captures(line) {
            let m_raw = caps.name("map").map_or("", |m| m.as_str());
            let lower = m_raw.to_lowercase();
            if lower.contains("frontend") || lower.contains("lobby") {
                if self.combat.in_match {
                    self.combat.in_match = false;
                    out.push(EmittedEvent {
                        kind: "match.end".into(),
                        confidence: Confidence::Inferred,
                        dedup_key: None,
                        data: json!({ "map": m_raw, "line": line }),
                    });
                }
            } else if lower.contains("athena")
                || lower.contains("apollo")
                || lower.contains("artemis")
                || lower.contains("asteria")
                || lower.contains("helios")
                || lower.contains("hermes")
                || lower.contains("terrain")
            {
                self.combat.current_map = "Battle Royale Island".into();
            }
        }

        if let Some(caps) = self.state_re.captures(line) {
            let from = caps.name("from").map_or("", |m| m.as_str());
            let to = caps.name("to").map_or("", |m| m.as_str());
            if to == "InProgress" && from != "InProgress" {
                self.combat.reset_match();
                self.combat.in_match = true;
                if self.combat.current_map.is_empty() {
                    self.combat.current_map = "Battle Royale Island".into();
                }
                if self.combat.current_mode.is_empty() {
                    self.combat.current_mode = "Battle Royale".into();
                }
                out.push(EmittedEvent {
                    kind: "presence.update".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({
                        "map": self.combat.current_map,
                        "mode": self.combat.current_mode,
                        "state": "InProgress",
                    }),
                });
                out.push(EmittedEvent {
                    kind: "match.start".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({
                        "from": from,
                        "to": to,
                        "map": self.combat.current_map,
                        "mode": self.combat.current_mode,
                        "line": line,
                    }),
                });
                return out;
            } else if from == "InProgress"
                && matches!(to, "WaitingPostMatch" | "LeavingMap" | "Aborted")
            {
                self.combat.in_match = false;
                out.push(EmittedEvent {
                    kind: "match.end".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({
                        "from": from,
                        "to": to,
                        "total_kills": self.combat.total_kills,
                        "line": line,
                    }),
                });
                return out;
            }
        }

        if self.win_re.is_match(line) {
            self.combat.clutch_emitted = true;
            self.combat.in_match = false;
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.mvp".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("fn_win_mvp:{}", self.combat.seq)),
                data: json!({
                    "victory_royale": true,
                    "total_kills": self.combat.total_kills,
                    "line": line,
                }),
            });
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("fn_win_clutch:{}", self.combat.seq)),
                data: json!({
                    "victory_royale": true,
                    "count": self.combat.round_kills.max(1),
                    "total_kills": self.combat.total_kills,
                    "line": line,
                }),
            });
            out.push(EmittedEvent {
                kind: "match.end".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "won": true,
                    "victory_royale": true,
                    "total_kills": self.combat.total_kills,
                    "line": line,
                }),
            });
            return out;
        }

        if self.headshot_re.is_match(line) {
            self.combat.last_kill_at = None;
            let mut evs = self.combat.record_kill(true, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if self.elim_re.is_match(line) {
            self.combat.last_kill_at = None;
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if self.knock_re.is_match(line) {
            self.combat.last_kill_at = None;
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    obj.insert("knockdown".into(), json!(true));
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if self.death_re.is_match(line) {
            let mut ev = self.combat.record_death("log");
            if let Some(obj) = ev.data.as_object_mut() {
                obj.insert("line".into(), json!(line));
            }
            out.push(ev);
            return out;
        }

        if self.assist_re.is_match(line) {
            let mut ev = self.combat.record_assist("log");
            if let Some(obj) = ev.data.as_object_mut() {
                obj.insert("line".into(), json!(line));
            }
            out.push(ev);
            return out;
        }

        out
    }
}



pub struct ApexTracker {
    pub(crate) combat: CombatHighlightTracker,
    pub(crate) local_name: String,
    pub(crate) local_legend: String,
    line_start_re: Regex,
    line_kill_re: Regex,
    line_down_re: Regex,
    line_win_re: Regex,
}

impl Default for ApexTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ApexTracker {
    pub fn new() -> Self {
        Self {
            combat: CombatHighlightTracker::default(),
            local_name: String::new(),
            local_legend: String::new(),
            line_start_re: compile_re(
                r"(?i)(?:MATCH_START|matchSetup|LoadingLevel|LoadMap)(?:[^\r\n]*?\bmap[=:\s]+(?P<map>[A-Za-z0-9_/-]+))?(?:[^\r\n]*?\bplaylist[=:\s]+(?P<mode>[A-Za-z0-9_ -]+))?",
            ),
            line_kill_re: compile_re(
                r"(?i)(?:PLAYER_KILLED|playerKilled|EliminatedEnemy|ConfirmKill)(?:.*?weapon[=:\s]+(?P<weapon>[A-Za-z0-9_-]+))?(?:.*?headshot[=:\s]+(?P<hs>true|1))?",
            ),
            line_down_re: compile_re(
                r"(?i)(?:PLAYER_DOWNED|playerDowned|EnemyDowned|KnockedDown)(?:.*?weapon[=:\s]+(?P<weapon>[A-Za-z0-9_-]+))?",
            ),
            line_win_re: compile_re(
                r"(?i)(?:APEX_CHAMPION|MatchWinner|SquadEliminated.*placement[=:\s]+1\b|WinnerDetermined)",
            ),
        }
    }

    pub fn normalize_apex_map(raw: &str) -> String {
        let lower = raw.to_lowercase();
        if lower.contains("canyonlands") || lower.contains("kings_canyon") {
            "Kings Canyon".into()
        } else if lower.contains("desertlands") || lower.contains("worlds_edge") {
            "World's Edge".into()
        } else if lower.contains("olympus") {
            "Olympus".into()
        } else if lower.contains("tropic_island") || lower.contains("storm_point") {
            "Storm Point".into()
        } else if lower.contains("divided_moon") || lower.contains("broken_moon") {
            "Broken Moon".into()
        } else if lower.contains("district") {
            "E-District".into()
        } else if lower.contains("freedm") || lower.contains("thunderdome") {
            "Thunderdome".into()
        } else {
            raw.to_string()
        }
    }

    fn is_local_actor(&self, actor: &Value) -> bool {
        if self.local_name.is_empty() {
            return true;
        }
        let name = actor
            .get("name")
            .or_else(|| actor.get("playerName"))
            .and_then(Value::as_str)
            .or_else(|| actor.as_str())
            .unwrap_or("");
        name.is_empty() || name.eq_ignore_ascii_case(&self.local_name)
    }

    pub fn ingest_json(&mut self, v: &Value) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        if let Some(arr) = v.as_array() {
            for item in arr {
                out.extend(self.ingest_json(item));
            }
            return out;
        }
        if let Some(events_arr) = v.get("events").and_then(Value::as_array) {
            for item in events_arr {
                out.extend(self.ingest_json(item));
            }
            return out;
        }

        let category = text(v, "/category")
            .chars()
            .collect::<String>();
        let cat = if !category.is_empty() {
            category
        } else if !text(v, "/event").is_empty() {
            text(v, "/event")
        } else if v.get("matchSetup").is_some() {
            "matchSetup".into()
        } else if v.get("gameStateChanged").is_some() {
            "gameStateChanged".into()
        } else if v.get("playerKilled").is_some() {
            "playerKilled".into()
        } else if v.get("playerDowned").is_some() {
            "playerDowned".into()
        } else if v.get("characterSelected").is_some() {
            "characterSelected".into()
        } else if v.get("squadEliminated").is_some() {
            "squadEliminated".into()
        } else {
            String::new()
        };

        let inner = v.get(&cat).unwrap_or(v);
        let now = Instant::now();

        match cat.as_str() {
            "init" => {
                return out;
            }
            "matchSetup" => {
                let raw_map = text(inner, "/map");
                let low_map = raw_map.to_lowercase();
                if low_map.contains("lobby")
                    || low_map.contains("staging")
                    || low_map.starts_with("init")
                    || low_map.contains("frontend")
                {
                    self.combat.in_match = false;
                    self.combat.current_map.clear();
                    out.push(EmittedEvent {
                        kind: "presence.update".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: None,
                        data: json!({
                            "session_loop_state": "MENUS",
                            "map": "",
                        }),
                    });
                    return out;
                }
                let clean_map = if raw_map.is_empty() {
                    "World's Edge".into()
                } else {
                    Self::normalize_apex_map(&raw_map)
                };
                let mode = {
                    let pl = text(inner, "/playlistName");
                    if pl.is_empty() {
                        text(inner, "/playlistDesc")
                    } else {
                        pl
                    }
                };
                self.combat.reset_match();
                self.combat.in_match = true;
                self.combat.current_map = clean_map.clone();
                self.combat.current_mode = if mode.is_empty() {
                    "Battle Royale".into()
                } else {
                    mode.clone()
                };
                out.push(EmittedEvent {
                    kind: "presence.update".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({
                        "map": clean_map,
                        "mode": self.combat.current_mode,
                        "hero": self.local_legend,
                        "state": "In Match",
                    }),
                });
                out.push(EmittedEvent {
                    kind: "match.start".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({
                        "map": clean_map,
                        "mode": self.combat.current_mode,
                    }),
                });
            }
            "characterSelected" => {
                let p_name = text(inner, "/player/name");
                let character = text(inner, "/player/character")
                    .or_else_non_empty(|| text(inner, "/character"));
                if !p_name.is_empty() && self.local_name.is_empty() {
                    self.local_name = p_name;
                }
                if !character.is_empty() {
                    self.local_legend = character.clone();
                    out.push(EmittedEvent {
                        kind: "presence.update".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: None,
                        data: json!({
                            "hero": character,
                            "map": self.combat.current_map,
                            "mode": self.combat.current_mode,
                        }),
                    });
                }
            }
            "gameStateChanged" => {
                let state = text(inner, "/state");
                if state.eq_ignore_ascii_case("Playing") && !self.combat.in_match {
                    self.combat.reset_match();
                    self.combat.in_match = true;
                    out.push(EmittedEvent {
                        kind: "match.start".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: None,
                        data: json!({ "state": state }),
                    });
                } else if matches!(
                    state.as_str(),
                    "Resolution" | "Postmatch" | "Prematch"
                ) && self.combat.in_match
                    && state != "Prematch"
                {
                    self.combat.in_match = false;
                    out.push(EmittedEvent {
                        kind: "match.end".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: None,
                        data: json!({
                            "state": state,
                            "total_kills": self.combat.total_kills,
                        }),
                    });
                }
            }
            "playerKilled" => {
                let attacker = inner
                    .get("attacker")
                    .or_else(|| inner.get("awardedTo"))
                    .unwrap_or(&Value::Null);
                let victim = inner.get("victim").unwrap_or(&Value::Null);
                let victim_is_local = !self.local_name.is_empty() && self.is_local_actor(victim);
                let by_local = opt_bool(inner, "/by_local").unwrap_or_else(|| self.is_local_actor(attacker));
                if victim_is_local && !by_local {
                    out.push(self.combat.record_death("log"));
                } else if by_local {
                    self.combat.last_kill_at = None;
                    let weapon = text(inner, "/weapon");
                    let headshot = opt_bool(inner, "/headshot").unwrap_or(false);
                    let legend = text(attacker, "/character")
                        .or_else_non_empty(|| self.local_legend.clone());
                    let mut evs = self.combat.record_kill(headshot, "log", now);
                    for ev in &mut evs {
                        if let Some(obj) = ev.data.as_object_mut() {
                            if !weapon.is_empty() {
                                obj.insert("weapon".into(), json!(weapon));
                            }
                            if !legend.is_empty() {
                                obj.insert("hero".into(), json!(legend));
                            }
                        }
                    }
                    out.extend(evs);
                    if self.combat.round_kills >= 3 && !self.combat.clutch_emitted {
                        self.combat.clutch_emitted = true;
                        self.combat.seq = self.combat.seq.wrapping_add(1);
                        out.push(EmittedEvent {
                            kind: "player.clutch".into(),
                            confidence: Confidence::Confirmed,
                            dedup_key: Some(format!("apex_squad_wipe:{}", self.combat.seq)),
                            data: json!({
                                "squad_wipe": true,
                                "count": self.combat.round_kills,
                                "weapon": weapon,
                            }),
                        });
                    }
                }
            }
            "playerDowned" => {
                let attacker = inner.get("attacker").unwrap_or(&Value::Null);
                if self.is_local_actor(attacker) {
                    self.combat.last_kill_at = None;
                    let weapon = text(inner, "/weapon");
                    let mut evs = self.combat.record_kill(false, "log", now);
                    for ev in &mut evs {
                        if let Some(obj) = ev.data.as_object_mut() {
                            obj.insert("knockdown".into(), json!(true));
                            if !weapon.is_empty() {
                                obj.insert("weapon".into(), json!(weapon));
                            }
                        }
                    }
                    out.extend(evs);
                }
            }
            "playerAssist" => {
                let assistant = inner.get("assistant").unwrap_or(&Value::Null);
                if self.is_local_actor(assistant) {
                    self.combat.seq = self.combat.seq.wrapping_add(1);
                    out.push(EmittedEvent {
                        kind: "player.assist".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: Some(format!("apex_assist:{}", self.combat.seq)),
                        data: json!({ "count": 1 }),
                    });
                }
            }
            "squadEliminated" | "matchStateEnd" => {
                let placement = opt_num(inner, "/placement").unwrap_or(0);
                let won = opt_bool(inner, "/won").unwrap_or(placement == 1);
                if won {
                    self.combat.in_match = false;
                    self.combat.seq = self.combat.seq.wrapping_add(1);
                    out.push(EmittedEvent {
                        kind: "player.mvp".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: Some(format!("apex_champ:{}", self.combat.seq)),
                        data: json!({
                            "champion": true,
                            "total_kills": self.combat.total_kills,
                        }),
                    });
                    out.push(EmittedEvent {
                        kind: "player.clutch".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: Some(format!("apex_champ_clutch:{}", self.combat.seq)),
                        data: json!({
                            "champion": true,
                            "count": self.combat.round_kills.max(1),
                        }),
                    });
                    out.push(EmittedEvent {
                        kind: "match.end".into(),
                        confidence: Confidence::Confirmed,
                        dedup_key: None,
                        data: json!({
                            "won": true,
                            "champion": true,
                            "total_kills": self.combat.total_kills,
                        }),
                    });
                }
            }
            _ => {}
        }

        out
    }

    pub fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let trimmed = line.trim();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                return self.ingest_json(&val);
            }
        }
        let mut out = Vec::new();
        let now = Instant::now();

        if self.line_win_re.is_match(trimmed) {
            self.combat.in_match = false;
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.mvp".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("apex_line_champ:{}", self.combat.seq)),
                data: json!({ "champion": true, "line": trimmed }),
            });
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("apex_line_clutch:{}", self.combat.seq)),
                data: json!({ "champion": true, "line": trimmed }),
            });
            out.push(EmittedEvent {
                kind: "match.end".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({ "won": true, "line": trimmed }),
            });
            return out;
        }

        if let Some(caps) = self.line_start_re.captures(trimmed) {
            let raw_map = caps.name("map").map_or("World's Edge", |m| m.as_str());
            let clean_map = Self::normalize_apex_map(raw_map);
            let mode = caps.name("mode").map_or("Ranked Leagues", |m| m.as_str());
            self.combat.reset_match();
            self.combat.in_match = true;
            self.combat.current_map = clean_map.clone();
            self.combat.current_mode = mode.to_string();
            out.push(EmittedEvent {
                kind: "match.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({ "map": clean_map, "mode": mode, "line": trimmed }),
            });
            return out;
        }

        if let Some(caps) = self.line_kill_re.captures(trimmed) {
            self.combat.last_kill_at = None;
            let weapon = caps.name("weapon").map_or("", |m| m.as_str());
            let hs = caps
                .name("hs")
                .map_or(false, |m| matches!(m.as_str(), "true" | "1"));
            let mut evs = self.combat.record_kill(hs, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !weapon.is_empty() {
                        obj.insert("weapon".into(), json!(weapon));
                    }
                    obj.insert("line".into(), json!(trimmed));
                }
            }
            out.extend(evs);
            if self.combat.round_kills >= 3 && !self.combat.clutch_emitted {
                self.combat.clutch_emitted = true;
                self.combat.seq = self.combat.seq.wrapping_add(1);
                out.push(EmittedEvent {
                    kind: "player.clutch".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("apex_line_wipe:{}", self.combat.seq)),
                    data: json!({ "squad_wipe": true, "count": self.combat.round_kills }),
                });
            }
            return out;
        }

        if let Some(caps) = self.line_down_re.captures(trimmed) {
            self.combat.last_kill_at = None;
            let weapon = caps.name("weapon").map_or("", |m| m.as_str());
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    obj.insert("knockdown".into(), json!(true));
                    if !weapon.is_empty() {
                        obj.insert("weapon".into(), json!(weapon));
                    }
                    obj.insert("line".into(), json!(trimmed));
                }
            }
            out.extend(evs);
            return out;
        }

        out
    }
}

trait StringFallbackExt {
    fn or_else_non_empty<F: FnOnce() -> String>(self, f: F) -> String;
}

impl StringFallbackExt for String {
    fn or_else_non_empty<F: FnOnce() -> String>(self, f: F) -> String {
        if self.trim().is_empty() {
            f()
        } else {
            self
        }
    }
}



pub struct RainbowSixTracker {
    pub(crate) combat: CombatHighlightTracker,
    pub(crate) active_match_folder: String,
    pub(crate) seen_round_files: Vec<String>,
    pub(crate) operator: String,
    match_re: Regex,
    round_start_re: Regex,
    round_end_re: Regex,
    kill_re: Regex,
    headshot_re: Regex,
    plant_re: Regex,
    defuse_re: Regex,
    clutch_re: Regex,
    ace_re: Regex,
    rec_round_re: Regex,
}

impl Default for RainbowSixTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RainbowSixTracker {
    pub fn new() -> Self {
        Self {
            combat: CombatHighlightTracker::default(),
            active_match_folder: String::new(),
            seen_round_files: Vec::new(),
            operator: String::new(),
            match_re: compile_re(
                r"(?i)(?:MATCH_START|LoadingMap|MapLoaded).*?(?:map[=:\s]+(?P<map>[A-Za-z0-9_ -]+))?(?:.*?mode[=:\s]+(?P<mode>[A-Za-z0-9_ -]+))?",
            ),
            round_start_re: compile_re(
                r"(?i)(?:ROUND_START|ActionPhaseStart|PrepPhaseStart)(?:.*?round[=:\s]+(?P<round>[0-9]+))?(?:.*?operator[=:\s]+(?P<op>[A-Za-z0-9_]+))?",
            ),
            round_end_re: compile_re(
                r"(?i)(?:ROUND_END|RoundCompleted|RoundWon|RoundLost)(?:.*?round[=:\s]+(?P<round>[0-9]+))?(?:.*?(?:won|result)[=:\s]+(?P<won>true|false|1|0|win|loss))?",
            ),
            kill_re: compile_re(
                r"(?i)(?:KILL|ELIMINATED|ConfirmKill|OperatorEliminated|InjureKill)(?:.*?weapon[=:\s]+(?P<weapon>[A-Za-z0-9_-]+))?",
            ),
            headshot_re: compile_re(
                r"(?i)(?:HEADSHOT|HeadshotBonus|HeadshotKill)(?:.*?weapon[=:\s]+(?P<weapon>[A-Za-z0-9_-]+))?",
            ),
            plant_re: compile_re(
                r"(?i)(?:DEFUSER_PLANTED|BombPlanted|PlantedDefuser)",
            ),
            defuse_re: compile_re(
                r"(?i)(?:DEFUSER_DISABLED|BombDefused|DisabledDefuser)",
            ),
            clutch_re: compile_re(
                r"(?i)(?:CLUTCH|OneVsManyWin|ClutchRoundWin)",
            ),
            ace_re: compile_re(
                r"(?i)(?:\bACE\b|TeamWipe5K)",
            ),
            rec_round_re: compile_re(r"(?i)-R(?P<round>[0-9]{1,2})\.rec$"),
        }
    }

    fn extract_header_hint(header_bytes: &[u8]) -> (Option<String>, Option<String>) {
        let lossy = String::from_utf8_lossy(header_bytes).to_lowercase();
        let known_maps = [
            ("oregon", "Oregon"),
            ("clubhouse", "Clubhouse"),
            ("coastline", "Coastline"),
            ("kafe", "Kafe Dostoyevsky"),
            ("chalet", "Chalet"),
            ("bank", "Bank"),
            ("border", "Border"),
            ("consulate", "Consulate"),
            ("villa", "Villa"),
            ("nighthaven", "Nighthaven Labs"),
            ("lair", "Lair"),
            ("emerald", "Emerald Plains"),
            ("kanal", "Kanal"),
            ("outback", "Outback"),
            ("skyscraper", "Skyscraper"),
            ("themepark", "Theme Park"),
        ];
        let known_ops = [
            "ash", "jager", "doc", "bandit", "thermite", "twitch", "smoke", "mute", "hibana",
            "ace", "azami", "fenrir", "deimos", "buck", "valkyrie", "lesion", "sledge",
        ];
        let found_map = known_maps
            .iter()
            .find(|(k, _)| lossy.contains(k))
            .map(|(_, v)| (*v).to_string());
        let found_op = known_ops
            .iter()
            .find(|k| lossy.contains(*k))
            .map(|v| {
                let mut c = v.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            });
        (found_map, found_op)
    }

    pub fn ingest_replay_round(
        &mut self,
        match_folder_name: &str,
        rec_file_name: &str,
        header_bytes: &[u8],
    ) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let (hint_map, hint_op) = Self::extract_header_hint(header_bytes);

        if let Some(ref m) = hint_map {
            self.combat.current_map = m.clone();
        }
        if let Some(ref op) = hint_op {
            self.operator = op.clone();
        }

        if match_folder_name != self.active_match_folder && !match_folder_name.is_empty() {
            self.active_match_folder = match_folder_name.to_string();
            self.seen_round_files.clear();
            self.combat.reset_match();
            self.combat.in_match = true;
            if self.combat.current_mode.is_empty() {
                self.combat.current_mode = "Bomb".into();
            }
            out.push(EmittedEvent {
                kind: "match.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("r6_match:{match_folder_name}")),
                data: json!({
                    "match_folder": match_folder_name,
                    "map": self.combat.current_map,
                    "mode": self.combat.current_mode,
                    "operator": self.operator,
                }),
            });
        }

        if !rec_file_name.is_empty()
            && !self
                .seen_round_files
                .iter()
                .any(|f| f.eq_ignore_ascii_case(rec_file_name))
        {
            self.seen_round_files.push(rec_file_name.to_string());
            let round_num = self
                .rec_round_re
                .captures(rec_file_name)
                .and_then(|c| c.name("round"))
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(self.seen_round_files.len() as u32);

            if round_num > 1 && self.combat.round_kills > 0 {
                let prev_r = (round_num - 1).to_string();
                out.extend(self.combat.record_round_end(Some(&prev_r), Some(true)));
            }

            self.combat.current_round = round_num;
            self.combat.reset_round();

            out.push(EmittedEvent {
                kind: "presence.update".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "map": self.combat.current_map,
                    "mode": self.combat.current_mode,
                    "hero": self.operator,
                    "rounds_played": round_num,
                }),
            });
            out.push(EmittedEvent {
                kind: "round.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("r6_round:{match_folder_name}:{round_num}")),
                data: json!({
                    "round": round_num,
                    "map": self.combat.current_map,
                    "operator": self.operator,
                    "file": rec_file_name,
                }),
            });
        }

        out
    }

    pub fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let now = Instant::now();

        if let Some(caps) = self.match_re.captures(line) {
            let map = caps.name("map").map_or("Oregon", |m| m.as_str().trim());
            let mode = caps.name("mode").map_or("Ranked Bomb", |m| m.as_str().trim());
            self.combat.reset_match();
            self.combat.in_match = true;
            self.combat.current_map = map.to_string();
            self.combat.current_mode = mode.to_string();
            out.push(EmittedEvent {
                kind: "presence.update".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({ "map": map, "mode": mode }),
            });
            out.push(EmittedEvent {
                kind: "match.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({ "map": map, "mode": mode, "line": line }),
            });
            return out;
        }

        if let Some(caps) = self.round_start_re.captures(line) {
            if let Some(r) = caps.name("round").and_then(|m| m.as_str().parse::<u32>().ok()) {
                self.combat.current_round = r;
            }
            if let Some(op) = caps.name("op").map(|m| m.as_str().trim()) {
                if !op.is_empty() {
                    self.operator = op.to_string();
                }
            }
            self.combat.reset_round();
            out.push(EmittedEvent {
                kind: "round.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "round": self.combat.current_round,
                    "operator": self.operator,
                    "line": line,
                }),
            });
            return out;
        }

        if let Some(caps) = self.round_end_re.captures(line) {
            let r_str = caps.name("round").map(|m| m.as_str());
            let won = caps.name("won").map(|m| {
                matches!(m.as_str().to_lowercase().as_str(), "true" | "1" | "win")
            });
            out.extend(self.combat.record_round_end(r_str, won));
            return out;
        }

        if self.plant_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "bomb.planted".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("r6_plant:{}", self.combat.seq)),
                data: json!({ "round": self.combat.current_round, "line": line }),
            });
            return out;
        }

        if self.defuse_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "bomb.defused".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("r6_defuse:{}", self.combat.seq)),
                data: json!({ "round": self.combat.current_round, "line": line }),
            });
            return out;
        }

        if self.ace_re.is_match(line) {
            self.combat.ace_emitted = true;
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.ace".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("r6_ace:{}", self.combat.seq)),
                data: json!({
                    "round": self.combat.current_round,
                    "round_kills": self.combat.round_kills.max(5),
                    "line": line,
                }),
            });
            return out;
        }

        if self.clutch_re.is_match(line) {
            self.combat.clutch_emitted = true;
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("r6_clutch:{}", self.combat.seq)),
                data: json!({
                    "round": self.combat.current_round,
                    "round_kills": self.combat.round_kills.max(1),
                    "line": line,
                }),
            });
            return out;
        }

        if let Some(caps) = self.headshot_re.captures(line) {
            self.combat.last_kill_at = None;
            let weapon = caps.name("weapon").map_or("", |m| m.as_str());
            let mut evs = self.combat.record_kill(true, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !weapon.is_empty() {
                        obj.insert("weapon".into(), json!(weapon));
                    }
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if let Some(caps) = self.kill_re.captures(line) {
            self.combat.last_kill_at = None;
            let weapon = caps.name("weapon").map_or("", |m| m.as_str());
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !weapon.is_empty() {
                        obj.insert("weapon".into(), json!(weapon));
                    }
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        out
    }
}



pub struct TheFinalsTracker {
    pub(crate) combat: CombatHighlightTracker,
    state_re: Regex,
    map_re: Regex,
    elim_re: Regex,
    headshot_re: Regex,
    wipe_re: Regex,
    cashout_re: Regex,
    win_re: Regex,
}

impl Default for TheFinalsTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TheFinalsTracker {
    pub fn new() -> Self {
        Self {
            combat: CombatHighlightTracker::default(),
            state_re: compile_re(
                r"(?i)(?:LogGameState:\s*Match State Changed from (?P<from>[A-Za-z0-9_]+) to (?P<to>[A-Za-z0-9_]+)|MATCH_START|TournamentRoundStart)",
            ),
            map_re: compile_re(
                r"(?i)(?:LoadMap:\s*|MapName[=:\s]+|Arena[=:\s]+)(?:[A-Za-z0-9_/-]+/)?(?P<map>Monaco|Seoul|Skyway_?Stadium|Las_?Vegas|SYS\$?HORIZON|SYS_?HORIZON|Kyoto|Fortune_?Stadium|Bernal)",
            ),
            elim_re: compile_re(
                r"(?i)(?:LogDiscovery|Elimination|PlayerEliminated|EnemyEliminated|CoinScatter|ConfirmKill|KillEvent)",
            ),
            headshot_re: compile_re(
                r"(?i)(?:HeadshotElimination|CriticalKill|Headshot)",
            ),
            wipe_re: compile_re(
                r"(?i)(?:TeamWipe|EnemyTeamWiped|SquadWipe)",
            ),
            cashout_re: compile_re(
                r"(?i)(?:CashoutStolen|CashoutCompleted|VaultOpened|ObjectiveCaptured)",
            ),
            win_re: compile_re(
                r"(?i)(?:TournamentWon|MatchWinner|QualifiedForNextRound|TheFinalsWinner)",
            ),
        }
    }

    pub fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let now = Instant::now();

        if let Some(caps) = self.map_re.captures(line) {
            let map = caps.name("map").map_or("Monaco", |m| m.as_str());
            if !map.to_lowercase().contains("frontend") && !map.to_lowercase().contains("lobby") {
                self.combat.current_map = map.replace('_', " ");
                out.push(EmittedEvent {
                    kind: "presence.update".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({
                        "map": self.combat.current_map,
                        "mode": if self.combat.current_mode.is_empty() { "Cashout" } else { &self.combat.current_mode },
                    }),
                });
            }
        }

        if let Some(caps) = self.state_re.captures(line) {
            let to = caps.name("to").map_or("InProgress", |m| m.as_str());
            if to == "InProgress" || line.to_uppercase().contains("MATCH_START") {
                self.combat.reset_match();
                self.combat.in_match = true;
                if self.combat.current_map.is_empty() {
                    self.combat.current_map = "Monaco".into();
                }
                if self.combat.current_mode.is_empty() {
                    self.combat.current_mode = "Cashout".into();
                }
                out.push(EmittedEvent {
                    kind: "match.start".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({
                        "map": self.combat.current_map,
                        "mode": self.combat.current_mode,
                        "line": line,
                    }),
                });
                return out;
            } else if matches!(to, "WaitingPostMatch" | "LeavingMap" | "Aborted") {
                self.combat.in_match = false;
                out.push(EmittedEvent {
                    kind: "match.end".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({ "line": line }),
                });
                return out;
            }
        }

        if self.win_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.mvp".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("finals_win:{}", self.combat.seq)),
                data: json!({ "won": true, "line": line }),
            });
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("finals_win_clutch:{}", self.combat.seq)),
                data: json!({ "won": true, "count": self.combat.round_kills.max(1) }),
            });
            out.push(EmittedEvent {
                kind: "match.end".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({ "won": true, "line": line }),
            });
            return out;
        }

        if self.cashout_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            let is_steal = line.to_lowercase().contains("stolen");
            out.push(EmittedEvent {
                kind: "objective.secured".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("finals_obj:{}", self.combat.seq)),
                data: json!({ "cashout_stolen": is_steal, "line": line }),
            });
            if is_steal {
                out.push(EmittedEvent {
                    kind: "player.clutch".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("finals_steal:{}", self.combat.seq)),
                    data: json!({ "cashout_stolen": true, "line": line }),
                });
            }
            return out;
        }

        if self.wipe_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("finals_wipe:{}", self.combat.seq)),
                data: json!({
                    "team_wipe": true,
                    "count": self.combat.round_kills.max(3),
                    "line": line,
                }),
            });
            return out;
        }

        if self.headshot_re.is_match(line) {
            self.combat.last_kill_at = None;
            out.extend(self.combat.record_kill(true, "log", now));
            return out;
        }

        if self.elim_re.is_match(line) {
            self.combat.last_kill_at = None;
            out.extend(self.combat.record_kill(false, "log", now));
            return out;
        }

        out
    }
}



pub struct ArcRaidersTracker {
    pub(crate) combat: CombatHighlightTracker,
    raid_start_re: Regex,
    kill_re: Regex,
    boss_arc_re: Regex,
    extract_re: Regex,
}

impl Default for ArcRaidersTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ArcRaidersTracker {
    pub fn new() -> Self {
        Self {
            combat: CombatHighlightTracker::default(),
            raid_start_re: compile_re(
                r"(?i)(?:LogGameState:\s*Match State Changed.*to InProgress|RaidStarted|DeployedToZone)(?:[^\r\n]*?\b(?:zone|map)[=:\s]+(?P<map>[A-Za-z0-9_ -]+))?",
            ),
            kill_re: compile_re(
                r"(?i)(?:RaiderEliminated|RaiderDowned|EnemyRaiderKilled|ArcDestroyed|ConfirmKill|PlayerKill)",
            ),
            boss_arc_re: compile_re(
                r"(?i)(?:BaronDestroyed|QueenDestroyed|BisonDestroyed|HarvesterSecured|VaultExtracted)",
            ),
            extract_re: compile_re(
                r"(?i)(?:ExtractionSuccessful|SafeExtraction|ReturnedToSperanza|RaidExtracted)",
            ),
        }
    }

    pub fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let now = Instant::now();

        if let Some(caps) = self.raid_start_re.captures(line) {
            let map = caps
                .name("map")
                .map(|m| m.as_str().trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("Buried City");
            self.combat.reset_match();
            self.combat.in_match = true;
            self.combat.current_map = map.replace('_', " ");
            self.combat.current_mode = "Extraction Raid".into();
            out.push(EmittedEvent {
                kind: "presence.update".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "map": self.combat.current_map,
                    "mode": "Extraction Raid",
                }),
            });
            out.push(EmittedEvent {
                kind: "match.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "map": self.combat.current_map,
                    "mode": "Extraction Raid",
                    "line": line,
                }),
            });
            return out;
        }

        if self.extract_re.is_match(line) {
            self.combat.in_match = false;
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "objective.secured".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("arc_extract:{}", self.combat.seq)),
                data: json!({
                    "extracted": true,
                    "total_kills": self.combat.total_kills,
                    "line": line,
                }),
            });
            out.push(EmittedEvent {
                kind: "player.clutch".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("arc_extract_clutch:{}", self.combat.seq)),
                data: json!({
                    "extracted": true,
                    "count": self.combat.round_kills.max(1),
                    "line": line,
                }),
            });
            out.push(EmittedEvent {
                kind: "match.end".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "won": true,
                    "extracted": true,
                    "line": line,
                }),
            });
            return out;
        }

        if self.boss_arc_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "objective.secured".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("arc_boss:{}", self.combat.seq)),
                data: json!({ "boss_takedown": true, "line": line }),
            });
            return out;
        }

        if self.kill_re.is_match(line) {
            self.combat.last_kill_at = None;
            out.extend(self.combat.record_kill(false, "log", now));
            return out;
        }

        out
    }
}



pub struct MarvelRivalsTracker {
    pub(crate) combat: CombatHighlightTracker,
    pub(crate) hero: String,
    match_start_re: Regex,
    hero_re: Regex,
    ko_re: Regex,
    headshot_re: Regex,
    multiko_re: Regex,
    mvp_re: Regex,
    match_end_re: Regex,
}

impl Default for MarvelRivalsTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl MarvelRivalsTracker {
    pub fn new() -> Self {
        Self {
            combat: CombatHighlightTracker::default(),
            hero: String::new(),
            match_start_re: compile_re(
                r"(?i)(?:LogGameState:\s*Match State Changed.*to InProgress|MatchStart)(?:[^\r\n]*?\bmap[=:\s]+(?P<map>[A-Za-z0-9_]+(?:\s+[0-9]{4})?))?(?:[^\r\n]*?\bmode[=:\s]+(?P<mode>[A-Za-z0-9_ -]+))?",
            ),
            hero_re: compile_re(
                r"(?i)(?:HeroSelected|CharacterSelected|SelectHero)[=:\s]+(?P<hero>[A-Za-z0-9_ -]+)",
            ),
            ko_re: compile_re(
                r"(?i)(?:PlayerKO|FinalHit|EnemyEliminated|ConfirmKO|LocalPlayerKill)(?:.*?hero[=:\s]+(?P<hero>[A-Za-z0-9_ -]+))?",
            ),
            headshot_re: compile_re(
                r"(?i)(?:HeadshotKO|CriticalKO|HeadshotKill)",
            ),
            multiko_re: compile_re(
                r"(?i)(?P<tier>DoubleKO|TripleKO|QuadraKO|PentaKO|HexaKO|TeamWipe)",
            ),
            mvp_re: compile_re(
                r"(?i)(?:MatchMVP|MatchSVP|AwardedMVP)",
            ),
            match_end_re: compile_re(
                r"(?i)(?:MatchEnd|GameOver|MatchComplete)(?:.*?won[=:\s]+(?P<won>true|false|1|0))?",
            ),
        }
    }

    pub fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let now = Instant::now();

        if let Some(caps) = self.hero_re.captures(line) {
            let h = caps.name("hero").map_or("", |m| m.as_str().trim());
            if !h.is_empty() {
                self.hero = h.to_string();
                out.push(EmittedEvent {
                    kind: "presence.update".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: None,
                    data: json!({
                        "hero": self.hero,
                        "map": self.combat.current_map,
                        "mode": self.combat.current_mode,
                    }),
                });
            }
            return out;
        }

        if let Some(caps) = self.match_start_re.captures(line) {
            let map = caps
                .name("map")
                .map(|m| m.as_str().trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("Tokyo 2099");
            let mode = caps
                .name("mode")
                .map(|m| m.as_str().trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("Competitive");
            self.combat.reset_match();
            self.combat.in_match = true;
            self.combat.current_map = map.to_string();
            self.combat.current_mode = mode.to_string();
            out.push(EmittedEvent {
                kind: "presence.update".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "map": map,
                    "mode": mode,
                    "hero": self.hero,
                }),
            });
            out.push(EmittedEvent {
                kind: "match.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "map": map,
                    "mode": mode,
                    "hero": self.hero,
                    "line": line,
                }),
            });
            return out;
        }

        if self.mvp_re.is_match(line) {
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.mvp".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("mr_mvp:{}", self.combat.seq)),
                data: json!({
                    "hero": self.hero,
                    "total_kills": self.combat.total_kills,
                    "line": line,
                }),
            });
            return out;
        }

        if let Some(caps) = self.multiko_re.captures(line) {
            let tier = caps.name("tier").map_or("", |m| m.as_str()).to_lowercase();
            let count = match tier.as_str() {
                "doubleko" => 2,
                "tripleko" => 3,
                "quadrako" => 4,
                "pentako" => 5,
                "hexako" | "teamwipe" => 6,
                _ => 2,
            };
            self.combat.seq = self.combat.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.multikill".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("mr_mko:{}:{count}", self.combat.seq)),
                data: json!({
                    "count": count,
                    "hero": self.hero,
                    "tier": tier,
                    "line": line,
                }),
            });
            if count >= 5 && !self.combat.ace_emitted {
                self.combat.ace_emitted = true;
                out.push(EmittedEvent {
                    kind: "player.ace".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("mr_ace:{}", self.combat.seq)),
                    data: json!({
                        "count": count,
                        "hero": self.hero,
                        "line": line,
                    }),
                });
            }
            return out;
        }

        if self.headshot_re.is_match(line) {
            self.combat.last_kill_at = None;
            let mut evs = self.combat.record_kill(true, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !self.hero.is_empty() {
                        obj.insert("hero".into(), json!(self.hero));
                    }
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if let Some(caps) = self.ko_re.captures(line) {
            self.combat.last_kill_at = None;
            if let Some(h) = caps.name("hero").map(|m| m.as_str().trim()) {
                if !h.is_empty() {
                    self.hero = h.to_string();
                }
            }
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
                    if !self.hero.is_empty() {
                        obj.insert("hero".into(), json!(self.hero));
                    }
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if let Some(caps) = self.match_end_re.captures(line) {
            self.combat.in_match = false;
            let won = caps.name("won").map(|m| matches!(m.as_str(), "true" | "1"));
            out.push(EmittedEvent {
                kind: "match.end".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "won": won,
                    "hero": self.hero,
                    "total_kills": self.combat.total_kills,
                    "line": line,
                }),
            });
            return out;
        }

        out
    }
}



#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FootballTitle {
    Fc26,
    Fc27,
    EFootball,
}

impl FootballTitle {
    pub fn game_key(self) -> &'static str {
        match self {
            Self::Fc26 => "fc26",
            Self::Fc27 => "fc27",
            Self::EFootball => "efootball",
        }
    }

    pub fn default_stadium(self) -> &'static str {
        match self {
            Self::Fc26 | Self::Fc27 => "Santiago Bernabeu",
            Self::EFootball => "Allianz Arena",
        }
    }

    pub fn default_mode(self) -> &'static str {
        match self {
            Self::Fc26 | Self::Fc27 => "Ultimate Team Champions",
            Self::EFootball => "eFootball League",
        }
    }
}

pub struct FootballTracker {
    pub(crate) title: FootballTitle,
    pub(crate) stadium: String,
    pub(crate) mode: String,
    pub(crate) local_team: String,
    pub(crate) opponent_team: String,
    pub(crate) goals_for: u32,
    pub(crate) goals_against: u32,
    pub(crate) player_goals: u32,
    pub(crate) assists: u32,
    pub(crate) saves: u32,
    pub(crate) in_match: bool,
    pub(crate) in_overtime: bool,
    pub(crate) hat_trick_emitted: bool,
    pub(crate) clutch_emitted: bool,
    pub(crate) seq: u64,
    match_start_re: Regex,
    goal_for_re: Regex,
    goal_against_re: Regex,
    assist_re: Regex,
    save_re: Regex,
    overtime_re: Regex,
    mvp_re: Regex,
    match_end_re: Regex,
}

impl FootballTracker {
    pub fn new(title: FootballTitle) -> Self {
        Self {
            title,
            stadium: String::new(),
            mode: String::new(),
            local_team: String::new(),
            opponent_team: String::new(),
            goals_for: 0,
            goals_against: 0,
            player_goals: 0,
            assists: 0,
            saves: 0,
            in_match: false,
            in_overtime: false,
            hat_trick_emitted: false,
            clutch_emitted: false,
            seq: 0,
            match_start_re: compile_re(
                r"(?i)(?:MatchStart|KickOff|StadiumLoaded|LoadStadium|GameFlow.*InMatch|eFootballMatchStart)(?:[^
]*?(?:stadium|arena|map)[=:\s]+(?P<stadium>[A-Za-z0-9_ -]+))?(?:[^
]*?(?:mode|competition)[=:\s]+(?P<mode>[A-Za-z0-9_ -]+))?(?:[^
]*?(?:team|club)[=:\s]+(?P<team>[A-Za-z0-9_ -]+))?",
            ),
            goal_for_re: compile_re(
                r"(?i)(?:GoalScored|LocalGoal|UserGoal|TeamGoalScored|ScoreEvent_Goal|PenaltyScored|FreeKickGoal)(?:[^
]*?(?:scorer|player)[=:\s]+(?P<scorer>[A-Za-z0-9_ -]+))?(?:[^
]*?minute[=:\s]+(?P<minute>[0-9]{1,3}))?",
            ),
            goal_against_re: compile_re(
                r"(?i)(?:GoalConceded|OpponentGoal|ConcededGoal|AwayGoal_Against|ScoreEvent_GoalAgainst)(?:[^
]*?minute[=:\s]+(?P<minute>[0-9]{1,3}))?",
            ),
            assist_re: compile_re(
                r"(?i)(?:GoalAssist|AssistRecorded|LocalAssist|ScoreEvent_Assist)(?:[^
]*?(?:player|assister)[=:\s]+(?P<player>[A-Za-z0-9_ -]+))?",
            ),
            save_re: compile_re(
                r"(?i)(?:EpicSave|PenaltySave|GoalkeeperSave|SuperSave|ReflexSave|OneOnOneSave)",
            ),
            overtime_re: compile_re(
                r"(?i)(?:ExtraTimeStart|OvertimeStart|PenaltyShootoutStart|GoldenGoalStart)",
            ),
            mvp_re: compile_re(
                r"(?i)(?:ManOfTheMatch|MatchMVP|PlayerOfTheMatch)",
            ),
            match_end_re: compile_re(
                r"(?i)(?:MatchEnd|FullTime|FinalWhistle|MatchComplete)(?:[^
]*?(?:won|result)[=:\s]+(?P<won>true|false|1|0|win|loss))?",
            ),
        }
    }

    fn reset_match(&mut self) {
        self.goals_for = 0;
        self.goals_against = 0;
        self.player_goals = 0;
        self.assists = 0;
        self.saves = 0;
        self.in_overtime = false;
        self.hat_trick_emitted = false;
        self.clutch_emitted = false;
    }

    pub fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return out;
        }
        let game_tag = self.title.game_key();

        if let Some(caps) = self.match_start_re.captures(trimmed) {
            let st = caps
                .name("stadium")
                .map(|m| m.as_str().trim().replace('_', " "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| self.title.default_stadium().to_string());
            let md = caps
                .name("mode")
                .map(|m| m.as_str().trim().replace('_', " "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| self.title.default_mode().to_string());
            if let Some(tm) = caps.name("team").map(|m| m.as_str().trim()).filter(|s| !s.is_empty()) {
                self.local_team = tm.replace('_', " ");
            }
            self.reset_match();
            self.in_match = true;
            self.stadium = st.clone();
            self.mode = md.clone();
            self.seq = self.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "presence.update".into(),
                confidence: Confidence::Confirmed,
                dedup_key: None,
                data: json!({
                    "map": st,
                    "arena": st,
                    "mode": md,
                    "team": self.local_team,
                    "state": "In Match",
                }),
            });
            out.push(EmittedEvent {
                kind: "match.start".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_start:{}", self.seq)),
                data: json!({
                    "map": self.stadium,
                    "arena": self.stadium,
                    "mode": self.mode,
                    "team": self.local_team,
                    "line": trimmed,
                }),
            });
            return out;
        }

        if let Some(caps) = self.goal_for_re.captures(trimmed) {
            if !self.in_match {
                self.in_match = true;
                if self.stadium.is_empty() {
                    self.stadium = self.title.default_stadium().to_string();
                }
                if self.mode.is_empty() {
                    self.mode = self.title.default_mode().to_string();
                }
            }
            self.goals_for = self.goals_for.saturating_add(1);
            self.player_goals = self.player_goals.saturating_add(1);
            self.seq = self.seq.wrapping_add(1);
            let scorer = caps
                .name("scorer")
                .map(|m| m.as_str().trim().replace('_', " "))
                .unwrap_or_default();
            let minute = caps
                .name("minute")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(45);

            out.push(EmittedEvent {
                kind: "sport.goal".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_goal:{}:{}", self.seq, self.goals_for)),
                data: json!({
                    "count": 1,
                    "goals": self.goals_for,
                    "goals_against": self.goals_against,
                    "scorer": scorer,
                    "minute": minute,
                    "map": self.stadium,
                    "mode": self.mode,
                    "by_local": true,
                    "line": trimmed,
                }),
            });

            if self.player_goals >= 3 && !self.hat_trick_emitted {
                self.hat_trick_emitted = true;
                out.push(EmittedEvent {
                    kind: "sport.hat_trick".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("{game_tag}_hattrick:{}", self.seq)),
                    data: json!({
                        "count": self.player_goals,
                        "scorer": scorer,
                        "map": self.stadium,
                        "mode": self.mode,
                    }),
                });
                out.push(EmittedEvent {
                    kind: "player.ace".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("{game_tag}_ace:{}", self.seq)),
                    data: json!({
                        "count": self.player_goals,
                        "hat_trick": true,
                        "scorer": scorer,
                        "map": self.stadium,
                    }),
                });
            }

            if (minute >= 85 || self.in_overtime)
                && self.goals_for >= self.goals_against
                && !self.clutch_emitted
            {
                self.clutch_emitted = true;
                out.push(EmittedEvent {
                    kind: "player.clutch".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("{game_tag}_clutch_goal:{}", self.seq)),
                    data: json!({
                        "minute": minute,
                        "goals_for": self.goals_for,
                        "goals_against": self.goals_against,
                        "map": self.stadium,
                        "mode": self.mode,
                    }),
                });
            }
            return out;
        }

        if let Some(caps) = self.goal_against_re.captures(trimmed) {
            self.goals_against = self.goals_against.saturating_add(1);
            self.seq = self.seq.wrapping_add(1);
            let minute = caps
                .name("minute")
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(45);
            out.push(EmittedEvent {
                kind: "sport.goal_conceded".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_conceded:{}:{}", self.seq, self.goals_against)),
                data: json!({
                    "count": 1,
                    "goals_for": self.goals_for,
                    "goals_against": self.goals_against,
                    "minute": minute,
                    "map": self.stadium,
                }),
            });
            out.push(EmittedEvent {
                kind: "player.death".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_death:{}:{}", self.seq, self.goals_against)),
                data: json!({
                    "count": 1,
                    "total": self.goals_against,
                    "minute": minute,
                    "map": self.stadium,
                }),
            });
            return out;
        }

        if self.assist_re.is_match(trimmed) {
            self.assists = self.assists.saturating_add(1);
            self.seq = self.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.assist".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_assist:{}:{}", self.seq, self.assists)),
                data: json!({
                    "count": 1,
                    "total": self.assists,
                    "map": self.stadium,
                    "mode": self.mode,
                    "line": trimmed,
                }),
            });
            return out;
        }

        if self.save_re.is_match(trimmed) {
            self.saves = self.saves.saturating_add(1);
            self.seq = self.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "sport.epic_save".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_save:{}:{}", self.seq, self.saves)),
                data: json!({
                    "count": 1,
                    "saves": self.saves,
                    "map": self.stadium,
                    "mode": self.mode,
                    "by_local": true,
                    "line": trimmed,
                }),
            });
            return out;
        }

        if self.overtime_re.is_match(trimmed) {
            self.in_overtime = true;
            self.seq = self.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "match.overtime".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_ot:{}", self.seq)),
                data: json!({
                    "map": self.stadium,
                    "goals_for": self.goals_for,
                    "goals_against": self.goals_against,
                }),
            });
            return out;
        }

        if self.mvp_re.is_match(trimmed) {
            self.seq = self.seq.wrapping_add(1);
            out.push(EmittedEvent {
                kind: "player.mvp".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_mvp:{}", self.seq)),
                data: json!({
                    "map": self.stadium,
                    "goals_for": self.goals_for,
                    "assists": self.assists,
                    "saves": self.saves,
                }),
            });
            return out;
        }

        if let Some(caps) = self.match_end_re.captures(trimmed) {
            self.in_match = false;
            self.seq = self.seq.wrapping_add(1);
            let explicit_won = caps.name("won").map(|m| {
                matches!(
                    m.as_str().to_lowercase().as_str(),
                    "true" | "1" | "win"
                )
            });
            let won = explicit_won.or_else(|| {
                if self.goals_for == self.goals_against {
                    None
                } else {
                    Some(self.goals_for > self.goals_against)
                }
            });
            if won == Some(true)
                && (self.in_overtime || (self.goals_against >= 1 && self.goals_for > self.goals_against))
                && !self.clutch_emitted
            {
                self.clutch_emitted = true;
                out.push(EmittedEvent {
                    kind: "player.clutch".into(),
                    confidence: Confidence::Confirmed,
                    dedup_key: Some(format!("{game_tag}_end_clutch:{}", self.seq)),
                    data: json!({
                        "won": true,
                        "goals_for": self.goals_for,
                        "goals_against": self.goals_against,
                        "map": self.stadium,
                    }),
                });
            }
            out.push(EmittedEvent {
                kind: "match.end".into(),
                confidence: Confidence::Confirmed,
                dedup_key: Some(format!("{game_tag}_end:{}", self.seq)),
                data: json!({
                    "map": self.stadium,
                    "mode": self.mode,
                    "won": won,
                    "goals_for": self.goals_for,
                    "goals_against": self.goals_against,
                    "assists": self.assists,
                    "saves": self.saves,
                    "line": trimmed,
                }),
            });
            return out;
        }

        out
    }
}



pub fn clean_weapon_display_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(vw) = ValorantTracker::normalize_valorant_weapon(trimmed) {
        return vw.to_string();
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
    if game.eq_ignore_ascii_case("valorant") {
        if let Some(ag) = ValorantTracker::normalize_valorant_agent(trimmed) {
            return ag.to_string();
        }
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
