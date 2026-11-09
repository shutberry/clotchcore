#![allow(dead_code)]
use std::collections::{HashMap, HashSet};
use std::io::{self, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use async_trait::async_trait;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use regex::Regex;

fn compile_re(pat: &str) -> Regex {
    match Regex::new(pat) {
        Ok(r) => r,
        Err(_) => match Regex::new("a^") {
            Ok(f) => f,
            Err(_) => unreachable!(),
        },
    }
}
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use super::bus::{now_ms, pause, Confidence, Event, EventSink, EventSource, SourceError, MAX_BUFFER, MAX_HTTP_BODY, MAX_LINE_LEN};
use super::sources::{FileIdentity, normalize, open_shared_read, scalar, snake_case};

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Local {
    pub(crate) team: String,
    pub(crate) health: Option<i64>,
    pub(crate) flashed: Option<i64>,
    pub(crate) round_kills: Option<i64>,
    pub(crate) round_hs: Option<i64>,
    pub(crate) kills: Option<i64>,
    pub(crate) deaths: Option<i64>,
    pub(crate) assists: Option<i64>,
    pub(crate) mvps: Option<i64>,
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

fn text(v: &Value, p: &str) -> String {
    v.pointer(p)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn opt_num(v: &Value, p: &str) -> Option<i64> {
    v.pointer(p).and_then(Value::as_i64)
}

fn opt_f64(v: &Value, p: &str) -> Option<f64> {
    v.pointer(p).and_then(Value::as_f64)
}

fn opt_bool(v: &Value, p: &str) -> Option<bool> {
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
                team: text(v, "/player/team"),
                health: opt_num(v, "/player/state/health"),
                flashed: opt_num(v, "/player/state/flashed"),
                round_kills: opt_num(v, "/player/state/round_kills"),
                round_hs: opt_num(v, "/player/state/round_killhs"),
                kills: opt_num(v, "/player/match_stats/kills"),
                deaths: opt_num(v, "/player/match_stats/deaths"),
                assists: opt_num(v, "/player/match_stats/assists"),
                mvps: opt_num(v, "/player/match_stats/mvps"),
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
pub(crate) struct EmittedEvent {
    pub(crate) kind: String,
    pub(crate) confidence: Confidence,
    pub(crate) dedup_key: Option<String>,
    pub(crate) data: Value,
}

#[derive(Default)]
pub(crate) struct Tracker {
    prev: Option<Snap>,
    prev_local: Option<Local>,
    ace_emitted_round: Option<i64>,
    clutch_emitted_round: Option<i64>,
    last_multikill_round: Option<(i64, i64)>,
    match_seq: u64,
}

impl Tracker {
    fn reset_match_state(&mut self) {
        self.ace_emitted_round = None;
        self.clutch_emitted_round = None;
        self.last_multikill_round = None;
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
                }
                let round_num = cur.rounds_played.unwrap_or(-1);

                if let (Some(pk), Some(ck)) = (p.kills, c.kills) {
                    let delta_kills = ck - pk;
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
                                "weapon": active_weapon,
                                "weapon_confidence": if active_weapon.is_some() { "inferred_active_weapon" } else { "unavailable" },
                                "knife": if active_weapon.is_some() { json!(knife) } else { Value::Null },
                                "health": c.health,
                                "flashed": c.flashed.map(|f| f > 0),
                                "rounds_played": cur.rounds_played,
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
                                json!({ "count": crk, "rounds_played": cur.rounds_played }),
                            );
                        }
                    }
                    if crk >= 5 && self.ace_emitted_round != Some(round_num) {
                        self.ace_emitted_round = Some(round_num);
                        push(
                            "player.ace",
                            Confidence::Confirmed,
                            Some(format!("ace:{match_seq}:{map_name}:{round_num}")),
                            json!({ "rounds_played": cur.rounds_played, "round_kills": crk }),
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

struct GsiShared {
    token: String,
    sink: EventSink,
    tracker: Mutex<Tracker>,
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    ha.iter().zip(hb.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn validate_token(token: &str) -> Result<String, SourceError> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(SourceError::Other("GSI token must not be empty".into()));
    }
    if trimmed.len() > 256
        || trimmed
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return Err(SourceError::Other(
            "GSI token contains invalid characters".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

async fn gsi_ingest(
    State(shared): State<Arc<GsiShared>>,
    Json(payload): Json<Value>,
) -> StatusCode {
    if !payload.is_object() {
        return StatusCode::BAD_REQUEST;
    }
    let req_token = text(&payload, "/auth/token");
    if req_token.is_empty() || !constant_time_eq(&req_token, &shared.token) {
        return StatusCode::UNAUTHORIZED;
    }
    let snap = Snap::from_payload(&payload);
    let activity = text(&payload, "/player/activity");
    let pres_data = json!({
        "activity": activity,
        "map": snap.map,
        "mode": snap.mode,
        "phase": snap.map_phase,
        "round_phase": snap.round_phase,
        "rounds_played": snap.rounds_played,
        "ct": snap.ct,
        "t": snap.t,
        "team": snap.local.as_ref().map(|l| l.team.clone()),
        "kills": snap.local.as_ref().and_then(|l| l.kills),
        "deaths": snap.local.as_ref().and_then(|l| l.deaths),
        "assists": snap.local.as_ref().and_then(|l| l.assists),
        "mvps": snap.local.as_ref().and_then(|l| l.mvps),
        "round_kills": snap.local.as_ref().and_then(|l| l.round_kills),
        "health": snap.local.as_ref().and_then(|l| l.health),
    });
    shared.sink.emit_detailed(
        "presence.update",
        None,
        Confidence::Confirmed,
        None,
        pres_data,
    );
    let events = match shared.tracker.lock() {
        Ok(mut tracker) => tracker.ingest(snap),
        Err(poisoned) => {
            let mut tracker = poisoned.into_inner();
            *tracker = Tracker::default();
            shared.sink.emit(
                "source.warning",
                None,
                json!({ "reason": "tracker_mutex_recovered_and_reset" }),
            );
            tracker.ingest(snap)
        }
    };
    for ev in events {
        shared
            .sink
            .emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
    }
    StatusCode::OK
}

const GSI_TEMPLATE: &str = r#""Skirrmish"
{
    "uri"        "http://127.0.0.1:__PORT__/gsi"
    "timeout"    "5.0"
    "buffer"     "0.05"
    "throttle"   "0.05"
    "heartbeat"  "5.0"
    "auth"
    {
        "token"  "__TOKEN__"
    }
    "data"
    {
        "provider"             "1"
        "map"                  "1"
        "map_round_wins"       "1"
        "round"                "1"
        "player_id"            "1"
        "player_state"         "1"
        "player_weapons"       "1"
        "player_match_stats"   "1"
        "player_position"      "1"
        "allplayers_id"        "1"
        "allplayers_state"     "1"
        "allplayers_match_stats" "1"
        "allplayers_weapons"   "1"
        "allplayers_position"  "1"
        "phase_countdowns"     "1"
        "allgrenades"          "1"
        "bomb"                 "1"
    }
}
"#;

pub struct GsiSource {
    addr: SocketAddr,
    token: String,
    source_name: String,
}

impl GsiSource {
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            token: token.into(),
            source_name: "gsi".into(),
        }
    }

    pub fn try_new(port: u16, token: impl Into<String>) -> Result<Self, SourceError> {
        let safe_token = validate_token(&token.into())?;
        Ok(Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            token: safe_token,
            source_name: "gsi".into(),
        })
    }

    pub fn config(&self) -> Result<String, SourceError> {
        let valid_token = validate_token(&self.token)?;
        Ok(GSI_TEMPLATE
            .replace("__PORT__", &self.addr.port().to_string())
            .replace("__TOKEN__", &valid_token))
    }

    pub fn install(&self, cfg_dir: &Path) -> io::Result<PathBuf> {
        if !cfg_dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "CS2 cfg directory does not exist",
            ));
        }
        let cfg = self
            .config()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let path = cfg_dir.join("gamestate_integration_skirrmish.cfg");
        if path.exists() {
            let bak = cfg_dir.join("gamestate_integration_skirrmish.cfg.bak");
            let _ = std::fs::copy(&path, &bak);
        }
        let tmp_path = cfg_dir.join("gamestate_integration_skirrmish.cfg.tmp");
        std::fs::write(&tmp_path, cfg)?;
        std::fs::rename(&tmp_path, &path)?;
        Ok(path)
    }
}

#[async_trait]
impl EventSource for GsiSource {
    fn name(&self) -> &str {
        &self.source_name
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let token = validate_token(&self.token)?;
        let listener = TcpListener::bind(self.addr).await?;
        let shared = Arc::new(GsiShared {
            token,
            sink,
            tracker: Mutex::new(Tracker::default()),
        });
        let app = Router::new()
            .route("/gsi", post(gsi_ingest))
            .layer(DefaultBodyLimit::max(MAX_HTTP_BODY))
            .with_state(shared);
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await?;
        Ok(())
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

struct DotaGsiShared {
    token: String,
    sink: EventSink,
    tracker: Mutex<DotaTracker>,
}

async fn dota_gsi_ingest(
    State(shared): State<Arc<DotaGsiShared>>,
    Json(payload): Json<Value>,
) -> StatusCode {
    if !payload.is_object() {
        return StatusCode::BAD_REQUEST;
    }
    let req_token = text(&payload, "/auth/token");
    if req_token.is_empty() || !constant_time_eq(&req_token, &shared.token) {
        return StatusCode::UNAUTHORIZED;
    }
    let clock_time = payload.pointer("/map/clock_time").and_then(Value::as_f64);
    shared.sink.emit_detailed(
        "presence.update",
        clock_time,
        Confidence::Confirmed,
        None,
        json!({
            "map": text(&payload, "/map/name"),
            "state": text(&payload, "/map/game_state"),
            "hero": text(&payload, "/hero/name"),
            "level": opt_num(&payload, "/hero/level"),
            "kills": opt_num(&payload, "/player/kills"),
            "deaths": opt_num(&payload, "/player/deaths"),
            "assists": opt_num(&payload, "/player/assists"),
            "team": text(&payload, "/player/team_name"),
            "radiant_score": opt_num(&payload, "/map/radiant_score"),
            "dire_score": opt_num(&payload, "/map/dire_score"),
        }),
    );
    let events = match shared.tracker.lock() {
        Ok(mut tracker) => tracker.ingest(&payload),
        Err(poisoned) => {
            let mut tracker = poisoned.into_inner();
            *tracker = DotaTracker::default();
            tracker.ingest(&payload)
        }
    };
    for ev in events {
        shared
            .sink
            .emit_detailed(ev.kind, clock_time, ev.confidence, ev.dedup_key, ev.data);
    }
    StatusCode::OK
}

pub struct Dota2GsiSource {
    addr: SocketAddr,
    token: String,
}

impl Dota2GsiSource {
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            token: token.into(),
        }
    }

    pub fn try_new(port: u16, token: impl Into<String>) -> Result<Self, SourceError> {
        let safe_token = validate_token(&token.into())?;
        Ok(Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            token: safe_token,
        })
    }
}

#[async_trait]
impl EventSource for Dota2GsiSource {
    fn name(&self) -> &str {
        "dota2_gsi"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let token = validate_token(&self.token)?;
        let listener = TcpListener::bind(self.addr).await?;
        let shared = Arc::new(DotaGsiShared {
            token,
            sink,
            tracker: Mutex::new(DotaTracker::default()),
        });
        let app = Router::new()
            .route("/gsi", post(dota_gsi_ingest))
            .layer(DefaultBodyLimit::max(MAX_HTTP_BODY))
            .with_state(shared);
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await?;
        Ok(())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EventList {
    #[serde(rename = "Events")]
    events: Vec<Map<String, Value>>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
struct Scores {
    kills: i64,
    deaths: i64,
    assists: i64,
    #[serde(rename = "creepScore")]
    cs: i64,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
pub(crate) struct Participant {
    #[serde(rename = "championName")]
    champion: String,
    #[serde(rename = "riotId")]
    riot_id: String,
    #[serde(rename = "riotIdGameName")]
    game_name: String,
    #[serde(rename = "summonerName")]
    summoner: String,
    team: String,
    level: i64,
    scores: Scores,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayerMatch {
    Exact,
    Inferred,
}

fn match_player(a: &str, b: &str) -> Option<PlayerMatch> {
    let norm = |s: &str| s.trim().to_lowercase();
    let na = norm(a);
    let nb = norm(b);
    if na.is_empty() || nb.is_empty() {
        return None;
    }
    if na.contains('#') && nb.contains('#') {
        if na == nb {
            Some(PlayerMatch::Exact)
        } else {
            None
        }
    } else {
        let base_a = na.split('#').next().unwrap_or(&na).trim();
        let base_b = nb.split('#').next().unwrap_or(&nb).trim();
        if !base_a.is_empty() && base_a == base_b {
            Some(PlayerMatch::Inferred)
        } else {
            None
        }
    }
}

fn same_player(a: &str, b: &str) -> bool {
    match_player(a, b).is_some()
}

impl Participant {
    fn is(&self, name: &str) -> bool {
        [&self.riot_id, &self.game_name, &self.summoner]
            .iter()
            .any(|n| same_player(n, name))
    }
}

fn resolve_riot_player_match(
    me: Option<&str>,
    candidate: Option<&str>,
    roster: &[Participant],
) -> Option<PlayerMatch> {
    let (m, c) = (me?, candidate?);
    if let Some(direct) = match_player(m, c) {
        return Some(direct);
    }
    if let Some(me_part) = roster.iter().find(|p| p.is(m)) {
        if me_part.is(c) {
            return Some(PlayerMatch::Exact);
        }
    }
    None
}

pub struct LeagueLiveClient {
    client: reqwest::Client,
    port: u16,
    poll: Duration,
}

impl LeagueLiveClient {
    pub fn new() -> Result<Self, SourceError> {
        Self::with_port(2999)
    }

    pub fn with_port(port: u16) -> Result<Self, SourceError> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(1500))
            .build()?;
        Ok(Self {
            client,
            port,
            poll: Duration::from_millis(350),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        let clean_path = path.trim_start_matches('/');
        format!(
            "https://{}:{}/liveclientdata/{}",
            Ipv4Addr::LOCALHOST,
            self.port,
            clean_path
        )
    }

    async fn fetch<T: serde::de::DeserializeOwned>(&self, path: &str) -> Option<T> {
        let url = self.endpoint(path);
        self.client
            .get(url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()
    }

    pub(crate) fn emit_event(
        sink: &EventSink,
        raw: &Map<String, Value>,
        me: Option<&str>,
        roster: &[Participant],
        replayed: bool,
    ) {
        let name = raw
            .get("EventName")
            .and_then(Value::as_str)
            .unwrap_or("Unknown");
        let event_id = raw.get("EventID").and_then(Value::as_i64);
        let time = raw.get("EventTime").and_then(Value::as_f64);
        let mut data = Map::new();
        for (k, v) in raw {
            if k != "EventName" && k != "EventTime" {
                data.insert(snake_case(k), normalize(v.clone()));
            }
        }
        let field = |k: &str| raw.get(k).and_then(Value::as_str);
        let actor = field("KillerName")
            .or_else(|| field("Acer"))
            .or_else(|| field("Recipient"));
        let victim = field("VictimName");
        let is_me_match = resolve_riot_player_match(me, actor, roster);
        let is_victim_match = resolve_riot_player_match(me, victim, roster);
        let is_me = is_me_match.is_some();
        let victim_is_local = is_victim_match.is_some();
        let mut has_inferred = is_me_match == Some(PlayerMatch::Inferred)
            || is_victim_match == Some(PlayerMatch::Inferred);
        let assisted = raw
            .get("Assisters")
            .and_then(Value::as_array)
            .map_or(false, |a| {
                a.iter().filter_map(Value::as_str).any(|n| {
                    let pm = resolve_riot_player_match(me, Some(n), roster);
                    if pm == Some(PlayerMatch::Inferred) {
                        has_inferred = true;
                    }
                    pm.is_some()
                })
            });
        let team_of = |n: Option<&str>| {
            n.and_then(|n| roster.iter().find(|p| p.is(n)))
                .map(|p| p.team.clone())
        };
        let my_team = me.and_then(|m| team_of(Some(m)));
        let actor_team = team_of(actor).or_else(|| field("AcingTeam").map(str::to_owned));
        let ours = match (&actor_team, &my_team) {
            (Some(a), Some(m)) => Some(a == m),
            _ => None,
        };
        data.insert("by_local".into(), json!(is_me));
        data.insert("victim_is_local".into(), json!(victim_is_local));
        data.insert("assisted_by_local".into(), json!(assisted));
        if let Some(o) = ours {
            data.insert("ours".into(), json!(o));
        }
        data.insert("replayed".into(), json!(replayed));
        let confidence = if has_inferred {
            data.insert("confidence_penalty".into(), json!(true));
            Confidence::Inferred
        } else {
            Confidence::Confirmed
        };
        let payload_obj = Value::Object(data);
        let raw_kind = snake_case(name);
        let dedup_key = event_id.map(|id| format!("lol_ev:{id}:{:?}", time));
        sink.emit_detailed(
            raw_kind.clone(),
            time,
            confidence,
            dedup_key,
            payload_obj.clone(),
        );

        if replayed {
            return;
        }

        let hl_dedup = event_id.map(|id| format!("lol_hl:{id}:{:?}", time));
        match raw_kind.as_str() {
            "game_start" => {
                sink.emit_detailed("match.start", time, confidence, hl_dedup, payload_obj);
            }
            "game_end" => {
                sink.emit_detailed("match.end", time, confidence, hl_dedup, payload_obj);
            }
            "champion_kill" if is_me => {
                let mut kd = payload_obj;
                if let Some(obj) = kd.as_object_mut() {
                    obj.insert("count".into(), json!(1));
                }
                sink.emit_detailed("player.kill", time, confidence, hl_dedup, kd);
            }
            "champion_kill" if victim_is_local => {
                let mut dd = payload_obj.clone();
                if let Some(obj) = dd.as_object_mut() {
                    obj.insert("count".into(), json!(1));
                }
                let death_dedup = event_id.map(|id| format!("lol_death:{id}:{:?}", time));
                sink.emit_detailed("player.death", time, confidence, death_dedup, dd);
                if assisted {
                    let mut ad = payload_obj;
                    if let Some(obj) = ad.as_object_mut() {
                        obj.insert("count".into(), json!(1));
                    }
                    let ast_dedup = event_id.map(|id| format!("lol_assist:{id}:{:?}", time));
                    sink.emit_detailed("player.assist", time, confidence, ast_dedup, ad);
                }
            }
            "champion_kill" if assisted => {
                let mut ad = payload_obj;
                if let Some(obj) = ad.as_object_mut() {
                    obj.insert("count".into(), json!(1));
                }
                let ast_dedup = event_id.map(|id| format!("lol_assist:{id}:{:?}", time));
                sink.emit_detailed("player.assist", time, confidence, ast_dedup, ad);
            }
            "multikill" if is_me => {
                let streak = raw
                    .get("KillStreak")
                    .and_then(Value::as_i64)
                    .unwrap_or(2);
                let mut mk = payload_obj.clone();
                if let Some(obj) = mk.as_object_mut() {
                    obj.insert("count".into(), json!(streak));
                }
                sink.emit_detailed("player.multikill", time, confidence, hl_dedup.clone(), mk);
                if streak >= 5 {
                    let ace_dedup = event_id.map(|id| format!("lol_penta:{id}"));
                    sink.emit_detailed("player.ace", time, confidence, ace_dedup, payload_obj);
                }
            }
            "ace" if is_me || ours == Some(true) => {
                sink.emit_detailed("player.ace", time, confidence, hl_dedup.clone(), payload_obj.clone());
                if is_me {
                    let cl_dedup = event_id.map(|id| format!("lol_clutch:{id}"));
                    sink.emit_detailed("player.clutch", time, confidence, cl_dedup, payload_obj);
                }
            }
            "first_blood" if is_me => {
                sink.emit_detailed("objective.first_blood", time, confidence, hl_dedup, payload_obj);
            }
            "dragon_kill" | "baron_kill" | "herald_kill" | "horde_kill" | "atakhan_kill"
                if is_me || assisted || ours == Some(true) =>
            {
                sink.emit_detailed("objective.secured", time, confidence, hl_dedup, payload_obj);
            }
            _ => {}
        }
    }
}

#[async_trait]
impl EventSource for LeagueLiveClient {
    fn name(&self) -> &str {
        "live_client"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let mut last_id: Option<i64> = None;
        let mut me: Option<String> = None;
        let mut roster: Vec<Participant> = Vec::new();
        let mut last_scores: Option<(i64, i64, i64, i64, i64)> = None;
        let mut in_session = false;
        let mut tick: u64 = 0;
        let mut ticker = interval(self.poll);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = ticker.tick() => {}
            }
            tick += 1;
            let Some(list) = self.fetch::<EventList>("eventdata").await else {
                if in_session {
                    in_session = false;
                    last_id = None;
                    me = None;
                    roster.clear();
                    last_scores = None;
                    sink.emit("match.leave", None, Value::Null);
                }
                continue;
            };
            if !in_session {
                in_session = true;
            }
            let id_of = |e: &Map<String, Value>| e.get("EventID").and_then(Value::as_i64);
            let max_id = list.events.iter().filter_map(id_of).max();
            if let (Some(l), Some(m)) = (last_id, max_id) {
                if m < l {
                    last_id = None;
                    last_scores = None;
                    me = None;
                    roster.clear();
                }
            }
            let first_batch = last_id.is_none();
            let latest_time = list
                .events
                .iter()
                .filter_map(|e| e.get("EventTime").and_then(Value::as_f64))
                .fold(0.0, f64::max);
            let replayed = first_batch && latest_time > 15.0;
            let fresh: Vec<&Map<String, Value>> = list
                .events
                .iter()
                .filter(|e| match (last_id, id_of(e)) {
                    (Some(l), Some(i)) => i > l,
                    _ => true,
                })
                .collect();
            if me.is_none() || tick % 10 == 1 {
                if let Some(active_name) = self.fetch::<String>("activeplayername").await {
                    if !active_name.is_empty() {
                        me = Some(active_name);
                    }
                }
            }
            if !fresh.is_empty() || roster.is_empty() || tick % 5 == 1 {
                if let Some(r) = self.fetch::<Vec<Participant>>("playerlist").await {
                    roster = r;
                }
            }
            let mut emitted_kill_in_batch = false;
            for e in fresh {
                if let Some(i) = id_of(e) {
                    last_id = Some(last_id.map_or(i, |l| l.max(i)));
                }
                if e.get("EventName").and_then(Value::as_str) == Some("ChampionKill") {
                    let killer = e.get("KillerName").and_then(Value::as_str);
                    if resolve_riot_player_match(me.as_deref(), killer, &roster).is_some() {
                        emitted_kill_in_batch = true;
                    }
                }
                Self::emit_event(&sink, e, me.as_deref(), &roster, replayed);
            }
            if let Some(p) = me
                .as_deref()
                .and_then(|m| roster.iter().find(|p| p.is(m)))
            {
                let s = (
                    p.scores.kills,
                    p.scores.deaths,
                    p.scores.assists,
                    p.scores.cs,
                    p.level,
                );
                if let Some(prev_s) = last_scores {
                    if s.0 > prev_s.0 && !emitted_kill_in_batch {
                        let delta = s.0 - prev_s.0;
                        sink.emit_detailed(
                            "player.kill",
                            None,
                            Confidence::Confirmed,
                            Some(format!("lol_score_kill:{}", s.0)),
                            json!({
                                "champion": p.champion,
                                "team": p.team,
                                "count": delta,
                                "total": s.0,
                            }),
                        );
                    }
                }
                if last_scores != Some(s) {
                    last_scores = Some(s);
                    sink.emit(
                        "player_stats",
                        None,
                        json!({
                            "champion": p.champion,
                            "team": p.team,
                            "kills": s.0,
                            "deaths": s.1,
                            "assists": s.2,
                            "cs": s.3,
                            "level": s.4,
                        }),
                    );
                }
            }
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
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
    fn reset(&mut self) {
        self.arena.clear();
        self.overtime = false;
        self.has_winner = false;
        self.local_name.clear();
        self.local_id.clear();
        self.local_team = None;
        self.players.clear();
    }

    fn is_target_local(&self, target: &Value) -> bool {
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

    fn ingest_update_state(&mut self, data: &Value) -> Vec<EmittedEvent> {
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

pub struct RocketLeagueStats {
    addr: SocketAddr,
    state_interval: Duration,
}

impl RocketLeagueStats {
    pub fn new(port: u16) -> Self {
        Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            state_interval: Duration::from_millis(250),
        }
    }

    pub fn state_interval(mut self, every: Duration) -> Self {
        self.state_interval = every;
        self
    }

    pub(crate) fn handle(
        &self,
        sink: &EventSink,
        msg: Value,
        last_state: &mut Option<Instant>,
        tracker: &mut RlTracker,
    ) {
        let Some(obj) = msg.as_object() else { return };
        let Some(name) = obj.get("Event").and_then(Value::as_str) else {
            return;
        };

        let raw_data = obj.get("Data").cloned().unwrap_or(Value::Null);
        let data = match raw_data {
            Value::String(s) => {
                if s.trim().is_empty() {
                    Value::Object(Map::new())
                } else {
                    match serde_json::from_str::<Value>(&s) {
                        Ok(parsed) => parsed,
                        Err(_) => {
                            sink.emit(
                                "source.warning",
                                None,
                                json!({ "reason": "unparseable_event_data_string", "event": name }),
                            );
                            return;
                        }
                    }
                }
            }
            other => other,
        };

        let kind = snake_case(name);
        let game_time = data
            .pointer("/Game/TimeSeconds")
            .or_else(|| data.pointer("/TimeSeconds"))
            .or_else(|| data.pointer("/MatchTime"))
            .and_then(Value::as_f64);

        match kind.as_str() {
            "update_state" => {
                for derived in tracker.ingest_update_state(&data) {
                    sink.emit_detailed(
                        derived.kind,
                        game_time,
                        derived.confidence,
                        derived.dedup_key,
                        derived.data,
                    );
                }
                let now = Instant::now();
                if last_state.map_or(false, |t| now.duration_since(t) < self.state_interval) {
                    return;
                }
                *last_state = Some(now);
                sink.emit(kind, game_time, data);
            }
            "statfeed_event" | "stat_event" => {
                let stat_type = text(&data, "/Type");
                let norm_type = snake_case(&stat_type);
                let main_target = data
                    .get("MainTarget")
                    .cloned()
                    .unwrap_or_else(|| json!(text(&data, "/Player")));
                let secondary_target = data.get("SecondaryTarget").cloned().unwrap_or(Value::Null);
                let target_is_local = enriched_target_is_local(tracker, &data);
                let mut payload = match data {
                    Value::Object(m) => m,
                    other => {
                        let mut m = Map::new();
                        m.insert("raw".into(), other);
                        m
                    }
                };
                if !stat_type.is_empty() {
                    payload.insert("stat_type_normalized".into(), json!(norm_type));
                }
                payload.insert("main_target".into(), main_target);
                payload.insert("secondary_target".into(), secondary_target);
                let enriched = Value::Object(payload);
                sink.emit(&kind, game_time, enriched.clone());
                if !stat_type.is_empty() && target_is_local {
                    sink.emit(
                        format!("statfeed.{norm_type}"),
                        game_time,
                        enriched.clone(),
                    );
                    match norm_type.as_str() {
                        "goal" | "aerial_goal" | "backwards_goal" | "bicycle_goal"
                        | "long_goal" | "overtime_goal" | "hoops_swish_goal" | "pool_shot"
                        | "turtle_goal" | "double_tap" | "hat_trick" => {
                            sink.emit("sport.goal", game_time, enriched);
                        }
                        "save" | "epic_save" | "savior" => {
                            sink.emit("sport.epic_save", game_time, enriched);
                        }
                        "demolish" | "extermination" => {
                            sink.emit("player.kill", game_time, enriched);
                        }
                        "mvp" => {
                            sink.emit("player.mvp", game_time, enriched);
                        }
                        _ => {}
                    }
                }
            }
            "goal_scored" => {
                sink.emit(&kind, game_time, data.clone());
                if enriched_target_is_local(tracker, &data) {
                    sink.emit("sport.goal", game_time, data);
                }
            }
            "ball_hit" | "boost_pickup" | "countdown_begin" | "round_started" | "podium_start" => {
                sink.emit(kind, game_time, data);
            }
            "match_ended" | "match_destroyed" => {
                tracker.reset();
                sink.emit(&kind, game_time, data.clone());
                if kind == "match_ended" {
                    sink.emit("match.end", game_time, data);
                }
            }
            _ => {
                sink.emit(kind, game_time, data);
            }
        }
    }
}

fn enriched_target_is_local(tracker: &RlTracker, data: &Value) -> bool {
    if let Some(mt) = data.get("MainTarget").or_else(|| data.get("Scorer")) {
        return tracker.is_target_local(mt);
    }
    if let Some(p) = data.get("Player") {
        return tracker.is_target_local(p);
    }
    true
}

pub(crate) fn drain_json_stream(buf: &mut Vec<u8>) -> (Vec<Value>, usize) {
    let mut values = Vec::new();
    let mut dropped_corrupt = 0usize;

    loop {
        let first_non_ws = buf.iter().position(|b| !b.is_ascii_whitespace() && *b != 0);
        match first_non_ws {
            Some(idx) if idx > 0 => {
                buf.drain(..idx);
            }
            None => {
                buf.clear();
                break;
            }
            _ => {}
        }
        if buf.is_empty() {
            break;
        }

        if buf[0] != b'{' && buf[0] != b'[' {
            dropped_corrupt += 1;
            if let Some(next_start) = buf[1..].iter().position(|&b| b == b'{' || b == b'[') {
                buf.drain(..=next_start);
                continue;
            } else {
                buf.clear();
                break;
            }
        }

        let mut stream = serde_json::Deserializer::from_slice(buf).into_iter::<Value>();
        match stream.next() {
            Some(Ok(v)) => {
                let consumed = stream.byte_offset();
                values.push(v);
                buf.drain(..consumed);
            }
            Some(Err(e)) if e.is_eof() => {
                if buf.len() > MAX_BUFFER {
                    dropped_corrupt += 1;
                    if let Some(next_start) = buf[1..].iter().position(|&b| b == b'{') {
                        buf.drain(..=next_start);
                    } else {
                        buf.clear();
                    }
                }
                break;
            }
            Some(Err(_)) => {
                dropped_corrupt += 1;
                if let Some(next_start) = buf[1..].iter().position(|&b| b == b'{') {
                    buf.drain(..=next_start);
                } else {
                    buf.clear();
                    break;
                }
            }
            None => break,
        }
    }

    (values, dropped_corrupt)
}

#[async_trait]
impl EventSource for RocketLeagueStats {
    fn name(&self) -> &str {
        "stats_api"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::RocketLeague,
                &["rocketleague.exe"],
            )
            .await;
        });

        let mut last_state: Option<Instant> = None;
        let mut tracker = RlTracker::default();
        loop {
            let connected = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                },
                r = TcpStream::connect(self.addr) => r,
            };
            let Ok(mut stream) = connected else {
                if !pause(&cancel, Duration::from_secs(1)).await {
                    let _ = hud_task.await;
                    return Ok(());
                }
                continue;
            };
            tracker.reset();
            sink.emit("source.connected", None, Value::Null);
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 16 * 1024];
            loop {
                let n = tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = hud_task.await;
                        return Ok(());
                    },
                    r = stream.read(&mut chunk) => r,
                };
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                let (messages, corrupt_count) = drain_json_stream(&mut buf);
                for msg in messages {
                    self.handle(&sink, msg, &mut last_state, &mut tracker);
                }
                if corrupt_count > 0 {
                    sink.emit(
                        "source.warning",
                        None,
                        json!({ "reason": "corrupt_json_frame_skipped", "count": corrupt_count }),
                    );
                }
            }
            sink.emit("source.disconnected", None, Value::Null);
            if !pause(&cancel, Duration::from_secs(1)).await {
                let _ = hud_task.await;
                return Ok(());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HudGameProfile {
    Valorant,
    Overwatch2,
    CallOfDuty,
    Battlefield,
    RocketLeague,
    ApexLegends,
    Fortnite,
    RainbowSixSiege,
    TheFinals,
    ArcRaiders,
    MarvelRivals,
    EaSportsFc,
    EFootball,
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
            if now.duration_since(last) > Duration::from_secs(14) {
                self.streak_kills = 0;
            }
        }
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
    flow_re: Regex,
    map_re: Regex,
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
            flow_re: compile_re(
                r"LogGameFlowStateManager:.*Transitioning from (?P<from>[A-Za-z0-9_]+) to (?P<to>[A-Za-z0-9_]+)",
            ),
            map_re: compile_re(r"LogMapLoadModel:.*Map Name:\s*(?P<map>[A-Za-z0-9_/-]+)"),
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

    pub(crate) fn ingest_line(&mut self, line: &str) -> Vec<EmittedEvent> {
        let mut out = Vec::new();
        let now = Instant::now();

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
                    obj.insert("line".into(), json!(line));
                }
            }
            out.extend(evs);
            return out;
        }

        if self.kill_re.is_match(line) {
            let mut evs = self.combat.record_kill(false, "log", now);
            for ev in &mut evs {
                if let Some(obj) = ev.data.as_object_mut() {
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

pub struct ValorantSource {
    name: String,
    log_path: PathBuf,
    from_start: bool,
    poll: Duration,
}

impl ValorantSource {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            name: "valorant_shooter_log".into(),
            log_path: log_path.into(),
            from_start: false,
            poll: Duration::from_millis(100),
        }
    }

    pub fn from_start(mut self, yes: bool) -> Self {
        self.from_start = yes;
        self
    }

    pub fn poll_every(mut self, every: Duration) -> Self {
        self.poll = every.max(Duration::from_millis(50));
        self
    }

    pub fn path(&self) -> &Path {
        &self.log_path
    }
}

async fn fetch_riot_local_puuid(
    client: &reqwest::Client,
    port: u16,
    password: &str,
) -> Option<String> {
    let session_url = format!("https://127.0.0.1:{port}/chat/v1/session");
    if let Ok(resp) = client
        .get(&session_url)
        .basic_auth("riot", Some(password))
        .send()
        .await
    {
        if let Ok(body) = resp.json::<Value>().await {
            if let Some(puuid) = body
                .get("puuid")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(puuid.to_string());
            }
        }
    }

    let ent_url = format!("https://127.0.0.1:{port}/entitlements/v1/token");
    if let Ok(resp) = client
        .get(&ent_url)
        .basic_auth("riot", Some(password))
        .send()
        .await
    {
        if let Ok(body) = resp.json::<Value>().await {
            if let Some(sub) = body
                .get("subject")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(sub.to_string());
            }
        }
    }

    let rso_url = format!("https://127.0.0.1:{port}/rso-auth/v1/authorization/userinfo");
    if let Ok(resp) = client
        .get(&rso_url)
        .basic_auth("riot", Some(password))
        .send()
        .await
    {
        if let Ok(body) = resp.json::<Value>().await {
            if let Some(sub) = body
                .get("sub")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(sub.to_string());
            }
        }
    }

    None
}

async fn poll_valorant_riot_presence(
    sink: EventSink,
    tracker: Arc<Mutex<ValorantTracker>>,
    cancel: CancellationToken,
) {
    let Ok(client) = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .timeout(Duration::from_millis(1200))
        .build()
    else {
        return;
    };

    let mut ticker = interval(Duration::from_millis(2500));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut prev_ally_score: Option<i64> = None;
    let mut prev_enemy_score: Option<i64> = None;
    let mut prev_loop_state = String::new();
    let mut cached_port: u16 = 0;
    let mut cached_password = String::new();
    let mut local_puuid: Option<String> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }

        if cached_port == 0 || cached_password.is_empty() {
            let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
                continue;
            };
            let lockfile_path = PathBuf::from(local_app_data).join("Riot Games/Riot Client/Config/lockfile");
            let Ok(mut file) = open_shared_read(&lockfile_path).await else {
                continue;
            };
            let mut content = String::new();
            if file.read_to_string(&mut content).await.is_err() {
                continue;
            }
            let parts: Vec<&str> = content.trim().split(':').collect();
            if parts.len() < 4 {
                continue;
            }
            let Ok(port) = parts[2].parse::<u16>() else {
                continue;
            };
            cached_port = port;
            cached_password = parts[3].to_string();
            local_puuid = None;
        }

        if local_puuid.is_none() {
            local_puuid = fetch_riot_local_puuid(&client, cached_port, &cached_password).await;
        }

        let url = format!("https://127.0.0.1:{cached_port}/chat/v4/presences");
        let Ok(resp) = client
            .get(&url)
            .basic_auth("riot", Some(cached_password.as_str()))
            .send()
            .await
        else {
            cached_port = 0;
            cached_password.clear();
            local_puuid = None;
            continue;
        };
        let Ok(body) = resp.json::<Value>().await else {
            continue;
        };
        let Some(presences) = body.get("presences").and_then(Value::as_array) else {
            continue;
        };

        let _valorant_count = presences
            .iter()
            .filter(|p| p.get("product").and_then(Value::as_str) == Some("valorant"))
            .count();

        for p in presences {
            if p.get("product").and_then(Value::as_str) != Some("valorant") {
                continue;
            }
            if let Some(ref my_puuid) = local_puuid {
                if p.get("puuid").and_then(Value::as_str) != Some(my_puuid.as_str()) {
                    continue;
                }
            }
            let Some(priv_b64) = p.get("private").and_then(Value::as_str) else {
                continue;
            };
            let Some(decoded_json) = decode_base64_json(priv_b64) else {
                continue;
            };
            let loop_state = text(&decoded_json, "/sessionLoopState");
            let party_state = text(&decoded_json, "/partyState");
            let queue_id = text(&decoded_json, "/queueId");
            let prov_flow = text(&decoded_json, "/provisioningFlow");
            let raw_map = text(&decoded_json, "/matchMap");
            let party_id = text(&decoded_json, "/partyId");
            let party_size = opt_num(&decoded_json, "/partySize").unwrap_or(1);
            let max_party_size = opt_num(&decoded_json, "/maxPartySize").unwrap_or(5);
            let in_menus = loop_state.eq_ignore_ascii_case("MENUS");
            let match_map = if in_menus { String::new() } else { raw_map };
            let ally_score = if in_menus {
                None
            } else {
                opt_num(&decoded_json, "/partyOwnerMatchScoreAllyTeam")
            };
            let enemy_score = if in_menus {
                None
            } else {
                opt_num(&decoded_json, "/partyOwnerMatchScoreEnemyTeam")
            };

            let effective_mode = if prov_flow.eq_ignore_ascii_case("ShootingRange")
                || party_state.eq_ignore_ascii_case("SOLO_PRACTICE")
                || match_map.to_lowercase().contains("poveglia")
                || match_map.to_lowercase().contains("range")
            {
                "ShootingRange".to_string()
            } else if prov_flow.eq_ignore_ascii_case("CustomGame")
                || party_state.starts_with("CUSTOM_GAME")
                || queue_id.eq_ignore_ascii_case("custom")
            {
                "custom".to_string()
            } else {
                queue_id.clone()
            };

            sink.emit_detailed(
                "presence.update",
                None,
                Confidence::Confirmed,
                None,
                json!({
                    "session_loop_state": loop_state,
                    "party_state": party_state,
                    "mode": effective_mode,
                    "queue_id": queue_id,
                    "provisioning_flow": prov_flow,
                    "map": match_map,
                    "party_id": party_id,
                    "party_size": party_size,
                    "party_max": max_party_size,
                    "ally_score": ally_score,
                    "enemy_score": enemy_score,
                }),
            );

            if !loop_state.is_empty() && loop_state != prev_loop_state {
                if loop_state == "INGAME" {
                    let mut should_emit_start = false;
                    if let Ok(mut t) = tracker.lock() {
                        if !t.combat.in_match {
                            t.combat.in_match = true;
                            t.combat.reset_match();
                            t.combat.current_mode = if !queue_id.is_empty() {
                                queue_id.clone()
                            } else {
                                prov_flow.clone()
                            };
                            should_emit_start = true;
                        }
                    }
                    if should_emit_start {
                        sink.emit_inferred(
                            "match.start",
                            None,
                            json!({
                                "queue_id": queue_id,
                                "provisioning_flow": prov_flow,
                                "via": "riot_presence",
                            }),
                        );
                    }
                } else if prev_loop_state == "INGAME" {
                    if let Ok(mut t) = tracker.lock() {
                        t.combat.in_match = false;
                        t.combat.reset_round();
                    }
                    prev_ally_score = None;
                    prev_enemy_score = None;
                    sink.emit_inferred(
                        "match.end",
                        None,
                        json!({
                            "session_loop_state": loop_state,
                            "via": "riot_presence",
                        }),
                    );
                }
                prev_loop_state = loop_state;
            }

            if let (Some(pa), Some(ca)) = (prev_ally_score, ally_score) {
                if ca > pa {
                    let mut emitted = Vec::new();
                    if let Ok(mut t) = tracker.lock() {
                        if t.combat.round_kills >= 1 && !t.combat.clutch_emitted {
                            t.combat.clutch_emitted = true;
                            t.combat.seq = t.combat.seq.wrapping_add(1);
                            emitted.push(EmittedEvent {
                                kind: "player.clutch".into(),
                                confidence: Confidence::Inferred,
                                dedup_key: Some(format!("val_pres_clutch:{}:{ca}", t.combat.seq)),
                                data: json!({
                                    "count": t.combat.round_kills,
                                    "round_kills": t.combat.round_kills,
                                    "ally_score": ca,
                                    "enemy_score": enemy_score,
                                    "via": "riot_presence",
                                }),
                            });
                        }
                        t.combat.reset_round();
                    }
                    for ev in emitted {
                        sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                    }
                }
            }
            if let (Some(pe), Some(ce)) = (prev_enemy_score, enemy_score) {
                if ce > pe {
                    if let Ok(mut t) = tracker.lock() {
                        t.combat.reset_round();
                    }
                }
            }
            prev_ally_score = ally_score;
            prev_enemy_score = enemy_score;
            break;
        }
    }
}

fn decode_base64_json(input: &str) -> Option<Value> {
    let clean: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if clean.is_empty() {
        return None;
    }
    let val_of = |b: u8| -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity((clean.len() * 3) / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for b in clean {
        if b == b'=' {
            break;
        }
        let v = val_of(b)?;
        buf = (buf << 6) | (v as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    serde_json::from_slice(&out).ok()
}

#[async_trait]
impl EventSource for ValorantSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let tracker = Arc::new(Mutex::new(ValorantTracker::new()));

        let pres_sink = sink.clone();
        let pres_tracker = tracker.clone();
        let pres_cancel = cancel.child_token();
        let pres_task = tokio::spawn(async move {
            poll_valorant_riot_presence(pres_sink, pres_tracker, pres_cancel).await;
        });

        let hud_sink = sink.clone();
        let hud_tracker = tracker.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_valorant_hud_loop(hud_sink, hud_tracker, hud_cancel).await;
        });

        let mut pos: Option<u64> = None;
        let mut identity: Option<FileIdentity> = None;
        let mut carry: Vec<u8> = Vec::new();
        let mut discarding_oversized_line = false;
        let mut ticker = interval(self.poll);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = pres_task.await;
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }
            let Ok(meta) = tokio::fs::metadata(&self.log_path).await else {
                if pos.is_some() {
                    pos = None;
                    identity = None;
                    carry.clear();
                    discarding_oversized_line = false;
                }
                continue;
            };

            let cur_id = FileIdentity::from_meta(&meta);
            let len = meta.len();

            let rotated_by_id = match (identity, cur_id.created_ms) {
                (Some(prev), Some(now_c))
                    if prev.created_ms.is_some() && prev.created_ms != Some(now_c) =>
                {
                    true
                }
                _ => false,
            };

            let start = match pos {
                None if self.from_start => 0,
                None => len,
                Some(p) if len < p || rotated_by_id => {
                    carry.clear();
                    discarding_oversized_line = false;
                    0
                }
                Some(p) => p,
            };
            identity = Some(cur_id);

            if start >= len {
                pos = Some(start.min(len));
                continue;
            }
            let Ok(mut file) = open_shared_read(&self.log_path).await else {
                continue;
            };
            if file.seek(SeekFrom::Start(start)).await.is_err() {
                continue;
            }
            let to_read = (len - start).min(256 * 1024);
            let mut chunk = Vec::with_capacity(to_read as usize);
            let read = match file.take(to_read).read_to_end(&mut chunk).await {
                Ok(n) => n,
                Err(e) => {
                    sink.emit("source.warning", None, json!({ "io_error": e.to_string() }));
                    continue;
                }
            };
            if read == 0 {
                continue;
            }
            pos = Some(start + read as u64);
            carry.extend_from_slice(&chunk);

            while let Some(i) = carry.iter().position(|b| *b == b'\n') {
                let raw: Vec<u8> = carry.drain(..=i).collect();
                if discarding_oversized_line {
                    discarding_oversized_line = false;
                    continue;
                }
                let text_line = String::from_utf8_lossy(&raw);
                let line = text_line.trim_end_matches(&['\r', '\n'][..]);
                if line.len() > MAX_LINE_LEN {
                    sink.emit(
                        "source.warning",
                        None,
                        json!({ "reason": "line_exceeded_max_length", "len": line.len() }),
                    );
                    continue;
                }
                if !line.is_empty() {
                    let events = match tracker.lock() {
                        Ok(mut t) => t.ingest_line(line),
                        Err(p) => p.into_inner().ingest_line(line),
                    };
                    for ev in events {
                        sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                    }
                }
            }
            if carry.len() > MAX_LINE_LEN {
                carry.clear();
                discarding_oversized_line = true;
                sink.emit(
                    "source.warning",
                    None,
                    json!({ "reason": "oversized_unterminated_line_dropped" }),
                );
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct RoiMetrics {
    pub center_luma: f32,
    pub border_luma: f32,
    pub border_bright_ratio: f32,
    pub edge_density: f32,
    pub red_hit_arms: u8,
    pub gold_teal_ratio: f32,
}

struct SharedNv12HudCache {
    updated_at: Option<Instant>,
    entries: [(f32, f32, RoiMetrics); 11],
}

static SHARED_NV12_HUD_CACHE: std::sync::OnceLock<Mutex<SharedNv12HudCache>> =
    std::sync::OnceLock::new();

const HUD_ROI_COORDS: [(f32, f32); 11] = [
    (0.50, 0.812),
    (0.50, 0.215),
    (0.50, 0.635),
    (0.50, 0.500),
    (0.50, 0.745),
    (0.50, 0.255),
    (0.50, 0.785),
    (0.50, 0.765),
    (0.50, 0.720),
    (0.50, 0.145),
    (0.50, 0.840),
];

pub fn publish_nv12_hud_sample(nv12: &[u8], width: u32, height: u32) {
    let w = width as usize;
    let h = height as usize;
    if w < 320 || h < 240 || nv12.len() < (w * h * 3) / 2 {
        return;
    }
    let y_plane = &nv12[..w * h];
    let uv_plane = &nv12[w * h..(w * h * 3) / 2];

    let mut next_entries = [(0.0f32, 0.0f32, RoiMetrics::default()); 11];
    for (idx, &(cx, cy)) in HUD_ROI_COORDS.iter().enumerate() {
        let m = compute_roi_metrics_nv12(y_plane, uv_plane, w, h, cx, cy);
        next_entries[idx] = (cx, cy, m);
    }

    let mtx = SHARED_NV12_HUD_CACHE.get_or_init(|| {
        Mutex::new(SharedNv12HudCache {
            updated_at: None,
            entries: [(0.0, 0.0, RoiMetrics::default()); 11],
        })
    });
    if let Ok(mut guard) = mtx.try_lock() {
        guard.updated_at = Some(Instant::now());
        guard.entries = next_entries;
    }
}

pub(crate) fn read_shared_nv12_hud_metrics(norm_cx: f32, norm_cy: f32) -> Option<RoiMetrics> {
    let mtx = SHARED_NV12_HUD_CACHE.get()?;
    let guard = mtx.lock().ok()?;
    let updated = guard.updated_at?;
    if updated.elapsed() > Duration::from_millis(1200) {
        return None;
    }
    for &(cx, cy, metrics) in &guard.entries {
        if (cx - norm_cx).abs() < 0.005 && (cy - norm_cy).abs() < 0.005 {
            if metrics.center_luma < 1.0 && metrics.border_luma < 1.0 {
                return None;
            }
            return Some(metrics);
        }
    }
    None
}

fn compute_roi_metrics_nv12(
    y_plane: &[u8],
    uv_plane: &[u8],
    w: usize,
    h: usize,
    norm_cx: f32,
    norm_cy: f32,
) -> RoiMetrics {
    let side: usize = 36;
    let wf = w as f32;
    let hf = h as f32;
    let target_aspect = 16.0f32 / 9.0f32;
    let cur_aspect = wf / hf.max(1.0);
    let (hud_x, hud_y, hud_w, hud_h) = if cur_aspect > target_aspect * 1.05 {
        let inner_w = hf * target_aspect;
        ((wf - inner_w) * 0.5, 0.0, inner_w, hf)
    } else {
        (0.0, 0.0, wf, hf)
    };

    let cx_px = (hud_x + hud_w * norm_cx) as i32;
    let cy_px = (hud_y + hud_h * norm_cy) as i32;
    let scale_num = (h as i32).max(360);

    let mut center_luma_sum = 0.0f32;
    let mut center_count = 0u32;
    let mut border_luma_sum = 0.0f32;
    let mut border_bright = 0u32;
    let mut border_count = 0u32;
    let mut edge_hits = 0u32;
    let mut edge_total = 0u32;
    let mut gold_teal = 0u32;
    let mut arm_red = [0u32; 4];

    let c_min = side / 4;
    let c_max = (side * 3) / 4;
    let half = (side / 2) as i32;

    for gy in 0..side {
        let py = (cy_px + ((gy as i32 - half) * scale_num) / 1080).clamp(0, h as i32 - 2) as usize;
        let py_next = (py + 1).min(h - 1);
        let y_row = py * w;
        let y_row_next = py_next * w;
        let uv_row = (py / 2) * w;

        for gx in 0..side {
            let px =
                (cx_px + ((gx as i32 - half) * scale_num) / 1080).clamp(0, w as i32 - 2) as usize;
            let px_next = (px + 1).min(w - 1);

            let luma = y_plane[y_row + px] as f32;
            let uv_idx = uv_row + (px & !1);
            let u = uv_plane[uv_idx] as f32 - 128.0;
            let v = uv_plane[uv_idx + 1] as f32 - 128.0;
            let r = (luma + 1.402 * v).clamp(0.0, 255.0);
            let g = (luma - 0.344136 * u - 0.714136 * v).clamp(0.0, 255.0);
            let b = (luma + 1.772 * u).clamp(0.0, 255.0);

            let in_center = gx >= c_min && gx < c_max && gy >= c_min && gy < c_max;
            if in_center {
                center_luma_sum += luma;
                center_count += 1;
                if (r > 180.0 && g > 140.0 && b < 110.0) || (b > 170.0 && g > 170.0 && r < 120.0) {
                    gold_teal += 1;
                }
                if gx + 1 < c_max && gy + 1 < c_max {
                    let luma_r = y_plane[y_row + px_next] as f32;
                    let luma_d = y_plane[y_row_next + px] as f32;
                    let grad = (luma - luma_r).abs() + (luma - luma_d).abs();
                    edge_total += 1;
                    if grad > 42.0 {
                        edge_hits += 1;
                    }
                }
            } else if gx < 4 || gx >= side - 4 || gy < 4 || gy >= side - 4 {
                border_luma_sum += luma;
                border_count += 1;
                if luma > 210.0 {
                    border_bright += 1;
                }
            }

            if r > 195.0 && g < 75.0 && b < 75.0 && (r - g) > 125.0 {
                if gx >= 6 && gx <= 14 && gy >= 6 && gy <= 14 {
                    arm_red[0] += 1;
                } else if gx >= side - 15 && gx <= side - 7 && gy >= 6 && gy <= 14 {
                    arm_red[1] += 1;
                } else if gx >= 6 && gx <= 14 && gy >= side - 15 && gy <= side - 7 {
                    arm_red[2] += 1;
                } else if gx >= side - 15 && gx <= side - 7 && gy >= side - 15 && gy <= side - 7 {
                    arm_red[3] += 1;
                }
            }
        }
    }

    let active_arms = arm_red.iter().filter(|&&c| c >= 3).count() as u8;

    RoiMetrics {
        center_luma: if center_count > 0 {
            center_luma_sum / (center_count as f32)
        } else {
            0.0
        },
        border_luma: if border_count > 0 {
            border_luma_sum / (border_count as f32)
        } else {
            0.0
        },
        border_bright_ratio: if border_count > 0 {
            (border_bright as f32) / (border_count as f32)
        } else {
            0.0
        },
        edge_density: if edge_total > 0 {
            (edge_hits as f32) / (edge_total as f32)
        } else {
            0.0
        },
        red_hit_arms: active_arms,
        gold_teal_ratio: if center_count > 0 {
            (gold_teal as f32) / (center_count as f32)
        } else {
            0.0
        },
    }
}

#[cfg(windows)]
struct TinyRoiSampler {
    last_hwnd: usize,
    last_allowed: bool,
}

#[cfg(windows)]
unsafe impl Send for TinyRoiSampler {}

#[cfg(windows)]
impl TinyRoiSampler {
    fn new(_side: i32) -> Option<Self> {
        Some(Self {
            last_hwnd: 0,
            last_allowed: false,
        })
    }

    fn sample_foreground(
        &mut self,
        allowed_procs: &[&str],
        norm_cx: f32,
        norm_cy: f32,
    ) -> Option<RoiMetrics> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
        };

        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() || IsIconic(hwnd).as_bool() {
                return None;
            }
            let raw_hwnd = hwnd.0 as usize;
            if raw_hwnd != self.last_hwnd {
                self.last_hwnd = raw_hwnd;
                self.last_allowed = false;

                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if pid == 0 {
                    return None;
                }
                let proc_handle =
                    OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
                let mut exe_buf = [0u16; 512];
                let mut len = exe_buf.len() as u32;
                let ok = QueryFullProcessImageNameW(
                    proc_handle,
                    PROCESS_NAME_FORMAT(0),
                    windows::core::PWSTR(exe_buf.as_mut_ptr()),
                    &mut len,
                )
                .is_ok();
                let _ = CloseHandle(proc_handle);
                if !ok || len == 0 {
                    return None;
                }
                let path_str = String::from_utf16_lossy(&exe_buf[..len as usize]);
                let exe_name = Path::new(&path_str)
                    .file_name()
                    .and_then(|n| n.to_str())?
                    .to_lowercase();
                self.last_allowed = allowed_procs.iter().any(|p| {
                    exe_name == *p || exe_name.contains(p.trim_end_matches(".exe"))
                });
            }

            if !self.last_allowed {
                return None;
            }

            read_shared_nv12_hud_metrics(norm_cx, norm_cy)
        }
    }
}

async fn run_valorant_hud_loop(
    sink: EventSink,
    tracker: Arc<Mutex<ValorantTracker>>,
    cancel: CancellationToken,
) {
    #[cfg(windows)]
    {
        let Some(mut sampler) = TinyRoiSampler::new(36) else {
            return;
        };
        let mut ticker = interval(Duration::from_millis(200));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let procs = ["valorant-win64-shipping.exe", "valorant.exe"];
        let mut ema_luma = 0.0f32;
        let mut ema_edge = 0.0f32;
        let mut initialized = false;
        let mut banner_active = false;
        let mut active_ticks = 0u32;
        let mut prev_active_luma = 0.0f32;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }

            let Some(m) = sampler.sample_foreground(&procs, 0.50, 0.812) else {
                banner_active = false;
                active_ticks = 0;
                continue;
            };

            if m.border_bright_ratio > 0.55 || m.border_luma > 185.0 {
                banner_active = false;
                active_ticks = 0;
                continue;
            }

            if !initialized {
                ema_luma = m.center_luma;
                ema_edge = m.edge_density;
                initialized = true;
                continue;
            }

            let contrast = m.center_luma - m.border_luma;
            let luma_rise = m.center_luma - ema_luma;
            let edge_rise = m.edge_density - ema_edge;

            let emblem_present = contrast > 30.0
                && m.edge_density > 0.17
                && (luma_rise > 20.0 || edge_rise > 0.07 || m.gold_teal_ratio > 0.14);

            let mut fire_kill = false;
            if emblem_present {
                active_ticks += 1;
                if !banner_active && active_ticks >= 1 && active_ticks <= 15 {
                    banner_active = true;
                    prev_active_luma = m.center_luma;
                    fire_kill = true;
                } else if banner_active && active_ticks <= 20 {
                    if (m.center_luma - prev_active_luma) > 22.0 && m.edge_density > 0.20 {
                        prev_active_luma = m.center_luma;
                        fire_kill = true;
                    } else {
                        prev_active_luma = prev_active_luma * 0.7 + m.center_luma * 0.3;
                    }
                } else if active_ticks > 22 {
                    ema_luma = ema_luma * 0.8 + m.center_luma * 0.2;
                    ema_edge = ema_edge * 0.8 + m.edge_density * 0.2;
                }
            } else {
                banner_active = false;
                active_ticks = 0;
                ema_luma = ema_luma * 0.88 + m.center_luma * 0.12;
                ema_edge = ema_edge * 0.88 + m.edge_density * 0.12;
            }

            if fire_kill {
                let now = Instant::now();
                let events = match tracker.lock() {
                    Ok(mut t) => {
                        t.combat.in_match = true;
                        t.combat.record_kill(false, "hud_banner", now)
                    }
                    Err(p) => {
                        let mut g = p.into_inner();
                        g.combat.in_match = true;
                        g.combat.record_kill(false, "hud_banner", now)
                    }
                };
                for ev in events {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            }

            if let Some(top_m) = sampler.sample_foreground(&procs, 0.50, 0.215) {
                if top_m.border_bright_ratio < 0.45
                    && (top_m.center_luma - top_m.border_luma) > 36.0
                    && top_m.edge_density > 0.22
                    && top_m.gold_teal_ratio > 0.18
                {
                    let mut clutch_ev = None;
                    if let Ok(mut t) = tracker.lock() {
                        if t.combat.round_kills >= 1 && !t.combat.clutch_emitted {
                            t.combat.clutch_emitted = true;
                            t.combat.seq = t.combat.seq.wrapping_add(1);
                            clutch_ev = Some(EmittedEvent {
                                kind: "player.clutch".into(),
                                confidence: Confidence::Inferred,
                                dedup_key: Some(format!("val_hud_clutch:{}", t.combat.seq)),
                                data: json!({
                                    "count": t.combat.round_kills,
                                    "round_kills": t.combat.round_kills,
                                    "round": t.combat.current_round,
                                    "via": "hud_announcement",
                                }),
                            });
                        }
                    }
                    if let Some(ev) = clutch_ev {
                        sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (sink, tracker);
        cancel.cancelled().await;
    }
}

async fn run_hud_highlight_loop(
    sink: EventSink,
    cancel: CancellationToken,
    profile: HudGameProfile,
    allowed_procs: &[&str],
) {
    #[cfg(windows)]
    {
        let Some(mut sampler) = TinyRoiSampler::new(36) else {
            return;
        };
        let mut combat = CombatHighlightTracker::default();
        let mut ticker = interval(Duration::from_millis(200));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut ema_luma = 0.0f32;
        let mut ema_edge = 0.0f32;
        let mut initialized = false;
        let mut banner_active = false;
        let mut active_ticks = 0u32;

        let (primary_cx, primary_cy) = match profile {
            HudGameProfile::Valorant => (0.50, 0.812),
            HudGameProfile::Overwatch2 => (0.50, 0.635),
            HudGameProfile::CallOfDuty => (0.50, 0.500),
            HudGameProfile::Battlefield | HudGameProfile::ArcRaiders => (0.50, 0.745),
            HudGameProfile::RocketLeague => (0.50, 0.255),
            HudGameProfile::ApexLegends | HudGameProfile::TheFinals => (0.50, 0.785),
            HudGameProfile::Fortnite => (0.50, 0.765),
            HudGameProfile::RainbowSixSiege | HudGameProfile::MarvelRivals => (0.50, 0.720),
            HudGameProfile::EaSportsFc => (0.50, 0.145),
            HudGameProfile::EFootball => (0.50, 0.840),
        };

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }

            let Some(m) = sampler.sample_foreground(allowed_procs, primary_cx, primary_cy) else {
                banner_active = false;
                active_ticks = 0;
                continue;
            };

            if m.border_bright_ratio > 0.55 || m.border_luma > 188.0 {
                banner_active = false;
                active_ticks = 0;
                continue;
            }

            if !initialized {
                ema_luma = m.center_luma;
                ema_edge = m.edge_density;
                initialized = true;
                continue;
            }

            let contrast = m.center_luma - m.border_luma;
            let luma_rise = m.center_luma - ema_luma;
            let edge_rise = m.edge_density - ema_edge;

            let triggered = match profile {
                HudGameProfile::CallOfDuty
                | HudGameProfile::RainbowSixSiege
                | HudGameProfile::ApexLegends => {
                    m.red_hit_arms >= 3
                        || (contrast > 32.0 && m.edge_density > 0.18 && m.gold_teal_ratio > 0.15 && luma_rise > 20.0)
                }
                HudGameProfile::Overwatch2 | HudGameProfile::MarvelRivals => {
                    m.red_hit_arms >= 3
                        || (contrast > 30.0 && m.edge_density > 0.18 && luma_rise > 22.0 && edge_rise > 0.06)
                }
                HudGameProfile::Battlefield
                | HudGameProfile::TheFinals
                | HudGameProfile::ArcRaiders
                | HudGameProfile::Fortnite => {
                    m.red_hit_arms >= 3
                        || (contrast > 30.0 && m.edge_density > 0.17 && (luma_rise > 20.0 || m.gold_teal_ratio > 0.14))
                }
                HudGameProfile::RocketLeague
                | HudGameProfile::EaSportsFc
                | HudGameProfile::EFootball => {
                    contrast > 34.0 && m.edge_density > 0.20 && m.gold_teal_ratio > 0.16 && luma_rise > 22.0
                }
                HudGameProfile::Valorant => {
                    contrast > 30.0 && m.edge_density > 0.17 && (luma_rise > 20.0 || edge_rise > 0.07)
                }
            };

            if triggered {
                active_ticks += 1;
                if !banner_active && active_ticks == 1 {
                    banner_active = true;
                    let now = Instant::now();
                    if matches!(
                        profile,
                        HudGameProfile::RocketLeague
                            | HudGameProfile::EaSportsFc
                            | HudGameProfile::EFootball
                    ) {
                        combat.seq = combat.seq.wrapping_add(1);
                        sink.emit_detailed(
                            "sport.goal",
                            None,
                            Confidence::Inferred,
                            Some(format!("sport_hud_goal:{}", combat.seq)),
                            json!({ "via": "hud_banner" }),
                        );
                    } else {
                        for ev in combat.record_kill(false, "hud_detector", now) {
                            sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                        }
                    }
                } else if active_ticks > 20 {
                    ema_luma = ema_luma * 0.8 + m.center_luma * 0.2;
                    ema_edge = ema_edge * 0.8 + m.edge_density * 0.2;
                }
            } else {
                banner_active = false;
                active_ticks = 0;
                ema_luma = ema_luma * 0.88 + m.center_luma * 0.12;
                ema_edge = ema_edge * 0.88 + m.edge_density * 0.12;
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (sink, profile, allowed_procs);
        cancel.cancelled().await;
    }
}

pub struct Overwatch2Source {
    workshop_dir: PathBuf,
}

impl Default for Overwatch2Source {
    fn default() -> Self {
        Self::new()
    }
}

impl Overwatch2Source {
    pub fn new() -> Self {
        Self {
            workshop_dir: locate_overwatch2_workshop_dir(),
        }
    }
}

#[async_trait]
impl EventSource for Overwatch2Source {
    fn name(&self) -> &str {
        "overwatch2_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::Overwatch2,
                &["overwatch.exe"],
            )
            .await;
        });

        let start_re = Regex::new(r"(?i)MATCH_START\s+map=(?P<map>.+)")?;
        let kill_re = Regex::new(
            r"(?i)(?:KILL|ELIMINATED|FINAL_BLOW)\s+attacker=(?P<attacker>[^;]+);victim=(?P<victim>[^;]+);ability=(?P<ability>.+)",
        )?;
        let end_re = Regex::new(r"(?i)MATCH_END\s+winner=(?P<winner>.+)")?;

        let mut active_log: Option<PathBuf> = None;
        let mut pos: u64 = 0;
        let mut combat = CombatHighlightTracker::default();
        let mut ticker = interval(Duration::from_millis(250));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            if let Some(newest) = newest_file_in_dir(&self.workshop_dir) {
                if active_log.as_ref() != Some(&newest) {
                    pos = tokio::fs::metadata(&newest).await.map(|m| m.len()).unwrap_or(0);
                    active_log = Some(newest);
                }
            }

            let Some(ref path) = active_log else {
                continue;
            };
            let Ok(meta) = tokio::fs::metadata(path).await else {
                continue;
            };
            let len = meta.len();
            if len < pos {
                pos = 0;
            }
            if pos >= len {
                continue;
            }
            let Ok(mut file) = open_shared_read(path).await else {
                continue;
            };
            if file.seek(SeekFrom::Start(pos)).await.is_err() {
                continue;
            }
            let to_read = (len - pos).min(128 * 1024);
            let mut buf = Vec::with_capacity(to_read as usize);
            let Ok(n) = file.take(to_read).read_to_end(&mut buf).await else {
                continue;
            };
            if n == 0 {
                continue;
            }
            pos += n as u64;
            let text_chunk = String::from_utf8_lossy(&buf);
            for raw_line in text_chunk.lines() {
                let line = raw_line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(caps) = start_re.captures(line) {
                    let map = caps.name("map").map_or("", |m| m.as_str());
                    combat.reset_match();
                    sink.emit("workshop.match_start", None, json!({ "map": map, "line": line }));
                    sink.emit("match.start", None, json!({ "map": map, "line": line }));
                } else if let Some(caps) = kill_re.captures(line) {
                    let attacker = caps.name("attacker").map_or("", |m| m.as_str());
                    let victim = caps.name("victim").map_or("", |m| m.as_str());
                    let ability = caps.name("ability").map_or("", |m| m.as_str());
                    sink.emit(
                        "workshop.kill",
                        None,
                        json!({
                            "attacker": attacker,
                            "victim": victim,
                            "ability": ability,
                            "line": line,
                        }),
                    );
                    for ev in combat.record_kill(false, "log", Instant::now()) {
                        sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                    }
                } else if let Some(caps) = end_re.captures(line) {
                    let winner = caps.name("winner").map_or("", |m| m.as_str());
                    sink.emit("workshop.match_end", None, json!({ "winner": winner, "line": line }));
                    sink.emit("match.end", None, json!({ "winner": winner, "line": line }));
                }
            }
        }
    }
}

pub const COD_EXECUTABLES: &[&str] = &[
    "cod.exe",
    "cod24-cod.exe",
    "cod23-cod.exe",
    "cod22-cod.exe",
    "blackops6.exe",
    "blackopscoldwar.exe",
    "modernwarfare.exe",
    "modernwarfareii.exe",
    "modernwarfareiii.exe",
    "mw2.exe",
    "mw3.exe",
    "warzone.exe",
    "vanguard.exe",
    "iw8_ship.exe",
    "t9_ship.exe",
    "s4_ship.exe",
];

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

pub struct CallOfDutySource {
    log_path: Option<PathBuf>,
}

impl Default for CallOfDutySource {
    fn default() -> Self {
        Self::new()
    }
}

impl CallOfDutySource {
    pub fn new() -> Self {
        Self {
            log_path: locate_cod_log_path(),
        }
    }

    pub fn with_log(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: Some(log_path.into()),
        }
    }
}

#[async_trait]
impl EventSource for CallOfDutySource {
    fn name(&self) -> &str {
        "cod_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::CallOfDuty,
                COD_EXECUTABLES,
            )
            .await;
        });

        let mut tracker = CallOfDutyTracker::new();
        let mut active_log = self.log_path.clone();
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(200));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            if active_log.is_none() {
                active_log = locate_cod_log_path();
            }
            let Some(ref path) = active_log else {
                continue;
            };
            tail_lines_with(path, &mut pos, false, |line| {
                for ev in tracker.ingest_line(line) {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            })
            .await;
        }
    }
}

pub struct BattlefieldSource;

impl Default for BattlefieldSource {
    fn default() -> Self {
        Self::new()
    }
}

impl BattlefieldSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EventSource for BattlefieldSource {
    fn name(&self) -> &str {
        "battlefield_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        run_hud_highlight_loop(
            sink,
            cancel,
            HudGameProfile::Battlefield,
            &[
                "bf2042.exe",
                "bf6.exe",
                "bfv.exe",
                "bf1.exe",
                "bf4.exe",
                "battlefield2042.exe",
                "battlefieldv.exe",
                "battlefield1.exe",
            ],
        )
        .await;
        Ok(())
    }
}

fn newest_file_in_dir(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<( std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map_or(true, |(t, _)| modified > *t) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, p)| p)
}

fn steam_libraries() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(p) = std::env::var_os(key) {
            roots.push(PathBuf::from(p).join("Steam"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let h = PathBuf::from(&home);
        roots.push(h.join(".steam/steam"));
        roots.push(h.join(".local/share/Steam"));
        roots.push(h.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"));
        roots.push(h.join("snap/steam/common/.local/share/Steam"));
    }
    roots.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
    let pattern = Regex::new(r#""path"\s+"([^"]+)""#).ok();
    let mut libs = Vec::new();
    for root in roots {
        if let (Some(re), Ok(vdf)) = (
            &pattern,
            std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")),
        ) {
            for c in re.captures_iter(&vdf) {
                libs.push(PathBuf::from(c[1].replace("\\\\", "\\")));
            }
        }
        libs.push(root);
    }
    libs
}

pub fn locate_cs2_cfg_dir() -> Option<PathBuf> {
    steam_libraries()
        .into_iter()
        .map(|l| l.join("steamapps/common/Counter-Strike Global Offensive/game/csgo/cfg"))
        .find(|p| p.is_dir())
}

pub fn locate_dota2_cfg_dir() -> Option<PathBuf> {
    steam_libraries()
        .into_iter()
        .map(|l| l.join("steamapps/common/dota 2 beta/game/dota/cfg"))
        .find(|p| p.is_dir())
}

pub fn locate_rocket_league_config_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = steam_libraries()
        .into_iter()
        .map(|l| l.join("steamapps/common/rocketleague/TAGame/Config"))
        .collect();
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(p) = std::env::var_os(key) {
            candidates.push(PathBuf::from(p).join("Epic Games/rocketleague/TAGame/Config"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let h = PathBuf::from(home);
        candidates.push(h.join("Games/Heroic/rocketleague/TAGame/Config"));
        candidates.push(h.join("Games/epic-games-store/drive_c/Program Files/Epic Games/rocketleague/TAGame/Config"));
    }
    candidates.push(PathBuf::from(
        r"C:\Program Files\Epic Games\rocketleague\TAGame\Config",
    ));
    candidates.into_iter().find(|p| p.is_dir())
}

pub fn locate_valorant_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(local_app_data).join("VALORANT/Saved/Logs/ShooterGame.log"));
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return Some(
            PathBuf::from(user_profile).join("AppData/Local/VALORANT/Saved/Logs/ShooterGame.log"),
        );
    }
    if let Some(wine_prefix) = std::env::var_os("WINEPREFIX") {
        let p = PathBuf::from(wine_prefix)
            .join("drive_c/users/steamuser/AppData/Local/VALORANT/Saved/Logs/ShooterGame.log");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn locate_overwatch2_log_dir() -> PathBuf {
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(user_profile).join("Documents/Overwatch/Logs");
    }
    for lib in steam_libraries() {
        let proton_doc = lib.join(
            "steamapps/compatdata/2357570/pfx/drive_c/users/steamuser/Documents/Overwatch/Logs",
        );
        if proton_doc.is_dir() {
            return proton_doc;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join("Documents/Overwatch/Logs")
    } else {
        PathBuf::from("Overwatch/Logs")
    }
}

pub fn locate_overwatch2_workshop_dir() -> PathBuf {
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(user_profile).join("Documents/Overwatch/Workshop");
    }
    for lib in steam_libraries() {
        let proton_ws = lib.join(
            "steamapps/compatdata/2357570/pfx/drive_c/users/steamuser/Documents/Overwatch/Workshop",
        );
        if proton_ws.is_dir() {
            return proton_ws;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join("Documents/Overwatch/Workshop")
    } else {
        PathBuf::from("Overwatch/Workshop")
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

pub const ROCKET_LEAGUE_WEB_PORT: u16 = 49124;

pub fn enable_rocket_league_stats_api(
    config_dir: &Path,
    port: u16,
    rate: u32,
) -> io::Result<PathBuf> {
    enable_rocket_league_stats_api_with_web_port(
        config_dir,
        port,
        ROCKET_LEAGUE_WEB_PORT,
        rate,
    )
}

pub fn enable_rocket_league_stats_api_with_web_port(
    config_dir: &Path,
    port: u16,
    web_port: u16,
    rate: u32,
) -> io::Result<PathBuf> {
    if !config_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Rocket League config directory does not exist",
        ));
    }
    let ta = config_dir.join("TAStatsAPI.ini");
    let path = if ta.exists() {
        ta
    } else {
        config_dir.join("DefaultStatsAPI.ini")
    };

    let current = match std::fs::read(&path) {
        Ok(bytes) => {
            if bytes.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "INI file appears to be binary or UTF-16 encoded; refusing to corrupt it",
                ));
            }
            String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };

    if path.exists() {
        let backup = path.with_extension("ini.bak");
        if !backup.exists() {
            let _ = std::fs::copy(&path, &backup);
        }
    }

    let section = "TAGame.MatchStatsExporter_TA";
    let updated = upsert_ini(
        &current,
        section,
        "PacketSendRate",
        &rate.clamp(1, 120).to_string(),
    );
    let updated = upsert_ini(&updated, section, "Port", &port.to_string());
    let updated = upsert_ini(&updated, section, "WebPort", &web_port.to_string());

    let tmp_path = path.with_extension("ini.tmp");
    std::fs::write(&tmp_path, updated)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(path)
}


async fn tail_lines_with<F>(
    path: &Path,
    pos: &mut Option<u64>,
    from_start: bool,
    mut on_line: F,
) where
    F: FnMut(&str),
{
    let Ok(meta) = tokio::fs::metadata(path).await else {
        *pos = None;
        return;
    };
    let len = meta.len();
    let start = match *pos {
        None if from_start => 0,
        None => len,
        Some(p) if len < p => 0,
        Some(p) => p,
    };
    if start >= len {
        *pos = Some(len);
        return;
    }
    let Ok(mut file) = open_shared_read(path).await else {
        return;
    };
    if file.seek(SeekFrom::Start(start)).await.is_err() {
        return;
    }
    let to_read = (len - start).min(256 * 1024);
    let mut buf = Vec::with_capacity(to_read as usize);
    let Ok(n) = file.take(to_read).read_to_end(&mut buf).await else {
        return;
    };
    if n == 0 {
        return;
    }
let consumed = match buf.iter().rposition(|&b| b == b'\n') {
        Some(last_nl) => last_nl + 1,
        None if n >= MAX_LINE_LEN => n,
        None => return,
    };
    *pos = Some(start + consumed as u64);
    let text_chunk = String::from_utf8_lossy(&buf[..consumed]);
    for raw_line in text_chunk.lines() {
        let line = raw_line.trim();
        if !line.is_empty() && line.len() <= MAX_LINE_LEN {
            on_line(line);
        }
    }
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

pub struct FortniteSource {
    log_path: PathBuf,
    from_start: bool,
    poll: Duration,
}

impl Default for FortniteSource {
    fn default() -> Self {
        Self::new(
            locate_fortnite_log_path()
                .unwrap_or_else(|| PathBuf::from("FortniteGame/Saved/Logs/FortniteGame.log")),
        )
    }
}

impl FortniteSource {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
            from_start: false,
            poll: Duration::from_millis(120),
        }
    }

    pub fn from_start(mut self, yes: bool) -> Self {
        self.from_start = yes;
        self
    }
}

#[async_trait]
impl EventSource for FortniteSource {
    fn name(&self) -> &str {
        "fortnite_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::Fortnite,
                &[
                    "fortniteclient-win64-shipping.exe",
                    "fortniteclient-win64-shipping_eac.exe",
                    "fortniteclient-win64-shipping_be.exe",
                ],
            )
            .await;
        });

        let mut tracker = FortniteTracker::new();
        let mut pos: Option<u64> = None;
        let mut ticker = interval(self.poll);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            tail_lines_with(&self.log_path, &mut pos, self.from_start, |line| {
                for ev in tracker.ingest_line(line) {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            })
            .await;
        }
    }
}

pub const APEX_LIVEAPI_PORT: u16 = 3002;

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

struct ApexShared {
    sink: EventSink,
    tracker: Mutex<ApexTracker>,
}

async fn apex_liveapi_ingest(
    State(shared): State<Arc<ApexShared>>,
    Json(payload): Json<Value>,
) -> StatusCode {
    let events = match shared.tracker.lock() {
        Ok(mut t) => t.ingest_json(&payload),
        Err(p) => p.into_inner().ingest_json(&payload),
    };
    for ev in events {
        shared
            .sink
            .emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
    }
    StatusCode::OK
}

pub struct ApexLegendsSource {
    addr: SocketAddr,
    log_path: Option<PathBuf>,
}

impl Default for ApexLegendsSource {
    fn default() -> Self {
        Self::new(APEX_LIVEAPI_PORT)
    }
}

impl ApexLegendsSource {
    pub fn new(port: u16) -> Self {
        Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            log_path: locate_apex_log_path(),
        }
    }

    pub fn with_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.log_path = Some(path.into());
        self
    }
}

#[async_trait]
impl EventSource for ApexLegendsSource {
    fn name(&self) -> &str {
        "apex_liveapi_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::ApexLegends,
                &["r5apex.exe", "r5apex_dx12.exe"],
            )
            .await;
        });

        let shared = Arc::new(ApexShared {
            sink: sink.clone(),
            tracker: Mutex::new(ApexTracker::new()),
        });

        let http_shared = shared.clone();
        let http_cancel = cancel.child_token();
        let addr = self.addr;
        let http_task = tokio::spawn(async move {
            if let Ok(listener) = TcpListener::bind(addr).await {
                let app = Router::new()
                    .route("/liveapi", post(apex_liveapi_ingest))
                    .route("/", post(apex_liveapi_ingest))
                    .layer(DefaultBodyLimit::max(MAX_HTTP_BODY))
                    .with_state(http_shared);
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move { http_cancel.cancelled().await })
                    .await;
            }
        });

        let mut active_log = self.log_path.clone();
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(200));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = http_task.await;
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            if active_log.is_none() {
                active_log = locate_apex_log_path();
            }
            if let Some(ref path) = active_log {
                tail_lines_with(path, &mut pos, false, |line| {
                    let evs = match shared.tracker.lock() {
                        Ok(mut t) => t.ingest_line(line),
                        Err(p) => p.into_inner().ingest_line(line),
                    };
                    for ev in evs {
                        sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                    }
                })
                .await;
            }
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

pub struct RainbowSixSiegeSource {
    replay_dirs: Vec<PathBuf>,
    log_path: Option<PathBuf>,
}

impl Default for RainbowSixSiegeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl RainbowSixSiegeSource {
    pub fn new() -> Self {
        Self {
            replay_dirs: locate_r6_match_replay_dirs(),
            log_path: None,
        }
    }

    pub fn with_replay_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            replay_dirs: vec![dir.into()],
            log_path: None,
        }
    }
}

#[async_trait]
impl EventSource for RainbowSixSiegeSource {
    fn name(&self) -> &str {
        "r6_siege_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::RainbowSixSiege,
                &[
                    "rainbowsix.exe",
                    "rainbowsix_vulkan.exe",
                    "rainbowsix_dx11.exe",
                ],
            )
            .await;
        });

        let mut tracker = RainbowSixTracker::new();
        let mut seeded_existing_replays = false;
        let mut known_old_rec_files: HashSet<String> = HashSet::new();
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(500));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            for r_dir in &self.replay_dirs {
                if let Ok(entries) = std::fs::read_dir(r_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if !path.is_dir() {
                            continue;
                        }
                        let Some(folder_name) = path.file_name().and_then(|s| s.to_str()) else {
                            continue;
                        };
                        if !folder_name.starts_with("Match-") {
                            continue;
                        }
                        if let Ok(sub_entries) = std::fs::read_dir(&path) {
                            let mut rec_files: Vec<PathBuf> = sub_entries
                                .flatten()
                                .map(|e| e.path())
                                .filter(|p| {
                                    p.extension()
                                        .and_then(|x| x.to_str())
                                        .map_or(false, |ext| ext.eq_ignore_ascii_case("rec"))
                                })
                                .collect();
                            rec_files.sort();
                            for rec_path in rec_files {
                                let Some(fname) = rec_path.file_name().and_then(|s| s.to_str())
                                else {
                                    continue;
                                };
                                let composite_key = format!("{folder_name}/{fname}");
                                if !seeded_existing_replays {
                                    known_old_rec_files.insert(composite_key);
                                    continue;
                                }
                                if known_old_rec_files.contains(&composite_key) {
                                    continue;
                                }
                                if tracker
                                    .seen_round_files
                                    .iter()
                                    .any(|s| s.eq_ignore_ascii_case(fname))
                                    && tracker.active_match_folder == folder_name
                                {
                                    continue;
                                }
                                known_old_rec_files.insert(composite_key);
                                let mut header = vec![0u8; 512];
                                if let Ok(mut f) = std::fs::File::open(&rec_path) {
                                    use std::io::Read;
                                    let n = f.read(&mut header).unwrap_or(0);
                                    header.truncate(n);
                                } else {
                                    header.clear();
                                }
                                for ev in tracker.ingest_replay_round(folder_name, fname, &header) {
                                    sink.emit_detailed(
                                        ev.kind,
                                        None,
                                        ev.confidence,
                                        ev.dedup_key,
                                        ev.data,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            seeded_existing_replays = true;

            if let Some(ref path) = self.log_path {
                tail_lines_with(path, &mut pos, false, |line| {
                    for ev in tracker.ingest_line(line) {
                        sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                    }
                })
                .await;
            }
        }
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

pub struct TheFinalsSource {
    log_path: PathBuf,
}

impl Default for TheFinalsSource {
    fn default() -> Self {
        Self::new(
            locate_the_finals_log_path()
                .unwrap_or_else(|| PathBuf::from("Discovery/Saved/Logs/Discovery.log")),
        )
    }
}

impl TheFinalsSource {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
        }
    }
}

#[async_trait]
impl EventSource for TheFinalsSource {
    fn name(&self) -> &str {
        "the_finals_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::TheFinals,
                &[
                    "discovery.exe",
                    "discovery-d.exe",
                    "discovery-win64-shipping.exe",
                    "thefinals.exe",
                ],
            )
            .await;
        });

        let mut tracker = TheFinalsTracker::new();
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(150));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            tail_lines_with(&self.log_path, &mut pos, false, |line| {
                for ev in tracker.ingest_line(line) {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            })
            .await;
        }
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

pub struct ArcRaidersSource {
    log_path: PathBuf,
}

impl Default for ArcRaidersSource {
    fn default() -> Self {
        Self::new(
            locate_arc_raiders_log_path()
                .unwrap_or_else(|| PathBuf::from("PioneerGame/Saved/Logs/PioneerGame.log")),
        )
    }
}

impl ArcRaidersSource {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
        }
    }
}

#[async_trait]
impl EventSource for ArcRaidersSource {
    fn name(&self) -> &str {
        "arc_raiders_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::ArcRaiders,
                &[
                    "pioneergame.exe",
                    "pioneergame-win64-shipping.exe",
                    "arcraiders.exe",
                    "arcraiders-win64-shipping.exe",
                ],
            )
            .await;
        });

        let mut tracker = ArcRaidersTracker::new();
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(150));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            tail_lines_with(&self.log_path, &mut pos, false, |line| {
                for ev in tracker.ingest_line(line) {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            })
            .await;
        }
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

pub struct MarvelRivalsSource {
    log_path: PathBuf,
}

impl Default for MarvelRivalsSource {
    fn default() -> Self {
        Self::new(
            locate_marvel_rivals_log_path()
                .unwrap_or_else(|| PathBuf::from("Marvel/Saved/Logs/Marvel.log")),
        )
    }
}

impl MarvelRivalsSource {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
        }
    }
}

#[async_trait]
impl EventSource for MarvelRivalsSource {
    fn name(&self) -> &str {
        "marvel_rivals_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::MarvelRivals,
                &[
                    "marvel-win64-shipping.exe",
                    "marvel.exe",
                    "marvelrivals.exe",
                ],
            )
            .await;
        });

        let mut tracker = MarvelRivalsTracker::new();
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(150));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            tail_lines_with(&self.log_path, &mut pos, false, |line| {
                for ev in tracker.ingest_line(line) {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            })
            .await;
        }
    }
}

pub fn locate_fortnite_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(local_app_data).join("FortniteGame/Saved/Logs/FortniteGame.log"));
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return Some(
            PathBuf::from(user_profile)
                .join("AppData/Local/FortniteGame/Saved/Logs/FortniteGame.log"),
        );
    }
    None
}

pub fn locate_apex_cfg_dir() -> Option<PathBuf> {
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let dir = PathBuf::from(user_profile).join("Saved Games/Respawn/Apex/assets/cfg");
        if dir.parent().map_or(false, |p| p.exists()) {
            return Some(dir);
        }
    }
    None
}

pub fn install_apex_liveapi_config(cfg_dir: &Path, port: u16) -> io::Result<PathBuf> {
    std::fs::create_dir_all(cfg_dir)?;
    let path = cfg_dir.join("config.json");
    if path.exists() {
        let bak = cfg_dir.join("config.json.bak");
        let _ = std::fs::copy(&path, &bak);
    }
    let body = serde_json::to_string_pretty(&json!({
        "cl_liveapi_enabled": true,
        "cl_liveapi_use_v2": true,
        "cl_liveapi_use_websocket": false,
        "cl_liveapi_use_protobuf": false,
        "cl_liveapi_pretty_print_log": false,
        "cl_liveapi_WriteToFile": true,
        "cl_liveapi_http_ClickhouseUrl": format!("http://127.0.0.1:{port}/liveapi")
    }))
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = cfg_dir.join("config.json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn locate_apex_log_path() -> Option<PathBuf> {
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let base = PathBuf::from(user_profile).join("Saved Games/Respawn/Apex");
        if let Some(f) = newest_file_in_dir(&base.join("assets/temp")) {
            return Some(f);
        }
    }
    None
}

pub fn locate_r6_match_replay_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let siege_docs = PathBuf::from(user_profile).join("Documents/My Games/Rainbow Six - Siege");
        if let Ok(entries) = std::fs::read_dir(&siege_docs) {
            for entry in entries.flatten() {
                let sub = entry.path().join("MatchReplay");
                if sub.is_dir() {
                    dirs.push(sub);
                }
            }
        }
        let direct = siege_docs.join("MatchReplay");
        if direct.is_dir() {
            dirs.push(direct);
        }
    }
    for lib in steam_libraries() {
        let steam_r6 = lib.join("steamapps/common/Tom Clancy's Rainbow Six Siege/MatchReplay");
        if steam_r6.is_dir() {
            dirs.push(steam_r6);
        }
    }
    dirs
}

pub fn locate_cod_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let act_dir = PathBuf::from(local_app_data).join("Activision/Call of Duty");
        if let Some(f) = newest_file_in_dir(&act_dir) {
            return Some(f);
        }
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let docs = PathBuf::from(user_profile).join("Documents/Call of Duty/players");
        if let Some(f) = newest_file_in_dir(&docs) {
            return Some(f);
        }
    }
    None
}

pub fn locate_the_finals_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(local_app_data).join("Discovery/Saved/Logs/Discovery.log"));
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return Some(
            PathBuf::from(user_profile).join("AppData/Local/Discovery/Saved/Logs/Discovery.log"),
        );
    }
    None
}

pub fn locate_arc_raiders_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let base = PathBuf::from(local_app_data);
        let pioneer = base.join("PioneerGame/Saved/Logs/PioneerGame.log");
        if pioneer.exists() {
            return Some(pioneer);
        }
        let arc = base.join("ArcRaiders/Saved/Logs/ArcRaiders.log");
        if arc.exists() {
            return Some(arc);
        }
        return Some(pioneer);
    }
    None
}

pub fn locate_marvel_rivals_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(local_app_data).join("Marvel/Saved/Logs/Marvel.log"));
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(user_profile).join("AppData/Local/Marvel/Saved/Logs/Marvel.log"));
    }
    None
}


pub const FC26_EXECUTABLES: &[&str] = &[
    "fc26.exe",
    "fc26_trial.exe",
    "easportsfc26.exe",
];

pub const FC27_EXECUTABLES: &[&str] = &[
    "fc27.exe",
    "fc27_trial.exe",
    "easportsfc27.exe",
];

pub const EFOOTBALL_EXECUTABLES: &[&str] = &[
    "efootball.exe",
    "efootball2026.exe",
    "efootball2027.exe",
    "pes2021.exe",
];

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

pub struct EaSportsFc26Source {
    log_path: PathBuf,
}

impl Default for EaSportsFc26Source {
    fn default() -> Self {
        Self::new(
            locate_fc26_log_path()
                .unwrap_or_else(|| PathBuf::from("EA SPORTS FC 26/logs/fc26_telemetry.log")),
        )
    }
}

impl EaSportsFc26Source {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
        }
    }
}

#[async_trait]
impl EventSource for EaSportsFc26Source {
    fn name(&self) -> &str {
        "fc26_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::EaSportsFc,
                FC26_EXECUTABLES,
            )
            .await;
        });

        let mut tracker = FootballTracker::new(FootballTitle::Fc26);
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(180));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            tail_lines_with(&self.log_path, &mut pos, false, |line| {
                for ev in tracker.ingest_line(line) {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            })
            .await;
        }
    }
}

pub struct EaSportsFc27Source {
    log_path: PathBuf,
}

impl Default for EaSportsFc27Source {
    fn default() -> Self {
        Self::new(
            locate_fc27_log_path()
                .unwrap_or_else(|| PathBuf::from("EA SPORTS FC 27/logs/fc27_telemetry.log")),
        )
    }
}

impl EaSportsFc27Source {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
        }
    }
}

#[async_trait]
impl EventSource for EaSportsFc27Source {
    fn name(&self) -> &str {
        "fc27_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::EaSportsFc,
                FC27_EXECUTABLES,
            )
            .await;
        });

        let mut tracker = FootballTracker::new(FootballTitle::Fc27);
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(180));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            tail_lines_with(&self.log_path, &mut pos, false, |line| {
                for ev in tracker.ingest_line(line) {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            })
            .await;
        }
    }
}

pub struct EFootballSource {
    log_path: PathBuf,
}

impl Default for EFootballSource {
    fn default() -> Self {
        Self::new(
            locate_efootball_log_path()
                .unwrap_or_else(|| PathBuf::from("eFootball/Saved/Logs/eFootball.log")),
        )
    }
}

impl EFootballSource {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
        }
    }
}

#[async_trait]
impl EventSource for EFootballSource {
    fn name(&self) -> &str {
        "efootball_hybrid"
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let hud_sink = sink.clone();
        let hud_cancel = cancel.child_token();
        let hud_task = tokio::spawn(async move {
            run_hud_highlight_loop(
                hud_sink,
                hud_cancel,
                HudGameProfile::EFootball,
                EFOOTBALL_EXECUTABLES,
            )
            .await;
        });

        let mut tracker = FootballTracker::new(FootballTitle::EFootball);
        let mut pos: Option<u64> = None;
        let mut ticker = interval(Duration::from_millis(180));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = hud_task.await;
                    return Ok(());
                }
                _ = ticker.tick() => {}
            }

            tail_lines_with(&self.log_path, &mut pos, false, |line| {
                for ev in tracker.ingest_line(line) {
                    sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                }
            })
            .await;
        }
    }
}

pub fn locate_fc26_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let base = PathBuf::from(local_app_data).join("EA SPORTS FC 26");
        if let Some(f) = newest_file_in_dir(&base.join("logs")) {
            return Some(f);
        }
        return Some(base.join("logs/fc26_telemetry.log"));
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return Some(
            PathBuf::from(user_profile)
                .join("Documents/EA SPORTS FC 26/logs/fc26_telemetry.log"),
        );
    }
    None
}

pub fn locate_fc27_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let base = PathBuf::from(local_app_data).join("EA SPORTS FC 27");
        if let Some(f) = newest_file_in_dir(&base.join("logs")) {
            return Some(f);
        }
        return Some(base.join("logs/fc27_telemetry.log"));
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return Some(
            PathBuf::from(user_profile)
                .join("Documents/EA SPORTS FC 27/logs/fc27_telemetry.log"),
        );
    }
    None
}

pub fn locate_efootball_log_path() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data).join("eFootball/Saved/Logs/eFootball.log");
        if p.exists() {
            return Some(p);
        }
        return Some(p);
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return Some(
            PathBuf::from(user_profile)
                .join("Documents/KONAMI/eFootball/logs/eFootball.log"),
        );
    }
    None
}

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
    pub multikills: u32,
    pub mvps: u32,
    pub goals: u32,
    pub goals_conceded: u32,
    pub saves: u32,
    pub objectives: u32,
    pub favorite_map: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MonthlyStatsSummary {
    pub window_days: u32,
    pub generated_at_ms: u64,
    pub total_matches: u32,
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
    pub total_multikills: u32,
    pub total_mvps: u32,
    pub total_goals: u32,
    pub total_goals_conceded: u32,
    pub total_saves: u32,
    pub total_objectives: u32,
    pub per_game: HashMap<String, GamePeriodStats>,
    pub recent_matches: Vec<MatchStatsRecord>,
}

#[derive(Clone, Debug, Default)]
struct ActiveMatchAccumulator {
    game: String,
    map: String,
    mode: String,
    hero_or_team: String,
    started_at_ms: u64,
    kills: u32,
    deaths: u32,
    assists: u32,
    headshots: u32,
    multikills: u32,
    aces: u32,
    clutches: u32,
    mvps: u32,
    goals: u32,
    goals_conceded: u32,
    saves: u32,
    objectives: u32,
    score_for: Option<i64>,
    score_against: Option<i64>,
    won: Option<bool>,
    has_activity: bool,
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

fn compute_kd(kills: u32, deaths: u32) -> f64 {
    let k = kills as f64;
    let d = (deaths.max(1)) as f64;
    ((k / d) * 100.0).round() / 100.0
}

fn compute_win_rate(wins: u32, losses: u32) -> f64 {
    let decided = wins + losses;
    if decided == 0 {
        0.0
    } else {
        (((wins as f64) / (decided as f64)) * 1000.0).round() / 10.0
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
        let history = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<MatchStatsRecord>>(&s).ok())
            .unwrap_or_default();
        let seq = history.len() as u64;
        Self {
            active: HashMap::new(),
            history,
            storage_path: Some(p),
            seq,
        }
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
            headshots: acc.headshots,
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
        let game = ev.game.trim().to_lowercase();
        if game.is_empty() || game == "system" {
            return None;
        }
        let kind = ev.kind.as_str();
        let at_ms = if ev.at_ms > 0 { ev.at_ms } else { now_ms() };

        if kind == "match.start" {
            let mut prev_finished = None;
            if let Some(existing) = self.active.get(&game) {
                if existing.has_activity {
                    prev_finished = self.finalize_game(&game, at_ms, None);
                }
            }
            let mut acc = ActiveMatchAccumulator {
                game: game.clone(),
                started_at_ms: at_ms,
                ..Default::default()
            };
            self.apply_metadata(&mut acc, &ev.data);
            self.active.insert(game, acc);
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
                return self.finalize_game(&game, at_ms, won_opt);
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
            "presence.update" | "player_stats" | "match.map_load" => {
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

        None
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
            .or_else(|| data.get("team"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            acc.hero_or_team = ht.to_string();
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

    fn finalize_game(
        &mut self,
        game: &str,
        ended_at_ms: u64,
        explicit_won: Option<bool>,
    ) -> Option<MatchStatsRecord> {
        let acc = self.active.remove(game)?;
        let total_events = acc.kills
            + acc.deaths
            + acc.assists
            + acc.clutches
            + acc.aces
            + acc.goals
            + acc.goals_conceded
            + acc.saves
            + acc.objectives;
        if !acc.has_activity && total_events == 0 && acc.map.is_empty() {
            return None;
        }
        self.seq = self.seq.wrapping_add(1);
        let end_ms = ended_at_ms.max(acc.started_at_ms);
        let duration_secs = end_ms.saturating_sub(acc.started_at_ms) / 1000;
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

        let record = MatchStatsRecord {
            match_id: format!("{}_{}_{}", acc.game, end_ms, self.seq),
            game: acc.game,
            map: if acc.map.is_empty() {
                "Unknown".into()
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
            headshots: acc.headshots,
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
        let _ = self.save_to_disk();
        Some(record)
    }

    pub fn summary_for_days(&self, days: u32, reference_now_ms: u64) -> MonthlyStatsSummary {
        let window_days = days.max(1);
        let window_ms = (window_days as u64).saturating_mul(86_400_000);
        let cutoff_ms = reference_now_ms.saturating_sub(window_ms);

        let mut summary = MonthlyStatsSummary {
            window_days,
            generated_at_ms: reference_now_ms,
            ..Default::default()
        };

        let mut map_counts_by_game: HashMap<String, HashMap<String, u32>> = HashMap::new();

        for rec in self.history.iter().filter(|r| r.ended_at_ms >= cutoff_ms) {
            summary.total_matches += 1;
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
            summary.total_headshots += rec.headshots;
            summary.total_multikills += rec.multikills;
            summary.total_mvps += rec.mvps;
            summary.total_goals += rec.goals;
            summary.total_goals_conceded += rec.goals_conceded;
            summary.total_saves += rec.saves;
            summary.total_objectives += rec.objectives;

            let g_entry = summary
                .per_game
                .entry(rec.game.clone())
                .or_insert_with(|| GamePeriodStats {
                    game: rec.game.clone(),
                    ..Default::default()
                });
            g_entry.matches_played += 1;
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
            g_entry.headshots += rec.headshots;
            g_entry.multikills += rec.multikills;
            g_entry.mvps += rec.mvps;
            g_entry.goals += rec.goals;
            g_entry.goals_conceded += rec.goals_conceded;
            g_entry.saves += rec.saves;
            g_entry.objectives += rec.objectives;

            if !rec.map.is_empty() && rec.map != "Unknown" {
                let m_map = map_counts_by_game.entry(rec.game.clone()).or_default();
                *m_map.entry(rec.map.clone()).or_insert(0) += 1;
            }
        }

        summary.win_rate_pct = compute_win_rate(summary.wins, summary.losses);
        summary.overall_kd = compute_kd(summary.total_kills, summary.total_deaths);

        for (g_name, g_stats) in &mut summary.per_game {
            g_stats.win_rate_pct = compute_win_rate(g_stats.wins, g_stats.losses);
            g_stats.kd_ratio = compute_kd(g_stats.kills, g_stats.deaths);
            if let Some(m_counts) = map_counts_by_game.get(g_name) {
                if let Some((best_map, _)) = m_counts.iter().max_by_key(|(_, c)| *c) {
                    g_stats.favorite_map = best_map.clone();
                }
            }
        }

        summary.recent_matches = self
            .history
            .iter()
            .rev()
            .filter(|r| r.ended_at_ms >= cutoff_ms)
            .take(50)
            .cloned()
            .collect();

        summary
    }

    pub fn monthly_summary(&self) -> MonthlyStatsSummary {
        self.summary_for_days(30, now_ms())
    }
}