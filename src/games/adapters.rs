#![allow(dead_code, unused_imports)]
use std::collections::{HashMap, HashSet};
use std::io::{self, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use async_trait::async_trait;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::core::{LocalLauncherIdentity, persist_cached_launcher_identity};
use crate::tracker::event_bus::{
    now_ms, pause, Confidence, Event, EventReader, EventSink, EventSource, Registration,
    SourceError, MAX_BUFFER, MAX_HTTP_BODY, MAX_LINE_LEN,
};
use crate::games::definitions::*;
use crate::games::detection::*;
use crate::games::event_mapping::*;

fn compile_re(pat: &str) -> Regex {
    match Regex::new(pat) {
        Ok(r) => r,
        Err(_) => match Regex::new("a^") {
            Ok(f) => f,
            Err(_) => unreachable!(),
        },
    }
}

pub(crate) fn snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if c.is_uppercase() && prev.map_or(false, |p| p.is_lowercase() || p.is_ascii_digit()) {
            out.push('_');
        }
        out.extend(c.to_lowercase());
        prev = Some(c);
    }
    out
}

pub(crate) fn scalar(s: &str) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return json!(f);
    }
    match s {
        "true" | "True" => json!(true),
        "false" | "False" => json!(false),
        _ => json!(s),
    }
}

pub(crate) fn normalize(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(snake_case(&k), normalize(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(normalize).collect()),
        other => other,
    }
}

pub struct UnsupportedOfficialLocalSource {
    name: String,
    reason: String,
}

impl UnsupportedOfficialLocalSource {
    pub fn new(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[async_trait]
impl EventSource for UnsupportedOfficialLocalSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_supported(&self) -> bool {
        false
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        sink.emit_detailed(
            "source.unsupported",
            None,
            Confidence::Unavailable,
            Some("unsupported_notice".into()),
            json!({
                "reason": self.reason,
                "supported": false,
            }),
        );
        cancel.cancelled().await;
        Ok(())
    }
}

#[deprecated(
    note = "Passive process detection does not extract game events. Use a real EventSource or UnsupportedOfficialLocalSource."
)]
pub struct Passive;

#[allow(deprecated)]
#[async_trait]
impl EventSource for Passive {
    fn name(&self) -> &str {
        "passive"
    }

    fn is_supported(&self) -> bool {
        false
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        sink.emit_detailed(
            "source.unsupported",
            None,
            Confidence::Unavailable,
            Some("passive_deprecated".into()),
            json!({ "reason": "Passive process watcher does not provide game telemetry" }),
        );
        cancel.cancelled().await;
        Ok(())
    }
}

#[derive(Clone)]
pub struct TailRule {
    pub(crate) kind: String,
    pub(crate) pattern: Regex,
    pub(crate) confidence: Confidence,
}

#[derive(Clone)]
pub struct TailSource {
    name: String,
    path: PathBuf,
    rules: Vec<TailRule>,
    from_start: bool,
    poll: Duration,
}

impl TailSource {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            rules: Vec::new(),
            from_start: false,
            poll: Duration::from_millis(150),
        }
    }

    pub fn rule(self, kind: impl Into<String>, pattern: &str) -> Result<Self, SourceError> {
        self.rule_with_confidence(kind, pattern, Confidence::Confirmed)
    }

    pub fn rule_with_confidence(
        mut self,
        kind: impl Into<String>,
        pattern: &str,
        confidence: Confidence,
    ) -> Result<Self, SourceError> {
        self.rules.push(TailRule {
            kind: kind.into(),
            pattern: Regex::new(pattern)?,
            confidence,
        });
        Ok(self)
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
        &self.path
    }

    pub(crate) fn dispatch(&self, sink: &EventSink, line: &str) {
        if line.len() > MAX_LINE_LEN {
            sink.emit(
                "source.warning",
                None,
                json!({ "reason": "line_exceeded_max_length", "len": line.len() }),
            );
            return;
        }
        for rule in &self.rules {
            let Some(caps) = rule.pattern.captures(line) else {
                continue;
            };
            let mut data = Map::new();
            for name in rule.pattern.capture_names().flatten() {
                if let Some(m) = caps.name(name) {
                    data.insert(name.to_owned(), scalar(m.as_str()));
                }
            }
            data.insert("line".into(), json!(line));
            sink.emit_detailed(
                rule.kind.clone(),
                None,
                rule.confidence,
                None,
                Value::Object(data),
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    pub(crate) created_ms: Option<u128>,
}

impl FileIdentity {
    pub(crate) fn from_meta(meta: &std::fs::Metadata) -> Self {
        let created_ms = meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis());
        Self { created_ms }
    }
}

#[cfg(windows)]
pub async fn open_shared_read(path: &Path) -> std::io::Result<tokio::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| std::io::Error::from_raw_os_error(e.code().0))?;

    let std_file = unsafe { std::fs::File::from_raw_handle(handle.0) };
    Ok(tokio::fs::File::from_std(std_file))
}

#[cfg(not(windows))]
pub async fn open_shared_read(path: &Path) -> std::io::Result<tokio::fs::File> {
    tokio::fs::File::open(path).await
}

#[async_trait]
impl EventSource for TailSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError> {
        let mut pos: Option<u64> = None;
        let mut identity: Option<FileIdentity> = None;
        let mut carry: Vec<u8> = Vec::new();
        let mut discarding_oversized_line = false;
        let mut ticker = interval(self.poll);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = ticker.tick() => {}
            }
            let Ok(meta) = tokio::fs::metadata(&self.path).await else {
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
            let Ok(mut file) = open_shared_read(&self.path).await else {
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
                let text = String::from_utf8_lossy(&raw);
                let line = text.trim_end_matches(&['\r', '\n'][..]);
                if !line.is_empty() {
                    self.dispatch(&sink, line);
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

fn format_valorant_shipping_version(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains("-shipping-") {
        return trimmed.to_string();
    }
    let re = compile_re(r"^(release-[0-9]+\.[0-9]+)-([0-9]+-[0-9]+)$");
    if let Some(caps) = re.captures(trimmed) {
        let p1 = caps.get(1).map_or("", |m| m.as_str());
        let p2 = caps.get(2).map_or("", |m| m.as_str());
        if !p1.is_empty() && !p2.is_empty() {
            return format!("{p1}-shipping-{p2}");
        }
    }
    trimmed.to_string()
}


fn extract_valorant_shard_and_version_from_log() -> (String, String) {
    let (s, v, _, _) = extract_valorant_log_metadata();
    (s, v)
}


pub(crate) async fn fetch_riot_local_session_identity(
    client: &reqwest::Client,
    port: u16,
    password: &str,
) -> (Option<String>, Option<String>) {
    let mut puuid_out = None;
    let mut riot_id_out = None;

    for ep in [
        format!("https://127.0.0.1:{port}/chat/v1/session"),
        format!("https://127.0.0.1:{port}/player-account/aliases/v1/active"),
        format!("https://127.0.0.1:{port}/rso-auth/v1/authorization/userinfo"),
    ] {
        if let Ok(resp) = client
            .get(&ep)
            .basic_auth("riot", Some(password))
            .send()
            .await
        {
            if let Ok(body) = resp.json::<Value>().await {
                if puuid_out.is_none() {
                    if let Some(p) = body
                        .get("puuid")
                        .or_else(|| body.get("sub"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        puuid_out = Some(p.to_string());
                    }
                }
                if riot_id_out.is_none() {
                    let g_name = body
                        .get("game_name")
                        .or_else(|| body.get("gameName"))
                        .or_else(|| body.pointer("/acct/game_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    let g_tag = body
                        .get("game_tag")
                        .or_else(|| body.get("tag_line"))
                        .or_else(|| body.get("tagLine"))
                        .or_else(|| body.pointer("/acct/tag_line"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !g_name.is_empty() {
                        riot_id_out = Some(if g_tag.is_empty() {
                            g_name.to_string()
                        } else {
                            format!("{g_name}#{g_tag}")
                        });
                    }
                }
            }
        }
    }

    if puuid_out.is_none() {
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
                    puuid_out = Some(sub.to_string());
                }
            }
        }
    }

    if riot_id_out.is_none() {
        let (shard, _) = extract_valorant_shard_and_version_from_log();
        if let (Some(ref puuid), false) = (&puuid_out, shard.is_empty()) {
            let ns_url = format!("https://pd.{shard}.a.pvp.net/name-service/v2/players");
            if let Ok(resp) = client.put(&ns_url).json(& serde_json::json!([puuid])).send().await {
                if let Ok(arr) = resp.json::<Vec<Value>>().await {
                    if let Some(first) = arr.first() {
                        let gn = first.get("GameName").and_then(Value::as_str).unwrap_or("").trim();
                        let tl = first.get("TagLine").and_then(Value::as_str).unwrap_or("").trim();
                        if !gn.is_empty() {
                            riot_id_out = Some(if tl.is_empty() {
                                gn.to_string()
                            } else {
                                format!("{gn}#{tl}")
                            });
                        }
                    }
                }
            }
        }
    }

    if let Some(ref rid) = riot_id_out {
        persist_cached_launcher_identity(LocalLauncherIdentity {
            launcher: "Riot Games".to_string(),
            display_id: rid.clone(),
            account_id: puuid_out.clone().unwrap_or_default(),
            region_or_tag: "Verified Lockfile".to_string(),
            online_status: "Connected".to_string(),
        });
    }
    (puuid_out, riot_id_out)
}

pub(crate) async fn fetch_valorant_recent_match_details_local(
    client: &reqwest::Client,
    port: u16,
    password: &str,
    puuid: &str,
    riot_id: Option<&str>,
) -> Vec<Value> {
    let mut results = Vec::new();
    let ent_url = format!("https://127.0.0.1:{port}/entitlements/v1/token");
    let Ok(ent_resp) = client
        .get(&ent_url)
        .basic_auth("riot", Some(password))
        .send()
        .await
    else {
        return results;
    };
    let Ok(ent_json) = ent_resp.json::<Value>().await else {
        return results;
    };
    let Some(access_token) = ent_json.get("accessToken").and_then(Value::as_str) else {
        return results;
    };
    let Some(ent_token) = ent_json.get("token").and_then(Value::as_str) else {
        return results;
    };

    let (mut shard, mut client_ver, _, log_match_ids) = extract_valorant_log_metadata();
    if shard.is_empty() {
        let ext_url = format!("https://127.0.0.1:{port}/product-session/v1/external-sessions");
        if let Ok(r) = client.get(&ext_url).basic_auth("riot", Some(password)).send().await {
            if let Ok(v) = r.json::<Value>().await {
                let s_txt = v.to_string();
                let dep_re = compile_re(r"-ares-deployment=([a-z0-9]+)");
                if let Some(c) = dep_re.captures(&s_txt).and_then(|c| c.get(1)) {
                    let reg = c.as_str();
                    shard = match reg {
                        "latam" | "br" | "na" | "pbe" => "na".to_string(),
                        "eu" | "tr" | "mena" => "eu".to_string(),
                        "kr" => "kr".to_string(),
                        _ => "ap".to_string(),
                    };
                }
            }
        }
    }
    if shard.is_empty() {
        shard = "eu".to_string();
    }
    if client_ver.is_empty() || !client_ver.contains("-shipping-") {
        if let Ok(v_resp) = client.get("https://valorant-api.com/v1/version").send().await {
            if let Ok(v_json) = v_resp.json::<Value>().await {
                if let Some(rcv) = v_json.pointer("/data/riotClientVersion").and_then(Value::as_str) {
                    client_ver = rcv.to_string();
                }
            }
        }
    }

    let platform_b64 = "ew0KCSJwbGF0Zm9ybVR5cGUiOiAiUEMiLA0KCSJwbGF0Zm9ybU9TIjogIldpbmRvd3MiLA0KCSJwbGF0Zm9ybU9TVmVyc2lvbiI6ICIxMC4wLjE5MDQyLjEuMjU2LjY0Yml0IiwNCgkicGxhdGZvcm1DaGlwc2V0IjogIlVua25vd24iDQp9";
    let mut candidate_match_ids: Vec<String> = Vec::new();

    let hist_url = format!(
        "https://pd.{shard}.a.pvp.net/match-history/v1/history/{puuid}?startIndex=0&endIndex=15"
    );
    if let Ok(hist_resp) = client
        .get(&hist_url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("X-Riot-Entitlements-JWT", ent_token)
        .header("X-Riot-ClientPlatform", platform_b64)
        .header("X-Riot-ClientVersion", &client_ver)
        .send()
        .await
    {
        if let Ok(hist_json) = hist_resp.json::<Value>().await {
            if let Some(history_arr) = hist_json.get("History").and_then(Value::as_array) {
                for item in history_arr.iter().take(12) {
                    if let Some(mid) = item.get("MatchID").and_then(Value::as_str) {
                        if !candidate_match_ids.contains(&mid.to_string()) {
                            candidate_match_ids.push(mid.to_string());
                        }
                    }
                }
            }
        }
    }

    for mid in log_match_ids {
        if !candidate_match_ids.contains(&mid) {
            candidate_match_ids.push(mid);
        }
    }

    for match_id in candidate_match_ids.into_iter().take(12) {
        let det_url = format!("https://pd.{shard}.a.pvp.net/match-details/v1/matches/{match_id}");
        let Ok(det_resp) = client
            .get(&det_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("X-Riot-Entitlements-JWT", ent_token)
            .header("X-Riot-ClientPlatform", platform_b64)
            .header("X-Riot-ClientVersion", &client_ver)
            .send()
            .await
        else {
            continue;
        };
        let Ok(det) = det_resp.json::<Value>().await else {
            continue;
        };
        let Some(players) = det.get("players").and_then(Value::as_array) else {
            continue;
        };
        let Some(me) = players
            .iter()
            .find(|p| p.get("subject").and_then(Value::as_str) == Some(puuid))
        else {
            continue;
        };
        let raw_map = det
            .pointer("/matchInfo/mapId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let map_name = ValorantTracker::normalize_valorant_map(raw_map).unwrap_or("Standard");
        let queue_id = det
            .pointer("/matchInfo/queueID")
            .and_then(Value::as_str)
            .unwrap_or("competitive");
        let start_ms = det
            .pointer("/matchInfo/gameStartMillis")
            .and_then(Value::as_u64)
            .unwrap_or_else(now_ms);
        let len_ms = det
            .pointer("/matchInfo/gameLengthMillis")
            .and_then(Value::as_u64)
            .unwrap_or(1_800_000);
        let char_id = me.get("characterId").and_then(Value::as_str).unwrap_or("");
        let agent = ValorantTracker::normalize_valorant_agent(char_id).unwrap_or("");
        let team_id = me.get("teamId").and_then(Value::as_str).unwrap_or("Blue");
        let kills = me.pointer("/stats/kills").and_then(Value::as_u64).unwrap_or(0) as u32;
        let deaths = me.pointer("/stats/deaths").and_then(Value::as_u64).unwrap_or(0) as u32;
        let assists = me.pointer("/stats/assists").and_then(Value::as_u64).unwrap_or(0) as u32;
        if kills == 0 && deaths == 0 && assists == 0 {
            continue;
        }
        let score = me.pointer("/stats/score").and_then(Value::as_u64).unwrap_or(0) as u32;
        let rounds_played = me
            .pointer("/stats/roundsPlayed")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as u32;
        let acs = score / rounds_played;

        let mut headshots = 0u32;
        let mut bodyshots = 0u32;
        let mut legshots = 0u32;
        let mut damage_dealt = 0u32;
        let mut multikills = 0u32;
        let mut aces = 0u32;
        let mut clutches = 0u32;
        let mut weapon_counts: HashMap<&'static str, u32> = HashMap::new();

        if let Some(rounds) = det.get("roundResults").and_then(Value::as_array) {
            for r in rounds {
                let winning_team = r.get("winningTeam").and_then(Value::as_str).unwrap_or("");
                let round_won = winning_team.eq_ignore_ascii_case(team_id);
                if let Some(pstats) = r.get("playerStats").and_then(Value::as_array) {
                    if let Some(my_round) = pstats
                        .iter()
                        .find(|ps| ps.get("subject").and_then(Value::as_str) == Some(puuid))
                    {
                        if let Some(dmg_arr) = my_round.get("damage").and_then(Value::as_array) {
                            for d in dmg_arr {
                                headshots += d.get("headshots").and_then(Value::as_u64).unwrap_or(0) as u32;
                                bodyshots += d.get("bodyshots").and_then(Value::as_u64).unwrap_or(0) as u32;
                                legshots += d.get("legshots").and_then(Value::as_u64).unwrap_or(0) as u32;
                                damage_dealt += d.get("damage").and_then(Value::as_u64).unwrap_or(0) as u32;
                            }
                        }
                        if let Some(k_arr) = my_round.get("kills").and_then(Value::as_array) {
                            let rk = k_arr.len() as u32;
                            if rk >= 2 {
                                multikills += 1;
                            }
                            if rk >= 5 {
                                aces += 1;
                            }
                            if round_won && rk >= 2 {
                                clutches += 1;
                            }
                            for k_ev in k_arr {
                                if let Some(item_id) = k_ev
                                    .pointer("/finishingDamage/damageItem")
                                    .and_then(Value::as_str)
                                {
                                    if let Some(w_name) = ValorantTracker::normalize_valorant_weapon(item_id) {
                                        *weapon_counts.entry(w_name).or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let fav_weapon = weapon_counts
            .into_iter()
            .max_by_key(|(_, c)| *c)
            .map(|(w, _)| w.to_string())
            .unwrap_or_default();

        let mut won = None;
        let mut ally_score = None;
        let mut enemy_score = None;
        if let Some(teams) = det.get("teams").and_then(Value::as_array) {
            for t in teams {
                let tid = t.get("teamId").and_then(Value::as_str).unwrap_or("");
                let r_won = t.get("roundsWon").and_then(Value::as_i64);
                if tid.eq_ignore_ascii_case(team_id) {
                    won = t.get("won").and_then(Value::as_bool);
                    ally_score = r_won;
                } else {
                    enemy_score = r_won;
                }
            }
        }

        let g_name = me.get("gameName").and_then(Value::as_str).unwrap_or("").trim();
        let g_tag = me.get("tagLine").and_then(Value::as_str).unwrap_or("").trim();
        let player_id_str = if !g_name.is_empty() {
            let full_id = if g_tag.is_empty() {
                g_name.to_string()
            } else {
                format!("{g_name}#{g_tag}")
            };
            persist_cached_launcher_identity(LocalLauncherIdentity {
                launcher: "Riot Games".to_string(),
                display_id: full_id.clone(),
                account_id: puuid.to_string(),
                region_or_tag: shard.to_uppercase(),
                online_status: "Verified Match Telemetry".to_string(),
            });
            full_id
        } else {
            riot_id.unwrap_or("").to_string()
        };

        results.push(json!({
            "match_id": format!("valorant_{match_id}"),
            "game": "valorant",
            "map": map_name,
            "mode": queue_id,
            "hero": agent,
            "player_identity": player_id_str,
            "favorite_weapon": fav_weapon,
            "started_at_ms": start_ms,
            "ended_at_ms": start_ms.saturating_add(len_ms),
            "duration_secs": len_ms / 1000,
            "kills": kills,
            "deaths": deaths,
            "assists": assists,
            "headshots": headshots,
            "bodyshots": bodyshots,
            "legshots": legshots,
            "damage_dealt": damage_dealt,
            "combat_score": acs,
            "multikills": multikills,
            "aces": aces,
            "clutches": clutches,
            "won": won,
            "ally_score": ally_score,
            "enemy_score": enemy_score,
        }));
    }
    results
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
    let mut local_riot_id: Option<String> = None;
    let mut synced_initial_history = false;

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
            let (puuid, rid) = fetch_riot_local_session_identity(&client, cached_port, &cached_password).await;
            local_puuid = puuid;
            if rid.is_some() {
                local_riot_id = rid;
            }
        }
        if !synced_initial_history {
            if let Some(ref my_puuid) = local_puuid {
                synced_initial_history = true;
                for match_val in fetch_valorant_recent_match_details_local(
                    &client,
                    cached_port,
                    &cached_password,
                    my_puuid,
                    local_riot_id.as_deref(),
                )
                .await
                {
                    sink.emit("match.official_details", None, match_val);
                }
            }
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
                            "player_identity": local_riot_id,
                            "via": "riot_presence",
                        }),
                    );
                    if let Some(ref my_puuid) = local_puuid {
                        for match_val in fetch_valorant_recent_match_details_local(
                            &client,
                            cached_port,
                            &cached_password,
                            my_puuid,
                            local_riot_id.as_deref(),
                        )
                        .await
                        {
                            sink.emit("match.official_details", None, match_val);
                        }
                    }
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

pub fn read_shared_nv12_hud_metrics(norm_cx: f32, norm_cy: f32) -> Option<RoiMetrics> {
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
        let mut ticker = interval(Duration::from_millis(250));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let procs = ["valorant-win64-shipping.exe", "valorant.exe"];
        let mut ema_luma = 0.0f32;
        let mut ema_edge = 0.0f32;
        let mut initialized = false;
        let mut banner_active = false;
        let mut cooldown_until: Option<Instant> = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }

            let in_active_match = tracker
                .lock()
                .map(|t| t.combat.in_match && t.combat.round_kills < 5)
                .unwrap_or(false);
            if !in_active_match {
                banner_active = false;
                continue;
            }

            let Some(m) = sampler.sample_foreground(&procs, 0.50, 0.812) else {
                banner_active = false;
                continue;
            };

            if m.border_bright_ratio > 0.40 || m.border_luma > 165.0 {
                banner_active = false;
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

            let emblem_present = m.red_hit_arms >= 4
                && contrast > 55.0
                && m.edge_density > 0.28
                && luma_rise > 38.0
                && edge_rise > 0.10
                && m.gold_teal_ratio > 0.22;

            if emblem_present {
                let now = Instant::now();
                let cooldown_ok = cooldown_until.map_or(true, |until| now >= until);
                if !banner_active && cooldown_ok {
                    banner_active = true;
                    cooldown_until = Some(now + Duration::from_millis(2400));
                    let events = match tracker.lock() {
                        Ok(mut t) => t.combat.record_kill(false, "hud_banner", now),
                        Err(p) => p.into_inner().combat.record_kill(false, "hud_banner", now),
                    };
                    for ev in events {
                        sink.emit_detailed(ev.kind, None, ev.confidence, ev.dedup_key, ev.data);
                    }
                }
            } else {
                banner_active = false;
                ema_luma = ema_luma * 0.90 + m.center_luma * 0.10;
                ema_edge = ema_edge * 0.90 + m.edge_density * 0.10;
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
