use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sysinfo::{ProcessRefreshKind, System, UpdateKind};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

pub(crate) const BACKOFF_MIN: Duration = Duration::from_millis(500);
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(15);
pub(crate) const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
pub(crate) const MAX_BUFFER: usize = 4 * 1024 * 1024;
pub(crate) const MAX_LINE_LEN: usize = 64 * 1024;
pub(crate) const MAX_HTTP_BODY: usize = 512 * 1024;
pub(crate) const DEDUP_WINDOW_SIZE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Confirmed,
    Inferred,
    Unavailable,
}

impl Default for Confidence {
    fn default() -> Self {
        Self::Confirmed
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub seq: u64,
    pub dedup_key: Option<String>,
    pub at_ms: u64,
    pub game: Arc<str>,
    pub source: Arc<str>,
    pub kind: String,
    pub game_time: Option<f64>,
    pub confidence: Confidence,
    pub data: Value,
    pub pid: Option<u32>,
}

struct DedupWindow {
    set: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl DedupWindow {
    fn new(capacity: usize) -> Self {
        Self {
            set: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity: capacity.max(16),
        }
    }

    fn check_and_insert(&mut self, key: &str) -> bool {
        if self.set.contains(key) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        let owned = key.to_owned();
        self.set.insert(owned.clone());
        self.order.push_back(owned);
        true
    }
}

pub(crate) struct BusState {
    seq: u64,
    dedup: DedupWindow,
    live_pids: HashMap<Arc<str>, u32>,
}

pub(crate) struct Bus {
    tx: broadcast::Sender<Event>,
    state: Mutex<BusState>,
}

impl Bus {
    fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            tx,
            state: Mutex::new(BusState {
                seq: 0,
                dedup: DedupWindow::new(DEDUP_WINDOW_SIZE),
                live_pids: HashMap::new(),
            }),
        }
    }
}

#[derive(Clone)]
pub struct EventSink {
    bus: Arc<Bus>,
    game: Arc<str>,
    source: Arc<str>,
}

impl EventSink {
    pub fn emit(&self, kind: impl Into<String>, game_time: Option<f64>, data: Value) {
        self.emit_detailed_with_pid(kind, game_time, Confidence::Confirmed, None, data, None);
    }

    pub fn emit_with_pid(
        &self,
        kind: impl Into<String>,
        pid: Option<u32>,
        game_time: Option<f64>,
        data: Value,
    ) {
        self.emit_detailed_with_pid(kind, game_time, Confidence::Confirmed, None, data, pid);
    }

    pub fn emit_inferred(&self, kind: impl Into<String>, game_time: Option<f64>, data: Value) {
        self.emit_detailed_with_pid(kind, game_time, Confidence::Inferred, None, data, None);
    }

    pub fn emit_detailed(
        &self,
        kind: impl Into<String>,
        game_time: Option<f64>,
        confidence: Confidence,
        dedup_key: Option<String>,
        data: Value,
    ) {
        self.emit_detailed_with_pid(kind, game_time, confidence, dedup_key, data, None);
    }

    pub fn emit_detailed_with_pid(
        &self,
        kind: impl Into<String>,
        game_time: Option<f64>,
        confidence: Confidence,
        dedup_key: Option<String>,
        data: Value,
        pid: Option<u32>,
    ) {
        let kind_str = kind.into();
        let mut state = match self.bus.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(ref key) = dedup_key {
            let scoped_key = format!("{}:{}:{}", self.game, kind_str, key);
            if !state.dedup.check_and_insert(&scoped_key) {
                return;
            }
        }
        let seq = state.seq;
        state.seq = state.seq.wrapping_add(1);
        let _ = self.bus.tx.send(Event {
            seq,
            dedup_key,
            at_ms: now_ms(),
            game: self.game.clone(),
            source: self.source.clone(),
            kind: kind_str,
            game_time,
            confidence,
            data,
            pid,
        });
    }

    pub fn game(&self) -> &str {
        &self.game
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Pattern(#[from] regex::Error),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

#[async_trait]
pub trait EventSource: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn is_supported(&self) -> bool {
        true
    }
    async fn run(&self, sink: EventSink, cancel: CancellationToken) -> Result<(), SourceError>;
}

#[derive(Clone)]
pub struct Registration {
    game: Arc<str>,
    processes: Vec<String>,
    source: Arc<dyn EventSource>,
}

impl Registration {
    pub fn new(game: impl Into<Arc<str>>, source: impl EventSource) -> Self {
        Self {
            game: game.into(),
            processes: Vec::new(),
            source: Arc::new(source),
        }
    }

    pub fn when_running<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.processes = names
            .into_iter()
            .map(|n| n.as_ref().to_lowercase())
            .collect();
        self
    }

    pub fn game(&self) -> &str {
        &self.game
    }

    pub fn source_name(&self) -> &str {
        self.source.name()
    }

    pub fn is_supported(&self) -> bool {
        self.source.is_supported()
    }
}

pub(crate) struct Slot {
    pub(crate) reg: Registration,
    pub(crate) task: Option<(CancellationToken, JoinHandle<()>)>,
}

pub struct EventReader {
    pub(crate) bus: Arc<Bus>,
    registrations: Vec<Registration>,
    scan_interval: Duration,
}

impl EventReader {
    pub fn new(capacity: usize) -> Self {
        Self {
            bus: Arc::new(Bus::new(capacity)),
            registrations: Vec::new(),
            scan_interval: Duration::from_millis(2500),
        }
    }

    pub fn scan_interval(&mut self, every: Duration) -> &mut Self {
        self.scan_interval = every.max(Duration::from_millis(250));
        self
    }

    pub fn register(&mut self, registration: Registration) -> &mut Self {
        self.registrations.push(registration);
        self
    }

    pub fn registrations(&self) -> &[Registration] {
        &self.registrations
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.bus.tx.subscribe()
    }

    pub fn sink(&self, game: impl Into<Arc<str>>, source: impl Into<Arc<str>>) -> EventSink {
        EventSink {
            bus: self.bus.clone(),
            game: game.into(),
            source: source.into(),
        }
    }

    pub fn spawn(self, cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(self.run(cancel))
    }

    async fn run(self, cancel: CancellationToken) {
        let Self {
            bus,
            registrations,
            scan_interval,
        } = self;

        let mut watched_names = HashSet::new();
        for reg in &registrations {
            for proc_name in &reg.processes {
                let lower = proc_name.trim().to_lowercase();
                if !lower.is_empty() {
                    if let Some(stripped) = lower.strip_suffix(".exe") {
                        watched_names.insert(stripped.to_string());
                    } else {
                        watched_names.insert(format!("{lower}.exe"));
                    }
                    watched_names.insert(lower);
                }
            }
        }

        let gated = !watched_names.is_empty();
        let mut watcher_rx: Option<watch::Receiver<HashMap<String, u32>>> = None;
        let mut watcher_handle: Option<std::thread::JoinHandle<()>> = None;

        if gated {
            match spawn_process_watcher(scan_interval, watched_names, cancel.clone()) {
                Ok((rx, handle)) => {
                    watcher_rx = Some(rx);
                    watcher_handle = Some(handle);
                }
                Err(e) => {
                    EventSink {
                        bus: bus.clone(),
                        game: Arc::from("system"),
                        source: Arc::from("reader"),
                    }
                    .emit(
                        "reader.error",
                        None,
                        json!({ "message": format!("failed to spawn process watcher: {e}") }),
                    );
                }
            }
        }

        let mut slots: Vec<Slot> = registrations
            .into_iter()
            .map(|reg| Slot { reg, task: None })
            .collect();
        let mut live: HashSet<Arc<str>> = HashSet::new();

        reconcile(&bus, &mut slots, &snapshot(&watcher_rx), &mut live, &cancel).await;

        loop {
            let alive = tokio::select! {
                _ = cancel.cancelled() => break,
                ok = changed(&mut watcher_rx) => ok,
            };
            if !alive {
                watcher_rx = None;
            }
            reconcile(&bus, &mut slots, &snapshot(&watcher_rx), &mut live, &cancel).await;
        }

        for slot in &slots {
            if let Some((token, _)) = &slot.task {
                token.cancel();
            }
        }
        for slot in slots {
            if let Some((_, mut handle)) = slot.task {
                if timeout(SHUTDOWN_GRACE, &mut handle).await.is_err() {
                    handle.abort();
                }
            }
        }
        if let Some(handle) = watcher_handle {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            })
            .await;
        }
    }
}

async fn changed(rx: &mut Option<watch::Receiver<HashMap<String, u32>>>) -> bool {
    match rx {
        Some(r) => r.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

fn snapshot(rx: &Option<watch::Receiver<HashMap<String, u32>>>) -> HashMap<String, u32> {
    rx.as_ref().map(|r| r.borrow().clone()).unwrap_or_default()
}

pub(crate) async fn reconcile(
    bus: &Arc<Bus>,
    slots: &mut [Slot],
    procs: &HashMap<String, u32>,
    live: &mut HashSet<Arc<str>>,
    parent: &CancellationToken,
) {
    let mut active_games: HashSet<Arc<str>> = HashSet::new();
    let mut game_pids: HashMap<Arc<str>, u32> = HashMap::new();
    for slot in slots.iter_mut() {
        let gated = !slot.reg.processes.is_empty();
        let target_pid = slot.reg.processes.iter().find_map(|p| procs.get(p).copied());
        let active = !gated || target_pid.is_some();
        if gated && active {
            active_games.insert(slot.reg.game.clone());
            if let Some(pid) = target_pid {
                game_pids.insert(slot.reg.game.clone(), pid);
            }
        }

        if let Some((_, ref handle)) = slot.task {
            if handle.is_finished() {
                if let Some((_, finished_handle)) = slot.task.take() {
                    let _ = finished_handle.await;
                }
            }
        }

        match (active, slot.task.is_some()) {
            (true, false) => {
                let token = parent.child_token();
                let sink = EventSink {
                    bus: bus.clone(),
                    game: slot.reg.game.clone(),
                    source: Arc::from(slot.reg.source.name()),
                };
                let handle = tokio::spawn(supervise(slot.reg.source.clone(), sink, token.clone()));
                slot.task = Some((token, handle));
            }
            (false, true) => {
                if let Some((token, mut handle)) = slot.task.take() {
                    token.cancel();
                    if timeout(SHUTDOWN_GRACE, &mut handle).await.is_err() {
                        handle.abort();
                        let _ = handle.await;
                    }
                }
            }
            _ => {}
        }
    }

    let started: Vec<Arc<str>> = active_games.difference(live).cloned().collect();
    let stopped: Vec<Arc<str>> = live.difference(&active_games).cloned().collect();
    let continuing: Vec<Arc<str>> = active_games.intersection(live).cloned().collect();

    for game in started {
        let pid = game_pids.get(&game).copied();
        if let Some(p) = pid {
            if let Ok(mut st) = bus.state.lock() {
                st.live_pids.insert(game.clone(), p);
            }
        }
        let data = if let Some(p) = pid {
            json!({ "pid": p })
        } else {
            Value::Null
        };
        EventSink {
            bus: bus.clone(),
            game,
            source: Arc::from("reader"),
        }
        .emit_with_pid("game.started", pid, None, data);
    }

    for game in continuing {
        if let Some(&new_pid) = game_pids.get(&game) {
            let changed_pid = match bus.state.lock() {
                Ok(mut st) => {
                    let prev = st.live_pids.insert(game.clone(), new_pid);
                    prev != Some(new_pid)
                }
                Err(_) => false,
            };
            if changed_pid {
                EventSink {
                    bus: bus.clone(),
                    game,
                    source: Arc::from("reader"),
                }
                .emit_with_pid(
                    "game.pid_updated",
                    Some(new_pid),
                    None,
                    json!({ "pid": new_pid }),
                );
            }
        }
    }

    for game in stopped {
        if let Ok(mut st) = bus.state.lock() {
            st.live_pids.remove(&game);
        }
        EventSink {
            bus: bus.clone(),
            game,
            source: Arc::from("reader"),
        }
        .emit("game.stopped", None, Value::Null);
    }
    *live = active_games;
}

async fn supervise(source: Arc<dyn EventSource>, sink: EventSink, cancel: CancellationToken) {
    let mut delay = BACKOFF_MIN;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let started = Instant::now();
        let outcome = {
            let run = source.run(sink.clone(), cancel.clone());
            tokio::pin!(run);
            tokio::select! {
                r = &mut run => r,
                _ = cancel.cancelled() => {
                    let _ = timeout(SHUTDOWN_GRACE, &mut run).await;
                    return;
                }
            }
        };
        if cancel.is_cancelled() {
            return;
        }
        match outcome {
            Ok(()) => {
                if !source.is_supported() {
                    return;
                }
                sink.emit(
                    "source.stopped_unexpectedly",
                    None,
                    json!({ "restarting": true }),
                );
            }
            Err(SourceError::Unsupported(reason)) => {
                sink.emit_detailed(
                    "source.unsupported",
                    None,
                    Confidence::Unavailable,
                    Some("unsupported_notice".into()),
                    json!({ "reason": reason }),
                );
                cancel.cancelled().await;
                return;
            }
            Err(e) => {
                sink.emit("source.error", None, json!({ "message": e.to_string() }));
            }
        }
        if started.elapsed() > BACKOFF_RESET_AFTER {
            delay = BACKOFF_MIN;
        }
        if !pause(&cancel, delay).await {
            return;
        }
        delay = (delay * 2).min(BACKOFF_MAX);
    }
}

pub(crate) async fn pause(cancel: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = sleep(d) => true,
    }
}

fn spawn_process_watcher(
    every: Duration,
    watched_names: HashSet<String>,
    cancel: CancellationToken,
) -> io::Result<(
    watch::Receiver<HashMap<String, u32>>,
    std::thread::JoinHandle<()>,
)> {
    let mut system = System::new();
    let (tx, rx) = watch::channel(scan(&mut system, &watched_names));
    let signal = Arc::new((Mutex::new(false), Condvar::new()));
    let signal_clone = signal.clone();
    let cancel_for_signal = cancel.clone();

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            cancel_for_signal.cancelled().await;
            let (lock, cvar) = &*signal_clone;
            if let Ok(mut stopped) = lock.lock() {
                *stopped = true;
                cvar.notify_all();
            }
        });
    }

    let handle = std::thread::Builder::new()
        .name("skirrmish-process-scan".into())
        .spawn(move || {
            let (lock, cvar) = &*signal;
            while !cancel.is_cancelled() && !tx.is_closed() {
                if let Ok(guard) = lock.lock() {
                    if *guard || cancel.is_cancelled() {
                        break;
                    }
                    let _ = cvar.wait_timeout_while(guard, every, |stopped| {
                        !*stopped && !cancel.is_cancelled()
                    });
                } else {
                    break;
                }
                if cancel.is_cancelled() || tx.is_closed() {
                    break;
                }
                let found = scan(&mut system, &watched_names);
                tx.send_if_modified(|cur| {
                    if *cur == found {
                        false
                    } else {
                        *cur = found;
                        true
                    }
                });
            }
        })?;
    Ok((rx, handle))
}

#[cfg(windows)]
fn query_process_name_windows(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::core::PWSTR;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let res = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if res.is_ok() && size > 0 {
            let path_str = String::from_utf16_lossy(&buf[..size as usize]);
            let name = std::path::Path::new(&path_str).file_name()?.to_str()?.to_string();
            return Some(name);
        }
    }
    None
}

#[cfg(windows)]
fn windowed_pids_windows() -> HashSet<u32> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowThreadProcessId, IsWindowVisible,
    };

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let set = unsafe { &mut *(lparam.0 as *mut HashSet<u32>) };
        unsafe {
            if IsWindowVisible(hwnd).as_bool() {
                let mut rect = RECT::default();
                if GetClientRect(hwnd, &mut rect).is_ok()
                    && (rect.right - rect.left) >= 320
                    && (rect.bottom - rect.top) >= 240
                {
                    let mut pid = 0u32;
                    GetWindowThreadProcessId(hwnd, Some(&mut pid));
                    if pid != 0 {
                        set.insert(pid);
                    }
                }
            }
        }
        BOOL(1)
    }

    let mut set = HashSet::new();
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut set as *mut _ as isize));
    }
    set
}

#[cfg(not(windows))]
fn windowed_pids_windows() -> HashSet<u32> {
    HashSet::new()
}

fn extract_proton_or_wine_exe(proc_entry: &sysinfo::Process) -> Option<String> {
    for arg in proc_entry.cmd() {
        let lower = arg.to_lowercase();
        if lower.ends_with(".exe") {
            let normalized = arg.replace('\\', "/");
            if let Some(fname) = std::path::Path::new(&normalized)
                .file_name()
                .and_then(|s| s.to_str())
            {
                let f_low = fname.to_lowercase();
                if !matches!(
                    f_low.as_str(),
                    "wineboot.exe"
                        | "wineserver.exe"
                        | "services.exe"
                        | "plugplay.exe"
                        | "svchost.exe"
                        | "rpcss.exe"
                        | "conhost.exe"
                        | "tabtip.exe"
                        | "explorer.exe"
                        | "steam.exe"
                ) {
                    return Some(f_low);
                }
            }
        }
    }
    None
}

fn scan(system: &mut System, watched_names: &HashSet<String>) -> HashMap<String, u32> {
    system.refresh_processes_specifics(
        ProcessRefreshKind::new()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );

    let mut matched_candidates: Vec<(String, u32)> = Vec::new();
    let filter_active = !watched_names.is_empty();

    for (&pid, p) in system.processes() {
        let raw_name = p.name().to_lowercase();
        let mut candidate_name = p
            .exe()
            .and_then(|e| e.file_name())
            .and_then(|f| f.to_str())
            .map(|s| s.to_lowercase());

        if candidate_name.is_none() && (raw_name.is_empty() || !filter_active || watched_names.contains(&raw_name)) {
            #[cfg(windows)]
            {
                candidate_name = query_process_name_windows(pid.as_u32()).map(|s| s.to_lowercase());
            }
        }

        let mut resolved = candidate_name.unwrap_or_else(|| raw_name.clone());
        if matches!(
            resolved.as_str(),
            "wine64-preloader"
                | "wine-preloader"
                | "wine"
                | "wine64"
                | "reaper"
                | "pressure-vessel-adverb"
                | "proton"
        ) {
            if let Some(wine_exe) = extract_proton_or_wine_exe(p) {
                resolved = wine_exe;
            }
        }

        if resolved.is_empty() {
            continue;
        }

        let with_exe = if resolved.ends_with(".exe") {
            resolved.clone()
        } else {
            format!("{resolved}.exe")
        };
        let without_exe = resolved
            .strip_suffix(".exe")
            .unwrap_or(&resolved)
            .to_string();

        if filter_active
            && !watched_names.contains(&with_exe)
            && !watched_names.contains(&without_exe)
            && !watched_names.contains(&resolved)
        {
            continue;
        }

        matched_candidates.push((resolved, pid.as_u32()));
    }

    if matched_candidates.is_empty() {
        return HashMap::new();
    }

    let windowed = windowed_pids_windows();
    let mut procs = HashMap::with_capacity(matched_candidates.len() * 2);

    for (lower, pid_u32) in matched_candidates {
        let has_win = windowed.contains(&pid_u32);
        let mut insert_key = |k: String| {
            match procs.get(&k).copied() {
                Some(existing_pid) if windowed.contains(&existing_pid) && !has_win => {}
                _ => {
                    procs.insert(k, pid_u32);
                }
            }
        };

        if let Some(stripped) = lower.strip_suffix(".exe") {
            insert_key(stripped.to_string());
        } else {
            insert_key(format!("{lower}.exe"));
        }
        insert_key(lower);
    }

    procs
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}