use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use super::bus::{Confidence, EventSink, EventSource, SourceError, MAX_LINE_LEN};

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
        Value::String(s) if s == "True" => Value::Bool(true),
        Value::String(s) if s == "False" => Value::Bool(false),
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