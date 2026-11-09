#![allow(dead_code, unused_imports)]
use bytes::Bytes;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub(crate) use crate::tracker::{Event, EventSink};

pub mod priority {
    #[cfg(windows)]
    pub fn set_low_priority() {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }

    #[cfg(not(windows))]
    pub fn set_low_priority() {}

    #[cfg(windows)]
    pub fn set_video_priority() {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(uperiod: u32) -> u32;
        }
        #[link(name = "avrt")]
        extern "system" {
            fn AvSetMmThreadCharacteristicsW(
                task_name: *const u16,
                task_index: *mut u32,
            ) -> isize;
        }
        unsafe {
            let _ = timeBeginPeriod(1);
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
            let task: [u16; 6] = [b'G' as u16, b'a' as u16, b'm' as u16, b'e' as u16, b's' as u16, 0];
            let mut idx: u32 = 0;
            let _ = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut idx);
        }
    }

    #[cfg(not(windows))]
    pub fn set_video_priority() {}

    #[cfg(windows)]
    pub fn set_audio_priority() {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        };
        #[link(name = "avrt")]
        extern "system" {
            fn AvSetMmThreadCharacteristicsW(
                task_name: *const u16,
                task_index: *mut u32,
            ) -> isize;
            fn AvSetMmThreadPriority(avrt_handle: isize, priority: i32) -> i32;
        }
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            let task: [u16; 10] = [
                b'P' as u16,
                b'r' as u16,
                b'o' as u16,
                b' ' as u16,
                b'A' as u16,
                b'u' as u16,
                b'd' as u16,
                b'i' as u16,
                b'o' as u16,
                0,
            ];
            let mut idx: u32 = 0;
            let h = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut idx);
            if h != 0 {
                let _ = AvSetMmThreadPriority(h, 2);
            }
        }
    }

    #[cfg(not(windows))]
    pub fn set_audio_priority() {}
}

pub mod diag {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    static LOG_LOCK: OnceLock<Mutex<PathBuf>> = OnceLock::new();

    pub fn resolve_log_path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join("clotch_diagnostics.log");
                if OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&candidate)
                    .is_ok()
                {
                    return candidate;
                }
            }
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let dir = PathBuf::from(local_app_data).join("Clotch");
            let _ = std::fs::create_dir_all(&dir);
            let candidate = dir.join("clotch_diagnostics.log");
            if OpenOptions::new()
                .create(true)
                .append(true)
                .open(&candidate)
                .is_ok()
            {
                return candidate;
            }
        }
        PathBuf::from("clotch_diagnostics.log")
    }

    pub fn log_path() -> PathBuf {
        let mtx = LOG_LOCK.get_or_init(|| Mutex::new(resolve_log_path()));
        match mtx.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    pub fn log_line(category: &str, message: &str) {
        let mtx = LOG_LOCK.get_or_init(|| Mutex::new(resolve_log_path()));
        let guard = match mtx.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&*guard) {
            let _ = writeln!(file, "[{ts_ms}] [{category}] {message}");
        }
    }

    pub fn log_startup_gpu_and_encoder(
        adapter_name: &str,
        vendor_id: u32,
        driver_version: &str,
        backend: &str,
        zero_copy: bool,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) {
        let path_mode = if zero_copy {
            "ZERO_COPY_DXGI_SURFACE"
        } else {
            "CPU_STAGING_READBACK"
        };
        super::record_detected_gpu_vendor(vendor_id);
        let msg = format!(
            "gpu='{adapter_name}' vendor_id=0x{vendor_id:04x} driver='{driver_version}' backend={backend} path={path_mode} res={width}x{height} fps={fps} bitrate_bps={bitrate_bps}"
        );
        log_line("ENCODER_INIT", &msg);
    }
}

static MF_STARTED: AtomicBool = AtomicBool::new(false);
static DETECTED_GPU_VENDOR_ID: AtomicUsize = AtomicUsize::new(0);

pub fn record_detected_gpu_vendor(vendor_id: u32) {
    if vendor_id != 0 {
        DETECTED_GPU_VENDOR_ID.store(vendor_id as usize, Ordering::Relaxed);
    }
}

pub fn detected_gpu_vendor_id() -> u32 {
    DETECTED_GPU_VENDOR_ID.load(Ordering::Relaxed) as u32
}
static MF_GUARD: Mutex<()> = Mutex::new(());

#[cfg(windows)]
pub fn ensure_mf_initialized() -> Result<(), String> {
    use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_NOSOCKET, MF_VERSION};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    if MF_STARTED.load(Ordering::Acquire) {
        return Ok(());
    }
    let _guard = match MF_GUARD.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if MF_STARTED.load(Ordering::Acquire) {
        return Ok(());
    }
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| {
            let err_str = format!("MFStartup failed: {e}");
            diag::log_line("MF_ERROR", &err_str);
            err_str
        })?;
    }
    MF_STARTED.store(true, Ordering::Release);
    diag::log_line("MF_LIFECYCLE", "MFStartup initialized once for process");
    Ok(())
}

#[cfg(not(windows))]
pub fn ensure_mf_initialized() -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
pub fn shutdown_media_foundation() {
    use windows::Win32::Media::MediaFoundation::MFShutdown;
    let _guard = match MF_GUARD.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if MF_STARTED.swap(false, Ordering::AcqRel) {
        unsafe {
            let _ = MFShutdown();
        }
        diag::log_line("MF_LIFECYCLE", "MFShutdown completed for process");
    }
}

#[cfg(not(windows))]
pub fn shutdown_media_foundation() {}

pub mod config {
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;
    use std::time::Duration;

    pub const MOD_ALT_FLAG: u32 = 0x0001;
    pub const MOD_CONTROL_FLAG: u32 = 0x0002;
    pub const MOD_SHIFT_FLAG: u32 = 0x0004;
    pub const MOD_WIN_FLAG: u32 = 0x0008;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Fps {
        Fps30,
        Fps60,
        Fps120,
        Custom(u32),
    }

    impl Fps {
        pub fn from_u32(val: u32) -> Self {
            match val {
                30 => Self::Fps30,
                60 => Self::Fps60,
                120 => Self::Fps120,
                other => Self::Custom(other.clamp(15, 240)),
            }
        }

        pub fn parse_str(raw: &str) -> Option<Self> {
            let clean = raw.trim().to_lowercase();
            let clean = clean.strip_suffix("fps").unwrap_or(&clean).trim();
            let val = clean.parse::<u32>().ok()?;
            if (15..=240).contains(&val) {
                Some(Self::from_u32(val))
            } else {
                None
            }
        }

        pub fn as_u32(self) -> u32 {
            match self {
                Self::Fps30 => 30,
                Self::Fps60 => 60,
                Self::Fps120 => 120,
                Self::Custom(v) => v.clamp(15, 240),
            }
        }

        pub fn frame_interval(self) -> Duration {
            Duration::from_nanos(1_000_000_000 / self.as_u32().max(1) as u64)
        }
    }

    impl Default for Fps {
        fn default() -> Self {
            Self::Fps60
        }
    }

    pub fn parse_duration_secs(raw: &str) -> Option<u64> {
        let s = raw.trim().to_lowercase();
        if s.is_empty() {
            return None;
        }
        if let Ok(plain) = s.parse::<u64>() {
            return Some(plain.max(1));
        }
        if let Ok(plain_f) = s.parse::<f64>() {
            if plain_f > 0.0 {
                return Some((plain_f.round() as u64).max(1));
            }
        }
        if s.contains(':') {
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() == 2 {
                let mins = parts[0].trim().parse::<u64>().ok()?;
                let secs = parts[1].trim().parse::<u64>().ok()?;
                return Some((mins * 60 + secs).max(1));
            } else if parts.len() == 3 {
                let hrs = parts[0].trim().parse::<u64>().ok()?;
                let mins = parts[1].trim().parse::<u64>().ok()?;
                let secs = parts[2].trim().parse::<u64>().ok()?;
                return Some((hrs * 3600 + mins * 60 + secs).max(1));
            }
        }

        let mut total_secs = 0.0f64;
        let mut num_buf = String::new();
        let mut unit_buf = String::new();
        let mut matched_any = false;

        for ch in s.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                if !unit_buf.is_empty() {
                    let val = num_buf.parse::<f64>().ok()?;
                    let mult = match unit_buf.as_str() {
                        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
                        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
                        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
                        _ => return None,
                    };
                    total_secs += val * mult;
                    matched_any = true;
                    num_buf.clear();
                    unit_buf.clear();
                }
                num_buf.push(ch);
            } else if ch.is_ascii_alphabetic() {
                unit_buf.push(ch);
            }
        }

        if !num_buf.is_empty() {
            let val = num_buf.parse::<f64>().ok()?;
            let mult = match unit_buf.as_str() {
                "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
                "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
                "" | "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
                _ => return None,
            };
            total_secs += val * mult;
            matched_any = true;
        }

        if matched_any && total_secs > 0.0 {
            Some((total_secs.round() as u64).max(1))
        } else {
            None
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Keybind {
        pub label: String,
        pub vk: u32,
        pub modifiers: u32,
    }

    impl Keybind {
        pub fn new(label: impl Into<String>, vk: u32, modifiers: u32) -> Self {
            Self {
                label: label.into(),
                vk,
                modifiers,
            }
        }

        pub fn parse(raw: &str) -> Option<Self> {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            let mut modifiers = 0u32;
            let mut main_vk: Option<u32> = None;
            let mut label_parts: Vec<String> = Vec::new();

            for token in trimmed.split('+').map(str::trim).filter(|t| !t.is_empty()) {
                let upper = token.to_uppercase();
                match upper.as_str() {
                    "CTRL" | "CONTROL" | "LCTRL" | "RCTRL" => {
                        modifiers |= MOD_CONTROL_FLAG;
                        label_parts.push("Ctrl".to_string());
                    }
                    "ALT" | "LALT" | "RALT" | "MENU" => {
                        modifiers |= MOD_ALT_FLAG;
                        label_parts.push("Alt".to_string());
                    }
                    "SHIFT" | "LSHIFT" | "RSHIFT" => {
                        modifiers |= MOD_SHIFT_FLAG;
                        label_parts.push("Shift".to_string());
                    }
                    "WIN" | "SUPER" | "META" | "LWIN" | "RWIN" => {
                        modifiers |= MOD_WIN_FLAG;
                        label_parts.push("Win".to_string());
                    }
                    key_tok => {
                        let (vk, canonical) = parse_virtual_key_token(key_tok)?;
                        main_vk = Some(vk);
                        label_parts.push(canonical);
                    }
                }
            }

            let vk = main_vk?;
            Some(Self {
                label: label_parts.join("+"),
                vk,
                modifiers,
            })
        }
    }

    fn parse_virtual_key_token(token: &str) -> Option<(u32, String)> {
        let up = token.trim().to_uppercase();
        if let Some(f_num_str) = up.strip_prefix('F') {
            if let Ok(f_num) = f_num_str.parse::<u32>() {
                if (1..=24).contains(&f_num) {
                    return Some((0x70 + (f_num - 1), format!("F{f_num}")));
                }
            }
        }
        if up.len() == 1 {
            let ch = up.chars().next()?;
            if ch.is_ascii_uppercase() {
                return Some((ch as u32, ch.to_string()));
            }
            if ch.is_ascii_digit() {
                return Some((ch as u32, ch.to_string()));
            }
        }
        if let Some(numpad_str) = up.strip_prefix("NUMPAD").or_else(|| up.strip_prefix("NUM")) {
            let numpad_trimmed = numpad_str.trim_start_matches('_');
            if let Ok(n) = numpad_trimmed.parse::<u32>() {
                if n <= 9 {
                    return Some((0x60 + n, format!("Numpad{n}")));
                }
            }
        }
        match up.as_str() {
            "INSERT" | "INS" => Some((0x2D, "Insert".to_string())),
            "DELETE" | "DEL" => Some((0x2E, "Delete".to_string())),
            "HOME" => Some((0x24, "Home".to_string())),
            "END" => Some((0x23, "End".to_string())),
            "PAGEUP" | "PGUP" | "PRIOR" => Some((0x21, "PageUp".to_string())),
            "PAGEDOWN" | "PGDN" | "NEXT" => Some((0x22, "PageDown".to_string())),
            "PRINTSCREEN" | "PRTSC" | "SNAPSHOT" => Some((0x2C, "PrintScreen".to_string())),
            "SCROLLLOCK" | "SCROLL" => Some((0x91, "ScrollLock".to_string())),
            "PAUSE" | "BREAK" => Some((0x13, "Pause".to_string())),
            "SPACE" | "SPACEBAR" => Some((0x20, "Space".to_string())),
            "TAB" => Some((0x09, "Tab".to_string())),
            "ENTER" | "RETURN" => Some((0x0D, "Enter".to_string())),
            "BACKSPACE" | "BACK" => Some((0x08, "Backspace".to_string())),
            "ESCAPE" | "ESC" => Some((0x1B, "Esc".to_string())),
            "UP" | "UPARROW" => Some((0x26, "Up".to_string())),
            "DOWN" | "DOWNARROW" => Some((0x28, "Down".to_string())),
            "LEFT" | "LEFTARROW" => Some((0x25, "Left".to_string())),
            "RIGHT" | "RIGHTARROW" => Some((0x27, "Right".to_string())),
            "MULTIPLY" | "NUMPAD_MULTIPLY" => Some((0x6A, "NumpadMultiply".to_string())),
            "ADD" | "NUMPAD_ADD" => Some((0x6B, "NumpadAdd".to_string())),
            "SUBTRACT" | "NUMPAD_SUBTRACT" => Some((0x6D, "NumpadSubtract".to_string())),
            "DECIMAL" | "NUMPAD_DECIMAL" => Some((0x6E, "NumpadDecimal".to_string())),
            "DIVIDE" | "NUMPAD_DIVIDE" => Some((0x6F, "NumpadDivide".to_string())),
            "TILDE" | "GRAVE" | "`" | "~" => Some((0xC0, "`".to_string())),
            "MINUS" | "-" => Some((0xBD, "-".to_string())),
            "EQUALS" | "PLUS" | "=" => Some((0xBB, "=".to_string())),
            "LBRACKET" | "[" => Some((0xDB, "[".to_string())),
            "RBRACKET" | "]" => Some((0xDD, "]".to_string())),
            "BACKSLASH" | "\\" => Some((0xDC, "\\".to_string())),
            "SEMICOLON" | ";" => Some((0xBA, ";".to_string())),
            "QUOTE" | "APOSTROPHE" | "'" => Some((0xDE, "'".to_string())),
            "COMMA" | "," => Some((0xBC, ",".to_string())),
            "PERIOD" | "." => Some((0xBE, ".".to_string())),
            "SLASH" | "/" => Some((0xBF, "/".to_string())),
            _ => None,
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct KeybindsConfig {
        pub replay_clip: Keybind,
        pub direct_clip: Keybind,
        pub discord_clip: Keybind,
        pub mic_mute: Keybind,
    }

    impl Default for KeybindsConfig {
        fn default() -> Self {
            Self {
                replay_clip: Keybind::new("F8", 0x77, 0),
                direct_clip: Keybind::new("F9", 0x78, 0),
                discord_clip: Keybind::new("F10", 0x79, 0),
                mic_mute: Keybind::new("F7", 0x76, 0),
            }
        }
    }

    impl KeybindsConfig {
        pub fn apply_key_value(&mut self, key: &str, val: &str) {
            match key {
                "hotkey_replay" | "replay_keybind" | "keybind_replay" | "key_replay" => {
                    if let Some(kb) = Keybind::parse(val) {
                        self.replay_clip = kb;
                    }
                }
                "hotkey_direct"
                | "hotkey_direct_clip"
                | "direct_clip_keybind"
                | "keybind_direct"
                | "key_direct" => {
                    if let Some(kb) = Keybind::parse(val) {
                        self.direct_clip = kb;
                    }
                }
                "hotkey_discord"
                | "discord_keybind"
                | "keybind_discord"
                | "key_discord" => {
                    if let Some(kb) = Keybind::parse(val) {
                        self.discord_clip = kb;
                    }
                }
                "hotkey_mic_mute"
                | "hotkey_mute"
                | "mic_mute_keybind"
                | "keybind_mic_mute"
                | "key_mic_mute" => {
                    if let Some(kb) = Keybind::parse(val) {
                        self.mic_mute = kb;
                    }
                }
                _ => {}
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Quality {
        Low,
        Medium,
        High,
        Lossless,
    }

    impl Quality {
        pub fn parse_str(raw: &str) -> Option<Self> {
            match raw.trim().to_lowercase().as_str() {
                "low" => Some(Self::Low),
                "medium" | "med" => Some(Self::Medium),
                "high" => Some(Self::High),
                "lossless" | "ultra" => Some(Self::Lossless),
                _ => None,
            }
        }

        pub fn bitrate_kbps(self, fps: u32) -> u32 {
            let mult = if fps >= 120 {
                1.85
            } else if fps >= 90 {
                1.55
            } else if fps >= 60 {
                1.25
            } else {
                1.0
            };
            let base = match self {
                Self::Low => 5500,
                Self::Medium => 10000,
                Self::High => 16000,
                Self::Lossless => 28000,
            };
            (base as f64 * mult) as u32
        }

        pub fn crf_or_qp(self) -> &'static str {
            match self {
                Self::Low => "28",
                Self::Medium => "23",
                Self::High => "18",
                Self::Lossless => "0",
            }
        }
    }

    impl Default for Quality {
        fn default() -> Self {
            Self::High
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Container {
        Mp4,
        Mkv,
    }

    impl Container {
        pub fn extension(self) -> &'static str {
            match self {
                Self::Mp4 => "mp4",
                Self::Mkv => "mkv",
            }
        }

        pub fn format_name(self) -> &'static str {
            match self {
                Self::Mp4 => "mp4",
                Self::Mkv => "matroska",
            }
        }
    }

    impl Default for Container {
        fn default() -> Self {
            Self::Mp4
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum AudioMode {
        None,
        SystemOnly,
        MicOnly,
        Both,
    }

    impl AudioMode {
        pub fn parse_str(raw: &str) -> Option<Self> {
            match raw.trim().to_lowercase().as_str() {
                "none" | "off" | "disabled" | "false" => Some(Self::None),
                "system" | "system_only" | "desktop" => Some(Self::SystemOnly),
                "mic" | "mic_only" | "microphone" => Some(Self::MicOnly),
                "both" | "all" | "system+mic" => Some(Self::Both),
                _ => None,
            }
        }

        pub fn to_usize(self) -> usize {
            match self {
                Self::None => 0,
                Self::SystemOnly => 1,
                Self::MicOnly => 2,
                Self::Both => 3,
            }
        }

        pub fn from_usize(v: usize) -> Self {
            match v {
                0 => Self::None,
                1 => Self::SystemOnly,
                2 => Self::MicOnly,
                _ => Self::Both,
            }
        }
    }

    impl Default for AudioMode {
        fn default() -> Self {
            Self::Both
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum StorageMode {
        AutoPartition,
        Custom(PathBuf),
    }

    impl Default for StorageMode {
        fn default() -> Self {
            Self::AutoPartition
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct StorageConfig {
        pub mode: StorageMode,
        pub min_free_gb: u64,
    }

    impl Default for StorageConfig {
        fn default() -> Self {
            Self {
                mode: StorageMode::AutoPartition,
                min_free_gb: 10,
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct RecorderConfig {
        pub fps: Fps,
        pub quality: Quality,
        pub container: Container,
        pub pre_roll_secs: u64,
        pub post_roll_secs: u64,
        pub audio: AudioMode,
        #[serde(default = "default_audio_bitrate_kbps")]
        pub audio_bitrate_kbps: u32,
        #[serde(default)]
        pub mic_device: Option<String>,
        #[serde(default)]
        pub mic_muted: bool,
        #[serde(default = "default_separate_audio_tracks")]
        pub separate_audio_tracks: bool,
        pub storage: StorageConfig,
        pub buffer_duration_secs: u64,
        pub direct_clip_duration_secs: u64,
        pub width: u32,
        pub height: u32,
        #[serde(default)]
        pub low_end_mode: bool,
        #[serde(default)]
        pub keybinds: KeybindsConfig,
        #[serde(default = "default_share_anonymous_stats")]
        pub share_anonymous_stats: bool,
        #[serde(default = "default_appearance")]
        pub appearance: String,
    }

    fn default_audio_bitrate_kbps() -> u32 {
        128
    }

    fn default_separate_audio_tracks() -> bool {
        false
    }

    fn default_share_anonymous_stats() -> bool {
        false
    }

    fn default_appearance() -> String {
        "system".to_string()
    }

    pub fn clean_config_value(raw: &str) -> &str {
        let trimmed = raw.trim();
        if trimmed.len() >= 2
            && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
                || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
        {
            return &trimmed[1..trimmed.len() - 1];
        }
        let mut in_double = false;
        let mut in_single = false;
        let mut cut_idx = trimmed.len();
        for (i, ch) in trimmed.char_indices() {
            match ch {
                '"' if !in_single => in_double = !in_double,
                '\'' if !in_double => in_single = !in_single,
                '#' | ';' if !in_double && !in_single => {
                    cut_idx = i;
                    break;
                }
                _ => {}
            }
        }
        trimmed[..cut_idx]
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim()
    }

    pub fn parse_resolution_pair(raw: &str) -> Option<(u32, u32)> {
        let lower = raw.trim().to_lowercase();
        match lower.as_str() {
            "720p" | "hd" => Some((1280, 720)),
            "900p" => Some((1600, 900)),
            "1080p" | "fhd" => Some((1920, 1080)),
            "1440p" | "2k" | "qhd" => Some((2560, 1440)),
            "2160p" | "4k" | "uhd" => Some((3840, 2160)),
            custom => {
                let (w_s, h_s) = custom.split_once('x')?;
                let w = w_s.trim().parse::<u32>().ok()?;
                let h = h_s.trim().parse::<u32>().ok()?;
                if (64..=7680).contains(&w) && (64..=4320).contains(&h) {
                    Some((w & !1, h & !1))
                } else {
                    None
                }
            }
        }
    }

    pub fn locate_keybinds_or_config_files() -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("clotch_config.toml"));
                candidates.push(dir.join("keybinds.toml"));
            }
        }
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let p = PathBuf::from(appdata).join("Clotch");
            candidates.push(p.join("clotch_config.toml"));
            candidates.push(p.join("keybinds.toml"));
        }
        if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
            let p = PathBuf::from(local_appdata).join("Clotch");
            candidates.push(p.join("clotch_config.toml"));
            candidates.push(p.join("keybinds.toml"));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            let p = PathBuf::from(xdg).join("clotch");
            candidates.push(p.join("clotch_config.toml"));
            candidates.push(p.join("keybinds.toml"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            let p = PathBuf::from(home).join(".config/clotch");
            candidates.push(p.join("clotch_config.toml"));
            candidates.push(p.join("keybinds.toml"));
        }
        let mut existing = Vec::new();
        for c in candidates {
            if c.is_file() && !existing.contains(&c) {
                existing.push(c);
            }
        }
        existing
    }

    impl RecorderConfig {
        pub fn apply_low_end_optimizations(&mut self) {
            self.low_end_mode = true;
            self.separate_audio_tracks = false;
            if self.width > 1280 || self.height > 720 {
                self.width = 1280;
                self.height = 720;
            }
            if matches!(self.quality, Quality::High | Quality::Lossless) {
                self.quality = Quality::Medium;
            }
            if self.fps.as_u32() > 60 {
                self.fps = Fps::Fps60;
            }
            if self.audio_bitrate_kbps > 96 {
                self.audio_bitrate_kbps = 96;
            }
            if self.buffer_duration_secs > 30 {
                self.buffer_duration_secs = 30;
            }
        }

        pub fn load() -> Self {
            let hw = super::hardware::SystemHardwareProfile::detect();
            let mut cfg = Self::default();
            if hw.is_low_end {
                cfg.apply_low_end_optimizations();
            }
            cfg.load_overrides_from_config_files();
            cfg
        }

        pub fn load_overrides_from_config_files(&mut self) {
            for path in locate_keybinds_or_config_files() {
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty()
                        || trimmed.starts_with('#')
                        || trimmed.starts_with(';')
                        || trimmed.starts_with('[')
                    {
                        continue;
                    }
                    let Some((k, v)) = trimmed.split_once('=') else {
                        continue;
                    };
                    let key = k.trim().to_lowercase();
                    let val = clean_config_value(v);
                    if val.is_empty() {
                        continue;
                    }
                    self.keybinds.apply_key_value(&key, val);
                    match key.as_str() {
                        "fps" | "capture_fps" | "recording_fps" => {
                            if let Some(f) = Fps::parse_str(val) {
                                self.fps = f;
                            }
                        }
                        "quality" | "video_quality" | "recording_quality" => {
                            if let Some(q) = Quality::parse_str(val) {
                                self.quality = q;
                            }
                        }
                        "resolution" | "video_resolution" => {
                            if let Some((w, h)) = parse_resolution_pair(val) {
                                self.width = w;
                                self.height = h;
                            }
                        }
                        "width" | "capture_width" => {
                            if let Ok(w) = val.parse::<u32>() {
                                if (64..=7680).contains(&w) {
                                    self.width = w & !1;
                                }
                            }
                        }
                        "height" | "capture_height" => {
                            if let Ok(h) = val.parse::<u32>() {
                                if (64..=4320).contains(&h) {
                                    self.height = h & !1;
                                }
                            }
                        }
                        "audio" | "audio_mode" => {
                            if let Some(a) = AudioMode::parse_str(val) {
                                self.audio = a;
                            }
                        }
                        "audio_bitrate_kbps" | "audio_bitrate" => {
                            let clean = val.trim_end_matches("kbps").trim_end_matches('k').trim();
                            if let Ok(kbps) = clean.parse::<u32>() {
                                self.audio_bitrate_kbps = kbps.clamp(64, 192);
                            }
                        }
                        "mic_device" | "microphone" | "microphone_device" | "input_device" => {
                            if val.eq_ignore_ascii_case("default") || val.eq_ignore_ascii_case("auto") {
                                self.mic_device = None;
                            } else {
                                self.mic_device = Some(val.to_string());
                            }
                        }
                        "mic_muted" | "mute_mic" | "start_mic_muted" => {
                            self.mic_muted =
                                matches!(val.to_lowercase().as_str(), "true" | "1" | "yes" | "on");
                        }
                        "separate_audio_tracks" | "multi_track_audio" | "split_audio_tracks" => {
                            self.separate_audio_tracks =
                                !matches!(val.to_lowercase().as_str(), "false" | "0" | "no" | "off");
                        }
                        "buffer_duration"
                        | "buffer_duration_secs"
                        | "buffer_secs"
                        | "replay_duration"
                        | "recording_time" => {
                            if let Some(secs) = parse_duration_secs(val) {
                                self.buffer_duration_secs = secs.clamp(5, 7200);
                            }
                        }
                        "direct_clip_duration"
                        | "direct_clip_duration_secs"
                        | "direct_clip_secs" => {
                            if let Some(secs) = parse_duration_secs(val) {
                                self.direct_clip_duration_secs = secs.clamp(3, 3600);
                            }
                        }
                        "pre_roll" | "pre_roll_secs" => {
                            if let Some(secs) = parse_duration_secs(val) {
                                self.pre_roll_secs = secs.clamp(1, 3600);
                            }
                        }
                        "post_roll" | "post_roll_secs" => {
                            if let Some(secs) = parse_duration_secs(val) {
                                self.post_roll_secs = secs.clamp(1, 3600);
                            }
                        }
                        "container" | "video_format" | "format" => {
                            if val.eq_ignore_ascii_case("mkv") || val.eq_ignore_ascii_case("matroska") {
                                self.container = Container::Mkv;
                            } else if val.eq_ignore_ascii_case("mp4") {
                                self.container = Container::Mp4;
                            }
                        }
                        "min_free_gb" | "stop_recording_below_gb" => {
                            let clean = val
                                .to_lowercase()
                                .replace("gb", "")
                                .replace("free", "")
                                .trim()
                                .to_string();
                            if let Ok(gb) = clean.parse::<u64>() {
                                self.storage.min_free_gb = gb.clamp(1, 500);
                            }
                        }
                        "output_dir" | "clips_dir" | "storage_path" => {
                            if val.eq_ignore_ascii_case("auto") {
                                self.storage.mode = StorageMode::AutoPartition;
                            } else {
                                self.storage.mode = StorageMode::Custom(PathBuf::from(val));
                            }
                        }
                        "low_end_mode" | "eco_mode" | "potato_mode" => {
                            self.low_end_mode =
                                matches!(val.to_lowercase().as_str(), "true" | "1" | "yes" | "on");
                        }
                        "share_anonymous_stats"
                        | "privacy_share_anonymous_stats"
                        | "telemetry_enabled"
                        | "analytics_enabled"
                        | "telemetry" => {
                            self.share_anonymous_stats = matches!(
                                val.to_lowercase().as_str(),
                                "true" | "1" | "yes" | "on" | "enabled"
                            );
                        }
                        "appearance" | "theme" | "ui_theme" => {
                            let low = val.trim().to_lowercase();
                            self.appearance = match low.as_str() {
                                "light" => "light".to_string(),
                                "dark" => "dark".to_string(),
                                _ => "system".to_string(),
                            };
                        }
                        _ => {}
                    }
                }
            }
        }
        pub fn save_to_disk(&self) -> Result<PathBuf, String> {
            let existing = locate_keybinds_or_config_files();
            let target_path = if let Some(first) = existing
                .iter()
                .find(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.eq_ignore_ascii_case("clotch_config.toml"))
                        .unwrap_or(false)
                })
                .cloned()
                .or_else(|| existing.into_iter().next())
            {
                first
            } else if let Ok(exe) = std::env::current_exe() {
                exe.parent()
                    .map(|d| d.join("clotch_config.toml"))
                    .unwrap_or_else(|| PathBuf::from("clotch_config.toml"))
            } else {
                PathBuf::from("clotch_config.toml")
            };

            let current = std::fs::read_to_string(&target_path).unwrap_or_default();
            let mut preserved_lines: Vec<String> = Vec::new();
            let managed_keys = [
                "fps",
                "capture_fps",
                "recording_fps",
                "quality",
                "video_quality",
                "recording_quality",
                "resolution",
                "video_resolution",
                "width",
                "capture_width",
                "height",
                "capture_height",
                "container",
                "video_format",
                "format",
                "audio",
                "audio_mode",
                "audio_bitrate_kbps",
                "audio_bitrate",
                "mic_device",
                "microphone",
                "microphone_device",
                "input_device",
                "mic_muted",
                "mute_mic",
                "start_mic_muted",
                "separate_audio_tracks",
                "multi_track_audio",
                "split_audio_tracks",
                "buffer_duration",
                "buffer_duration_secs",
                "buffer_secs",
                "replay_duration",
                "recording_time",
                "direct_clip_duration",
                "direct_clip_duration_secs",
                "direct_clip_secs",
                "pre_roll",
                "pre_roll_secs",
                "post_roll",
                "post_roll_secs",
                "output_dir",
                "clips_dir",
                "storage_path",
                "min_free_gb",
                "stop_recording_below_gb",
                "low_end_mode",
                "eco_mode",
                "potato_mode",
                "share_anonymous_stats",
                "privacy_share_anonymous_stats",
                "telemetry_enabled",
                "analytics_enabled",
                "telemetry",
                "appearance",
                "theme",
                "ui_theme",
                "hotkey_replay",
                "replay_keybind",
                "keybind_replay",
                "key_replay",
                "hotkey_direct",
                "hotkey_direct_clip",
                "direct_clip_keybind",
                "keybind_direct",
                "key_direct",
                "hotkey_discord",
                "discord_keybind",
                "keybind_discord",
                "key_discord",
                "hotkey_mic_mute",
                "hotkey_mute",
                "mic_mute_keybind",
                "keybind_mic_mute",
                "key_mic_mute",
            ];

            for line in current.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty()
                    && !trimmed.starts_with('#')
                    && !trimmed.starts_with(';')
                    && !trimmed.starts_with('[')
                {
                    if let Some((k, _)) = trimmed.split_once('=') {
                        let key_low = k.trim().to_lowercase();
                        if managed_keys.contains(&key_low.as_str()) {
                            continue;
                        }
                    }
                }
                preserved_lines.push(line.to_string());
            }

            let qual_str = match self.quality {
                Quality::Low => "low",
                Quality::Medium => "medium",
                Quality::High => "high",
                Quality::Lossless => "lossless",
            };
            let audio_str = match self.audio {
                AudioMode::None => "none",
                AudioMode::SystemOnly => "system_only",
                AudioMode::MicOnly => "mic_only",
                AudioMode::Both => "both",
            };
            let mic_str = self.mic_device.as_deref().unwrap_or("default");
            let out_dir_str = match &self.storage.mode {
                StorageMode::AutoPartition => "auto".to_string(),
                StorageMode::Custom(p) => p.to_string_lossy().to_string(),
            };
            let container_str = self.container.extension();

            let mut out = preserved_lines;
            out.push(format!("share_anonymous_stats = {}", self.share_anonymous_stats));
            out.push(format!("telemetry_enabled = {}", self.share_anonymous_stats));
out.push(format!("appearance = \"{}\"", self.appearance));
            out.push(format!("fps = {}", self.fps.as_u32()));
            out.push(format!("quality = \"{qual_str}\""));
            out.push(format!("width = {}", self.width));
            out.push(format!("height = {}", self.height));
            out.push(format!("container = \"{container_str}\""));
            out.push(format!("audio_mode = \"{audio_str}\""));
            out.push(format!("audio_bitrate_kbps = {}", self.audio_bitrate_kbps));
            out.push(format!("mic_device = \"{}\"", mic_str.replace('"', "")));
            out.push(format!("mic_muted = {}", self.mic_muted));
            out.push(format!("separate_audio_tracks = {}", self.separate_audio_tracks));
            out.push(format!("buffer_duration_secs = {}", self.buffer_duration_secs));
            out.push(format!("direct_clip_duration_secs = {}", self.direct_clip_duration_secs));
            out.push(format!("pre_roll_secs = {}", self.pre_roll_secs));
            out.push(format!("post_roll_secs = {}", self.post_roll_secs));
            out.push(format!("output_dir = \"{}\"", out_dir_str.replace('\\', "/")));
            out.push(format!("min_free_gb = {}", self.storage.min_free_gb));
            out.push(format!("low_end_mode = {}", self.low_end_mode));
            out.push(format!("hotkey_replay = \"{}\"", self.keybinds.replay_clip.label));
            out.push(format!("hotkey_direct = \"{}\"", self.keybinds.direct_clip.label));
            out.push(format!("hotkey_discord = \"{}\"", self.keybinds.discord_clip.label));
            out.push(format!("hotkey_mic_mute = \"{}\"", self.keybinds.mic_mute.label));

            let mut final_body = out.join("\n");
            if !final_body.ends_with('\n') {
                final_body.push('\n');
            }
            std::fs::write(&target_path, final_body).map_err(|e| {
                format!("failed to save configuration to {}: {e}", target_path.display())
            })?;
            Ok(target_path)
        }
    }

    impl Default for RecorderConfig {
        fn default() -> Self {
            Self {
                fps: Fps::Fps60,
                quality: Quality::High,
                container: Container::Mp4,
                audio: AudioMode::Both,
                audio_bitrate_kbps: 128,
                mic_device: None,
                mic_muted: false,
                separate_audio_tracks: false,
                pre_roll_secs: 14,
                post_roll_secs: 10,
                storage: StorageConfig::default(),
                buffer_duration_secs: 90,
                direct_clip_duration_secs: 15,
                width: 1920,
                height: 1080,
                low_end_mode: false,
                keybinds: KeybindsConfig::default(),
                share_anonymous_stats: false,
                appearance: default_appearance(),
            }
        }
    }
}

pub mod hardware {
    use serde::{Deserialize, Serialize};
    use sysinfo::System;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct SystemHardwareProfile {
        pub logical_cores: usize,
        pub total_ram_mb: u64,
        pub available_ram_mb: u64,
        pub is_low_end: bool,
    }

    impl SystemHardwareProfile {
        pub fn detect() -> Self {
            let logical_cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let mut sys = System::new();
            sys.refresh_memory();
            let total_ram_mb = sys.total_memory() / (1024 * 1024);
            let available_ram_mb = sys.available_memory() / (1024 * 1024);
            let env_forced = std::env::var("CLOTCH_LOW_END")
                .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false);
            let is_low_end = env_forced
                || (logical_cores <= 4 && total_ram_mb > 0 && total_ram_mb <= 8500)
                || (total_ram_mb > 0 && total_ram_mb <= 6144);
            Self {
                logical_cores,
                total_ram_mb,
                available_ram_mb,
                is_low_end,
            }
        }
    }
}

pub mod linux_support {
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum LinuxDisplayServer {
        Wayland,
        X11,
        Headless,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum LinuxCaptureBackend {
        PipeWirePortal,
        X11ShmGrab,
        KmsDrm,
        Unavailable,
    }

    impl LinuxDisplayServer {
        pub fn detect() -> Self {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                Self::Wayland
            } else if std::env::var_os("DISPLAY").is_some() {
                Self::X11
            } else {
                Self::Headless
            }
        }

        pub fn recommended_backend(self) -> LinuxCaptureBackend {
            match self {
                Self::Wayland => LinuxCaptureBackend::PipeWirePortal,
                Self::X11 => LinuxCaptureBackend::X11ShmGrab,
                Self::Headless => LinuxCaptureBackend::Unavailable,
            }
        }
    }
}

pub mod storage {
    use super::config::{StorageConfig, StorageMode};
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use sysinfo::Disks;

    static STICKY_DRIVE_CACHE: OnceLock<PathBuf> = OnceLock::new();
    static DISK_SPACE_CHECK_CACHE: OnceLock<Mutex<(Instant, bool)>> = OnceLock::new();

    pub struct StorageManager;

    impl StorageManager {
        pub fn sanitize_folder_name(name: &str) -> String {
            let forbidden = [
                '<', '>', ':', '"', '/', '\\', '|', '?', '*', ',', ';', '=', '[',
                ']', '@', '#', '!', '$', '%', '^', '&', '(', ')', '+', '~', '`',
            ];
            let mut out = String::with_capacity(name.len().min(40));
            let mut prev_underscore = false;
            for c in name.chars() {
                if forbidden.contains(&c) || c.is_control() {
                    continue;
                }
                if c.is_whitespace() || c == '-' || c == '.' || c == '_' {
                    if !out.is_empty() && !prev_underscore {
                        out.push('_');
                        prev_underscore = true;
                    }
                } else if c.is_alphanumeric() {
                    out.push(c);
                    prev_underscore = false;
                }
                if out.chars().count() >= 36 {
                    break;
                }
            }
            let trimmed = out.trim_matches(|c: char| c == '_' || c == '.' || c.is_whitespace());
            if trimmed.is_empty() {
                return "UnknownGame".to_string();
            }
            let upper = trimmed.to_ascii_uppercase();
            if matches!(
                upper.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            ) {
                return format!("Game_{trimmed}");
            }
            trimmed.to_string()
        }

        pub fn resolve_clip_dir(
            config: &StorageConfig,
            base_override: Option<&Path>,
        ) -> Result<PathBuf, String> {
            if let Some(base) = base_override {
                std::fs::create_dir_all(base)
                    .map_err(|e| format!("failed to create dir: {e}"))?;
                return Ok(base.to_path_buf());
            }
            match &config.mode {
                StorageMode::Custom(path) => {
                    let _ = STICKY_DRIVE_CACHE.set(path.clone());
                    std::fs::create_dir_all(path)
                        .map_err(|e| format!("failed to create dir: {e}"))?;
                    Ok(path.clone())
                }
                StorageMode::AutoPartition => {
                    let sub = Self::resolve_game_clip_dir(config, "Clips", None)?;
                    Ok(sub.parent().map(Path::to_path_buf).unwrap_or(sub))
                }
            }
        }

        pub fn resolve_game_clip_dir(
            config: &StorageConfig,
            game_name: &str,
            base_override: Option<&Path>,
        ) -> Result<PathBuf, String> {
            let safe_game = Self::sanitize_folder_name(game_name);

            if let Some(base) = base_override {
                let target = base.join(&safe_game);
                std::fs::create_dir_all(&target)
                    .map_err(|e| format!("failed to create dir: {e}"))?;
                return Ok(target);
            }

            match &config.mode {
                StorageMode::Custom(path) => {
                    let _ = STICKY_DRIVE_CACHE.set(path.clone());
                    let target = path.join(&safe_game);
                    std::fs::create_dir_all(&target)
                        .map_err(|e| format!("failed to create dir: {e}"))?;
                    Ok(target)
                }
                StorageMode::AutoPartition => {
                    if let Some(cached_root) = STICKY_DRIVE_CACHE.get() {
                        let target = cached_root.join("ClotchClips").join(&safe_game);
                        if std::fs::create_dir_all(&target).is_ok() {
                            return Ok(target);
                        }
                    }

                    let disks = Disks::new_with_refreshed_list();
                    let min_bytes = config.min_free_gb * 1024 * 1024 * 1024;

                    let mut existing_dir_candidate = None;
                    for disk in disks.list() {
                        if disk.available_space() >= min_bytes {
                            let mount = disk.mount_point();
                            let candidate = mount.join("ClotchClips");
                            if candidate.exists() && candidate.is_dir() {
                                existing_dir_candidate = Some(mount.to_path_buf());
                                break;
                            }
                        }
                    }

                    let chosen_root = if let Some(found) = existing_dir_candidate {
                        found
                    } else {
                        let mut non_sys_best: Option<(PathBuf, u64)> = None;
                        let mut sys_best: Option<(PathBuf, u64)> = None;

                        for disk in disks.list() {
                            let free = disk.available_space();
                            if free < min_bytes {
                                continue;
                            }
                            let mount = disk.mount_point();
                            let is_c_drive =
                                mount.to_string_lossy().to_uppercase().starts_with("C:");

                            if is_c_drive {
                                if sys_best.as_ref().map_or(true, |(_, b)| free > *b) {
                                    sys_best = Some((mount.to_path_buf(), free));
                                }
                            } else if non_sys_best.as_ref().map_or(true, |(_, b)| free > *b) {
                                non_sys_best = Some((mount.to_path_buf(), free));
                            }
                        }

                        if let Some((root, _)) = non_sys_best {
                            root
                        } else if let Some((root, _)) = sys_best {
                            root
                        } else {
                            PathBuf::from(".")
                        }
                    };

                    let _ = STICKY_DRIVE_CACHE.set(chosen_root.clone());
                    let target = chosen_root.join("ClotchClips").join(&safe_game);
                    std::fs::create_dir_all(&target)
                        .map_err(|e| format!("failed to create dir: {e}"))?;
                    Ok(target)
                }
            }
        }

        pub fn resolve_cache_dir() -> PathBuf {
            if let Some(cached_root) = STICKY_DRIVE_CACHE.get() {
                let cache_dir = cached_root.join("ClotchClips").join(".cache");
                if std::fs::create_dir_all(&cache_dir).is_ok() {
                    return cache_dir;
                }
            }
            let fallback = PathBuf::from("ClotchClips").join(".cache");
            let _ = std::fs::create_dir_all(&fallback);
            fallback
        }

        pub fn purge_cache_dir() {
            let cache_dir = Self::resolve_cache_dir();
            if let Ok(entries) = std::fs::read_dir(&cache_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }

        pub fn purge_stale_tmp_files(config: &StorageConfig) {
            let Ok(base_clip_dir) = Self::resolve_clip_dir(config, None) else {
                return;
            };
            let root_dir = base_clip_dir
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(base_clip_dir);
            if let Ok(entries) = std::fs::read_dir(&root_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("tmp") {
                        let _ = std::fs::remove_file(&p);
                    } else if p.is_dir() {
                        if let Ok(sub_entries) = std::fs::read_dir(&p) {
                            for sub in sub_entries.flatten() {
                                let sp = sub.path();
                                if sp.is_file()
                                    && sp.extension().and_then(|e| e.to_str()) == Some("tmp")
                                {
                                    let _ = std::fs::remove_file(&sp);
                                }
                            }
                        }
                    }
                }
            }
        }

        pub fn ensure_free_space(dir: &Path, min_free_gb: u64) -> Result<(), String> {
            let cache_mtx = DISK_SPACE_CHECK_CACHE
                .get_or_init(|| Mutex::new((Instant::now() - Duration::from_secs(60), true)));
            if let Ok(guard) = cache_mtx.lock() {
                if guard.0.elapsed() < Duration::from_secs(15) && guard.1 {
                    return Ok(());
                }
            }

            let disks = Disks::new_with_refreshed_list();
            let min_bytes = min_free_gb * 1024 * 1024 * 1024;
            let dir_canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());

            for disk in disks.list() {
                let mount = disk.mount_point();
                if dir_canonical.starts_with(mount) {
                    if disk.available_space() < min_bytes {
                        if let Ok(mut guard) = cache_mtx.lock() {
                            *guard = (Instant::now(), false);
                        }
                        return Err(format!(
                            "insufficient free disk space: {} GB available, {} GB required",
                            disk.available_space() / (1024 * 1024 * 1024),
                            min_free_gb
                        ));
                    }
                    if let Ok(mut guard) = cache_mtx.lock() {
                        *guard = (Instant::now(), true);
                    }
                    return Ok(());
                }
            }
            Ok(())
        }
    }
}

pub mod buffer {
    use bytes::Bytes;
    use serde::{Deserialize, Serialize};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PacketKind {
        Video,
        Audio,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CaptureContext {
        Desktop = 0,
        Game = 1,
        Social = 2,
        Media = 3,
    }

    impl CaptureContext {
        pub fn from_usize(v: usize) -> Self {
            match v {
                1 => Self::Game,
                2 => Self::Social,
                3 => Self::Media,
                _ => Self::Desktop,
            }
        }

        pub fn label(self) -> &'static str {
            match self {
                Self::Game => "Game",
                Self::Social => "Social",
                Self::Media => "Media",
                Self::Desktop => "Desktop",
            }
        }
    }

    impl Default for CaptureContext {
        fn default() -> Self {
            Self::Desktop
        }
    }

    #[derive(Clone, Debug)]
    pub struct AudioStemPackets {
        pub game: Bytes,
        pub discord: Bytes,
        pub mic: Bytes,
        pub other: Bytes,
    }

    #[derive(Clone, Debug)]
    pub struct CompressedPacket {
        pub kind: PacketKind,
        pub pts_ms: u64,
        pub pts: i64,
        pub dts: i64,
        pub duration: i64,
        pub is_keyframe: bool,
        pub data: Bytes,
        pub stems: Option<Arc<AudioStemPackets>>,
    }

    impl CompressedPacket {
        pub fn byte_footprint(&self) -> usize {
            let base = self.data.len();
            if let Some(ref s) = self.stems {
                base + s.game.len() + s.discord.len() + s.mic.len() + s.other.len()
            } else {
                base
            }
        }
    }

    #[derive(Clone)]
    pub struct RingBuffer<T = CompressedPacket> {
        video_items: Arc<parking_lot::Mutex<VecDeque<(u64, CaptureContext, Arc<CompressedPacket>)>>>,
        audio_items: Arc<parking_lot::Mutex<VecDeque<(u64, CaptureContext, Arc<CompressedPacket>)>>>,
        max_duration_ms: Arc<AtomicU64>,
        max_video_bytes: Arc<AtomicUsize>,
        max_audio_bytes: Arc<AtomicUsize>,
        video_bytes: Arc<AtomicUsize>,
        audio_bytes: Arc<AtomicUsize>,
        active_context: Arc<AtomicUsize>,
        _phantom: std::marker::PhantomData<T>,
    }

    const BASE_MAX_VIDEO_BYTES: usize = 512 * 1024 * 1024;
    const BASE_MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;

    fn compute_dynamic_byte_caps(max_duration: Duration) -> (usize, usize) {
        let secs = max_duration.as_secs().max(120) as usize;
        let scaled_video = (secs.saturating_mul(6 * 1024 * 1024)).max(BASE_MAX_VIDEO_BYTES);
        let scaled_audio = (secs.saturating_mul(48 * 1024)).max(BASE_MAX_AUDIO_BYTES);
        let max_safe_v = 1536 * 1024 * 1024;
        let max_safe_a = 128 * 1024 * 1024;
        (scaled_video.min(max_safe_v), scaled_audio.min(max_safe_a))
    }

    impl RingBuffer<CompressedPacket> {
        pub fn new_with_capacities(
            max_duration: Duration,
            video_capacity: usize,
            audio_capacity: usize,
        ) -> Self {
            let (v_cap_bytes, a_cap_bytes) = compute_dynamic_byte_caps(max_duration);
            Self {
                video_items: Arc::new(parking_lot::Mutex::new(VecDeque::with_capacity(
                    video_capacity.clamp(64, 512),
                ))),
                audio_items: Arc::new(parking_lot::Mutex::new(VecDeque::with_capacity(
                    audio_capacity.clamp(64, 512),
                ))),
                max_duration_ms: Arc::new(AtomicU64::new(max_duration.as_millis() as u64)),
                max_video_bytes: Arc::new(AtomicUsize::new(v_cap_bytes)),
                max_audio_bytes: Arc::new(AtomicUsize::new(a_cap_bytes)),
                video_bytes: Arc::new(AtomicUsize::new(0)),
                audio_bytes: Arc::new(AtomicUsize::new(0)),
                active_context: Arc::new(AtomicUsize::new(CaptureContext::Desktop as usize)),
                _phantom: std::marker::PhantomData,
            }
        }

        pub fn new(max_duration: Duration, max_items: usize) -> Self {
            Self::new_with_capacities(
                max_duration,
                max_items,
                (max_duration.as_secs() * 48) as usize,
            )
        }

        pub fn set_active_context(&self, ctx: CaptureContext) {
            self.active_context.store(ctx as usize, Ordering::Release);
        }

        pub fn active_context(&self) -> CaptureContext {
            CaptureContext::from_usize(self.active_context.load(Ordering::Acquire))
        }

        pub fn set_limits(&self, max_duration: Duration, _max_items: usize) {
            let (v_cap_bytes, a_cap_bytes) = compute_dynamic_byte_caps(max_duration);
            self.max_duration_ms
                .store(max_duration.as_millis() as u64, Ordering::Release);
            self.max_video_bytes.store(v_cap_bytes, Ordering::Release);
            self.max_audio_bytes.store(a_cap_bytes, Ordering::Release);
        }

        pub fn push(&self, timestamp_ms: u64, item: Arc<CompressedPacket>) {
            match item.kind {
                PacketKind::Video => self.push_video(timestamp_ms, item),
                PacketKind::Audio => self.push_audio(timestamp_ms, item),
            }
        }

        pub fn push_video(&self, ts_ms: u64, packet: Arc<CompressedPacket>) {
            let p_len = packet.data.len();
            self.video_bytes.fetch_add(p_len, Ordering::Release);
            let max_dur = self.max_duration_ms.load(Ordering::Acquire);
            let v_byte_limit = self.max_video_bytes.load(Ordering::Acquire);
            let ctx = self.active_context();
            let mut guard = self.video_items.lock();
            let (safe_ts, safe_pts_90k) = if let Some(&(last_ts, _, ref last_pkt)) = guard.back() {
                let st = ts_ms.max(last_ts.saturating_add(1));
                let raw_90k = if packet.pts >= (packet.pts_ms as i64) * 45 && packet.pts > 0 {
                    packet.pts
                } else {
                    (st as i64) * 90
                };
                let last_90k = if last_pkt.pts >= (last_pkt.pts_ms as i64) * 45 && last_pkt.pts > 0 {
                    last_pkt.pts
                } else {
                    (last_ts as i64) * 90
                };
                (st, raw_90k.max(last_90k.saturating_add(1)))
            } else {
                let raw_90k = if packet.pts >= (packet.pts_ms as i64) * 45 && packet.pts > 0 {
                    packet.pts
                } else {
                    (ts_ms as i64) * 90
                };
                (ts_ms, raw_90k)
            };
            let pkt = if safe_ts != packet.pts_ms || safe_pts_90k != packet.pts {
                Arc::new(CompressedPacket {
                    kind: packet.kind,
                    pts_ms: safe_ts,
                    pts: safe_pts_90k,
                    dts: safe_pts_90k,
                    duration: packet.duration,
                    is_keyframe: packet.is_keyframe,
                    data: packet.data.clone(),
                    stems: packet.stems.clone(),
                })
            } else {
                packet
            };
            guard.push_back((safe_ts, ctx, pkt));
            if let Some(&(newest_ts, _, _)) = guard.back() {
                let effective_keep_ms = max_dur.max(120_000);
                let cutoff = newest_ts.saturating_sub(effective_keep_ms + 5000);
                while let Some(&(oldest_ts, _, ref p)) = guard.front() {
                    if oldest_ts < cutoff
                        || self.video_bytes.load(Ordering::Acquire) > v_byte_limit
                    {
                        self.video_bytes
                            .fetch_sub(p.data.len(), Ordering::Release);
                        guard.pop_front();
                    } else {
                        break;
                    }
                }
            }
        }

        pub fn push_audio(&self, ts_ms: u64, packet: Arc<CompressedPacket>) {
            let p_len = packet.byte_footprint();
            self.audio_bytes.fetch_add(p_len, Ordering::Release);
            let max_dur = self.max_duration_ms.load(Ordering::Acquire);
            let a_byte_limit = self.max_audio_bytes.load(Ordering::Acquire);
            let ctx = self.active_context();
            let mut guard = self.audio_items.lock();
            guard.push_back((ts_ms, ctx, packet));
            if let Some(&(newest_ts, _, _)) = guard.back() {
                let effective_keep_ms = max_dur.max(120_000);
                let cutoff = newest_ts.saturating_sub(effective_keep_ms + 5000);
                while let Some(&(oldest_ts, _, ref p)) = guard.front() {
                    if oldest_ts < cutoff
                        || self.audio_bytes.load(Ordering::Acquire) > a_byte_limit
                    {
                        self.audio_bytes
                            .fetch_sub(p.byte_footprint(), Ordering::Release);
                        guard.pop_front();
                    } else {
                        break;
                    }
                }
            }
        }

        pub fn snapshot(&self) -> (Vec<Arc<CompressedPacket>>, Vec<Arc<CompressedPacket>>) {
            let max_dur = self.max_duration_ms.load(Ordering::Acquire);
            let newest = self
                .video_items
                .lock()
                .back()
                .map(|(ts, _, _)| *ts)
                .unwrap_or(0);
            let since = newest.saturating_sub(max_dur);
            self.snapshot_since(Some(since))
        }

        pub fn snapshot_since(
            &self,
            since_ms: Option<u64>,
        ) -> (Vec<Arc<CompressedPacket>>, Vec<Arc<CompressedPacket>>) {
            let max_dur = self.max_duration_ms.load(Ordering::Acquire);
            let (video_slice, start_pts, end_pts) = {
                let v_guard = self.video_items.lock();
                if v_guard.is_empty() {
                    return (Vec::new(), Vec::new());
                }
                let newest = v_guard.back().map(|(ts, _, _)| *ts).unwrap_or(0);
                let effective_keep_ms = max_dur.max(120_000);
                let min_ts = since_ms
                    .map(|s| s.max(newest.saturating_sub(effective_keep_ms)))
                    .unwrap_or_else(|| newest.saturating_sub(max_dur));

                let first_key_idx = v_guard
                    .iter()
                    .rposition(|(ts, _, p)| *ts <= min_ts && p.is_keyframe)
                    .or_else(|| {
                        v_guard
                            .iter()
                            .position(|(ts, _, p)| *ts >= min_ts && p.is_keyframe)
                    })
                    .or_else(|| v_guard.iter().position(|(_, _, p)| p.is_keyframe))
                    .unwrap_or(0);

                let v_slice: Vec<Arc<CompressedPacket>> = v_guard
                    .iter()
                    .skip(first_key_idx)
                    .map(|(_, _, p)| Arc::clone(p))
                    .collect();
                let start_pts = v_slice.first().map(|p| p.pts_ms).unwrap_or(min_ts);
                let end_pts = v_slice.last().map(|p| p.pts_ms).unwrap_or(u64::MAX);
                (v_slice, start_pts, end_pts)
            };

            let audio_slice: Vec<Arc<CompressedPacket>> = {
                let a_guard = self.audio_items.lock();
                a_guard
                    .iter()
                    .filter(|(ts, _, _)| {
                        *ts >= start_pts.saturating_sub(500)
                            && (end_pts == u64::MAX || *ts <= end_pts.saturating_add(200))
                    })
                    .map(|(_, _, p)| Arc::clone(p))
                    .collect()
            };

            (video_slice, audio_slice)
        }

        pub fn snapshot_game_only_since(
            &self,
            since_ms: Option<u64>,
        ) -> (Vec<Arc<CompressedPacket>>, Vec<Arc<CompressedPacket>>) {
            let max_dur = self.max_duration_ms.load(Ordering::Acquire);
            let mut game_ranges: Vec<(u64, u64)> = Vec::new();

            let video_slice: Vec<Arc<CompressedPacket>> = {
                let v_guard = self.video_items.lock();
                if v_guard.is_empty() {
                    return (Vec::new(), Vec::new());
                }
                let newest = v_guard.back().map(|(ts, _, _)| *ts).unwrap_or(0);
                let effective_keep_ms = max_dur.max(120_000);
                let min_ts = since_ms
                    .map(|s| s.max(newest.saturating_sub(effective_keep_ms)))
                    .unwrap_or_else(|| newest.saturating_sub(max_dur));

                let start_idx = v_guard
                    .iter()
                    .rposition(|(ts, ctx, p)| {
                        *ts <= min_ts && *ctx == CaptureContext::Game && p.is_keyframe
                    })
                    .or_else(|| {
                        v_guard
                            .iter()
                            .position(|(ts, ctx, p)| {
                                *ts >= min_ts && *ctx == CaptureContext::Game && p.is_keyframe
                            })
                    });

                let Some(first_idx) = start_idx else {
                    return (Vec::new(), Vec::new());
                };

                let mut out_v = Vec::new();
                let mut in_game_gop = false;
                let mut seg_start: Option<u64> = None;
                let mut seg_end: u64 = 0;

                for &(ts, ctx, ref pkt) in v_guard.iter().skip(first_idx) {
                    if ctx == CaptureContext::Game {
                        if !in_game_gop {
                            if pkt.is_keyframe {
                                in_game_gop = true;
                                seg_start = Some(ts);
                                seg_end = ts;
                                out_v.push(Arc::clone(pkt));
                            }
                        } else {
                            seg_end = ts;
                            out_v.push(Arc::clone(pkt));
                        }
                    } else if in_game_gop {
                        in_game_gop = false;
                        if let Some(s) = seg_start.take() {
                            game_ranges.push((s, seg_end));
                        }
                    }
                }
                if let Some(s) = seg_start.take() {
                    game_ranges.push((s, seg_end));
                }
                out_v
            };

            if video_slice.is_empty() {
                return (Vec::new(), Vec::new());
            }

            let audio_slice: Vec<Arc<CompressedPacket>> = {
                let a_guard = self.audio_items.lock();
                let mut matched: Vec<Arc<CompressedPacket>> = a_guard
                    .iter()
                    .filter(|(ts, _, _)| {
                        game_ranges.iter().any(|&(s, e)| {
                            *ts >= s.saturating_sub(250) && *ts <= e.saturating_add(250)
                        })
                    })
                    .map(|(_, _, p)| Arc::clone(p))
                    .collect();
                if matched.is_empty() {
                    if let (Some(first_v), Some(last_v)) = (video_slice.first(), video_slice.last()) {
                        let s = first_v.pts_ms.saturating_sub(350);
                        let e = last_v.pts_ms.saturating_add(350);
                        matched = a_guard
                            .iter()
                            .filter(|(ts, _, _)| *ts >= s && *ts <= e)
                            .map(|(_, _, p)| Arc::clone(p))
                            .collect();
                    }
                }
                matched
            };

            (video_slice, audio_slice)
        }

        pub fn snapshot_smart_manual(
            &self,
            since_ms: Option<u64>,
        ) -> (Vec<Arc<CompressedPacket>>, Vec<Arc<CompressedPacket>>, CaptureContext) {
            let current_ctx = self.active_context();
            if current_ctx == CaptureContext::Game {
                let (v, a) = self.snapshot_game_only_since(since_ms);
                if !v.is_empty() {
                    return (v, a, CaptureContext::Game);
                }
            }
            let (v, a) = self.snapshot_since(since_ms);
            (v, a, current_ctx)
        }

        pub fn len(&self) -> usize {
            self.video_items.lock().len() + self.audio_items.lock().len()
        }

        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        pub fn clear(&self) {
            self.video_items.lock().clear();
            self.audio_items.lock().clear();
            self.video_bytes.store(0, Ordering::Release);
            self.audio_bytes.store(0, Ordering::Release);
        }
    }
}

pub mod encoder {
    use super::buffer::CompressedPacket;
    use bytes::Bytes;
    use super::config::RecorderConfig;
    use mp4::{
        AacConfig, AudioObjectType, AvcConfig, ChannelConfig, Mp4Config, Mp4Sample, Mp4Writer,
        SampleFreqIndex, TrackConfig,
    };
    use openh264::encoder::Complexity;
    use std::fs::File;
    use std::io::BufWriter;
    use std::path::Path;

    pub fn derive_complexity(_quality: super::config::Quality) -> Complexity {
        Complexity::Low
    }

    #[derive(Clone, Debug)]
    pub struct CodecContextInfo {
        pub width: u32,
        pub height: u32,
        pub fps: u32,
        pub sps: Vec<u8>,
        pub pps: Vec<u8>,
    }

    pub fn split_annex_b(bytes: &[u8]) -> Vec<&[u8]> {
        let mut nals = Vec::new();
        let len = bytes.len();
        let mut i = 0;
        while i < len {
            let start_len = if i + 3 < len
                && bytes[i] == 0
                && bytes[i + 1] == 0
                && bytes[i + 2] == 0
                && bytes[i + 3] == 1
            {
                4
            } else if i + 2 < len && bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
                3
            } else {
                i += 1;
                continue;
            };

            let nal_start = i + start_len;
            let mut next_i = nal_start;
            while next_i < len {
                if (next_i + 3 < len
                    && bytes[next_i] == 0
                    && bytes[next_i + 1] == 0
                    && bytes[next_i + 2] == 0
                    && bytes[next_i + 3] == 1)
                    || (next_i + 2 < len
                        && bytes[next_i] == 0
                        && bytes[next_i + 1] == 0
                        && bytes[next_i + 2] == 1)
                {
                    break;
                }
                next_i += 1;
            }
            if nal_start < next_i {
                nals.push(&bytes[nal_start..next_i]);
            }
            i = next_i;
        }
        nals
    }

    pub struct EncoderPipeline;

    fn write_ebml_id(out: &mut Vec<u8>, id: u32) {
        if id <= 0xFF {
            out.push(id as u8);
        } else if id <= 0xFFFF {
            out.extend_from_slice(&(id as u16).to_be_bytes());
        } else if id <= 0xFF_FFFF {
            out.push(((id >> 16) & 0xFF) as u8);
            out.push(((id >> 8) & 0xFF) as u8);
            out.push((id & 0xFF) as u8);
        } else {
            out.extend_from_slice(&id.to_be_bytes());
        }
    }

    fn write_ebml_size(out: &mut Vec<u8>, size: u64) {
        if size < 126 {
            out.push(0x80 | (size as u8));
        } else if size < 16382 {
            let v = 0x4000u16 | (size as u16);
            out.extend_from_slice(&v.to_be_bytes());
        } else if size < 268_435_454 {
            let v = 0x1000_0000u32 | (size as u32);
            out.extend_from_slice(&v.to_be_bytes());
        } else {
            let v = 0x0100_0000_0000_0000u64 | size;
            out.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn write_ebml_element(out: &mut Vec<u8>, id: u32, payload: &[u8]) {
        write_ebml_id(out, id);
        write_ebml_size(out, payload.len() as u64);
        out.extend_from_slice(payload);
    }

    fn write_ebml_uint(out: &mut Vec<u8>, id: u32, val: u64) {
        let be = val.to_be_bytes();
        let first_nz = be.iter().position(|&b| b != 0).unwrap_or(7);
        write_ebml_element(out, id, &be[first_nz..]);
    }

    fn write_ebml_float64(out: &mut Vec<u8>, id: u32, val: f64) {
        write_ebml_element(out, id, &val.to_be_bytes());
    }

    fn write_ebml_str(out: &mut Vec<u8>, id: u32, s: &str) {
        write_ebml_element(out, id, s.as_bytes());
    }

    fn strip_annexb_prefix(nal: &[u8]) -> &[u8] {
        if nal.len() >= 4 && nal[0] == 0 && nal[1] == 0 && nal[2] == 0 && nal[3] == 1 {
            &nal[4..]
        } else if nal.len() >= 3 && nal[0] == 0 && nal[1] == 0 && nal[2] == 1 {
            &nal[3..]
        } else {
            nal
        }
    }

    fn build_avcc_codec_private(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let sps_clean = strip_annexb_prefix(sps);
        let pps_clean = strip_annexb_prefix(pps);
        let profile_idc = sps_clean.get(1).copied().unwrap_or(100);
        let profile_compat = sps_clean.get(2).copied().unwrap_or(0);
        let level_idc = sps_clean.get(3).copied().unwrap_or(40);
        let mut avcc = Vec::with_capacity(11 + sps_clean.len() + pps_clean.len());
        avcc.push(1);
        avcc.push(profile_idc);
        avcc.push(profile_compat);
        avcc.push(level_idc);
        avcc.push(0xFF);
        avcc.push(0xE1);
        avcc.extend_from_slice(&(sps_clean.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps_clean);
        avcc.push(1);
        avcc.extend_from_slice(&(pps_clean.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps_clean);
        avcc
    }

    impl EncoderPipeline {
        pub fn mux_mkv_clip(
            video_packets: &[std::sync::Arc<CompressedPacket>],
            audio_packets: &[std::sync::Arc<CompressedPacket>],
            output_path: &Path,
            config: &RecorderConfig,
            info: &CodecContextInfo,
        ) -> Result<(), String> {
            use std::io::Write;
            if video_packets.is_empty() && audio_packets.is_empty() {
                return Err("cannot mux empty packet list".into());
            }

            let temp_path = output_path.with_extension("mkv.tmp");
            let mux_res = (|| -> Result<(), String> {
                let file = File::create(&temp_path)
                    .map_err(|e| format!("failed to create temporary mkv file: {e}"))?;
                let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, file);

                let mut ebml_hdr_body = Vec::with_capacity(64);
                write_ebml_uint(&mut ebml_hdr_body, 0x4286, 1);
                write_ebml_uint(&mut ebml_hdr_body, 0x42F7, 1);
                write_ebml_uint(&mut ebml_hdr_body, 0x42F2, 4);
                write_ebml_uint(&mut ebml_hdr_body, 0x42F3, 8);
                write_ebml_str(&mut ebml_hdr_body, 0x4282, "matroska");
                write_ebml_uint(&mut ebml_hdr_body, 0x4287, 4);
                write_ebml_uint(&mut ebml_hdr_body, 0x4285, 2);

                let mut header_bytes = Vec::with_capacity(128);
                write_ebml_element(&mut header_bytes, 0x1A45DFA3, &ebml_hdr_body);
                write_ebml_id(&mut header_bytes, 0x18538067);
                header_bytes.extend_from_slice(&0x01FF_FFFF_FFFF_FFFFu64.to_be_bytes());
                writer
                    .write_all(&header_bytes)
                    .map_err(|e| format!("write mkv header failed: {e}"))?;

                let first_v_pts = video_packets
                    .first()
                    .map(|p| p.pts_ms)
                    .or_else(|| audio_packets.first().map(|p| p.pts_ms))
                    .unwrap_or(0);

                let n_v = video_packets.len();
                let fps_val = info.fps.max(1);
                let nominal_ms: u64 = (1000 / fps_val as u64).max(1);
                let jitter_limit_ms: u64 = (nominal_ms * 3).max(45);
                let mut v_norm_pts: Vec<u64> = Vec::with_capacity(n_v);
                let mut running_v_ms: u64 = 0;
                let _ = jitter_limit_ms;
                for i in 0..n_v {
                    v_norm_pts.push(running_v_ms);
                    let dur_ms = if i + 1 < n_v {
                        video_packets[i + 1]
                            .pts_ms
                            .saturating_sub(video_packets[i].pts_ms)
                            .clamp(1, 10_000)
                    } else {
                        nominal_ms
                    };
                    running_v_ms += dur_ms;
                }
                let total_duration_ms = running_v_ms.max(100);

                let mut info_body = Vec::with_capacity(64);
                write_ebml_uint(&mut info_body, 0x2AD7B1, 1_000_000);
                write_ebml_float64(&mut info_body, 0x4489, total_duration_ms as f64);
                write_ebml_str(&mut info_body, 0x4D80, "Clotch");
                write_ebml_str(&mut info_body, 0x5741, "Clotch");
                let mut info_elem = Vec::with_capacity(80);
                write_ebml_element(&mut info_elem, 0x1549A966, &info_body);
                writer
                    .write_all(&info_elem)
                    .map_err(|e| format!("write mkv SegmentInfo failed: {e}"))?;

                let has_audio = !audio_packets.is_empty();
                let has_stems = has_audio
                    && config.separate_audio_tracks
                    && audio_packets.iter().any(|p| p.stems.is_some());

                let mut tracks_body = Vec::with_capacity(512);
                if !video_packets.is_empty() {
                    let mut v_entry = Vec::with_capacity(160);
                    write_ebml_uint(&mut v_entry, 0xD7, 1);
                    write_ebml_uint(&mut v_entry, 0x73C5, 1);
                    write_ebml_uint(&mut v_entry, 0x83, 1);
                    write_ebml_str(&mut v_entry, 0x86, "V_MPEG4/ISO/AVC");
                    let avcc = build_avcc_codec_private(&info.sps, &info.pps);
                    write_ebml_element(&mut v_entry, 0x63A2, &avcc);
                    let frame_ns = (1_000_000_000u64 / (fps_val as u64)).max(1);
                    write_ebml_uint(&mut v_entry, 0x23E383, frame_ns);
                    let mut v_sub = Vec::with_capacity(32);
                    write_ebml_uint(&mut v_sub, 0xB0, info.width.max(64) as u64);
                    write_ebml_uint(&mut v_sub, 0xBA, info.height.max(64) as u64);
                    write_ebml_element(&mut v_entry, 0xE0, &v_sub);
                    write_ebml_element(&mut tracks_body, 0xAE, &v_entry);
                }

                if has_audio {
                    let aac_asc: [u8; 2] = [0x11, 0x90];
                    let audio_track_count = if has_stems { 5usize } else { 1usize };
                    let track_names = ["Full Mix", "Game", "Discord", "Microphone", "Other"];
                    for a_idx in 0..audio_track_count {
                        let t_num = (a_idx + 2) as u64;
                        let mut a_entry = Vec::with_capacity(96);
                        write_ebml_uint(&mut a_entry, 0xD7, t_num);
                        write_ebml_uint(&mut a_entry, 0x73C5, t_num);
                        write_ebml_uint(&mut a_entry, 0x83, 2);
                        write_ebml_str(&mut a_entry, 0x536E, track_names[a_idx]);
                        write_ebml_str(&mut a_entry, 0x86, "A_AAC");
                        write_ebml_element(&mut a_entry, 0x63A2, &aac_asc);
                        let mut a_sub = Vec::with_capacity(24);
                        write_ebml_float64(&mut a_sub, 0xB5, 48000.0);
                        write_ebml_uint(&mut a_sub, 0x9F, 2);
                        write_ebml_element(&mut a_entry, 0xE1, &a_sub);
                        write_ebml_element(&mut tracks_body, 0xAE, &a_entry);
                    }
                }

                let mut tracks_elem = Vec::with_capacity(tracks_body.len() + 16);
                write_ebml_element(&mut tracks_elem, 0x1654AE6B, &tracks_body);
                writer
                    .write_all(&tracks_elem)
                    .map_err(|e| format!("write mkv Tracks failed: {e}"))?;

                enum MkvMuxItem<'a> {
                    Video {
                        pts_ms: u64,
                        is_keyframe: bool,
                        data: &'a [u8],
                    },
                    Audio {
                        pts_ms: u64,
                        pkt: &'a CompressedPacket,
                    },
                }

                let start_a_idx = audio_packets
                    .iter()
                    .position(|p| p.pts_ms.saturating_add(22) >= first_v_pts)
                    .unwrap_or(0);
                let usable_audio = if has_audio {
                    &audio_packets[start_a_idx..]
                } else {
                    &[]
                };

                let mut items: Vec<MkvMuxItem<'_>> =
                    Vec::with_capacity(video_packets.len() + usable_audio.len());
                for (i, vp) in video_packets.iter().enumerate() {
                    items.push(MkvMuxItem::Video {
                        pts_ms: v_norm_pts[i],
                        is_keyframe: vp.is_keyframe,
                        data: &vp.data,
                    });
                }
                for (i, ap) in usable_audio.iter().enumerate() {
                    let a_pts_ms = ap
                        .pts_ms
                        .checked_sub(first_v_pts)
                        .unwrap_or_else(|| ((i as u64) * 1024) / 48);
                    if a_pts_ms <= total_duration_ms.saturating_add(250) {
                        items.push(MkvMuxItem::Audio {
                            pts_ms: a_pts_ms,
                            pkt: ap,
                        });
                    }
                }
                items.sort_by_key(|it| match it {
                    MkvMuxItem::Video { pts_ms, .. } => (*pts_ms, 0u8),
                    MkvMuxItem::Audio { pts_ms, .. } => (*pts_ms, 1u8),
                });

                let mut cluster_body: Vec<u8> = Vec::with_capacity(512 * 1024);
                let mut cluster_start_ms: u64 = 0;
                let mut cluster_open = false;
                let mut block_buf: Vec<u8> = Vec::with_capacity(128 * 1024);

                let flush_cluster =
                    |writer: &mut BufWriter<File>, body: &mut Vec<u8>| -> Result<(), String> {
                        if body.is_empty() {
                            return Ok(());
                        }
                        let mut hdr = Vec::with_capacity(12);
                        write_ebml_id(&mut hdr, 0x1F43B675);
                        write_ebml_size(&mut hdr, body.len() as u64);
                        writer
                            .write_all(&hdr)
                            .map_err(|e| format!("write cluster header: {e}"))?;
                        writer
                            .write_all(body)
                            .map_err(|e| format!("write cluster body: {e}"))?;
                        body.clear();
                        Ok(())
                    };

                for item in items {
                    let item_pts = match item {
                        MkvMuxItem::Video { pts_ms, .. } => pts_ms,
                        MkvMuxItem::Audio { pts_ms, .. } => pts_ms,
                    };
                    let is_v_kf = matches!(
                        item,
                        MkvMuxItem::Video {
                            is_keyframe: true,
                            ..
                        }
                    );
                    let rel_ms = item_pts.saturating_sub(cluster_start_ms);
                    if !cluster_open || rel_ms > 30_000 || (is_v_kf && rel_ms >= 1000) {
                        if cluster_open {
                            flush_cluster(&mut writer, &mut cluster_body)?;
                        }
                        cluster_start_ms = item_pts;
                        cluster_open = true;
                        write_ebml_uint(&mut cluster_body, 0xE7, cluster_start_ms);
                    }
                    let rel_i16 = (item_pts.saturating_sub(cluster_start_ms)).min(32767) as i16;

                    match item {
                        MkvMuxItem::Video {
                            is_keyframe, data, ..
                        } => {
                            block_buf.clear();
                            block_buf.push(0x81);
                            block_buf.extend_from_slice(&rel_i16.to_be_bytes());
                            block_buf.push(if is_keyframe { 0x80 } else { 0x00 });
                            block_buf.extend_from_slice(data);
                            write_ebml_element(&mut cluster_body, 0xA3, &block_buf);
                        }
                        MkvMuxItem::Audio { pkt, .. } => {
                            block_buf.clear();
                            block_buf.push(0x82);
                            block_buf.extend_from_slice(&rel_i16.to_be_bytes());
                            block_buf.push(0x80);
                            block_buf.extend_from_slice(&pkt.data);
                            write_ebml_element(&mut cluster_body, 0xA3, &block_buf);

                            if has_stems {
                                if let Some(ref stems) = pkt.stems {
                                    let stem_slices: [(u8, &[u8]); 4] = [
                                        (0x83, &stems.game),
                                        (0x84, &stems.discord),
                                        (0x85, &stems.mic),
                                        (0x86, &stems.other),
                                    ];
                                    for (t_byte, s_data) in stem_slices {
                                        block_buf.clear();
                                        block_buf.push(t_byte);
                                        block_buf.extend_from_slice(&rel_i16.to_be_bytes());
                                        block_buf.push(0x80);
                                        block_buf.extend_from_slice(s_data);
                                        write_ebml_element(&mut cluster_body, 0xA3, &block_buf);
                                    }
                                }
                            }
                        }
                    }
                }

                if cluster_open {
                    flush_cluster(&mut writer, &mut cluster_body)?;
                }

                writer
                    .flush()
                    .map_err(|e| format!("flush mkv writer: {e}"))?;
                let inner = writer
                    .into_inner()
                    .map_err(|e| format!("unwrap mkv writer: {e}"))?;
                inner
                    .sync_all()
                    .map_err(|e| format!("fsync mkv file: {e}"))?;
                drop(inner);

                if output_path.exists() {
                    let _ = std::fs::remove_file(output_path);
                }
                std::fs::rename(&temp_path, output_path)
                    .map_err(|e| format!("rename mkv file failed: {e}"))?;
                Ok(())
            })();

            if mux_res.is_err() {
                let _ = std::fs::remove_file(&temp_path);
            }
            mux_res
        }

        pub fn mux_clip(
            video_packets: &[std::sync::Arc<CompressedPacket>],
            audio_packets: &[std::sync::Arc<CompressedPacket>],
            output_path: &Path,
            config: &RecorderConfig,
            info: &CodecContextInfo,
        ) -> Result<(), String> {
            use std::io::Write;
            if video_packets.is_empty() && audio_packets.is_empty() {
                return Err("cannot mux empty packet list".into());
            }

            let is_mkv_ext = output_path
                .extension()
                .and_then(|s| s.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("mkv") || ext.eq_ignore_ascii_case("matroska"))
                .unwrap_or(false);
            if is_mkv_ext {
                return Self::mux_mkv_clip(video_packets, audio_packets, output_path, config, info);
            }

            let temp_path = output_path.with_extension("mp4.tmp");
            let mux_result = (|| -> Result<(), String> {
                let file = File::create(&temp_path)
                    .map_err(|e| format!("failed to create temporary output file: {e}"))?;
                let buf_writer = BufWriter::with_capacity(4 * 1024 * 1024, file);

                let brand_isom = "isom".parse().map_err(|e| format!("{e}"))?;
                let brand_iso2 = "iso2".parse().map_err(|e| format!("{e}"))?;
                let brand_avc1 = "avc1".parse().map_err(|e| format!("{e}"))?;
                let brand_mp41 = "mp41".parse().map_err(|e| format!("{e}"))?;

                let mp4_cfg = Mp4Config {
                    major_brand: brand_isom,
                    minor_version: 512,
                    compatible_brands: vec![brand_isom, brand_iso2, brand_avc1, brand_mp41],
                    timescale: 90_000,
                };

                let mut writer = Mp4Writer::write_start(buf_writer, &mp4_cfg)
                    .map_err(|e| format!("failed to start mp4 writer: {e}"))?;

                let has_video = !video_packets.is_empty();
                let video_track_id = if has_video {
                    let avc_conf = AvcConfig {
                        width: info.width as u16,
                        height: info.height as u16,
                        seq_param_set: info.sps.clone(),
                        pic_param_set: info.pps.clone(),
                    };
                    let mut video_track_conf = TrackConfig::from(avc_conf);
                    video_track_conf.timescale = 90_000;
                    writer
                        .add_track(&video_track_conf)
                        .map_err(|e| format!("failed to add video track: {e}"))?;
                    Some(1u32)
                } else {
                    None
                };

                let has_audio = !audio_packets.is_empty();
                let has_stems = has_audio
                    && config.separate_audio_tracks
                    && audio_packets.iter().any(|p| p.stems.is_some());
                let (audio_track_id, stem_track_ids) = if has_audio {
                    let aac_conf = AacConfig {
                        bitrate: config.audio_bitrate_kbps.clamp(64, 192) * 1000,
                        profile: AudioObjectType::AacLowComplexity,
                        freq_index: SampleFreqIndex::Freq48000,
                        chan_conf: ChannelConfig::Stereo,
                    };
                    let mut audio_track_conf = TrackConfig::from(aac_conf);
                    audio_track_conf.timescale = 48_000;
                    let track_id = if video_track_id.is_some() { 2u32 } else { 1u32 };
                    writer
                        .add_track(&audio_track_conf)
                        .map_err(|e| format!("failed to add audio track: {e}"))?;
                    let stems_ids = if has_stems {
                        for _ in 0..4 {
                            writer
                                .add_track(&audio_track_conf)
                                .map_err(|e| format!("failed to add audio stem track: {e}"))?;
                        }
                        Some((track_id + 1, track_id + 2, track_id + 3, track_id + 4))
                    } else {
                        None
                    };
                    (Some(track_id), stems_ids)
                } else {
                    (None, None)
                };

                let first_v_pts = video_packets
                    .first()
                    .map(|p| p.pts_ms)
                    .or_else(|| audio_packets.first().map(|p| p.pts_ms))
                    .unwrap_or(0);
                let mut total_video_duration_ms: u64 = 0;

                if let Some(v_track) = video_track_id {
                    let n = video_packets.len();
                    let fps_val = info.fps.max(1);
                    let nominal_ticks: u32 = (90_000 / fps_val).max(1);
                    let nominal_ms: u64 = (1000 / fps_val as u64).max(1);
                    let jitter_limit_ms: u64 = (nominal_ms * 3).max(45);
                    let mut running_start_ticks: u64 = 0;

                    for i in 0..n {
                        let p = &video_packets[i];
                        let _ = jitter_limit_ms;
                        let dur_ticks = if i + 1 < n {
                            let next_p = &video_packets[i + 1];
                            let cur_is_90k = p.pts >= (p.pts_ms as i64) * 45 && p.pts > 0;
                            let next_is_90k = next_p.pts >= (next_p.pts_ms as i64) * 45 && next_p.pts > 0;
                            if cur_is_90k && next_is_90k && next_p.pts > p.pts {
                                ((next_p.pts - p.pts) as u32).clamp(1, 900_000)
                            } else {
                                let raw_delta_ms = next_p.pts_ms.saturating_sub(p.pts_ms).max(1);
                                ((raw_delta_ms * 90) as u32).clamp(1, 900_000)
                            }
                        } else {
                            nominal_ticks
                        };

                        let sample = Mp4Sample {
                            start_time: running_start_ticks,
                            duration: dur_ticks,
                            rendering_offset: 0,
                            is_sync: p.is_keyframe,
                            bytes: p.data.clone(),
                        };
                        writer
                            .write_sample(v_track, &sample)
                            .map_err(|e| format!("failed to write video sample {i}: {e}"))?;
                        running_start_ticks += dur_ticks as u64;
                        total_video_duration_ms = (running_start_ticks + 45) / 90;
                    }
                }

                if let Some(a_track) = audio_track_id {
                    let start_idx = audio_packets
                        .iter()
                        .position(|p| p.pts_ms.saturating_add(22) >= first_v_pts)
                        .unwrap_or(0);
                    let usable_audio = &audio_packets[start_idx..];
                    let first_a_pts = usable_audio
                        .first()
                        .map(|p| p.pts_ms)
                        .unwrap_or(first_v_pts);
                    let initial_offset_ticks = first_a_pts
                        .checked_sub(first_v_pts)
                        .map(|delta| delta.min(200) * 48)
                        .unwrap_or(0);
                    let max_audio_ticks = if total_video_duration_ms > 0 {
                        total_video_duration_ms.saturating_add(250) * 48
                    } else {
                        u64::MAX
                    };
                    let mut current_a_ticks = initial_offset_ticks;

                    let mut silent_fill_frame: Option<Bytes> = None;

                    for (i, p) in usable_audio.iter().enumerate() {
                        if total_video_duration_ms > 0 && current_a_ticks >= max_audio_ticks {
                            break;
                        }
                        let dur: u32 = 1024;

                        let sample = Mp4Sample {
                            start_time: current_a_ticks,
                            duration: dur,
                            rendering_offset: 0,
                            is_sync: true,
                            bytes: p.data.clone(),
                        };
                        writer
                            .write_sample(a_track, &sample)
                            .map_err(|e| format!("failed to write audio sample {i}: {e}"))?;
                        if let (Some((t_game, t_discord, t_mic, t_other)), Some(ref stems)) =
                            (stem_track_ids, &p.stems)
                        {
                            let game_sample = Mp4Sample {
                                start_time: current_a_ticks,
                                duration: dur,
                                rendering_offset: 0,
                                is_sync: true,
                                bytes: stems.game.clone(),
                            };
                            let discord_sample = Mp4Sample {
                                start_time: current_a_ticks,
                                duration: dur,
                                rendering_offset: 0,
                                is_sync: true,
                                bytes: stems.discord.clone(),
                            };
                            let mic_sample = Mp4Sample {
                                start_time: current_a_ticks,
                                duration: dur,
                                rendering_offset: 0,
                                is_sync: true,
                                bytes: stems.mic.clone(),
                            };
                            let other_sample = Mp4Sample {
                                start_time: current_a_ticks,
                                duration: dur,
                                rendering_offset: 0,
                                is_sync: true,
                                bytes: stems.other.clone(),
                            };
                            writer
                                .write_sample(t_game, &game_sample)
                                .map_err(|e| format!("failed to write game stem sample {i}: {e}"))?;
                            writer
                                .write_sample(t_discord, &discord_sample)
                                .map_err(|e| format!("failed to write discord stem sample {i}: {e}"))?;
                            writer
                                .write_sample(t_mic, &mic_sample)
                                .map_err(|e| format!("failed to write mic stem sample {i}: {e}"))?;
                            writer
                                .write_sample(t_other, &other_sample)
                                .map_err(|e| format!("failed to write other stem sample {i}: {e}"))?;
                        }
                        current_a_ticks += dur as u64;

                        if i + 1 < usable_audio.len() {
                            let next_pts = usable_audio[i + 1].pts_ms;
                            let delta_ms = next_pts.saturating_sub(p.pts_ms);
                            if delta_ms >= 45 {
                                let gap_ticks = (delta_ms * 48).saturating_sub(1024);
                                let missing_frames = (gap_ticks / 1024).min(240);
                                if missing_frames > 0 {
                                    let sil = silent_fill_frame
                                        .get_or_insert_with(|| {
                                            crate::audio::build_silent_aac_packet(
                                                config.audio_bitrate_kbps,
                                            )
                                        })
                                        .clone();
                                    for _ in 0..missing_frames {
                                        if total_video_duration_ms > 0
                                            && current_a_ticks >= max_audio_ticks
                                        {
                                            break;
                                        }
                                        let sil_sample = Mp4Sample {
                                            start_time: current_a_ticks,
                                            duration: 1024,
                                            rendering_offset: 0,
                                            is_sync: true,
                                            bytes: sil.clone(),
                                        };
                                        writer
                                            .write_sample(a_track, &sil_sample)
                                            .map_err(|e| {
                                                format!("failed to write silent audio sample: {e}")
                                            })?;
                                        if let Some((t_game, t_discord, t_mic, t_other)) =
                                            stem_track_ids
                                        {
                                            let _ = writer.write_sample(t_game, &sil_sample);
                                            let _ = writer.write_sample(t_discord, &sil_sample);
                                            let _ = writer.write_sample(t_mic, &sil_sample);
                                            let _ = writer.write_sample(t_other, &sil_sample);
                                        }
                                        current_a_ticks += 1024;
                                    }
                                }
                            }
                        }
                    }
                }

                writer
                    .write_end()
                    .map_err(|e| format!("failed to finalize mp4: {e}"))?;

                let mut buf_writer = writer.into_writer();
                buf_writer
                    .flush()
                    .map_err(|e| format!("failed to flush mp4 buffer: {e}"))?;
                let inner_file = buf_writer
                    .into_inner()
                    .map_err(|e| format!("failed to unwrap mp4 buffered writer: {e}"))?;
                inner_file
                    .sync_all()
                    .map_err(|e| format!("failed to fsync temporary mp4 file: {e}"))?;
                drop(inner_file);

                if output_path.exists() {
                    let _ = std::fs::remove_file(output_path);
                }
                std::fs::rename(&temp_path, output_path)
                    .map_err(|e| format!("failed to atomically rename clip file: {e}"))?;
                Ok(())
            })();

            if mux_result.is_err() {
                let _ = std::fs::remove_file(&temp_path);
            }
            mux_result
        }
    }
}

pub mod capture {
    pub fn is_known_game_executable(exe_lower: &str) -> bool {
        let stem = exe_lower.trim_end_matches(".exe");
        matches!(
            stem,
            "cs2"
                | "valorant-win64-shipping"
                | "valorant"
                | "dota2"
                | "league of legends"
                | "rocketleague"
                | "overwatch"
                | "cod"
                | "cod24-cod"
                | "cod23-cod"
                | "cod22-cod"
                | "blackops6"
                | "modernwarfare"
                | "mw2"
                | "mw3"
                | "warzone"
                | "blackopscoldwar"
                | "vanguard"
                | "s2_mp64_ship"
                | "iw8_ship"
                | "t9_ship"
                | "s4_ship"
                | "sp24-cod"
                | "bf2042"
                | "bf6"
                | "bfv"
                | "bf1"
                | "bf4"
                | "battlefield2042"
                | "battlefieldv"
                | "battlefield1"
                | "r5apex"
                | "r5apex_dx12"
                | "fortniteclient-win64-shipping"
                | "fortniteclient-win64-shipping_eac"
                | "fortniteclient-win64-shipping_be"
                | "rainbowsix"
                | "rainbowsix_vulkan"
                | "rainbowsix_dx11"
                | "rainbowsix_be"
                | "discovery"
                | "discovery-d"
                | "discovery-win64-shipping"
                | "thefinals"
                | "pioneergame"
                | "pioneergame-win64-shipping"
                | "arcraiders"
                | "arcraiders-win64-shipping"
                | "marvel-win64-shipping"
                | "marvel"
                | "marvelrivals"
                | "fc26"
                | "fc26_trial"
                | "easportsfc26"
                | "fc27"
                | "fc27_trial"
                | "easportsfc27"
                | "efootball"
                | "efootball2026"
                | "efootball2027"
                | "pes2021"
                | "minecraft.windows"
                | "javaw"
                | "lunarclient"
                | "badlionclient"
                | "feather"
        )
    }

    pub fn classify_executable_context(
        exe_lower: &str,
        fg_pid: u32,
        active_pid_filter: Option<u32>,
        active_game_name: Option<&str>,
    ) -> super::buffer::CaptureContext {
        use super::buffer::CaptureContext;
        if let Some(t_pid) = active_pid_filter {
            if t_pid != 0 && fg_pid == t_pid {
                return CaptureContext::Game;
            }
        }
        if let Some(g_name) = active_game_name {
            if !g_name.eq_ignore_ascii_case("desktop")
                && matches_game_executable(exe_lower, &g_name.to_lowercase())
            {
                return CaptureContext::Game;
            }
        }
        if is_known_game_executable(exe_lower) {
            return CaptureContext::Game;
        }
        let stem = exe_lower.trim_end_matches(".exe");
        if matches!(
            stem,
            "discord"
                | "discordptb"
                | "discordcanary"
                | "vesktop"
                | "teamspeak"
                | "ts3client_win64"
                | "slack"
                | "telegram"
                | "whatsapp"
                | "guilded"
        ) {
            return CaptureContext::Social;
        }
        if matches!(
            stem,
            "chrome"
                | "msedge"
                | "firefox"
                | "brave"
                | "opera"
                | "vivaldi"
                | "vlc"
                | "mpv"
                | "potplayer"
                | "potplayermini64"
                | "spotify"
                | "netflix"
        ) {
            return CaptureContext::Media;
        }
        CaptureContext::Desktop
    }

    pub fn matches_game_executable(exe_lower: &str, game_lower: &str) -> bool {
        if exe_lower == "explorer.exe" || exe_lower.is_empty() || game_lower.is_empty() {
            return false;
        }
        let stem = exe_lower.trim_end_matches(".exe");
        let g = game_lower.trim_end_matches(".exe");
        if stem == g || exe_lower.contains(g) {
            return true;
        }
        match g {
            "valorant" => stem.contains("valorant-win64-shipping") || stem == "valorant",
            "cs2" | "counter-strike 2" => stem == "cs2",
            "dota2" | "dota 2" => stem == "dota2",
            "lol" | "league_of_legends" | "league of legends" => {
                stem == "league of legends" || stem.contains("league")
            }
            "rocket_league" | "rocketleague" | "rocket league" => stem == "rocketleague",
            "overwatch2" | "overwatch" | "overwatch 2" => stem == "overwatch",
            "warzone" | "cod" | "call_of_duty" | "call of duty" => {
                matches!(
                    stem,
                    "cod"
                        | "cod24-cod"
                        | "cod23-cod"
                        | "cod22-cod"
                        | "blackops6"
                        | "modernwarfare"
                        | "mw2"
                        | "mw3"
                        | "warzone"
                        | "blackopscoldwar"
                        | "vanguard"
                        | "s2_mp64_ship"
                        | "iw8_ship"
                        | "t9_ship"
                        | "s4_ship"
                        | "sp24-cod"
                )
            }
            "battlefield" => {
                matches!(
                    stem,
                    "bf2042"
                        | "bf6"
                        | "bfv"
                        | "bf1"
                        | "bf4"
                        | "battlefield2042"
                        | "battlefieldv"
                        | "battlefield1"
                )
            }
            "apex_legends" | "apex" | "apex legends" => {
                matches!(stem, "r5apex" | "r5apex_dx12")
            }
            "fortnite" => {
                matches!(
                    stem,
                    "fortniteclient-win64-shipping"
                        | "fortniteclient-win64-shipping_eac"
                        | "fortniteclient-win64-shipping_be"
                )
            }
            "rainbow_six_siege" | "rainbowsix" | "r6" | "rainbow six siege" => {
                matches!(
                    stem,
                    "rainbowsix" | "rainbowsix_vulkan" | "rainbowsix_dx11" | "rainbowsix_be"
                )
            }
            "the_finals" | "thefinals" | "the finals" => {
                matches!(
                    stem,
                    "discovery" | "discovery-d" | "discovery-win64-shipping" | "thefinals"
                )
            }
            "arc_raiders" | "arcraiders" | "arc raiders" => {
                matches!(
                    stem,
                    "pioneergame"
                        | "pioneergame-win64-shipping"
                        | "arcraiders"
                        | "arcraiders-win64-shipping"
                )
            }
            "marvel_rivals" | "marvelrivals" | "marvel rivals" => {
                matches!(stem, "marvel-win64-shipping" | "marvel" | "marvelrivals")
            }
            "fc26" | "ea sports fc 26" | "easportsfc26" => {
                matches!(stem, "fc26" | "fc26_trial" | "easportsfc26")
            }
            "fc27" | "ea sports fc 27" | "easportsfc27" => {
                matches!(stem, "fc27" | "fc27_trial" | "easportsfc27")
            }
            "efootball" | "efootball 2026" | "efootball 2027" | "pes" => {
                matches!(stem, "efootball" | "efootball2026" | "efootball2027" | "pes2021")
            }
            "minecraft" => {
                matches!(
                    stem,
                    "minecraft.windows" | "javaw" | "java" | "lunarclient" | "badlionclient" | "feather"
                )
            }
            _ => false,
        }
    }

    #[cfg(windows)]
    mod win_impl {
        use super::matches_game_executable;
        use super::super::buffer::{CaptureContext, CompressedPacket, PacketKind, RingBuffer};
        use super::super::encoder::{split_annex_b, CodecContextInfo};
        use bytes::Bytes;
        use std::ffi::c_void;
        use std::mem::ManuallyDrop;
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use windows::core::{Interface, GUID};
        use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
        use windows::Win32::Graphics::Direct3D::{
            D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_11_1,
        };
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource,
            ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
            ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
            ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
            D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
            D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
            D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
            D3D11_VPOV_DIMENSION_TEXTURE2D,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
        };
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
            IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, DXGI_ADAPTER_DESC1,
            DXGI_OUTPUT_DESC,
        };
        use windows::Win32::Graphics::Gdi::{
            MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTONEAREST,
        };
        use windows::Win32::Media::MediaFoundation::{
            IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator,
            IMFMediaType, IMFSample, IMFTransform, MFCreateDXGIDeviceManager,
            MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
            MFTEnumEx, MFMediaType_Video, MFSampleExtension_CleanPoint, MFVideoFormat_H264,
            MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
            MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_FLAG_TRANSCODE_ONLY,
            MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
            MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
            MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
            MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
            MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
            MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
            MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER,
            MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC,
            MF_TRANSFORM_ASYNC_UNLOCK,
        };
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::System::Performance::{
            QueryPerformanceCounter, QueryPerformanceFrequency,
        };
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetClientRect, GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
            IsWindow, IsWindowVisible,
        };

        static LAST_CHECKED_HWND: AtomicUsize = AtomicUsize::new(0);
        static LAST_CHECKED_PID: AtomicU32 = AtomicU32::new(0);
        static LAST_CHECKED_FOCUSED: AtomicBool = AtomicBool::new(true);
        static LAST_CTX_HWND: AtomicUsize = AtomicUsize::new(0);
        static LAST_CTX_TARGET_PID: AtomicU32 = AtomicU32::new(0);
        static LAST_CTX_VAL: AtomicUsize = AtomicUsize::new(0);

        const VENDOR_NVIDIA: u32 = 0x10DE;
        const VENDOR_AMD: u32 = 0x1002;
        const VENDOR_INTEL: u32 = 0x8086;

        const CODECAPI_AV_LOW_LATENCY_MODE: GUID =
            GUID::from_u128(0x9c27891a_ed7a_40e1_88e8_b22727a024ee);
        const CODECAPI_AV_ENC_COMMON_REALTIME: GUID =
            GUID::from_u128(0x165e109e_b06d_4c6e_8147_3c86538f2510);
        const CODECAPI_AV_ENC_MPV_DEFAULT_B_PICTURE_COUNT: GUID =
            GUID::from_u128(0x4d030034_39b0_4821_b444_c00896b70a2b);
        const CODECAPI_AV_ENC_COMMON_QUALITY_VS_SPEED: GUID =
            GUID::from_u128(0x98332df8_03cd_476b_89fa_3f9e442dec9f);
        const MF_MT_MPEG2_LEVEL_GUID: GUID =
            GUID::from_u128(0x96f66574_11c5_4015_8666_bff516436da7);
        const MF_MT_VIDEO_NOMINAL_RANGE_GUID: GUID =
            GUID::from_u128(0xc21b8ee5_b956_4071_8daf_325edf5cab11);
        const IID_ICODEC_API: GUID =
            GUID::from_u128(0x901db4c7_31ce_41a2_85dc_8fa0bf41b8da);

        #[repr(C)]
        struct CodecApiVariant {
            vt: u16,
            r1: u16,
            r2: u16,
            r3: u16,
            val_u64: u64,
            pad_u64: u64,
        }

        unsafe fn apply_codec_api_low_latency_no_bframes(mft: &IMFTransform) {
            if let Ok(attrs) = mft.GetAttributes() {
                let _ = attrs.SetUINT32(&CODECAPI_AV_LOW_LATENCY_MODE, 1);
                let _ = attrs.SetUINT32(&CODECAPI_AV_ENC_COMMON_REALTIME, 1);
                let _ = attrs.SetUINT32(&CODECAPI_AV_ENC_MPV_DEFAULT_B_PICTURE_COUNT, 0);
                let _ = attrs.SetUINT32(&CODECAPI_AV_ENC_COMMON_QUALITY_VS_SPEED, 65);
            }
            let mut codec_api_ptr: *mut c_void = std::ptr::null_mut();
            if mft.query(&IID_ICODEC_API, &mut codec_api_ptr).is_ok() && !codec_api_ptr.is_null() {
                let vtbl = *(codec_api_ptr as *const *const usize);
                let set_value_fn: unsafe extern "system" fn(
                    *mut c_void,
                    *const GUID,
                    *const CodecApiVariant,
                ) -> i32 = std::mem::transmute(*vtbl.add(9));
                let release_fn: unsafe extern "system" fn(*mut c_void) -> u32 =
                    std::mem::transmute(*vtbl.add(2));

                let var_bool_true = CodecApiVariant {
                    vt: 11,
                    r1: 0,
                    r2: 0,
                    r3: 0,
                    val_u64: 0xFFFF,
                    pad_u64: 0,
                };
                let var_ui4_one = CodecApiVariant {
                    vt: 19,
                    r1: 0,
                    r2: 0,
                    r3: 0,
                    val_u64: 1,
                    pad_u64: 0,
                };
                let var_ui4_zero = CodecApiVariant {
                    vt: 19,
                    r1: 0,
                    r2: 0,
                    r3: 0,
                    val_u64: 0,
                    pad_u64: 0,
                };

                if set_value_fn(codec_api_ptr, &CODECAPI_AV_LOW_LATENCY_MODE, &var_bool_true) < 0 {
                    let _ = set_value_fn(codec_api_ptr, &CODECAPI_AV_LOW_LATENCY_MODE, &var_ui4_one);
                }
                if set_value_fn(codec_api_ptr, &CODECAPI_AV_ENC_COMMON_REALTIME, &var_bool_true) < 0
                {
                    let _ = set_value_fn(
                        codec_api_ptr,
                        &CODECAPI_AV_ENC_COMMON_REALTIME,
                        &var_ui4_one,
                    );
                }
                let var_ui4_qvs = CodecApiVariant {
                    vt: 19,
                    r1: 0,
                    r2: 0,
                    r3: 0,
                    val_u64: 65,
                    pad_u64: 0,
                };
                let _ = set_value_fn(
                    codec_api_ptr,
                    &CODECAPI_AV_ENC_MPV_DEFAULT_B_PICTURE_COUNT,
                    &var_ui4_zero,
                );
                let _ = set_value_fn(
                    codec_api_ptr,
                    &CODECAPI_AV_ENC_COMMON_QUALITY_VS_SPEED,
                    &var_ui4_qvs,
                );
                release_fn(codec_api_ptr);
            }
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum EncoderBackendKind {
            NvencD3d11,
            AmfMft,
            QuickSyncMft,
            SoftwareMft,
        }

        pub fn query_qpc() -> i64 {
            let mut count = 0i64;
            unsafe {
                let _ = QueryPerformanceCounter(&mut count);
            }
            count
        }

        pub fn query_qpc_freq() -> i64 {
            let mut freq = 0i64;
            unsafe {
                let _ = QueryPerformanceFrequency(&mut freq);
            }
            freq.max(1)
        }

        pub fn is_valid_game_window(hwnd: HWND) -> bool {
            if hwnd.0.is_null() {
                return false;
            }
            unsafe {
                IsWindow(hwnd).as_bool()
                    && IsWindowVisible(hwnd).as_bool()
                    && !IsIconic(hwnd).as_bool()
            }
        }

        pub fn detect_foreground_context(
            active_pid_filter: Option<u32>,
            active_game_name: Option<&str>,
        ) -> CaptureContext {
            use super::classify_executable_context;
            unsafe {
                let hwnd = GetForegroundWindow();
                if hwnd.0.is_null() {
                    return CaptureContext::from_usize(LAST_CTX_VAL.load(Ordering::Relaxed));
                }
                let raw_hwnd = hwnd.0 as usize;
                let t_pid = active_pid_filter.unwrap_or(0);
                if raw_hwnd == LAST_CTX_HWND.load(Ordering::Relaxed)
                    && t_pid == LAST_CTX_TARGET_PID.load(Ordering::Relaxed)
                {
                    return CaptureContext::from_usize(LAST_CTX_VAL.load(Ordering::Relaxed));
                }
                let mut fg_pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut fg_pid));
                let mut exe_lower = String::new();
                if fg_pid != 0 {
                    if let Ok(proc_handle) =
                        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, fg_pid)
                    {
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
                        if ok && len > 0 {
                            let path_str = String::from_utf16_lossy(&exe_buf[..len as usize]);
                            if let Some(fname) = Path::new(&path_str)
                                .file_name()
                                .and_then(|n| n.to_str())
                            {
                                exe_lower = fname.to_lowercase();
                            }
                        }
                    }
                }
                let ctx = classify_executable_context(
                    &exe_lower,
                    fg_pid,
                    active_pid_filter,
                    active_game_name,
                );
                LAST_CTX_HWND.store(raw_hwnd, Ordering::Relaxed);
                LAST_CTX_TARGET_PID.store(t_pid, Ordering::Relaxed);
                LAST_CTX_VAL.store(ctx as usize, Ordering::Relaxed);
                ctx
            }
        }

        pub fn is_target_process_focused(target_pid: Option<u32>) -> bool {
            let Some(t_pid) = target_pid else {
                return true;
            };
            if t_pid == 0 {
                return true;
            }
            unsafe {
                let hwnd = GetForegroundWindow();
                if hwnd.0.is_null() {
                    return false;
                }
                let raw_hwnd = hwnd.0 as usize;
                if raw_hwnd == LAST_CHECKED_HWND.load(Ordering::Relaxed)
                    && t_pid == LAST_CHECKED_PID.load(Ordering::Relaxed)
                {
                    return LAST_CHECKED_FOCUSED.load(Ordering::Relaxed);
                }
                let mut fg_pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut fg_pid));
                let matched = if fg_pid == t_pid {
                    true
                } else if let Some(game_hwnd) = find_window_by_pid(t_pid) {
                    !IsIconic(game_hwnd).as_bool()
                } else {
                    !IsIconic(hwnd).as_bool()
                };
                LAST_CHECKED_HWND.store(raw_hwnd, Ordering::Relaxed);
                LAST_CHECKED_PID.store(t_pid, Ordering::Relaxed);
                LAST_CHECKED_FOCUSED.store(matched, Ordering::Relaxed);
                matched
            }
        }

        pub fn find_window_by_pid(target_pid: u32) -> Option<HWND> {
            struct SearchState {
                target_pid: u32,
                best_hwnd: Option<HWND>,
                best_area: i32,
            }

            unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
                let state = unsafe { &mut *(lparam.0 as *mut SearchState) };
                let mut proc_id = 0u32;
                unsafe {
                    GetWindowThreadProcessId(hwnd, Some(&mut proc_id));
                }
                if proc_id == state.target_pid {
                    unsafe {
                        if IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() {
                            let mut rect = RECT::default();
                            if GetClientRect(hwnd, &mut rect).is_ok() {
                                let width = rect.right - rect.left;
                                let height = rect.bottom - rect.top;
                                if width >= 640 && height >= 480 {
                                    let area = width * height;
                                    if area > state.best_area {
                                        state.best_area = area;
                                        state.best_hwnd = Some(hwnd);
                                    }
                                }
                            }
                        }
                    }
                }
                BOOL(1)
            }

            let mut state = SearchState {
                target_pid,
                best_hwnd: None,
                best_area: 0,
            };

            unsafe {
                let _ = EnumWindows(Some(enum_proc), LPARAM(&mut state as *mut _ as isize));
            }

            state.best_hwnd
        }

        pub fn find_target_window(game_name: Option<&str>) -> Option<(HWND, u32)> {
            let target = game_name?;
            let target_lower = target.to_lowercase();
            if target_lower.is_empty() || target_lower == "desktop" {
                return None;
            }

            struct NameSearchState {
                target_lower: String,
                found: Option<(HWND, u32)>,
                best_score: i64,
            }

            unsafe extern "system" fn enum_by_name(hwnd: HWND, lparam: LPARAM) -> BOOL {
                let state = unsafe { &mut *(lparam.0 as *mut NameSearchState) };
                unsafe {
                    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                        return BOOL(1);
                    }
                    let mut rect = RECT::default();
                    if GetClientRect(hwnd, &mut rect).is_err() {
                        return BOOL(1);
                    }
                    let w = rect.right - rect.left;
                    let h = rect.bottom - rect.top;
                    if w < 640 || h < 480 {
                        return BOOL(1);
                    }

                    let mut pid = 0u32;
                    GetWindowThreadProcessId(hwnd, Some(&mut pid));
                    if pid != 0 {
                        if let Ok(proc_handle) =
                            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                        {
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
                            if ok && len > 0 {
                                let path_str = String::from_utf16_lossy(&exe_buf[..len as usize]);
                                if let Some(fname) = Path::new(&path_str)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                {
                                    let f_lower = fname.to_lowercase();
                                    if matches_game_executable(&f_lower, &state.target_lower) {
                                        let shipping_bonus = if f_lower.contains("shipping") {
                                            10_000_000i64
                                        } else {
                                            0i64
                                        };
                                        let score = (w as i64) * (h as i64) + shipping_bonus;
                                        if score > state.best_score {
                                            state.best_score = score;
                                            state.found = Some((hwnd, pid));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                BOOL(1)
            }

            let mut state = NameSearchState {
                target_lower,
                found: None,
                best_score: 0,
            };

            unsafe {
                let _ = EnumWindows(Some(enum_by_name), LPARAM(&mut state as *mut _ as isize));
            }

            state.found
        }

        pub struct DxgiDuplicator {
            pub device: ID3D11Device,
            pub context: ID3D11DeviceContext,
            pub video_device: Option<ID3D11VideoDevice>,
            pub video_context: Option<ID3D11VideoContext>,
            pub duplication: Option<IDXGIOutputDuplication>,
            pub adapter: IDXGIAdapter1,
            pub monitor: HMONITOR,
            pub output: IDXGIOutput,
            pub vendor_id: u32,
            pub adapter_name: String,
            pub driver_version: String,
            pub src_width: u32,
            pub src_height: u32,
        }

        impl DxgiDuplicator {
            fn try_create_from_device(
                device: ID3D11Device,
                context: ID3D11DeviceContext,
                target_monitor: Option<HMONITOR>,
            ) -> Result<Self, String> {
                if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                    unsafe {
                        let _ = mt.SetMultithreadProtected(true);
                    }
                }

                let video_device: Option<ID3D11VideoDevice> = device.cast().ok();
                let video_context: Option<ID3D11VideoContext> = context.cast().ok();

                let dxgi_device: IDXGIDevice = device
                    .cast()
                    .map_err(|e| format!("cast IDXGIDevice: {e}"))?;
                let dxgi_adapter: IDXGIAdapter = unsafe {
                    dxgi_device
                        .GetAdapter()
                        .map_err(|e| format!("IDXGIDevice::GetAdapter: {e}"))?
                };
                let adapter1: IDXGIAdapter1 = dxgi_adapter
                    .cast()
                    .map_err(|e| format!("cast IDXGIAdapter1: {e}"))?;
                let adapter_desc: DXGI_ADAPTER_DESC1 = unsafe {
                    adapter1
                        .GetDesc1()
                        .map_err(|e| format!("GetDesc1: {e}"))?
                };

                let mut outputs: Vec<(IDXGIOutput, DXGI_OUTPUT_DESC)> = Vec::new();
                let mut out_idx = 0u32;
                while let Ok(out) = unsafe { dxgi_adapter.EnumOutputs(out_idx) } {
                    out_idx += 1;
                    if let Ok(desc) = unsafe { out.GetDesc() } {
                        if desc.AttachedToDesktop.as_bool() {
                            if let Some(target_hmon) = target_monitor {
                                if desc.Monitor == target_hmon {
                                    outputs.insert(0, (out, desc));
                                    continue;
                                }
                            }
                            outputs.push((out, desc));
                        }
                    }
                }

                let mut last_err = String::from("no attached outputs on adapter");
                for (output, out_desc) in outputs {
                    let Ok(output1) = output.cast::<IDXGIOutput1>() else {
                        continue;
                    };
                    let duplication_result = unsafe { output1.DuplicateOutput(&device) }
                        .or_else(|_| {
                            output1
                                .cast::<IDXGIOutput5>()
                                .ok()
                                .and_then(|o5| {
                                    let fmts = [DXGI_FORMAT_B8G8R8A8_UNORM];
                                    unsafe { o5.DuplicateOutput1(&device, 0, &fmts) }.ok()
                                })
                                .ok_or_else(|| windows::core::Error::from_win32())
                        });
                    match duplication_result {
                        Ok(duplication) => {
                            let src_width = (out_desc.DesktopCoordinates.right
                                - out_desc.DesktopCoordinates.left)
                                .max(64) as u32;
                            let src_height = (out_desc.DesktopCoordinates.bottom
                                - out_desc.DesktopCoordinates.top)
                                .max(64) as u32;
                            let desc_len = adapter_desc
                                .Description
                                .iter()
                                .position(|&c| c == 0)
                                .unwrap_or(adapter_desc.Description.len());
                            let adapter_name = String::from_utf16_lossy(
                                &adapter_desc.Description[..desc_len],
                            )
                            .trim()
                            .to_string();
                            let umd_ver = unsafe {
                                adapter1
                                    .CheckInterfaceSupport(&IDXGIDevice::IID)
                                    .unwrap_or(0) as u64
                            };
                            let driver_version = if umd_ver != 0 {
                                format!(
                                    "{}.{}.{}.{}",
                                    (umd_ver >> 48) & 0xFFFF,
                                    (umd_ver >> 32) & 0xFFFF,
                                    (umd_ver >> 16) & 0xFFFF,
                                    umd_ver & 0xFFFF
                                )
                            } else {
                                format!("rev_{}", adapter_desc.Revision)
                            };
                            return Ok(Self {
                                device,
                                context,
                                video_device,
                                video_context,
                                duplication: Some(duplication),
                                adapter: adapter1,
                                monitor: out_desc.Monitor,
                                output,
                                vendor_id: adapter_desc.VendorId,
                                adapter_name,
                                driver_version,
                                src_width,
                                src_height,
                            });
                        }
                        Err(e) => {
                            last_err = format!("DuplicateOutput failed: {e}");
                        }
                    }
                }

                Err(last_err)
            }

            pub fn new_for_window(target_hwnd: Option<HWND>) -> Result<Self, String> {
                let target_monitor: Option<HMONITOR> = target_hwnd.and_then(|hwnd| {
                    if hwnd.0.is_null() {
                        None
                    } else {
                        let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
                        if hmon.0.is_null() {
                            None
                        } else {
                            Some(hmon)
                        }
                    }
                });

                let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
                let mut last_error = String::new();
                let video_bgra_flags = windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG(
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | 0x800,
                );

                let mut hw_dev: Option<ID3D11Device> = None;
                let mut hw_ctx: Option<ID3D11DeviceContext> = None;
                let mut hr = unsafe {
                    D3D11CreateDevice(
                        None,
                        D3D_DRIVER_TYPE_HARDWARE,
                        None,
                        video_bgra_flags,
                        Some(&feature_levels),
                        D3D11_SDK_VERSION,
                        Some(&mut hw_dev),
                        None,
                        Some(&mut hw_ctx),
                    )
                };
                if hr.is_err() {
                    hr = unsafe {
                        D3D11CreateDevice(
                            None,
                            D3D_DRIVER_TYPE_HARDWARE,
                            None,
                            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                            Some(&feature_levels),
                            D3D11_SDK_VERSION,
                            Some(&mut hw_dev),
                            None,
                            Some(&mut hw_ctx),
                        )
                    };
                }
                if hr.is_ok() {
                    if let (Some(dev), Some(ctx)) = (hw_dev, hw_ctx) {
                        match Self::try_create_from_device(dev, ctx, target_monitor) {
                            Ok(dupl) => return Ok(dupl),
                            Err(e) => last_error = e,
                        }
                    }
                }

                if let Ok(factory) = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
                    let mut adapter_idx = 0u32;
                    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_idx) } {
                        adapter_idx += 1;
                        let mut device: Option<ID3D11Device> = None;
                        let mut context: Option<ID3D11DeviceContext> = None;
                        let mut hr = unsafe {
                            D3D11CreateDevice(
                                &adapter,
                                D3D_DRIVER_TYPE_UNKNOWN,
                                None,
                                video_bgra_flags,
                                Some(&feature_levels),
                                D3D11_SDK_VERSION,
                                Some(&mut device),
                                None,
                                Some(&mut context),
                            )
                        };
                        if hr.is_err() {
                            hr = unsafe {
                                D3D11CreateDevice(
                                    &adapter,
                                    D3D_DRIVER_TYPE_UNKNOWN,
                                    None,
                                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                                    Some(&feature_levels),
                                    D3D11_SDK_VERSION,
                                    Some(&mut device),
                                    None,
                                    Some(&mut context),
                                )
                            };
                        }
                        if hr.is_ok() {
                            if let (Some(dev), Some(ctx)) = (device, context) {
                                match Self::try_create_from_device(dev, ctx, target_monitor) {
                                    Ok(dupl) => return Ok(dupl),
                                    Err(e) => last_error = e,
                                }
                            }
                        }
                    }
                }

                Err(if last_error.is_empty() {
                    "failed to initialize DXGI desktop duplication".to_string()
                } else {
                    last_error
                })
            }

            pub fn reinitialize_duplication(&mut self) -> Result<(), String> {
                self.duplication = None;

                let output1: IDXGIOutput1 = self
                    .output
                    .cast()
                    .map_err(|e| format!("IDXGIOutput1 cast failed: {e}"))?;
                let dupl_res = unsafe { output1.DuplicateOutput(&self.device) }
                    .or_else(|_| {
                        output1
                            .cast::<IDXGIOutput5>()
                            .ok()
                            .and_then(|o5| {
                                let fmts = [DXGI_FORMAT_B8G8R8A8_UNORM];
                                unsafe { o5.DuplicateOutput1(&self.device, 0, &fmts) }.ok()
                            })
                            .ok_or_else(|| windows::core::Error::from_win32())
                    });
                if let Ok(dupl) = dupl_res {
                    self.duplication = Some(dupl);
                    return Ok(());
                }

                std::thread::sleep(std::time::Duration::from_millis(8));

                let dxgi_device: IDXGIDevice = self
                    .device
                    .cast()
                    .map_err(|e| format!("cast IDXGIDevice: {e}"))?;
                let dxgi_adapter: IDXGIAdapter = unsafe {
                    dxgi_device
                        .GetAdapter()
                        .map_err(|e| format!("GetAdapter: {e}"))?
                };
                let output: IDXGIOutput = unsafe {
                    dxgi_adapter
                        .EnumOutputs(0)
                        .map_err(|e| format!("EnumOutputs(0): {e}"))?
                };
                let output1: IDXGIOutput1 = output
                    .cast()
                    .map_err(|e| format!("IDXGIOutput1 cast failed: {e}"))?;
                let dupl = unsafe { output1.DuplicateOutput(&self.device) }
                    .or_else(|_| {
                        output1
                            .cast::<IDXGIOutput5>()
                            .ok()
                            .and_then(|o5| {
                                let fmts = [DXGI_FORMAT_B8G8R8A8_UNORM];
                                unsafe { o5.DuplicateOutput1(&self.device, 0, &fmts) }.ok()
                            })
                            .ok_or_else(|| windows::core::Error::from_win32())
                    })
                    .map_err(|e| format!("re-DuplicateOutput failed: {e}"))?;
                if let Ok(d) = unsafe { output.GetDesc() } {
                    self.monitor = d.Monitor;
                }
                self.output = output;
                self.duplication = Some(dupl);
                Ok(())
            }
        }

        pub fn normalize_video_input_format(
            fmt: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        ) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
            use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
            match fmt.0 {
                87 | 88 | 90 | 91 => DXGI_FORMAT(87),
                27 | 28 | 29 => DXGI_FORMAT(28),
                23 | 24 => DXGI_FORMAT(24),
                9 | 10 => DXGI_FORMAT(10),
                _ => DXGI_FORMAT(87),
            }
        }

        pub struct GpuColorConverter {
            processor_enum: Option<ID3D11VideoProcessorEnumerator>,
            processor: Option<ID3D11VideoProcessor>,
            pub src_copy_tex: ID3D11Texture2D,
            pub src_copy_res: ID3D11Resource,
            pub bgra_staging_tex: Option<ID3D11Texture2D>,
            input_view: Option<ID3D11VideoProcessorInputView>,
            pub nv12_gpu_tex: ID3D11Texture2D,
            pub nv12_staging_tex: ID3D11Texture2D,
            pub nv12_staging_tex_alt: ID3D11Texture2D,
            pub staging_flip: AtomicUsize,
            pub staging_primed: AtomicBool,
            output_view: Option<ID3D11VideoProcessorOutputView>,
            pub src_width: u32,
            pub src_height: u32,
            pub dst_width: u32,
            pub dst_height: u32,
            pub raw_src_format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        }

        impl GpuColorConverter {
            pub fn new(
                device: &ID3D11Device,
                video_device: &Option<ID3D11VideoDevice>,
                src_w: u32,
                src_h: u32,
                dst_w: u32,
                dst_h: u32,
                fps: u32,
            ) -> Result<Self, String> {
                Self::new_with_format(
                    device,
                    video_device,
                    src_w,
                    src_h,
                    dst_w,
                    dst_h,
                    fps,
                    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(87),
                )
            }

            pub fn clamp_resolution_for_vram(
                device: &ID3D11Device,
                req_w: u32,
                req_h: u32,
            ) -> (u32, u32) {
                if req_w <= 2560 && req_h <= 1440 {
                    return ((req_w.max(64)) & !1, (req_h.max(64)) & !1);
                }
                let vram_mb = unsafe {
                    device
                        .cast::<IDXGIDevice>()
                        .ok()
                        .and_then(|dxgi| dxgi.GetAdapter().ok())
                        .and_then(|adap| adap.cast::<IDXGIAdapter1>().ok())
                        .and_then(|adap1| adap1.GetDesc1().ok())
                        .map(|desc| (desc.DedicatedVideoMemory as u64) / (1024 * 1024))
                        .unwrap_or(0)
                };
                if vram_mb > 0 && vram_mb <= 4500 {
                    let scale = (1920.0 / req_w.max(1) as f64)
                        .min(1080.0 / req_h.max(1) as f64)
                        .min(1.0);
                    let w = (((req_w as f64 * scale).round() as u32).max(64)) & !1;
                    let h = (((req_h as f64 * scale).round() as u32).max(64)) & !1;
                    (w, h)
                } else {
                    ((req_w.max(64)) & !1, (req_h.max(64)) & !1)
                }
            }

            pub fn new_with_format(
                device: &ID3D11Device,
                video_device: &Option<ID3D11VideoDevice>,
                src_w: u32,
                src_h: u32,
                dst_w: u32,
                dst_h: u32,
                fps: u32,
                raw_src_format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
            ) -> Result<Self, String> {
                let (dst_w, dst_h) = Self::clamp_resolution_for_vram(device, dst_w, dst_h);
                let norm_fmt = normalize_video_input_format(raw_src_format);
                let src_copy_desc = D3D11_TEXTURE2D_DESC {
                    Width: src_w,
                    Height: src_h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: norm_fmt,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };

                let gpu_tex_desc = D3D11_TEXTURE2D_DESC {
                    Width: dst_w,
                    Height: dst_h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };

                let staging_tex_desc = D3D11_TEXTURE2D_DESC {
                    Width: dst_w,
                    Height: dst_h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | 0x10000) as u32,
                    MiscFlags: 0,
                };

                let staging_tex_desc_ro = D3D11_TEXTURE2D_DESC {
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..staging_tex_desc
                };

                let mut src_copy_tex: Option<ID3D11Texture2D> = None;
                let mut nv12_gpu_tex: Option<ID3D11Texture2D> = None;
                let mut nv12_staging_tex: Option<ID3D11Texture2D> = None;
                let mut nv12_staging_tex_alt: Option<ID3D11Texture2D> = None;

                unsafe {
                    device
                        .CreateTexture2D(&src_copy_desc, None, Some(&mut src_copy_tex))
                        .map_err(|e| format!("CreateTexture2D src_copy: {e}"))?;
                    device
                        .CreateTexture2D(&gpu_tex_desc, None, Some(&mut nv12_gpu_tex))
                        .map_err(|e| format!("CreateTexture2D NV12 GPU: {e}"))?;
                    if device
                        .CreateTexture2D(&staging_tex_desc, None, Some(&mut nv12_staging_tex))
                        .is_err()
                    {
                        device
                            .CreateTexture2D(&staging_tex_desc_ro, None, Some(&mut nv12_staging_tex))
                            .map_err(|e| format!("CreateTexture2D NV12 staging: {e}"))?;
                    }
                    if device
                        .CreateTexture2D(&staging_tex_desc, None, Some(&mut nv12_staging_tex_alt))
                        .is_err()
                    {
                        device
                            .CreateTexture2D(
                                &staging_tex_desc_ro,
                                None,
                                Some(&mut nv12_staging_tex_alt),
                            )
                            .map_err(|e| format!("CreateTexture2D NV12 staging alt: {e}"))?;
                    }
                }

                let src_copy_tex = src_copy_tex.ok_or("null src_copy texture")?;
                let src_copy_res: ID3D11Resource = src_copy_tex
                    .cast()
                    .map_err(|e| format!("cast src_copy ID3D11Resource: {e}"))?;
                let nv12_gpu_tex = nv12_gpu_tex.ok_or("null NV12 GPU texture")?;
                let nv12_staging_tex = nv12_staging_tex.ok_or("null NV12 staging texture")?;
                let nv12_staging_tex_alt =
                    nv12_staging_tex_alt.ok_or("null NV12 staging alt texture")?;
                let nv12_gpu_res: ID3D11Resource = nv12_gpu_tex
                    .cast()
                    .map_err(|e| format!("cast ID3D11Resource: {e}"))?;

                let mut processor_enum_opt = None;
                let mut processor_opt = None;
                let mut input_view_opt = None;
                let mut output_view_opt = None;

                if let Some(ref vd) = video_device {
                    let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                        InputFrameRate: DXGI_RATIONAL {
                            Numerator: fps.max(1),
                            Denominator: 1,
                        },
                        InputWidth: src_w,
                        InputHeight: src_h,
                        OutputFrameRate: DXGI_RATIONAL {
                            Numerator: fps.max(1),
                            Denominator: 1,
                        },
                        OutputWidth: dst_w,
                        OutputHeight: dst_h,
                        Usage: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_USAGE(0),
                    };

                    if let Ok(pe) = unsafe { vd.CreateVideoProcessorEnumerator(&content_desc) } {
                        if let Ok(proc) = unsafe { vd.CreateVideoProcessor(&pe, 0) } {
                            let in_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                                FourCC: 0,
                                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                                Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                                    Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPIV {
                                        MipSlice: 0,
                                        ArraySlice: 0,
                                    },
                                },
                            };
                            let out_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                                Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                                    Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPOV { MipSlice: 0 },
                                },
                            };
                            let mut iv: Option<ID3D11VideoProcessorInputView> = None;
                            let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
                            let iv_ok = unsafe {
                                vd.CreateVideoProcessorInputView(
                                    &src_copy_res,
                                    &pe,
                                    &in_view_desc,
                                    Some(&mut iv),
                                )
                                .is_ok()
                            };
                            let ov_ok = unsafe {
                                vd.CreateVideoProcessorOutputView(
                                    &nv12_gpu_res,
                                    &pe,
                                    &out_view_desc,
                                    Some(&mut ov),
                                )
                                .is_ok()
                            };
                            if iv_ok && ov_ok && iv.is_some() && ov.is_some() {
                                processor_enum_opt = Some(pe);
                                processor_opt = Some(proc);
                                input_view_opt = iv;
                                output_view_opt = ov;
                            }
                        }
                    }
                }

                let bgra_staging_tex = if processor_opt.is_none() {
                    let bgra_stg_desc = D3D11_TEXTURE2D_DESC {
                        Width: src_w,
                        Height: src_h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: norm_fmt,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut stg: Option<ID3D11Texture2D> = None;
                    unsafe {
                        let _ = device.CreateTexture2D(&bgra_stg_desc, None, Some(&mut stg));
                    }
                    stg
                } else {
                    None
                };

                Ok(Self {
                    processor_enum: processor_enum_opt,
                    processor: processor_opt,
                    src_copy_tex,
                    src_copy_res,
                    bgra_staging_tex,
                    input_view: input_view_opt,
                    nv12_gpu_tex,
                    nv12_staging_tex,
                    nv12_staging_tex_alt,
                    staging_flip: AtomicUsize::new(0),
                    staging_primed: AtomicBool::new(false),
                    output_view: output_view_opt,
                    src_width: src_w,
                    src_height: src_h,
                    dst_width: dst_w,
                    dst_height: dst_h,
                    raw_src_format,
                })
            }

            pub fn has_hardware_processor(&self) -> bool {
                self.processor.is_some() && self.input_view.is_some() && self.output_view.is_some()
            }

            pub fn stage_desktop_frame(
                &self,
                device_context: &ID3D11DeviceContext,
                bgra_tex: &ID3D11Texture2D,
            ) -> Result<(), String> {
                let bgra_res: ID3D11Resource = bgra_tex
                    .cast()
                    .map_err(|e| format!("bgra cast ID3D11Resource: {e}"))?;
                unsafe {
                    device_context.CopyResource(&self.src_copy_res, &bgra_res);
                }
                Ok(())
            }

            pub fn convert_staged_to_nv12(
                &self,
                device_context: &ID3D11DeviceContext,
                video_context: &Option<ID3D11VideoContext>,
            ) -> Result<&ID3D11Texture2D, String> {
                if let (Some(ref vc), Some(ref proc), Some(ref iv), Some(ref ov)) = (
                    video_context,
                    &self.processor,
                    &self.input_view,
                    &self.output_view,
                ) {
                    unsafe {
                        vc.VideoProcessorSetStreamAutoProcessingMode(proc, 0, BOOL(0));
                    }

                    let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                        Enable: BOOL(1),
                        OutputIndex: 0,
                        InputFrameOrField: 0,
                        PastFrames: 0,
                        FutureFrames: 0,
                        ppPastSurfaces: std::ptr::null_mut(),
                        pInputSurface: ManuallyDrop::new(Some(iv.clone())),
                        ppFutureSurfaces: std::ptr::null_mut(),
                        ppPastSurfacesRight: std::ptr::null_mut(),
                        pInputSurfaceRight: ManuallyDrop::new(None),
                        ppFutureSurfacesRight: std::ptr::null_mut(),
                    };

                    let streams = [stream];
                    let blt_res = unsafe { vc.VideoProcessorBlt(proc, ov, 0, &streams) };

                    let [mut stream_cleanup] = streams;
                    unsafe {
                        let _ = ManuallyDrop::take(&mut stream_cleanup.pInputSurface);
                    }

                    if blt_res.is_ok() {
                        return Ok(&self.nv12_gpu_tex);
                    }
                }

                if let Some(ref bgra_stg) = self.bgra_staging_tex {
                    unsafe {
                        let bgra_stg_res: ID3D11Resource = bgra_stg
                            .cast()
                            .map_err(|e| format!("cast bgra_stg: {e}"))?;
                        device_context.CopyResource(&bgra_stg_res, &self.src_copy_res);
                        let mut mapped_src = D3D11_MAPPED_SUBRESOURCE::default();
                        device_context
                            .Map(&bgra_stg_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped_src))
                            .map_err(|e| format!("Map bgra_stg failed: {e}"))?;
                        let src_pitch = mapped_src.RowPitch as usize;
                        let sw = self.src_width as usize;
                        let sh = self.src_height as usize;
                        let dw = self.dst_width as usize;
                        let dh = self.dst_height as usize;
                        let src_slice =
                            std::slice::from_raw_parts(mapped_src.pData as *const u8, src_pitch * sh);

                        let mut nv12_buf = vec![0u8; dw * dh * 3 / 2];
                        let (y_plane, uv_plane) = nv12_buf.split_at_mut(dw * dh);

                        for dy in 0..dh {
                            let sy = (dy * sh) / dh.max(1);
                            let s_row = sy * src_pitch;
                            let d_row = dy * dw;
                            for dx in 0..dw {
                                let sx = (dx * sw) / dw.max(1);
                                let off = s_row + sx * 4;
                                if off + 2 < src_slice.len() {
                                    let b = src_slice[off] as i32;
                                    let g = src_slice[off + 1] as i32;
                                    let r = src_slice[off + 2] as i32;
                                    y_plane[d_row + dx] =
                                        (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(16, 235)
                                            as u8;
                                }
                            }
                        }

                        for dy in (0..dh).step_by(2) {
                            let sy = (dy * sh) / dh.max(1);
                            let s_row = sy * src_pitch;
                            let uv_row = (dy / 2) * dw;
                            for dx in (0..dw).step_by(2) {
                                let sx = (dx * sw) / dw.max(1);
                                let off = s_row + sx * 4;
                                if off + 2 < src_slice.len() {
                                    let b = src_slice[off] as i32;
                                    let g = src_slice[off + 1] as i32;
                                    let r = src_slice[off + 2] as i32;
                                    uv_plane[uv_row + dx] =
                                        (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128)
                                            .clamp(16, 240) as u8;
                                    uv_plane[uv_row + dx + 1] =
                                        (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128)
                                            .clamp(16, 240) as u8;
                                }
                            }
                        }

                        device_context.Unmap(&bgra_stg_res, 0);

                        let gpu_res: ID3D11Resource = self
                            .nv12_gpu_tex
                            .cast()
                            .map_err(|e| format!("cast nv12_gpu_tex: {e}"))?;
                        device_context.UpdateSubresource(
                            &gpu_res,
                            0,
                            None,
                            nv12_buf.as_ptr() as *const std::ffi::c_void,
                            dw as u32,
                            0,
                        );
                    }
                    return Ok(&self.nv12_gpu_tex);
                }

                Err("no video processor or staging fallback available".to_string())
            }

            pub fn convert_bgra_to_nv12(
                &self,
                device_context: &ID3D11DeviceContext,
                _video_device: &Option<ID3D11VideoDevice>,
                video_context: &Option<ID3D11VideoContext>,
                bgra_tex: &ID3D11Texture2D,
            ) -> Result<&ID3D11Texture2D, String> {
                self.stage_desktop_frame(device_context, bgra_tex)?;
                self.convert_staged_to_nv12(device_context, video_context)
            }
        }

        pub struct HardwareH264Encoder {
            pub backend_kind: EncoderBackendKind,
            transform: IMFTransform,
            event_gen: Option<IMFMediaEventGenerator>,
            dxgi_manager: Option<IMFDXGIDeviceManager>,
            in_stream_id: u32,
            out_stream_id: u32,
            need_input_count: usize,
            input_pool: Vec<(IMFSample, IMFMediaBuffer)>,
            gpu_input_pool: Vec<(ID3D11Texture2D, IMFSample)>,
            pub zero_copy_active: bool,
            pool_idx: usize,
            last_filled_slot: Option<usize>,
            output_sample: Option<IMFSample>,
            output_buffer: IMFMediaBuffer,
            pub width: u32,
            pub height: u32,
            pub fps: u32,
            pub bitrate_bps: u32,
            base_start_ms: u64,
            keyframe_interval_frames: u64,
            encoded_frames: u64,
            produced_packets: u64,
            last_hud_sample_ms: u64,
            sps: Option<Vec<u8>>,
            pps: Option<Vec<u8>>,
            sink: Option<crate::tracker::EventSink>,
        }

        impl HardwareH264Encoder {
            pub fn new(
                duplicator: &DxgiDuplicator,
                width: u32,
                height: u32,
                fps: u32,
                bitrate_bps: u32,
                keyframe_interval_frames: u32,
                base_start_ms: u64,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) -> Result<Self, String> {
                Self::new_with_sink(
                    duplicator,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    keyframe_interval_frames,
                    base_start_ms,
                    codec_info,
                    None,
                )
            }

            pub fn new_with_sink(
                duplicator: &DxgiDuplicator,
                width: u32,
                height: u32,
                fps: u32,
                bitrate_bps: u32,
                keyframe_interval_frames: u32,
                base_start_ms: u64,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
                sink: Option<crate::tracker::EventSink>,
            ) -> Result<Self, String> {
                super::super::ensure_mf_initialized()?;

                let mut reset_token = 0u32;
                let mut dxgi_manager: Option<IMFDXGIDeviceManager> = None;
                unsafe {
                    if MFCreateDXGIDeviceManager(&mut reset_token, &mut dxgi_manager).is_ok() {
                        if let Some(ref mgr) = dxgi_manager {
                            let _ = mgr.ResetDevice(&duplicator.device, reset_token);
                        }
                    }
                }

                let candidates = [
                    EncoderBackendKind::NvencD3d11,
                    EncoderBackendKind::AmfMft,
                    EncoderBackendKind::QuickSyncMft,
                    EncoderBackendKind::SoftwareMft,
                ];

                let mut chosen_transform: Option<(
                    IMFTransform,
                    Option<IMFMediaEventGenerator>,
                    EncoderBackendKind,
                    u32,
                    u32,
                    bool,
                )> = None;

                for &kind in &candidates {
                    if kind == EncoderBackendKind::NvencD3d11
                        && duplicator.vendor_id != VENDOR_NVIDIA
                    {
                        continue;
                    }
                    if kind == EncoderBackendKind::AmfMft && duplicator.vendor_id != VENDOR_AMD {
                        continue;
                    }
                    if kind == EncoderBackendKind::QuickSyncMft
                        && duplicator.vendor_id != VENDOR_INTEL
                    {
                        continue;
                    }

                    if let Ok((mft, ev_gen, in_id, out_id, d3d_bound)) = Self::try_init_mft(
                        kind,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        keyframe_interval_frames,
                        dxgi_manager.as_ref(),
                    ) {
                        chosen_transform = Some((mft, ev_gen, kind, in_id, out_id, d3d_bound));
                        break;
                    }
                }

                if chosen_transform.is_none() {
                    for &kind in &candidates {
                        if let Ok((mft, ev_gen, in_id, out_id, d3d_bound)) = Self::try_init_mft(
                            kind,
                            width,
                            height,
                            fps,
                            bitrate_bps,
                            keyframe_interval_frames,
                            dxgi_manager.as_ref(),
                        ) {
                            chosen_transform = Some((mft, ev_gen, kind, in_id, out_id, d3d_bound));
                            break;
                        }
                    }
                }

                let (transform, event_gen, backend_kind, in_stream_id, out_stream_id, d3d_bound) =
                    chosen_transform.ok_or_else(|| {
                        let msg = "no suitable H264 encoder MFT found on system".to_string();
                        super::super::diag::log_line("ENCODER_ERROR", &msg);
                        msg
                    })?;

                let mut gpu_input_pool: Vec<(ID3D11Texture2D, IMFSample)> = Vec::new();
                let mut zero_copy_active = false;
                if backend_kind != EncoderBackendKind::SoftwareMft && d3d_bound {
                    if let Ok(pool) = Self::allocate_gpu_surface_pool(&duplicator.device, width, height) {
                        gpu_input_pool = pool;
                        zero_copy_active = true;
                    } else {
                        unsafe {
                            let _ = transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
                        }
                        super::super::diag::log_line(
                            "ZERO_COPY_FALLBACK",
                            "MFCreateDXGISurfaceBuffer pool allocation failed; using CPU staging pool",
                        );
                    }
                }

                let nv12_size = (width * height * 3) / 2;
                let (input_pool, output_sample, output_buffer) = unsafe {
                    let mut pool = Vec::new();
                    if !zero_copy_active {
                        pool.reserve_exact(4);
                        for _ in 0..4 {
                            let s =
                                MFCreateSample().map_err(|e| format!("MFCreateSample input: {e}"))?;
                            let b = MFCreateMemoryBuffer(nv12_size)
                                .map_err(|e| format!("MFCreateMemoryBuffer input: {e}"))?;
                            s.AddBuffer(&b)
                                .map_err(|e| format!("AddBuffer input: {e}"))?;
                            pool.push((s, b));
                        }
                    }

                    let out_info = transform
                        .GetOutputStreamInfo(out_stream_id)
                        .map_err(|e| format!("GetOutputStreamInfo: {e}"))?;
                    let mft_provides_samples = (out_info.dwFlags
                        & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                            | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                        != 0;

                    let out_buf_size = out_info.cbSize.max(512 * 1024);
                    let out_buf = MFCreateMemoryBuffer(out_buf_size)
                        .map_err(|e| format!("MFCreateMemoryBuffer output: {e}"))?;

                    let out_sample = if mft_provides_samples {
                        None
                    } else {
                        let s =
                            MFCreateSample().map_err(|e| format!("MFCreateSample output: {e}"))?;
                        s.AddBuffer(&out_buf)
                            .map_err(|e| format!("AddBuffer output: {e}"))?;
                        Some(s)
                    };

                    (pool, out_sample, out_buf)
                };

                super::super::diag::log_startup_gpu_and_encoder(
                    &duplicator.adapter_name,
                    duplicator.vendor_id,
                    &duplicator.driver_version,
                    &format!("{backend_kind:?}"),
                    zero_copy_active,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                );

                let mut enc = Self {
                    backend_kind,
                    transform,
                    event_gen,
                    dxgi_manager,
                    in_stream_id,
                    out_stream_id,
                    need_input_count: 0,
                    input_pool,
                    gpu_input_pool,
                    zero_copy_active,
                    pool_idx: 0,
                    last_filled_slot: None,
                    output_sample,
                    output_buffer,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    base_start_ms,
                    keyframe_interval_frames: keyframe_interval_frames.max(1) as u64,
                    encoded_frames: 0,
                    produced_packets: 0,
                    last_hud_sample_ms: 0,
                    sps: None,
                    pps: None,
                    sink,
                };

                enc.extract_sequence_header_from_media_type(codec_info);
                Ok(enc)
            }

            fn allocate_gpu_surface_pool(
                device: &ID3D11Device,
                width: u32,
                height: u32,
            ) -> Result<Vec<(ID3D11Texture2D, IMFSample)>, String> {
                let tex_desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut pool = Vec::with_capacity(8);
                unsafe {
                    for _ in 0..8 {
                        let mut tex_opt: Option<ID3D11Texture2D> = None;
                        device
                            .CreateTexture2D(&tex_desc, None, Some(&mut tex_opt))
                            .map_err(|e| format!("CreateTexture2D zero-copy pool: {e}"))?;
                        let tex = tex_opt.ok_or_else(|| "null zero-copy texture".to_string())?;
                        let surf_buf = MFCreateDXGISurfaceBuffer(
                            &ID3D11Texture2D::IID,
                            &tex,
                            0,
                            false,
                        )
                        .map_err(|e| format!("MFCreateDXGISurfaceBuffer: {e}"))?;
                        let sample =
                            MFCreateSample().map_err(|e| format!("MFCreateSample dxgi: {e}"))?;
                        sample
                            .AddBuffer(&surf_buf)
                            .map_err(|e| format!("AddBuffer dxgi: {e}"))?;
                        pool.push((tex, sample));
                    }
                }
                Ok(pool)
            }

           pub fn disable_zero_copy(&mut self, reason: &str) {
                if !self.zero_copy_active {
                    return;
                }
                self.zero_copy_active = false;
                self.last_filled_slot = None;
                unsafe {
                    if self.input_pool.is_empty() {
                        let nv12_size = (self.width * self.height * 3) / 2;
                        self.input_pool.reserve_exact(4);
                        for _ in 0..4 {
                            if let (Ok(s), Ok(b)) = (MFCreateSample(), MFCreateMemoryBuffer(nv12_size)) {
                                if s.AddBuffer(&b).is_ok() {
                                    self.input_pool.push((s, b));
                                }
                            }
                        }
                    }
                    let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                    let _ = self.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
                    let _ = self
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                    let _ = self
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                }
                let msg = format!(
                    "Zero-copy GPU surface encoding fell back to CPU staging pool: {reason}"
                );
                super::super::diag::log_line("ZERO_COPY_FALLBACK", &msg);
                if let Some(ref s) = self.sink {
                    s.emit(
                        "recorder.encoder_fallback",
                        None,
                        serde_json::json!({
                            "kind": "zero_copy_to_cpu_staging",
                            "backend": format!("{:?}", self.backend_kind),
                            "reason": reason,
                        }),
                    );
                }
            }

            fn switch_to_software_mft(
                &mut self,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
                reason: &str,
            ) -> bool {
                if self.backend_kind == EncoderBackendKind::SoftwareMft {
                    return false;
                }
                let prev_backend = self.backend_kind;
                if let Ok((mft, ev_gen, in_id, out_id, _)) = Self::try_init_mft(
                    EncoderBackendKind::SoftwareMft,
                    self.width,
                    self.height,
                    self.fps,
                    self.bitrate_bps,
                    self.keyframe_interval_frames as u32,
                    None,
                ) {
                    unsafe {
                        let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                        if let Ok(out_info) = mft.GetOutputStreamInfo(out_id) {
                            let mft_provides_samples = (out_info.dwFlags
                                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                                != 0;
                            if mft_provides_samples {
                                self.output_sample = None;
                            } else if self.output_sample.is_none() {
                                if let Ok(s) = MFCreateSample() {
                                    let _ = s.AddBuffer(&self.output_buffer);
                                    self.output_sample = Some(s);
                                }
                            }
                        }
                    }
                    self.transform = mft;
                    self.event_gen = ev_gen;
                    self.in_stream_id = in_id;
                    self.out_stream_id = out_id;
                    self.backend_kind = EncoderBackendKind::SoftwareMft;
                    self.zero_copy_active = false;
                    self.need_input_count = 0;
                    self.encoded_frames = 0;
                    self.last_filled_slot = None;
                    self.extract_sequence_header_from_media_type(codec_info);

                    let log_msg = format!(
                        "Switched from {prev_backend:?} to SoftwareMft ({}x{} @ {}fps): {reason}",
                        self.width, self.height, self.fps
                    );
                    super::super::diag::log_line("ENCODER_FALLBACK", &log_msg);
                    if let Some(ref s) = self.sink {
                        s.emit(
                            "recorder.encoder_fallback",
                            None,
                            serde_json::json!({
                                "kind": "hardware_to_software_mft",
                                "from_backend": format!("{prev_backend:?}"),
                                "to_backend": "SoftwareMft",
                                "width": self.width,
                                "height": self.height,
                                "fps": self.fps,
                                "reason": reason,
                            }),
                        );
                    }
                    return true;
                }
                false
            }

            fn extract_sequence_header_from_media_type(
                &mut self,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) {
                unsafe {
                    if let Ok(out_type) = self.transform.GetOutputCurrentType(self.out_stream_id) {
                        if let Ok(blob_size) = out_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                            if blob_size > 0 && blob_size < 65536 {
                                let mut seq_hdr = vec![0u8; blob_size as usize];
                                if out_type
                                    .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut seq_hdr, None)
                                    .is_ok()
                                {
                                    for nal in split_annex_b(&seq_hdr) {
                                        if nal.is_empty() {
                                            continue;
                                        }
                                        let nal_type = nal[0] & 0x1F;
                                        if nal_type == 7 {
                                            self.sps = Some(nal.to_vec());
                                        } else if nal_type == 8 {
                                            self.pps = Some(nal.to_vec());
                                        }
                                    }
                                    if let (Some(ref sps), Some(ref pps)) = (&self.sps, &self.pps) {
                                        let mut guard = match codec_info.lock() {
                                            Ok(g) => g,
                                            Err(p) => p.into_inner(),
                                        };
                                        *guard = Some(CodecContextInfo {
                                            width: self.width,
                                            height: self.height,
                                            fps: self.fps,
                                            sps: sps.clone(),
                                            pps: pps.clone(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            fn try_init_mft(
                kind: EncoderBackendKind,
                width: u32,
                height: u32,
                fps: u32,
                bitrate_bps: u32,
                keyframe_interval_frames: u32,
                dxgi_manager: Option<&IMFDXGIDeviceManager>,
            ) -> Result<(IMFTransform, Option<IMFMediaEventGenerator>, u32, u32, bool), String> {
                let input_info = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_NV12,
                };
                let output_info = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_H264,
                };

                let flags = match kind {
                    EncoderBackendKind::NvencD3d11
                    | EncoderBackendKind::AmfMft
                    | EncoderBackendKind::QuickSyncMft => MFT_ENUM_FLAG(
                        MFT_ENUM_FLAG_HARDWARE.0
                            | MFT_ENUM_FLAG_ASYNCMFT.0
                            | MFT_ENUM_FLAG_SYNCMFT.0
                            | MFT_ENUM_FLAG_SORTANDFILTER.0,
                    ),
                    EncoderBackendKind::SoftwareMft => MFT_ENUM_FLAG(
                        MFT_ENUM_FLAG_SYNCMFT.0
                            | MFT_ENUM_FLAG_TRANSCODE_ONLY.0
                            | MFT_ENUM_FLAG_SORTANDFILTER.0,
                    ),
                };

                let mut activates_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut count = 0u32;

                unsafe {
                    MFTEnumEx(
                        MFT_CATEGORY_VIDEO_ENCODER,
                        flags,
                        Some(&input_info as *const MFT_REGISTER_TYPE_INFO),
                        Some(&output_info as *const MFT_REGISTER_TYPE_INFO),
                        &mut activates_ptr,
                        &mut count,
                    )
                    .map_err(|e| format!("MFTEnumEx failed: {e}"))?;
                }

                if count == 0 || activates_ptr.is_null() {
                    return Err("no MFTs returned".to_string());
                }

                let mut selected_mft: Option<(
                    IMFTransform,
                    Option<IMFMediaEventGenerator>,
                    u32,
                    u32,
                    bool,
                )> = None;

                unsafe {
                    let slice = std::slice::from_raw_parts_mut(activates_ptr, count as usize);
                    for slot in slice.iter_mut() {
                        if let Some(activate) = slot.take() {
                            if selected_mft.is_some() {
                                continue;
                            }
                            if let Ok(mft) = activate.ActivateObject::<IMFTransform>() {
                                let is_async = mft
                                    .GetAttributes()
                                    .and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC))
                                    .unwrap_or(0)
                                    != 0;

                                if let Ok(attrs) = mft.GetAttributes() {
                                    if is_async {
                                        let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                                    }
                                }

                                apply_codec_api_low_latency_no_bframes(&mft);

                                let mut d3d_bound = false;
                                if kind != EncoderBackendKind::SoftwareMft {
                                    if let Some(mgr) = dxgi_manager {
                                        let mgr_ptr = mgr.as_raw() as usize;
                                        if mft
                                            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr_ptr)
                                            .is_ok()
                                        {
                                            d3d_bound = true;
                                        }
                                    }
                                }

                                let mut in_id = 0u32;
                                let mut out_id = 0u32;
                                let _ = mft.GetStreamIDs(
                                    std::slice::from_mut(&mut in_id),
                                    std::slice::from_mut(&mut out_id),
                                );

                                let mut configured = Self::configure_media_types(
                                    &mft,
                                    in_id,
                                    out_id,
                                    width,
                                    height,
                                    fps,
                                    bitrate_bps,
                                    keyframe_interval_frames,
                                )
                                .is_ok();

                                if !configured && d3d_bound {
                                    let _ = mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
                                    d3d_bound = false;
                                    configured = Self::configure_media_types(
                                        &mft,
                                        in_id,
                                        out_id,
                                        width,
                                        height,
                                        fps,
                                        bitrate_bps,
                                        keyframe_interval_frames,
                                    )
                                    .is_ok();
                                }

                                if configured {
                                    apply_codec_api_low_latency_no_bframes(&mft);
                                    let ev_gen = if is_async {
                                        mft.cast::<IMFMediaEventGenerator>().ok()
                                    } else {
                                        None
                                    };
                                    let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                                    let _ =
                                        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                                    let _ =
                                        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                                    selected_mft = Some((mft, ev_gen, in_id, out_id, d3d_bound));
                                }
                            }
                        }
                    }
                    CoTaskMemFree(Some(activates_ptr as *const c_void));
                }

                selected_mft.ok_or_else(|| "failed to configure any enumerated MFT".to_string())
            }

            fn configure_media_types(
                mft: &IMFTransform,
                in_id: u32,
                out_id: u32,
                width: u32,
                height: u32,
                fps: u32,
                bitrate_bps: u32,
                keyframe_interval_frames: u32,
            ) -> Result<(), String> {
                let frame_size_packed = ((width as u64) << 32) | (height as u64);
                let frame_rate_packed = ((fps.max(1) as u64) << 32) | 1u64;
                let aspect_packed = (1u64 << 32) | 1u64;

                unsafe {
                    let out_type: IMFMediaType =
                        MFCreateMediaType().map_err(|e| format!("MFCreateMediaType out: {e}"))?;
                    out_type
                        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                        .map_err(|e| format!("{e}"))?;
                    out_type
                        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                        .map_err(|e| format!("{e}"))?;
                    out_type
                        .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
                        .map_err(|e| format!("{e}"))?;
                    out_type
                        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                        .map_err(|e| format!("{e}"))?;
                    out_type
                        .SetUINT64(&MF_MT_FRAME_SIZE, frame_size_packed)
                        .map_err(|e| format!("{e}"))?;
                    out_type
                        .SetUINT64(&MF_MT_FRAME_RATE, frame_rate_packed)
                        .map_err(|e| format!("{e}"))?;
                    out_type
                        .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, aspect_packed)
                        .map_err(|e| format!("{e}"))?;
                    let _ = out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, 77);
                    let h264_level: u32 = if fps > 60 || width > 1920 || height > 1080 {
                        51
                    } else {
                        42
                    };
                    let _ = out_type.SetUINT32(&MF_MT_MPEG2_LEVEL_GUID, h264_level);
                    let _ = out_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE_GUID, 2);
                    let _ =
                        out_type.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, keyframe_interval_frames);

                    if mft.SetOutputType(out_id, &out_type, 0).is_err() {
                        let _ = out_type.DeleteItem(&MF_MT_MPEG2_LEVEL_GUID);
                        let _ = out_type.DeleteItem(&MF_MT_VIDEO_NOMINAL_RANGE_GUID);
                        if mft.SetOutputType(out_id, &out_type, 0).is_err() {
                            let _ = out_type.DeleteItem(&MF_MT_MAX_KEYFRAME_SPACING);
                            mft.SetOutputType(out_id, &out_type, 0)
                                .map_err(|e| format!("SetOutputType H264 failed: {e}"))?;
                        }
                    }

                    let mut input_configured = false;
                    for idx in 0..32 {
                        let Ok(avail_in) = mft.GetInputAvailableType(in_id, idx) else {
                            break;
                        };
                        if let Ok(sub) = avail_in.GetGUID(&MF_MT_SUBTYPE) {
                            if sub == MFVideoFormat_NV12 {
                                let _ = avail_in.SetUINT32(
                                    &MF_MT_INTERLACE_MODE,
                                    MFVideoInterlace_Progressive.0 as u32,
                                );
                                let _ = avail_in.SetUINT64(&MF_MT_FRAME_SIZE, frame_size_packed);
                                let _ = avail_in.SetUINT64(&MF_MT_FRAME_RATE, frame_rate_packed);
                                let _ = avail_in.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, aspect_packed);
                                if mft.SetInputType(in_id, &avail_in, 0).is_ok() {
                                    input_configured = true;
                                    break;
                                }
                            }
                        }
                    }

                    if !input_configured {
                        let in_type: IMFMediaType = MFCreateMediaType()
                            .map_err(|e| format!("MFCreateMediaType in: {e}"))?;
                        in_type
                            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                            .map_err(|e| format!("{e}"))?;
                        in_type
                            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                            .map_err(|e| format!("{e}"))?;
                        in_type
                            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                            .map_err(|e| format!("{e}"))?;
                        in_type
                            .SetUINT64(&MF_MT_FRAME_SIZE, frame_size_packed)
                            .map_err(|e| format!("{e}"))?;
                        in_type
                            .SetUINT64(&MF_MT_FRAME_RATE, frame_rate_packed)
                            .map_err(|e| format!("{e}"))?;
                        in_type
                            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, aspect_packed)
                            .map_err(|e| format!("{e}"))?;

                        mft.SetInputType(in_id, &in_type, 0)
                            .map_err(|e| format!("SetInputType NV12 failed: {e}"))?;
                    }
                }
                Ok(())
            }

            fn submit_sample_to_mft(
                &mut self,
                input_sample: &IMFSample,
                pts_ms: u64,
                avcc_scratch: &mut Vec<u8>,
                buffer: &RingBuffer<CompressedPacket>,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) -> bool {
                unsafe {
                    self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);

                    if self.event_gen.is_some() {
                        if self.need_input_count == 0 {
                            self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                            if self.need_input_count == 0 {
                                let max_wait = match self.backend_kind {
                                    EncoderBackendKind::NvencD3d11 => std::time::Duration::from_micros(600),
                                    EncoderBackendKind::SoftwareMft => std::time::Duration::from_millis(2),
                                    _ => std::time::Duration::from_millis(1),
                                };
                                let wait_start = std::time::Instant::now();
                                while self.need_input_count == 0 && wait_start.elapsed() < max_wait {
                                    std::thread::yield_now();
                                    self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                                }
                            }
                        }

                        if self.need_input_count > 0 {
                            if self
                                .transform
                                .ProcessInput(self.in_stream_id, input_sample, 0)
                                .is_ok()
                            {
                                self.need_input_count -= 1;
                                self.encoded_frames = self.encoded_frames.wrapping_add(1);
                                self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                                return true;
                            }
                        } else if self.encoded_frames == 0 {
                            if self
                                .transform
                                .ProcessInput(self.in_stream_id, input_sample, 0)
                                .is_ok()
                            {
                                self.encoded_frames = self.encoded_frames.wrapping_add(1);
                                self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                                return true;
                            }
                        }
                        self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                        return false;
                    } else {
                        if self
                            .transform
                            .ProcessInput(self.in_stream_id, input_sample, 0)
                            .is_err()
                        {
                            self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                            if self
                                .transform
                                .ProcessInput(self.in_stream_id, input_sample, 0)
                                .is_err()
                            {
                                return self.produced_packets > 0;
                            }
                        }
                        self.encoded_frames = self.encoded_frames.wrapping_add(1);
                        self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                        return true;
                    }
                }
            }

            pub fn encode_repeat_frame(
                &mut self,
                pts_ms: u64,
                rel_hns: i64,
                dur_hns: i64,
                avcc_scratch: &mut Vec<u8>,
                buffer: &RingBuffer<CompressedPacket>,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) -> Result<(), String> {
                let Some(slot) = self.last_filled_slot else {
                    self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                    return Ok(());
                };
                let input_sample = if self.zero_copy_active && slot < self.gpu_input_pool.len() {
                    self.gpu_input_pool[slot].1.clone()
                } else if slot < self.input_pool.len() {
                    self.input_pool[slot].0.clone()
                } else {
                    self.drain_output_packets(pts_ms, avcc_scratch, buffer, codec_info);
                    return Ok(());
                };
                unsafe {
                    let _ = input_sample.SetSampleTime(rel_hns);
                    let _ = input_sample.SetSampleDuration(dur_hns);
                    let is_idr = self.encoded_frames % self.keyframe_interval_frames == 0;
                    let _ = input_sample.SetUINT32(
                        &MFSampleExtension_CleanPoint,
                        if is_idr { 1 } else { 0 },
                    );
                }
                let _ = self.submit_sample_to_mft(
                    &input_sample,
                    pts_ms,
                    avcc_scratch,
                    buffer,
                    codec_info,
                );
                Ok(())
            }

            fn maybe_sample_hud_from_staging(
                &mut self,
                duplicator: &DxgiDuplicator,
                converter: &GpuColorConverter,
                gpu_res: &ID3D11Resource,
                pts_ms: u64,
            ) {
                unsafe {
                    let flip = converter.staging_flip.fetch_add(1, Ordering::AcqRel);
                    let (write_tex, read_tex) = if (flip & 1) == 0 {
                        (&converter.nv12_staging_tex, &converter.nv12_staging_tex_alt)
                    } else {
                        (&converter.nv12_staging_tex_alt, &converter.nv12_staging_tex)
                    };
                    let Ok(write_res) = write_tex.cast::<ID3D11Resource>() else {
                        return;
                    };
                    let Ok(read_res) = read_tex.cast::<ID3D11Resource>() else {
                        return;
                    };
                    duplicator.context.CopyResource(&write_res, gpu_res);
                    if !converter.staging_primed.swap(true, Ordering::AcqRel) {
                        return;
                    }
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    if duplicator
                        .context
                        .Map(&read_res, 0, D3D11_MAP_READ, 0x100000, Some(&mut mapped))
                        .is_ok()
                    {
                        let w = self.width as usize;
                        let h = self.height as usize;
                        let pitch = mapped.RowPitch as usize;
                        let src_ptr = mapped.pData as *const u8;
                        if !src_ptr.is_null() && pitch >= w {
                            self.last_hud_sample_ms = pts_ms;
                            if pitch == w {
                                let total_needed = (w * h * 3) / 2;
                                let nv12_slice = std::slice::from_raw_parts(src_ptr, total_needed);
                                crate::games::publish_nv12_hud_sample(
                                    nv12_slice,
                                    self.width,
                                    self.height,
                                );
                            } else {
                                let y_only = std::slice::from_raw_parts(src_ptr, h * pitch);
                                crate::games::publish_nv12_hud_sample(
                                    y_only,
                                    pitch as u32,
                                    self.height,
                                );
                            }
                        }
                        duplicator.context.Unmap(&read_res, 0);
                    }
                }
            }

            pub fn encode_nv12_frame(
                &mut self,
                duplicator: &DxgiDuplicator,
                converter: &GpuColorConverter,
                pts_ms: u64,
                rel_hns: i64,
                dur_hns: i64,
                force_keyframe: bool,
                avcc_scratch: &mut Vec<u8>,
                buffer: &RingBuffer<CompressedPacket>,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) -> Result<(), String> {
                let gpu_res: ID3D11Resource = converter
                    .nv12_gpu_tex
                    .cast()
                    .map_err(|e| format!("cast gpu: {e}"))?;

                let should_sample_hud = buffer.active_context() == CaptureContext::Game
                    && pts_ms.saturating_sub(self.last_hud_sample_ms) >= 500;

                if self.zero_copy_active && !self.gpu_input_pool.is_empty() {
                    let slot = self.pool_idx % self.gpu_input_pool.len();
                    self.pool_idx = self.pool_idx.wrapping_add(1);
                    let dst_tex = self.gpu_input_pool[slot].0.clone();
                    let gpu_sample = self.gpu_input_pool[slot].1.clone();

                    let zero_copy_ok = unsafe {
                        if let Ok(dst_res) = dst_tex.cast::<ID3D11Resource>() {
                            duplicator.context.CopyResource(&dst_res, &gpu_res);
                            let _ = gpu_sample.SetSampleTime(rel_hns);
                            let _ = gpu_sample.SetSampleDuration(dur_hns);
                            let request_clean_point = force_keyframe
                                || (self.encoded_frames % self.keyframe_interval_frames == 0);
                            let _ = gpu_sample.SetUINT32(
                                &MFSampleExtension_CleanPoint,
                                if request_clean_point { 1 } else { 0 },
                            );
                            if should_sample_hud {
                                self.maybe_sample_hud_from_staging(
                                    duplicator,
                                    converter,
                                    &gpu_res,
                                    pts_ms,
                                );
                            }
                            self.last_filled_slot = Some(slot);
                            self.submit_sample_to_mft(
                                &gpu_sample,
                                pts_ms,
                                avcc_scratch,
                                buffer,
                                codec_info,
                            )
                        } else {
                            false
                        }
                    };

                    if zero_copy_ok {
                        return Ok(());
                    }

                    if self.encoded_frames > (self.fps.max(30) as u64 * 2)
                        && self.produced_packets == 0
                    {
                        self.disable_zero_copy("DXGI surface produced 0 packets after 2s");
                    } else {
                        return Ok(());
                    }
                }

                let slot = self.pool_idx % self.input_pool.len();
                self.pool_idx = self.pool_idx.wrapping_add(1);
                let input_sample = self.input_pool[slot].0.clone();
                let cpu_input_buffer = self.input_pool[slot].1.clone();

                unsafe {
                    input_sample
                        .SetSampleTime(rel_hns)
                        .map_err(|e| format!("SetSampleTime: {e}"))?;
                    input_sample
                        .SetSampleDuration(dur_hns)
                        .map_err(|e| format!("SetSampleDuration: {e}"))?;

                    let request_clean_point = force_keyframe
                        || (self.encoded_frames % self.keyframe_interval_frames == 0);
                    let _ = input_sample.SetUINT32(
                        &MFSampleExtension_CleanPoint,
                        if request_clean_point { 1 } else { 0 },
                    );

                    let flip = converter.staging_flip.fetch_add(1, Ordering::AcqRel);
                    let (write_tex, read_tex) = if (flip & 1) == 0 {
                        (&converter.nv12_staging_tex, &converter.nv12_staging_tex_alt)
                    } else {
                        (&converter.nv12_staging_tex_alt, &converter.nv12_staging_tex)
                    };

                    let write_res: ID3D11Resource = write_tex
                        .cast()
                        .map_err(|e| format!("cast write staging: {e}"))?;
                    let read_res: ID3D11Resource = read_tex
                        .cast()
                        .map_err(|e| format!("cast read staging: {e}"))?;

                    duplicator.context.CopyResource(&write_res, &gpu_res);
                    if force_keyframe || !converter.staging_primed.swap(true, Ordering::AcqRel) {
                        duplicator.context.CopyResource(&read_res, &gpu_res);
                    }

                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    let map_res = duplicator
                        .context
                        .Map(&read_res, 0, D3D11_MAP_READ, 0x100000, Some(&mut mapped))
                        .or_else(|_| {
                            duplicator
                                .context
                                .Map(&read_res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        });
                    map_res.map_err(|e| format!("Map NV12 staging: {e}"))?;

                    let mut dst_ptr: *mut u8 = std::ptr::null_mut();
                    let mut max_len = 0u32;
                    let w = self.width as usize;
                    let h = self.height as usize;
                    let pitch = mapped.RowPitch as usize;
                    let src_ptr = mapped.pData as *const u8;
                    let total_needed = (w * h * 3) / 2;
                    let mut copied = false;

                    if cpu_input_buffer
                        .Lock(&mut dst_ptr, Some(&mut max_len), None)
                        .is_ok()
                    {
                        if !src_ptr.is_null()
                            && !dst_ptr.is_null()
                            && (max_len as usize) >= total_needed
                        {
                            if pitch == w {
                                std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, w * h);
                                std::ptr::copy_nonoverlapping(
                                    src_ptr.add(h * pitch),
                                    dst_ptr.add(w * h),
                                    w * h / 2,
                                );
                            } else {
                                for y in 0..h {
                                    std::ptr::copy_nonoverlapping(
                                        src_ptr.add(y * pitch),
                                        dst_ptr.add(y * w),
                                        w,
                                    );
                                }
                                let uv_src = src_ptr.add(h * pitch);
                                let uv_dst = dst_ptr.add(w * h);
                                for y in 0..(h / 2) {
                                    std::ptr::copy_nonoverlapping(
                                        uv_src.add(y * pitch),
                                        uv_dst.add(y * w),
                                        w,
                                    );
                                }
                            }
                            let _ = cpu_input_buffer.SetCurrentLength(total_needed as u32);
                            copied = true;
                        }
                        duplicator.context.Unmap(&read_res, 0);
                        if copied && should_sample_hud {
                            self.last_hud_sample_ms = pts_ms;
                            let nv12_slice = std::slice::from_raw_parts(dst_ptr, total_needed);
                            crate::games::publish_nv12_hud_sample(
                                nv12_slice,
                                self.width,
                                self.height,
                            );
                        }
                        let _ = cpu_input_buffer.Unlock();
                    } else {
                        duplicator.context.Unmap(&read_res, 0);
                    }

                    if !copied {
                        return Err("failed to fill NV12 input buffer".to_string());
                    }

                    self.last_filled_slot = Some(slot);
                    let _ = self.submit_sample_to_mft(
                        &input_sample,
                        pts_ms,
                        avcc_scratch,
                        buffer,
                        codec_info,
                    );
                }

                Ok(())
            }

            fn pull_single_output_sample(
                &mut self,
                fallback_pts_ms: u64,
                avcc_scratch: &mut Vec<u8>,
                buffer: &RingBuffer<CompressedPacket>,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) -> bool {
                unsafe {
                    if let Some(ref s) = self.output_sample {
                        let _ = self.output_buffer.SetCurrentLength(0);
                        let _ = s.RemoveAllBuffers();
                        let _ = s.AddBuffer(&self.output_buffer);
                    }

                    let mut out_data = [MFT_OUTPUT_DATA_BUFFER {
                        dwStreamID: self.out_stream_id,
                        pSample: ManuallyDrop::new(self.output_sample.clone()),
                        dwStatus: 0,
                        pEvents: ManuallyDrop::new(None),
                    }];
                    let mut status = 0u32;

                    let hr = self.transform.ProcessOutput(0, &mut out_data, &mut status);
                    let _ = ManuallyDrop::take(&mut out_data[0].pEvents);
                    let produced_sample = ManuallyDrop::take(&mut out_data[0].pSample);

                    match hr {
                        Ok(()) => {
                            if let Some(sample) = produced_sample {
                                let sample_clean_point = sample
                                    .GetUINT32(&MFSampleExtension_CleanPoint)
                                    .unwrap_or(0)
                                    != 0;
                                let sample_hns_opt = sample.GetSampleTime().ok();
                                let sample_pts_ms = sample_hns_opt
                                    .map(|hns| {
                                        self.base_start_ms
                                            .wrapping_add((hns.max(0) as u64) / 10_000)
                                    })
                                    .unwrap_or(fallback_pts_ms);
                                let sample_pts_90k = sample_hns_opt
                                    .map(|hns| {
                                        ((self.base_start_ms as i64) * 90)
                                            .wrapping_add(((hns.max(0) as i128 * 9) / 1000) as i64)
                                    })
                                    .unwrap_or((sample_pts_ms as i64) * 90);

                                if let Ok(media_buf) = sample.ConvertToContiguousBuffer() {
                                    let mut raw_ptr: *mut u8 = std::ptr::null_mut();
                                    let mut cur_len = 0u32;
                                    if media_buf
                                        .Lock(&mut raw_ptr, None, Some(&mut cur_len))
                                        .is_ok()
                                    {
                                        if !raw_ptr.is_null() && cur_len > 0 {
                                            let annex_b = std::slice::from_raw_parts(
                                                raw_ptr,
                                                cur_len as usize,
                                            );
                                            self.ingest_annex_b_slice(
                                                annex_b,
                                                sample_pts_ms,
                                                sample_pts_90k,
                                                sample_clean_point,
                                                avcc_scratch,
                                                buffer,
                                                codec_info,
                                            );
                                        }
                                        let _ = media_buf.Unlock();
                                    }
                                }
                            }
                            true
                        }
                        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                            if let Ok(new_out_type) = self
                                .transform
                                .GetOutputAvailableType(self.out_stream_id, 0)
                            {
                                let _ = self
                                    .transform
                                    .SetOutputType(self.out_stream_id, &new_out_type, 0);
                                self.extract_sequence_header_from_media_type(codec_info);
                            }
                            true
                        }
                        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => false,
                        Err(_) => false,
                    }
                }
            }

            pub fn drain_output_packets(
                &mut self,
                pts_ms: u64,
                avcc_scratch: &mut Vec<u8>,
                buffer: &RingBuffer<CompressedPacket>,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) {
                if self.event_gen.is_some() {
                    loop {
                        let ev_opt = unsafe {
                            self.event_gen
                                .as_ref()
                                .and_then(|g| g.GetEvent(MF_EVENT_FLAG_NO_WAIT).ok())
                        };
                        let Some(ev) = ev_opt else {
                            break;
                        };
                        let ev_type = unsafe { ev.GetType().unwrap_or(0) };
                        if ev_type == 601 {
                            self.need_input_count += 1;
                        } else if ev_type == 602 {
                            let _ = self.pull_single_output_sample(
                                pts_ms,
                                avcc_scratch,
                                buffer,
                                codec_info,
                            );
                        }
                    }
                } else {
                    while self.pull_single_output_sample(pts_ms, avcc_scratch, buffer, codec_info) {}
                }
            }

            fn ingest_annex_b_slice(
                &mut self,
                annex_b: &[u8],
                pts_ms: u64,
                pts_90k: i64,
                sample_clean_point: bool,
                avcc_scratch: &mut Vec<u8>,
                buffer: &RingBuffer<CompressedPacket>,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) {
                let nals = split_annex_b(annex_b);
                avcc_scratch.clear();
                let mut is_keyframe = sample_clean_point;
                let mut updated_headers = false;

                for nal in nals {
                    if nal.is_empty() {
                        continue;
                    }
                    let nal_type = nal[0] & 0x1F;
                    if nal_type == 7 {
                        self.sps = Some(nal.to_vec());
                        updated_headers = true;
                    } else if nal_type == 8 {
                        self.pps = Some(nal.to_vec());
                        updated_headers = true;
                    } else if nal_type == 9 {
                        continue;
                    } else {
                        if nal_type == 5 {
                            is_keyframe = true;
                        }
                        let len_be = (nal.len() as u32).to_be_bytes();
                        avcc_scratch.extend_from_slice(&len_be);
                        avcc_scratch.extend_from_slice(nal);
                    }
                }

                if self.sps.is_none() || self.pps.is_none() {
                    self.extract_sequence_header_from_media_type(codec_info);
                } else if updated_headers {
                    if let (Some(ref sps), Some(ref pps)) = (&self.sps, &self.pps) {
                        let mut guard = match codec_info.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        *guard = Some(CodecContextInfo {
                            width: self.width,
                            height: self.height,
                            fps: self.fps,
                            sps: sps.clone(),
                            pps: pps.clone(),
                        });
                    }
                }

                if !avcc_scratch.is_empty() {
                    self.produced_packets = self.produced_packets.wrapping_add(1);
                    let dur_ms = (1000 / self.fps.max(1)).max(1) as i64;
                    let pkt = Arc::new(CompressedPacket {
                        kind: PacketKind::Video,
                        pts_ms,
                        pts: pts_90k,
                        dts: pts_90k,
                        duration: dur_ms,
                        is_keyframe,
                        data: Bytes::copy_from_slice(avcc_scratch),
                        stems: None,
                    });
                    buffer.push_video(pts_ms, pkt);
                }
            }
        }

        impl HardwareH264Encoder {
            pub fn new_standalone(
                width: u32,
                height: u32,
                fps: u32,
                bitrate_bps: u32,
                keyframe_interval_frames: u32,
                base_start_ms: u64,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) -> Result<Self, String> {
                super::super::ensure_mf_initialized()?;

                let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
                let mut hw_dev: Option<ID3D11Device> = None;
                let mut hw_ctx: Option<ID3D11DeviceContext> = None;
                let mut vendor_id = 0u32;
                let mut dxgi_manager: Option<IMFDXGIDeviceManager> = None;

                unsafe {
                    if D3D11CreateDevice(
                        None,
                        D3D_DRIVER_TYPE_HARDWARE,
                        None,
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        Some(&feature_levels),
                        D3D11_SDK_VERSION,
                        Some(&mut hw_dev),
                        None,
                        Some(&mut hw_ctx),
                    )
                    .is_ok()
                    {
                        if let Some(ref dev) = hw_dev {
                            if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
                                let _ = mt.SetMultithreadProtected(true);
                            }
                            if let Ok(dxgi_dev) = dev.cast::<IDXGIDevice>() {
                                if let Ok(adapter) = dxgi_dev.GetAdapter() {
                                    if let Ok(adapter1) = adapter.cast::<IDXGIAdapter1>() {
                                        if let Ok(desc) = adapter1.GetDesc1() {
                                            vendor_id = desc.VendorId;
                                        }
                                    }
                                }
                            }
                            let mut reset_token = 0u32;
                            if MFCreateDXGIDeviceManager(&mut reset_token, &mut dxgi_manager)
                                .is_ok()
                            {
                                if let Some(ref mgr) = dxgi_manager {
                                    let _ = mgr.ResetDevice(dev, reset_token);
                                }
                            }
                        }
                    }
                }

                let candidates = [
                    EncoderBackendKind::NvencD3d11,
                    EncoderBackendKind::AmfMft,
                    EncoderBackendKind::QuickSyncMft,
                    EncoderBackendKind::SoftwareMft,
                ];

                let mut chosen_transform: Option<(
                    IMFTransform,
                    Option<IMFMediaEventGenerator>,
                    EncoderBackendKind,
                    u32,
                    u32,
                )> = None;

                for &kind in &candidates {
                    if kind == EncoderBackendKind::NvencD3d11 && vendor_id != VENDOR_NVIDIA {
                        continue;
                    }
                    if kind == EncoderBackendKind::AmfMft && vendor_id != VENDOR_AMD {
                        continue;
                    }
                    if kind == EncoderBackendKind::QuickSyncMft && vendor_id != VENDOR_INTEL {
                        continue;
                    }

                    if let Ok((mft, ev_gen, in_id, out_id, _)) = Self::try_init_mft(
                        kind,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        keyframe_interval_frames,
                        dxgi_manager.as_ref(),
                    ) {
                        chosen_transform = Some((mft, ev_gen, kind, in_id, out_id));
                        break;
                    }
                }

                if chosen_transform.is_none() {
                    for &kind in &candidates {
                        if let Ok((mft, ev_gen, in_id, out_id, _)) = Self::try_init_mft(
                            kind,
                            width,
                            height,
                            fps,
                            bitrate_bps,
                            keyframe_interval_frames,
                            dxgi_manager.as_ref(),
                        ) {
                            chosen_transform = Some((mft, ev_gen, kind, in_id, out_id));
                            break;
                        }
                    }
                }

                let (transform, event_gen, backend_kind, in_stream_id, out_stream_id) =
                    chosen_transform.ok_or_else(|| {
                        "no suitable H264 encoder MFT found on system".to_string()
                    })?;

                let nv12_size = (width * height * 3) / 2;
                let (input_pool, output_sample, output_buffer) = unsafe {
                    let mut pool = Vec::with_capacity(4);
                    for _ in 0..4 {
                        let s =
                            MFCreateSample().map_err(|e| format!("MFCreateSample input: {e}"))?;
                        let b = MFCreateMemoryBuffer(nv12_size)
                            .map_err(|e| format!("MFCreateMemoryBuffer input: {e}"))?;
                        s.AddBuffer(&b)
                            .map_err(|e| format!("AddBuffer input: {e}"))?;
                        pool.push((s, b));
                    }

                    let out_info = transform
                        .GetOutputStreamInfo(out_stream_id)
                        .map_err(|e| format!("GetOutputStreamInfo: {e}"))?;
                    let mft_provides_samples = (out_info.dwFlags
                        & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                            | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                        != 0;

                    let out_buf_size = out_info.cbSize.max(512 * 1024);
                    let out_buf = MFCreateMemoryBuffer(out_buf_size)
                        .map_err(|e| format!("MFCreateMemoryBuffer output: {e}"))?;

                    let out_sample = if mft_provides_samples {
                        None
                    } else {
                        let s =
                            MFCreateSample().map_err(|e| format!("MFCreateSample output: {e}"))?;
                        s.AddBuffer(&out_buf)
                            .map_err(|e| format!("AddBuffer output: {e}"))?;
                        Some(s)
                    };

                    (pool, out_sample, out_buf)
                };

                let mut enc = Self {
                    backend_kind,
                    transform,
                    event_gen,
                    dxgi_manager,
                    in_stream_id,
                    out_stream_id,
                    need_input_count: 0,
                    input_pool,
                    gpu_input_pool: Vec::new(),
                    zero_copy_active: false,
                    pool_idx: 0,
                    last_filled_slot: None,
                    output_sample,
                    output_buffer,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    base_start_ms,
                    keyframe_interval_frames: keyframe_interval_frames.max(1) as u64,
                    encoded_frames: 0,
                    produced_packets: 0,
                    last_hud_sample_ms: 0,
                    sps: None,
                    pps: None,
                    sink: None,
                };

                enc.extract_sequence_header_from_media_type(codec_info);
                Ok(enc)
            }

            pub fn encode_nv12_bytes(
                &mut self,
                nv12_bytes: &[u8],
                pts_ms: u64,
                rel_hns: i64,
                dur_hns: i64,
                force_keyframe: bool,
                avcc_scratch: &mut Vec<u8>,
                buffer: &RingBuffer<CompressedPacket>,
                codec_info: &Arc<Mutex<Option<CodecContextInfo>>>,
            ) -> Result<(), String> {
                let total_needed = ((self.width * self.height * 3) / 2) as usize;
                if nv12_bytes.len() < total_needed {
                    return Err("nv12_bytes buffer smaller than required frame size".to_string());
                }

                let slot = self.pool_idx % self.input_pool.len();
                self.pool_idx = self.pool_idx.wrapping_add(1);
                let input_sample = self.input_pool[slot].0.clone();
                let cpu_input_buffer = self.input_pool[slot].1.clone();

                unsafe {
                    input_sample
                        .SetSampleTime(rel_hns)
                        .map_err(|e| format!("SetSampleTime: {e}"))?;
                    input_sample
                        .SetSampleDuration(dur_hns)
                        .map_err(|e| format!("SetSampleDuration: {e}"))?;

                    let request_clean_point = force_keyframe
                        || (self.encoded_frames % self.keyframe_interval_frames == 0);
                    let _ = input_sample.SetUINT32(
                        &MFSampleExtension_CleanPoint,
                        if request_clean_point { 1 } else { 0 },
                    );

                    let mut dst_ptr: *mut u8 = std::ptr::null_mut();
                    let mut max_len = 0u32;
                    let mut copied = false;
                    if cpu_input_buffer
                        .Lock(&mut dst_ptr, Some(&mut max_len), None)
                        .is_ok()
                    {
                        if !dst_ptr.is_null() && (max_len as usize) >= total_needed {
                            std::ptr::copy_nonoverlapping(
                                nv12_bytes.as_ptr(),
                                dst_ptr,
                                total_needed,
                            );
                            let _ = cpu_input_buffer.SetCurrentLength(total_needed as u32);
                            copied = true;
                        }
                        let _ = cpu_input_buffer.Unlock();
                    }
                    if !copied {
                        return Err("failed to copy NV12 bytes into MFT sample buffer".to_string());
                    }

                    self.last_filled_slot = Some(slot);
                    let _ = self.submit_sample_to_mft(
                        &input_sample,
                        pts_ms,
                        avcc_scratch,
                        buffer,
                        codec_info,
                    );
                }

                Ok(())
            }
        }

        impl Drop for HardwareH264Encoder {
            fn drop(&mut self) {
                unsafe {
                    let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                }
            }
        }

        const MFT_CATEGORY_VIDEO_DECODER_GUID: GUID =
            GUID::from_u128(0xd6c02d4b_6833_45b4_971a_05a4b04bab91);

        pub struct MftH264Decoder {
            transform: IMFTransform,
            in_stream_id: u32,
            out_stream_id: u32,
            output_sample: Option<IMFSample>,
            output_buffer: IMFMediaBuffer,
            pub dec_width: u32,
            pub dec_height: u32,
        }

        impl MftH264Decoder {
            pub fn new(width: u32, height: u32, sps: &[u8], pps: &[u8]) -> Result<Self, String> {
                super::super::ensure_mf_initialized()?;

                let input_info = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_H264,
                };
                let output_info = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_NV12,
                };

                let flags = MFT_ENUM_FLAG(
                    MFT_ENUM_FLAG_SYNCMFT.0
                        | MFT_ENUM_FLAG_HARDWARE.0
                        | MFT_ENUM_FLAG_SORTANDFILTER.0,
                );

                let mut activates_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut count = 0u32;

                unsafe {
                    MFTEnumEx(
                        MFT_CATEGORY_VIDEO_DECODER_GUID,
                        flags,
                        Some(&input_info as *const MFT_REGISTER_TYPE_INFO),
                        Some(&output_info as *const MFT_REGISTER_TYPE_INFO),
                        &mut activates_ptr,
                        &mut count,
                    )
                    .map_err(|e| format!("MFTEnumEx decoder failed: {e}"))?;
                }

                if count == 0 || activates_ptr.is_null() {
                    return Err("no H264 MFT decoder found".to_string());
                }

                let mut selected: Option<(IMFTransform, u32, u32, u32, u32)> = None;
                let frame_size_packed = ((width as u64) << 32) | (height as u64);

                unsafe {
                    let slice = std::slice::from_raw_parts_mut(activates_ptr, count as usize);
                    for slot in slice.iter_mut() {
                        if let Some(activate) = slot.take() {
                            if selected.is_some() {
                                continue;
                            }
                            if let Ok(mft) = activate.ActivateObject::<IMFTransform>() {
                                let mut in_id = 0u32;
                                let mut out_id = 0u32;
                                let _ = mft.GetStreamIDs(
                                    std::slice::from_mut(&mut in_id),
                                    std::slice::from_mut(&mut out_id),
                                );

                                if let Ok(in_type) = MFCreateMediaType() {
                                    let _ = in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
                                    let _ = in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264);
                                    let _ = in_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size_packed);
                                    let _ = in_type.SetUINT32(
                                        &MF_MT_INTERLACE_MODE,
                                        MFVideoInterlace_Progressive.0 as u32,
                                    );
                                    if !sps.is_empty() && !pps.is_empty() {
                                        let mut seq_blob =
                                            Vec::with_capacity(sps.len() + pps.len() + 8);
                                        seq_blob.extend_from_slice(&[0, 0, 0, 1]);
                                        seq_blob.extend_from_slice(sps);
                                        seq_blob.extend_from_slice(&[0, 0, 0, 1]);
                                        seq_blob.extend_from_slice(pps);
                                        let _ = in_type
                                            .SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &seq_blob);
                                    }

                                    if mft.SetInputType(in_id, &in_type, 0).is_ok() {
                                        let mut out_ok = false;
                                        let mut dec_w = width;
                                        let mut dec_h = height;
                                        for idx in 0..32 {
                                            let Ok(avail_out) =
                                                mft.GetOutputAvailableType(out_id, idx)
                                            else {
                                                break;
                                            };
                                            if avail_out.GetGUID(&MF_MT_SUBTYPE)
                                                == Ok(MFVideoFormat_NV12)
                                            {
                                                if let Ok(packed) =
                                                    avail_out.GetUINT64(&MF_MT_FRAME_SIZE)
                                                {
                                                    let w = (packed >> 32) as u32;
                                                    let h = (packed & 0xFFFF_FFFF) as u32;
                                                    if w > 0 && h > 0 {
                                                        dec_w = w;
                                                        dec_h = h;
                                                    }
                                                }
                                                if mft.SetOutputType(out_id, &avail_out, 0).is_ok()
                                                {
                                                    out_ok = true;
                                                    break;
                                                }
                                            }
                                        }
                                        if out_ok {
                                            let _ =
                                                mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                                            let _ = mft.ProcessMessage(
                                                MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
                                                0,
                                            );
                                            let _ = mft.ProcessMessage(
                                                MFT_MESSAGE_NOTIFY_START_OF_STREAM,
                                                0,
                                            );
                                            selected = Some((mft, in_id, out_id, dec_w, dec_h));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    CoTaskMemFree(Some(activates_ptr as *const c_void));
                }

                let (transform, in_stream_id, out_stream_id, dec_width, dec_height) =
                    selected.ok_or_else(|| "failed to configure H264 MFT decoder".to_string())?;

                let aligned_w = (dec_width.max(width) + 127) & !127;
                let aligned_h = (dec_height.max(height) + 31) & !31;
                let buf_cap = ((aligned_w * aligned_h * 3) / 2).max(4 * 1024 * 1024);

                let (output_sample, output_buffer) = unsafe {
                    let out_info = transform
                        .GetOutputStreamInfo(out_stream_id)
                        .map_err(|e| format!("decoder GetOutputStreamInfo: {e}"))?;
                    let provides = (out_info.dwFlags
                        & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                            | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                        != 0;
                    let alloc_sz = out_info.cbSize.max(buf_cap);
                    let out_buf = MFCreateMemoryBuffer(alloc_sz)
                        .map_err(|e| format!("decoder MFCreateMemoryBuffer: {e}"))?;
                    let out_samp = if provides {
                        None
                    } else {
                        let s = MFCreateSample()
                            .map_err(|e| format!("decoder MFCreateSample: {e}"))?;
                        s.AddBuffer(&out_buf)
                            .map_err(|e| format!("decoder AddBuffer: {e}"))?;
                        Some(s)
                    };
                    (out_samp, out_buf)
                };

                Ok(Self {
                    transform,
                    in_stream_id,
                    out_stream_id,
                    output_sample,
                    output_buffer,
                    dec_width,
                    dec_height,
                })
            }

            pub fn push_annex_b(
                &mut self,
                annex_b: &[u8],
                sample_time_hns: i64,
            ) -> Result<(), String> {
                unsafe {
                    let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
                    let buf = MFCreateMemoryBuffer(annex_b.len().max(64) as u32)
                        .map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    if buf.Lock(&mut ptr, None, None).is_ok() {
                        if !ptr.is_null() && !annex_b.is_empty() {
                            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
                            let _ = buf.SetCurrentLength(annex_b.len() as u32);
                        }
                        let _ = buf.Unlock();
                    }
                    sample
                        .AddBuffer(&buf)
                        .map_err(|e| format!("AddBuffer: {e}"))?;
                    let _ = sample.SetSampleTime(sample_time_hns);
                    self.transform
                        .ProcessInput(self.in_stream_id, &sample, 0)
                        .map_err(|e| format!("decoder ProcessInput: {e}"))
                }
            }

            pub fn pull_nv12_frame<F>(&mut self, mut on_frame: F) -> bool
            where
                F: FnMut(&[u8], u32, u32),
            {
                unsafe {
                    loop {
                        if let Some(ref s) = self.output_sample {
                            let _ = self.output_buffer.SetCurrentLength(0);
                            let _ = s.RemoveAllBuffers();
                            let _ = s.AddBuffer(&self.output_buffer);
                        }

                        let mut out_data = [MFT_OUTPUT_DATA_BUFFER {
                            dwStreamID: self.out_stream_id,
                            pSample: ManuallyDrop::new(self.output_sample.clone()),
                            dwStatus: 0,
                            pEvents: ManuallyDrop::new(None),
                        }];
                        let mut status = 0u32;
                        let hr = self.transform.ProcessOutput(0, &mut out_data, &mut status);
                        let _ = ManuallyDrop::take(&mut out_data[0].pEvents);
                        let produced = ManuallyDrop::take(&mut out_data[0].pSample);

                        match hr {
                            Ok(()) => {
                                if let Some(sample) = produced {
                                    if let Ok(media_buf) = sample.ConvertToContiguousBuffer() {
                                        let mut raw_ptr: *mut u8 = std::ptr::null_mut();
                                        let mut cur_len = 0u32;
                                        if media_buf
                                            .Lock(&mut raw_ptr, None, Some(&mut cur_len))
                                            .is_ok()
                                        {
                                            if !raw_ptr.is_null() && cur_len > 0 {
                                                let slice = std::slice::from_raw_parts(
                                                    raw_ptr,
                                                    cur_len as usize,
                                                );
                                                on_frame(slice, self.dec_width, self.dec_height);
                                            }
                                            let _ = media_buf.Unlock();
                                        }
                                    }
                                }
                                return true;
                            }
                            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                                for idx in 0..32 {
                                    let Ok(avail_out) = self
                                        .transform
                                        .GetOutputAvailableType(self.out_stream_id, idx)
                                    else {
                                        break;
                                    };
                                    if avail_out.GetGUID(&MF_MT_SUBTYPE) == Ok(MFVideoFormat_NV12) {
                                        if let Ok(packed) = avail_out.GetUINT64(&MF_MT_FRAME_SIZE) {
                                            let w = (packed >> 32) as u32;
                                            let h = (packed & 0xFFFF_FFFF) as u32;
                                            if w > 0 && h > 0 {
                                                self.dec_width = w;
                                                self.dec_height = h;
                                            }
                                        }
                                        if self
                                            .transform
                                            .SetOutputType(self.out_stream_id, &avail_out, 0)
                                            .is_ok()
                                        {
                                            let needed = ((self.dec_width + 127)
                                                * (self.dec_height + 31)
                                                * 3)
                                                / 2;
                                            if let Ok(new_buf) = MFCreateMemoryBuffer(needed) {
                                                self.output_buffer = new_buf;
                                            }
                                            break;
                                        }
                                    }
                                }
                                continue;
                            }
                            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return false,
                            Err(_) => return false,
                        }
                    }
                }
            }
        }

        impl MftH264Decoder {
            pub fn flush(&mut self) {
                unsafe {
                    let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                    let _ = self
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                    let _ = self
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                }
            }

            pub fn drain(&mut self) {
                unsafe {
                    let _ = self.transform.ProcessMessage(
                        windows::Win32::Media::MediaFoundation::MFT_MESSAGE_COMMAND_DRAIN,
                        0,
                    );
                }
            }
        }

        impl Drop for MftH264Decoder {
            fn drop(&mut self) {
                unsafe {
                    let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                }
            }
        }


    }

    #[cfg(windows)]
    pub use win_impl::*;

    #[cfg(not(windows))]
    mod unix_impl {
        use super::matches_game_executable;
        use std::time::{SystemTime, UNIX_EPOCH};

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum EncoderBackendKind {
            NvencD3d11,
            AmfMft,
            QuickSyncMft,
            SoftwareMft,
            LinuxVaapi,
            LinuxNvenc,
            LinuxSoftwareX264,
        }

        pub fn query_qpc() -> i64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(1)
        }

        pub fn query_qpc_freq() -> i64 {
            1_000_000_000
        }

        pub fn detect_foreground_context(
            active_pid_filter: Option<u32>,
            active_game_name: Option<&str>,
        ) -> super::super::buffer::CaptureContext {
            if active_pid_filter.is_some() || active_game_name.is_some() {
                super::super::buffer::CaptureContext::Game
            } else {
                super::super::buffer::CaptureContext::Desktop
            }
        }

        pub fn is_target_process_focused(target_pid: Option<u32>) -> bool {
            let Some(pid) = target_pid else {
                return true;
            };
            if pid == 0 {
                return true;
            }
            std::path::Path::new(&format!("/proc/{pid}")).exists()
        }

        pub fn find_window_by_pid(target_pid: u32) -> Option<usize> {
            if target_pid != 0 && std::path::Path::new(&format!("/proc/{target_pid}")).exists() {
                Some(target_pid as usize)
            } else {
                None
            }
        }

        pub fn find_target_window(game_name: Option<&str>) -> Option<(usize, u32)> {
            let target = game_name?;
            let entries = std::fs::read_dir("/proc").ok()?;
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let Ok(pid) = fname.to_string_lossy().parse::<u32>() else {
                    continue;
                };
                if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
                    let c_low = comm.trim().to_lowercase();
                    if matches_game_executable(&c_low, &target.to_lowercase()) {
                        return Some((pid as usize, pid));
                    }
                }
            }
            None
        }
    }

    #[cfg(not(windows))]
    pub use unix_impl::*;
}

pub use crate::audio as audio;

pub use super::formats::discord_compress;

#[allow(unused_imports)]
pub use audio::{list_input_devices, AudioInputDeviceInfo};
pub use config::{
    parse_duration_secs, parse_resolution_pair, AudioMode, Container, Fps, Keybind,
    KeybindsConfig, Quality, RecorderConfig, StorageConfig, StorageMode,
};
pub use discord_compress::{
    make_it_for_discord, DiscordCompressOptions, DiscordCompressResult, DEFAULT_DISCORD_TARGET_MB,
};
pub use hardware::SystemHardwareProfile;
pub use storage::StorageManager;

struct PendingHighlight {
    game: String,
    event_kind: String,
    first_event_ms: u64,
    last_event_ms: u64,
    first_instant: Instant,
    deadline: Instant,
    priority_rank: u8,
    map_name: String,
    mode_name: String,
    weapon_name: String,
    kill_count: u32,
    supersede_clip_path: Option<std::path::PathBuf>,
}

static LAST_SAVED_CLIP_PATH: std::sync::OnceLock<Mutex<Option<std::path::PathBuf>>> =
    std::sync::OnceLock::new();

fn set_last_saved_clip(path: std::path::PathBuf) {
    let mtx = LAST_SAVED_CLIP_PATH.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = mtx.lock() {
        *guard = Some(path);
    }
}

pub fn last_saved_clip() -> Option<std::path::PathBuf> {
    let mtx = LAST_SAVED_CLIP_PATH.get_or_init(|| Mutex::new(None));
    mtx.lock().ok().and_then(|g| g.clone())
}

pub async fn make_it_for_discord_async(
    input_path: std::path::PathBuf,
    options: DiscordCompressOptions,
    sink: Option<EventSink>,
) -> Result<DiscordCompressResult, String> {
    if let Some(ref s) = sink {
        s.emit(
            "discord.compress_started",
            None,
            json!({
                "input_path": input_path.to_string_lossy(),
                "target_mb": options.target_mb,
            }),
        );
    }

    let in_for_err = input_path.clone();
    let res = tokio::task::spawn_blocking(move || make_it_for_discord(input_path, options)).await;

    match res {
        Ok(Ok(info)) => {
            if let Some(ref s) = sink {
                s.emit(
                    "discord.compress_done",
                    None,
                    json!({
                        "input_path": info.input_path.to_string_lossy(),
                        "output_path": info.output_path.to_string_lossy(),
                        "input_bytes": info.input_bytes,
                        "output_bytes": info.output_bytes,
                        "width": info.width,
                        "height": info.height,
                        "fps": info.fps,
                        "video_bitrate_kbps": info.video_bitrate_kbps,
                        "backend": info.backend,
                        "elapsed_ms": info.elapsed_ms,
                    }),
                );
            }
            Ok(info)
        }
        Ok(Err(e)) => {
            if let Some(ref s) = sink {
                s.emit(
                    "discord.compress_error",
                    None,
                    json!({
                        "input_path": in_for_err.to_string_lossy(),
                        "message": e,
                    }),
                );
            }
            Err(e)
        }
        Err(join_err) => {
            let msg = format!("discord compress worker panicked: {join_err}");
            if let Some(ref s) = sink {
                s.emit(
                    "discord.compress_error",
                    None,
                    json!({
                        "input_path": in_for_err.to_string_lossy(),
                        "message": msg,
                    }),
                );
            }
            Err(msg)
        }
    }
}

struct SessionState {
    active_game: Option<String>,
    active_pid: Option<u32>,
    active_map: String,
    active_mode: String,
    last_pushed_pid: Option<u32>,
    shared_target_pid: Arc<AtomicUsize>,
    shared_audio_mode: Arc<AtomicUsize>,
    shared_audio_kbps: Arc<AtomicUsize>,
    shared_mic_muted: Arc<AtomicBool>,
    shared_separate_tracks: Arc<AtomicBool>,
    shared_mic_device: Arc<RwLock<Option<String>>>,
    shared_game_name: Arc<RwLock<Option<String>>>,
    captured_hwnd: Arc<AtomicUsize>,
    reset_capture_signal: Arc<AtomicBool>,
    flush_signal: Arc<AtomicBool>,
    flush_done: Arc<AtomicBool>,
    capture_cancel: Option<CancellationToken>,
    video_thread: Option<std::thread::JoinHandle<()>>,
    audio_thread: Option<std::thread::JoinHandle<()>>,
    pending_highlight: Option<PendingHighlight>,
    sequence_first_event_ms: Option<u64>,
    sequence_last_event_ms: Option<u64>,
    sequence_kill_count: u32,
    last_sequence_saved: Arc<Mutex<Option<(std::path::PathBuf, u64)>>>,
    stats_store: crate::stats::LocalMatchStatsStore,
}

const MAX_SAVES: u32 = 16;

pub struct Recorder {
    config: Arc<RwLock<RecorderConfig>>,
    buffer: buffer::RingBuffer<buffer::CompressedPacket>,
    codec_info: Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    sink: Option<EventSink>,
    cancel_token: CancellationToken,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Recorder {
    pub fn new(config: RecorderConfig, event_rx: broadcast::Receiver<Event>) -> Self {
        Self::with_sink_internal(config, event_rx, None)
    }

    pub fn with_sink(
        config: RecorderConfig,
        event_rx: broadcast::Receiver<Event>,
        sink: EventSink,
    ) -> Self {
        Self::with_sink_internal(config, event_rx, Some(sink))
    }

    fn with_sink_internal(
        config: RecorderConfig,
        event_rx: broadcast::Receiver<Event>,
        sink: Option<EventSink>,
    ) -> Self {
        let _ = StorageManager::resolve_clip_dir(&config.storage, None);
        StorageManager::purge_cache_dir();
        StorageManager::purge_stale_tmp_files(&config.storage);

        let safe_buf_secs = config.buffer_duration_secs.clamp(5, 7200);
        let max_duration = Duration::from_secs(safe_buf_secs);
        let video_cap = (config.fps.as_u32() as u64 * safe_buf_secs).min(32_768) as usize;
        let audio_cap = (safe_buf_secs * 48).min(16_384) as usize;
        let ring_buf =
            buffer::RingBuffer::new_with_capacities(max_duration, video_cap, audio_cap);
        let config_arc = Arc::new(RwLock::new(config));
        let codec_info_arc = Arc::new(Mutex::new(None));
        let cancel_token = CancellationToken::new();

        let cfg_clone = config_arc.clone();
        let buf_clone = ring_buf.clone();
        let c_info_clone = codec_info_arc.clone();
        let sink_clone = sink.clone();
        let cancel_clone = cancel_token.clone();

        let handle = tokio::spawn(async move {
            run_event_loop(
                cfg_clone,
                buf_clone,
                c_info_clone,
                sink_clone,
                event_rx,
                cancel_clone,
            )
            .await;
        });

        Self {
            config: config_arc,
            buffer: ring_buf,
            codec_info: codec_info_arc,
            sink,
            cancel_token,
            task_handle: Some(handle),
        }
    }

    pub fn ring_buffer(&self) -> buffer::RingBuffer<buffer::CompressedPacket> {
        self.buffer.clone()
    }

    pub fn update_config(&self, new_config: RecorderConfig) {
        let mut cfg = match self.config.write() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        };
        let clamped_secs = new_config.buffer_duration_secs.clamp(5, 7200);
        self.buffer.set_limits(
            Duration::from_secs(clamped_secs),
            (new_config.fps.as_u32() as u64 * clamped_secs) as usize,
        );
        *cfg = new_config;
    }

    pub async fn reload_config(&self, new_config: RecorderConfig) {
        self.update_config(new_config);
    }

    pub fn set_mic_muted(&self, muted: bool) {
        if let Ok(mut cfg) = self.config.write() {
            cfg.mic_muted = muted;
        }
    }

    pub fn is_mic_muted(&self) -> bool {
        self.config.read().map(|c| c.mic_muted).unwrap_or(false)
    }

    pub fn share_anonymous_stats(&self) -> bool {
        self.config
            .read()
            .map(|c| c.share_anonymous_stats)
            .unwrap_or(true)
    }

    pub fn set_share_anonymous_stats(&self, enabled: bool) {
        if let Ok(mut cfg) = self.config.write() {
            cfg.share_anonymous_stats = enabled;
        }
        let _ = crate::stats::persist_share_anonymous_stats(enabled);
        if let Some(ref s) = self.sink {
            s.emit(
                "settings.privacy.share_anonymous_stats",
                None,
                json!({
                    "path": crate::stats::PRIVACY_SETTING_PATH,
                    "share_anonymous_stats": enabled,
                    "enabled": enabled,
                    "status": if enabled { "On" } else { "Off" },
                }),
            );
        }
    }

    pub fn current_config(&self) -> RecorderConfig {
        lock_config(&self.config)
    }

    pub fn shared_config(&self) -> Arc<RwLock<RecorderConfig>> {
        self.config.clone()
    }

    pub fn list_microphones() -> Vec<audio::AudioInputDeviceInfo> {
        audio::list_input_devices()
    }

    pub async fn make_it_for_discord(
        &self,
        input_path: impl Into<std::path::PathBuf>,
        target_mb: Option<f64>,
    ) -> Result<DiscordCompressResult, String> {
        let in_path = input_path.into();
        let opts = DiscordCompressOptions {
            target_mb: target_mb.unwrap_or(DEFAULT_DISCORD_TARGET_MB),
            output_path: None,
            max_fps: None,
            prefer_hardware: true,
        };
        let sink_clone = self.sink.clone();
        make_it_for_discord_async(in_path, opts, sink_clone).await
    }

    pub async fn shutdown(mut self) {
        self.cancel_token.cancel();
        if let Some(handle) = self.task_handle.take() {
            let _ = handle.await;
        }
        StorageManager::purge_cache_dir();
        shutdown_media_foundation();
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}



pub fn extract_clean_window_app_name(raw_title: &str) -> String {
    let trimmed = raw_title.trim();
    if trimmed.is_empty() {
        return "Desktop_Clip".to_string();
    }
    let lower = trimmed.to_lowercase();
    if lower.contains("minecraft") || lower.contains("lunar client") || lower.contains("badlion") || lower.contains("feather client") {
        return "Minecraft".to_string();
    }
    if lower.contains("discord") || lower.contains("vesktop") {
        return "Discord".to_string();
    }
    if lower.contains("teamspeak") {
        return "TeamSpeak".to_string();
    }
    if lower.contains("telegram") {
        return "Telegram".to_string();
    }
    if lower.contains("whatsapp") {
        return "WhatsApp".to_string();
    }
    if lower.contains("youtube") {
        return "YouTube".to_string();
    }
    if lower.contains("twitch") {
        return "Twitch".to_string();
    }
    if lower.contains("spotify") {
        return "Spotify".to_string();
    }
    if lower.contains("netflix") {
        return "Netflix".to_string();
    }
    if lower.contains("roblox") {
        return "Roblox".to_string();
    }
    if lower.contains("grand theft auto") || lower.contains("gta v") || lower.contains("fivem") {
        return "GTA_V".to_string();
    }
    if lower.contains("elden ring") {
        return "Elden_Ring".to_string();
    }
    if lower.contains("cyberpunk") {
        return "Cyberpunk_2077".to_string();
    }
    if lower.contains("fc 27") || lower.contains("fc27") {
        return "FC27".to_string();
    }
    if lower.contains("fc 26") || lower.contains("fc26") {
        return "FC26".to_string();
    }
    if lower.contains("efootball") {
        return "eFootball".to_string();
    }
    if lower.contains("ea sports fc") || lower.contains("fifa") {
        return "EA_Sports_FC".to_string();
    }
    let separators = [" - ", " \u{2014} ", " \u{2013} ", " | "];
    let candidate = separators
        .iter()
        .filter_map(|sep| trimmed.rsplit_once(sep))
        .max_by_key(|(before, _)| before.len())
        .map(|(_, after)| after.trim())
        .unwrap_or(trimmed);
    let base = if candidate.is_empty() { trimmed } else { candidate };
    let clean = storage::StorageManager::sanitize_folder_name(base);
    if clean.is_empty() || clean == "UnknownGame" {
        "Game_Clip".to_string()
    } else {
        clean
    }
}

fn lock_config(arc: &Arc<RwLock<RecorderConfig>>) -> RecorderConfig {
    match arc.read() {
        Ok(g) => g.clone(),
        Err(poison) => poison.into_inner().clone(),
    }
}

pub(crate) fn emit_error(sink: &Option<EventSink>, kind: &str, msg: &str) {
    diag::log_line("RECORDER_ERROR", &format!("code={kind} message={msg}"));
    if let Some(ref s) = sink {
        s.emit(
            "recorder.error",
            None,
            json!({ "error": kind, "code": kind, "message": msg }),
        );
    }
}

async fn wait_for_flush(flush_signal: &Arc<AtomicBool>, flush_done: &Arc<AtomicBool>) {
    flush_done.store(false, Ordering::Release);
    flush_signal.store(true, Ordering::Release);
    let start = Instant::now();
    while !flush_done.load(Ordering::Acquire) && start.elapsed() < Duration::from_millis(120) {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn launch_direct_clip_capture(
    event: &Event,
    active_game: Option<String>,
    config: &Arc<RwLock<RecorderConfig>>,
    buffer: &buffer::RingBuffer<buffer::CompressedPacket>,
    codec_info: &Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    flush_signal: &Arc<AtomicBool>,
    flush_done: &Arc<AtomicBool>,
    sink: &Option<EventSink>,
) {
    let raw_title = event
        .data
        .get("window_title")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let game_title = active_game.unwrap_or_else(|| extract_clean_window_app_name(raw_title));

    let cfg = lock_config(config);
    let direct_dur = Duration::from_secs(cfg.direct_clip_duration_secs.clamp(3, 3600));
    let start_ms = now_ms();
    let event_at_ms = event.at_ms;
    let sink_clone = sink.clone();
    let buf_clone = buffer.clone();
    let codec_clone = codec_info.clone();
    let flush_sig = flush_signal.clone();
    let flush_dn = flush_done.clone();

    if let Some(ref s) = sink {
        s.emit(
            "clip.direct_started",
            None,
            json!({
                "window_title": game_title,
                "duration_secs": direct_dur.as_secs(),
            }),
        );
    }

    tokio::spawn(async move {
        tokio::time::sleep(direct_dur).await;
        wait_for_flush(&flush_sig, &flush_dn).await;
        let (v_pkts, a_pkts) = buf_clone.snapshot_since(Some(start_ms));
        save_clip_async(
            v_pkts,
            a_pkts,
            cfg,
            codec_clone,
            &game_title,
            "direct.clip",
            event_at_ms,
            sink_clone,
            true,
        )
        .await;
    });
}

async fn run_event_loop(
    config: Arc<RwLock<RecorderConfig>>,
    buffer: buffer::RingBuffer<buffer::CompressedPacket>,
    codec_info: Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    sink: Option<EventSink>,
    mut event_rx: broadcast::Receiver<Event>,
    cancel_token: CancellationToken,
) {
    let init_cfg = lock_config(&config);
    let mut state = SessionState {
        active_game: None,
        active_pid: None,
        active_map: String::new(),
        active_mode: String::new(),
        last_pushed_pid: None,
        shared_target_pid: Arc::new(AtomicUsize::new(0)),
        shared_audio_mode: Arc::new(AtomicUsize::new(init_cfg.audio.to_usize())),
        shared_audio_kbps: Arc::new(AtomicUsize::new(init_cfg.audio_bitrate_kbps as usize)),
        shared_mic_muted: Arc::new(AtomicBool::new(init_cfg.mic_muted)),
        shared_separate_tracks: Arc::new(AtomicBool::new(init_cfg.separate_audio_tracks)),
        shared_mic_device: Arc::new(RwLock::new(init_cfg.mic_device.clone())),
        shared_game_name: Arc::new(RwLock::new(None)),
        captured_hwnd: Arc::new(AtomicUsize::new(0)),
        reset_capture_signal: Arc::new(AtomicBool::new(false)),
        flush_signal: Arc::new(AtomicBool::new(false)),
        flush_done: Arc::new(AtomicBool::new(false)),
        capture_cancel: None,
        video_thread: None,
        audio_thread: None,
        pending_highlight: None,
        sequence_first_event_ms: None,
        sequence_last_event_ms: None,
        sequence_kill_count: 0,
        last_sequence_saved: Arc::new(Mutex::new(None)),
        stats_store: crate::stats::LocalMatchStatsStore::load_or_default(
            crate::stats::resolve_local_match_stats_path(),
        ),
    };

    let save_tasks = Arc::new(tokio::sync::Semaphore::new(MAX_SAVES as usize));
    let mut ticker = tokio::time::interval(Duration::from_millis(100));

    ensure_capture(
        &mut state,
        &config,
        &buffer,
        &codec_info,
        &sink,
        &cancel_token,
        "Desktop".to_string(),
        None,
    );

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                stop_capture(&mut state).await;
                buffer.clear();
                break;
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                if let Some(ref pending) = state.pending_highlight {
                    if now >= pending.deadline {
                        if let Some(h) = state.pending_highlight.take() {
                            wait_for_flush(&state.flush_signal, &state.flush_done).await;
                            let cfg = lock_config(&config);
                            let base_pre_ms = cfg.pre_roll_secs.clamp(8, 3600) * 1000;
                            let extra_pre_ms = if h.priority_rank >= 7 || h.kill_count >= 2 {
                                4_000
                            } else {
                                2_000
                            };
                            let clip_window_start = h
                                .first_event_ms
                                .saturating_sub(base_pre_ms.saturating_add(extra_pre_ms));
                            let (mut v_pkts, mut a_pkts) =
                                buffer.snapshot_game_only_since(Some(clip_window_start));
                            if v_pkts.is_empty() {
                                diag::log_line(
                                    "HIGHLIGHT_FALLBACK",
                                    "snapshot_game_only_since returned 0 video packets on deadline; falling back to snapshot_since",
                                );
                                let (fv, fa) = buffer.snapshot_since(Some(clip_window_start));
                                v_pkts = fv;
                                a_pkts = fa;
                            }

                            let c_info = codec_info.clone();
                            let s = sink.clone();
                            let sem = save_tasks.clone();
                            let permit = sem.try_acquire_owned().ok();
                            let last_seq_slot = state.last_sequence_saved.clone();
                            let seq_start_for_slot = h.first_event_ms;
                            let supersede_old = h.supersede_clip_path.clone();
                            let hl_meta = ClipHighlightMeta {
                                map: h.map_name.clone(),
                                mode: h.mode_name.clone(),
                                weapon: h.weapon_name.clone(),
                                kills: h.kill_count,
                            };
                            tokio::spawn(async move {
                                let _permit = permit;
                                if let Some(saved_p) = save_clip_with_meta_async(
                                    v_pkts,
                                    a_pkts,
                                    cfg,
                                    c_info,
                                    &h.game,
                                    &h.event_kind,
                                    h.first_event_ms,
                                    s,
                                    true,
                                    hl_meta,
                                )
                                .await
                                {
                                    if let Some(old_p) = supersede_old {
                                        if old_p != saved_p && old_p.exists() {
                                            let _ = std::fs::remove_file(&old_p);
                                            let old_json = old_p.with_extension("json");
                                            if old_json.exists() {
                                                let _ = std::fs::remove_file(&old_json);
                                            }
                                        }
                                    }
                                    if let Ok(mut guard) = last_seq_slot.lock() {
                                        *guard = Some((saved_p, seq_start_for_slot));
                                    }
                                }
                            });
                        }
                    }
                }
                let game_label = state
                    .active_game
                    .clone()
                    .unwrap_or_else(|| "Desktop".to_string());
                let pid = state.active_pid;
                ensure_capture(
                    &mut state,
                    &config,
                    &buffer,
                    &codec_info,
                    &sink,
                    &cancel_token,
                    game_label,
                    pid,
                );
            }
            res = event_rx.recv() => {
                match res {
                    Ok(event) => {
                        handle_event(
                            &event,
                            &mut state,
                            &config,
                            &buffer,
                            &codec_info,
                            &sink,
                            &cancel_token,
                            &save_tasks,
                        ).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        emit_error(&sink, "event_bus_lagged", &format!("missed {missed} events"));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        stop_capture(&mut state).await;
                        buffer.clear();
                        break;
                    }
                }
            }
        }
    }
}

fn ensure_capture(
    state: &mut SessionState,
    config: &Arc<RwLock<RecorderConfig>>,
    buffer: &buffer::RingBuffer<buffer::CompressedPacket>,
    codec_info: &Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    sink: &Option<EventSink>,
    parent_cancel: &CancellationToken,
    game_name: String,
    target_pid: Option<u32>,
) {
    if state.last_pushed_pid != target_pid {
        state.last_pushed_pid = target_pid;
        state
            .shared_target_pid
            .store(target_pid.unwrap_or(0) as usize, Ordering::Release);
    }
    if let Ok(cfg_g) = config.try_read() {
        state
            .shared_audio_mode
            .store(cfg_g.audio.to_usize(), Ordering::Relaxed);
        state
            .shared_audio_kbps
            .store(cfg_g.audio_bitrate_kbps as usize, Ordering::Relaxed);
        state
            .shared_mic_muted
            .store(cfg_g.mic_muted, Ordering::Relaxed);
        state
            .shared_separate_tracks
            .store(cfg_g.separate_audio_tracks, Ordering::Relaxed);
        if let Ok(mut md) = state.shared_mic_device.try_write() {
            if *md != cfg_g.mic_device {
                *md = cfg_g.mic_device.clone();
            }
        }
    }
    if let Ok(mut g) = state.shared_game_name.write() {
        if game_name.eq_ignore_ascii_case("desktop") {
            *g = None;
        } else {
            *g = Some(game_name.clone());
        }
    }

    let needs_spawn = match &state.video_thread {
        Some(h) => h.is_finished(),
        None => true,
    };
    if needs_spawn {
        let token = parent_cancel.child_token();
        let (v_handle, a_handle) = spawn_capture_workers(
            config.clone(),
            buffer.clone(),
            codec_info.clone(),
            sink.clone(),
            token.clone(),
            Some(game_name),
            target_pid,
            state.shared_target_pid.clone(),
            state.shared_audio_mode.clone(),
            state.shared_audio_kbps.clone(),
            state.shared_mic_muted.clone(),
            state.shared_separate_tracks.clone(),
            state.shared_mic_device.clone(),
            state.shared_game_name.clone(),
            state.captured_hwnd.clone(),
            state.reset_capture_signal.clone(),
            state.flush_signal.clone(),
            state.flush_done.clone(),
        );
        state.capture_cancel = Some(token);
        state.video_thread = v_handle;
        state.audio_thread = a_handle;
    }
}

async fn handle_event(
    event: &Event,
    state: &mut SessionState,
    config: &Arc<RwLock<RecorderConfig>>,
    buffer: &buffer::RingBuffer<buffer::CompressedPacket>,
    codec_info: &Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    sink: &Option<EventSink>,
    parent_cancel: &CancellationToken,
    save_tasks: &Arc<tokio::sync::Semaphore>,
) {
    if !event.kind.starts_with("stats.") {
        if let Some(match_rec) = state.stats_store.ingest_event(event) {
            if let Some(ref s) = sink {
                let monthly = state.stats_store.monthly_summary();
                s.emit(
                    "stats.match_recorded",
                    None,
                    json!({
                        "match": match_rec,
                        "monthly_summary": monthly,
                    }),
                );
            }
        }
    }
    match event.kind.as_str() {
        "stats.query_monthly" | "stats.request_summary" => {
            let days = event
                .data
                .get("days")
                .and_then(|v| v.as_u64())
                .unwrap_or(30) as u32;
            let summary = state.stats_store.summary_for_days(days, now_ms());
            if let Some(ref s) = sink {
                s.emit("stats.monthly_summary", None, json!(summary));
            }
            return;
        }
        "stats.query_game_history" => {
            let game = event
                .data
                .get("game")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let limit = event
                .data
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(25) as usize;
            let matches = state.stats_store.matches_for_game(game, limit);
            let live = state.stats_store.active_match_snapshot(game);
            if let Some(ref s) = sink {
                s.emit(
                    "stats.game_history",
                    None,
                    json!({
                        "game": game,
                        "live_match": live,
                        "matches": matches,
                    }),
                );
            }
            return;
        }
        "stats.reload" | "stats.add_custom_match" | "stats.delete_match" => {
            state.stats_store = crate::stats::LocalMatchStatsStore::load_or_default(
                crate::stats::resolve_local_match_stats_path(),
            );
            if let Some(ref s) = sink {
                let monthly = state.stats_store.monthly_summary();
                s.emit("stats.monthly_summary", None, json!(monthly));
            }
            return;
        }
        "round.start" | "round.freezetime" | "player.death" => {
            state.sequence_first_event_ms = None;
            state.sequence_last_event_ms = None;
            state.sequence_kill_count = 0;
            if let Ok(mut g) = state.last_sequence_saved.lock() {
                *g = None;
            }
            return;
        }
        "match.map_load" | "presence.update" => {
            if let Some(m) = event.data.get("map").and_then(|v| v.as_str()).map(str::trim) {
                if !m.is_empty() {
                    state.active_map = m.to_string();
                }
            }
            if let Some(md) = event.data.get("mode").or_else(|| event.data.get("queue_id")).and_then(|v| v.as_str()).map(str::trim) {
                if !md.is_empty() {
                    state.active_mode = md.to_string();
                }
            }
        }
        "game.started" | "game.pid_updated" | "match.start" => {
            if let Some(m) = event.data.get("map").and_then(|v| v.as_str()).map(str::trim) {
                if !m.is_empty() {
                    state.active_map = m.to_string();
                }
            }
            if let Some(md) = event.data.get("mode").or_else(|| event.data.get("queue_id")).and_then(|v| v.as_str()).map(str::trim) {
                if !md.is_empty() {
                    state.active_mode = md.to_string();
                }
            }
            let pid = event.pid.or_else(|| {
                event
                    .data
                    .get("pid")
                    .and_then(|v| v.as_u64())
                    .map(|p| p as u32)
            });
            let new_game = event.game.to_string();
            let switched_game = state.active_game.as_deref() != Some(new_game.as_str());
            let switched_pid = pid.is_some() && pid != state.active_pid;

            state.active_game = Some(new_game.clone());
            if pid.is_some() {
                state.active_pid = pid;
            }
            if switched_game {
                buffer.clear();
            }
            if switched_game || switched_pid {
                state.captured_hwnd.store(0, Ordering::Release);
                state.reset_capture_signal.store(true, Ordering::Release);
            }
            ensure_capture(
                state,
                config,
                buffer,
                codec_info,
                sink,
                parent_cancel,
                new_game,
                state.active_pid,
            );
        }
        "game.stopped" => {
            if let Some(ref current) = state.active_game {
                if event.game.as_ref() != current.as_str() {
                    return;
                }
            }
            state.active_pid = None;
            state.last_pushed_pid = None;
            state.shared_target_pid.store(0, Ordering::Release);

            if let Some(h) = state.pending_highlight.take() {
                wait_for_flush(&state.flush_signal, &state.flush_done).await;
                let cfg = lock_config(config);
                let pre_roll_ms = cfg.pre_roll_secs.clamp(1, 3600) * 1000;
                let clip_window_start = h.first_event_ms.saturating_sub(pre_roll_ms);
                let (mut v_pkts, mut a_pkts) =
                    buffer.snapshot_game_only_since(Some(clip_window_start));
                if v_pkts.is_empty() {
                    diag::log_line(
                        "HIGHLIGHT_FALLBACK",
                        "snapshot_game_only_since returned 0 video packets on game.stopped; falling back to snapshot_since",
                    );
                    let (fv, fa) = buffer.snapshot_since(Some(clip_window_start));
                    v_pkts = fv;
                    a_pkts = fa;
                }
                let hl_meta = ClipHighlightMeta {
                    map: h.map_name.clone(),
                    mode: h.mode_name.clone(),
                    weapon: h.weapon_name.clone(),
                    kills: h.kill_count,
                };
                let _ = save_clip_with_meta_async(
                    v_pkts,
                    a_pkts,
                    cfg,
                    codec_info.clone(),
                    &h.game,
                    &h.event_kind,
                    h.first_event_ms,
                    sink.clone(),
                    true,
                    hl_meta,
                )
                .await;
            }
            state.active_game = None;
            state.captured_hwnd.store(0, Ordering::Release);
            state.reset_capture_signal.store(true, Ordering::Release);
            ensure_capture(
                state,
                config,
                buffer,
                codec_info,
                sink,
                parent_cancel,
                "Desktop".to_string(),
                None,
            );
        }
        "manual.direct_clip" => {
            launch_direct_clip_capture(
                event,
                state.active_game.clone(),
                config,
                buffer,
                codec_info,
                &state.flush_signal,
                &state.flush_done,
                sink,
            );
        }
        "manual.clip" | "replay.hotkey" => {
            if state.video_thread.is_some() {
                wait_for_flush(&state.flush_signal, &state.flush_done).await;
            }
            let cfg = lock_config(config);
            let replay_window_ms = cfg.buffer_duration_secs.clamp(5, 7200) * 1000;
            let since_ms = event.at_ms.saturating_sub(replay_window_ms);
            let (v_pkts, a_pkts, detected_ctx) = buffer.snapshot_smart_manual(Some(since_ms));
            if !v_pkts.is_empty() {
                let fg_title = event
                    .data
                    .get("window_title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                let game = match detected_ctx {
                    buffer::CaptureContext::Game => {
                        state
                            .active_game
                            .clone()
                            .unwrap_or_else(|| extract_clean_window_app_name(fg_title))
                    }
                    buffer::CaptureContext::Social => {
                        let low = fg_title.to_lowercase();
                        if low.contains("teamspeak") {
                            "TeamSpeak".to_string()
                        } else if low.contains("telegram") {
                            "Telegram".to_string()
                        } else if low.contains("whatsapp") {
                            "WhatsApp".to_string()
                        } else {
                            "Discord".to_string()
                        }
                    }
                    buffer::CaptureContext::Media => {
                        let low = fg_title.to_lowercase();
                        if low.contains("youtube") {
                            "YouTube".to_string()
                        } else if low.contains("twitch") {
                            "Twitch".to_string()
                        } else if low.contains("spotify") {
                            "Spotify".to_string()
                        } else if low.contains("netflix") {
                            "Netflix".to_string()
                        } else {
                            "Media_Clip".to_string()
                        }
                    }
                    buffer::CaptureContext::Desktop => {
                        if !fg_title.is_empty() {
                            extract_clean_window_app_name(fg_title)
                        } else {
                            "Desktop_Replay".to_string()
                        }
                    }
                };
                let c_info = codec_info.clone();
                let s = sink.clone();
                let kind = event.kind.clone();
                let at = event.at_ms;
                let sem = save_tasks.clone();
                let permit = sem.try_acquire_owned().ok();
                tokio::spawn(async move {
                    let _permit = permit;
                    save_clip_async(v_pkts, a_pkts, cfg, c_info, &game, &kind, at, s, true).await;
                });
            } else if state.active_game.is_none() {
                launch_direct_clip_capture(
                    event,
                    None,
                    config,
                    buffer,
                    codec_info,
                    &state.flush_signal,
                    &state.flush_done,
                    sink,
                );
            } else {
                emit_error(
                    sink,
                    "buffer_empty_during_game",
                    "ring buffer empty, check focus detection",
                );
            }
        }
        "player.kill"
        | "player.knockdown"
        | "kill"
        | "workshop.kill"
        | "champion_kill"
        | "player.headshot"
        | "player.multikill"
        | "multikill"
        | "player.ace"
        | "ace"
        | "player.clutch"
        | "player.round_multikill_win"
        | "player.mvp"
        | "match.mvp"
        | "statfeed.mvp"
        | "bomb.defused"
        | "bomb.planted"
        | "sport.goal"
        | "sport.hat_trick"
        | "goal_scored"
        | "statfeed.goal"
        | "statfeed.aerial_goal"
        | "statfeed.long_goal"
        | "statfeed.overtime_goal"
        | "statfeed.hat_trick"
        | "statfeed.double_tap"
        | "sport.epic_save"
        | "statfeed.epic_save"
        | "statfeed.save"
        | "statfeed.savior"
        | "statfeed.demolish"
        | "statfeed.extermination"
        | "objective.first_blood"
        | "first_blood"
        | "objective.secured"
        | "dragon_kill"
        | "baron_kill"
        | "herald_kill"
        | "horde_kill"
        | "atakhan_kill" => {
            if matches!(
                event.kind.as_str(),
                "champion_kill"
                    | "multikill"
                    | "first_blood"
                    | "dragon_kill"
                    | "baron_kill"
                    | "herald_kill"
                    | "horde_kill"
                    | "atakhan_kill"
            ) {
                let by_local = event
                    .data
                    .get("by_local")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let assisted = event
                    .data
                    .get("assisted_by_local")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !by_local && !assisted {
                    return;
                }
            }
            if matches!(event.kind.as_str(), "bomb.planted" | "bomb.defused")
                && state.pending_highlight.is_none()
                && event.data.get("by_local").and_then(|v| v.as_bool()) != Some(true)
            {
                return;
            }
            let r = match event.kind.as_str() {
                "player.ace" | "ace" | "sport.hat_trick" => 10,
                "player.clutch" | "player.round_multikill_win" => 8,
                "player.multikill" | "multikill" => {
                    let count = event
                        .data
                        .get("count")
                        .or_else(|| event.data.get("kill_streak"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(2);
                    if count < 2 {
                        return;
                    }
                    7
                }
                "bomb.defused"
                | "sport.goal"
                | "goal_scored"
                | "statfeed.goal"
                | "statfeed.aerial_goal"
                | "statfeed.long_goal"
                | "statfeed.overtime_goal"
                | "statfeed.hat_trick"
                | "statfeed.double_tap"
                | "sport.epic_save"
                | "statfeed.epic_save"
                | "statfeed.save"
                | "statfeed.savior" => 6,
                "player.mvp"
                | "match.mvp"
                | "statfeed.mvp"
                | "objective.first_blood"
                | "first_blood"
                | "objective.secured"
                | "dragon_kill"
                | "baron_kill"
                | "herald_kill"
                | "horde_kill"
                | "atakhan_kill" => 4,
                "player.headshot" | "bomb.planted" => 3,
                _ => 2,
            };
            let cfg = lock_config(config);
            let chain_hold_secs = if r >= 8 {
                cfg.post_roll_secs.clamp(8, 3600)
            } else {
                cfg.post_roll_secs.clamp(12, 3600)
            };
            let (rank, post_roll_dur) = (r, Duration::from_secs(chain_hold_secs));

            let game = state
                .active_game
                .clone()
                .unwrap_or_else(|| event.game.to_string());
            let now = Instant::now();
            let max_highlight_span =
                Duration::from_secs(cfg.buffer_duration_secs.clamp(30, 7200).max(75));

            let ev_at_ms = if event.at_ms > 0 { event.at_ms } else { now_ms() };
            if let Some(prev_last_ms) = state.sequence_last_event_ms {
                if ev_at_ms.saturating_sub(prev_last_ms) > 65_000 {
                    state.sequence_first_event_ms = None;
                    state.sequence_kill_count = 0;
                    if let Ok(mut g) = state.last_sequence_saved.lock() {
                        *g = None;
                    }
                }
            }
            let payload_seq_start = event
                .data
                .get("sequence_start_ms")
                .and_then(|v| v.as_u64())
                .filter(|&ms| ms > 0 && ms <= ev_at_ms && ev_at_ms.saturating_sub(ms) <= 75_000);

            let effective_seq_start = match (state.sequence_first_event_ms, payload_seq_start) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => ev_at_ms,
            };
            state.sequence_first_event_ms = Some(effective_seq_start);
            state.sequence_last_event_ms = Some(ev_at_ms);

            let ev_map = event
                .data
                .get("map")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| state.active_map.clone());
            let ev_mode = event
                .data
                .get("mode")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| state.active_mode.clone());
            let ev_weapon = event
                .data
                .get("weapon")
                .or_else(|| event.data.get("ability"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_default();
            let raw_kills = event
                .data
                .get("round_kills")
                .or_else(|| event.data.get("streak_kills"))
                .or_else(|| event.data.get("count"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as u32;
            if matches!(
                event.kind.as_str(),
                "player.kill" | "kill" | "workshop.kill" | "champion_kill" | "player.knockdown"
            ) {
                state.sequence_kill_count = state.sequence_kill_count.saturating_add(1).max(raw_kills);
            } else {
                state.sequence_kill_count = state.sequence_kill_count.max(raw_kills);
            }
            let ev_kills = state.sequence_kill_count.max(raw_kills);
            let upgraded_kind = if ev_kills >= 5 && rank < 10 {
                "player.ace".to_string()
            } else if ev_kills >= 2 && rank < 7 {
                "player.multikill".to_string()
            } else {
                event.kind.clone()
            };
            let upgraded_rank = if ev_kills >= 5 {
                rank.max(10)
            } else if ev_kills >= 2 {
                rank.max(7)
            } else {
                rank
            };

            if let Some(ref mut pending) = state.pending_highlight {
                if effective_seq_start < pending.first_event_ms {
                    pending.first_event_ms = effective_seq_start;
                }
                pending.last_event_ms = ev_at_ms;
                let max_deadline = pending.first_instant + max_highlight_span;
                let new_deadline = (now + post_roll_dur).min(max_deadline);
                if new_deadline > pending.deadline {
                    pending.deadline = new_deadline;
                }
                if upgraded_rank >= pending.priority_rank {
                    pending.event_kind = upgraded_kind;
                    pending.priority_rank = upgraded_rank;
                }
                if !ev_map.is_empty() {
                    pending.map_name = ev_map;
                }
                if !ev_mode.is_empty() {
                    pending.mode_name = ev_mode;
                }
                if !ev_weapon.is_empty() {
                    pending.weapon_name = ev_weapon;
                }
                pending.kill_count = pending.kill_count.max(ev_kills);
            } else {
                let prev_saved_path = state
                    .last_sequence_saved
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .and_then(|(p, saved_seq_ms)| {
                        if saved_seq_ms.abs_diff(effective_seq_start) <= 5_000 {
                            Some(p)
                        } else {
                            None
                        }
                    });
                state.pending_highlight = Some(PendingHighlight {
                    game,
                    event_kind: upgraded_kind,
                    first_event_ms: effective_seq_start,
                    last_event_ms: ev_at_ms,
                    first_instant: now,
                    deadline: now + post_roll_dur,
                    priority_rank: upgraded_rank,
                    map_name: ev_map,
                    mode_name: ev_mode,
                    weapon_name: ev_weapon,
                    kill_count: ev_kills,
                    supersede_clip_path: prev_saved_path,
                });
            }
        }
        "discord.compress" | "clip.make_for_discord" | "manual.discord_clip" => {
            let explicit_path = event
                .data
                .get("path")
                .or_else(|| event.data.get("input_path"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from);

            let Some(target_video) = explicit_path.or_else(last_saved_clip) else {
                emit_error(
                    sink,
                    "discord_no_clip_found",
                    "no video path provided and no recent clip available to compress for Discord",
                );
                return;
            };

            let target_mb = event
                .data
                .get("target_mb")
                .and_then(|v| v.as_f64())
                .unwrap_or(DEFAULT_DISCORD_TARGET_MB);

            let output_path = event
                .data
                .get("output_path")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from);

            let opts = DiscordCompressOptions {
                target_mb,
                output_path,
                max_fps: None,
                prefer_hardware: true,
            };

            let s = sink.clone();
            let sem = save_tasks.clone();
            let permit = sem.try_acquire_owned().ok();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = make_it_for_discord_async(target_video, opts, s).await;
            });
        }
        "audio.toggle_mic_mute" | "mic.toggle_mute" | "hotkey.mic_mute" => {
            let new_muted = !state.shared_mic_muted.load(Ordering::Acquire);
            state.shared_mic_muted.store(new_muted, Ordering::Release);
            if let Ok(mut cfg_g) = config.write() {
                cfg_g.mic_muted = new_muted;
            }
            if let Some(ref s) = sink {
                s.emit(
                    "audio.mic_mute_changed",
                    None,
                    json!({
                        "muted": new_muted,
                    }),
                );
            }
        }
        "audio.set_mic_mute" => {
            let new_muted = event
                .data
                .get("muted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            state.shared_mic_muted.store(new_muted, Ordering::Release);
            if let Ok(mut cfg_g) = config.write() {
                cfg_g.mic_muted = new_muted;
            }
            if let Some(ref s) = sink {
                s.emit(
                    "audio.mic_mute_changed",
                    None,
                    json!({
                        "muted": new_muted,
                    }),
                );
            }
        }
        "audio.set_mic_device" => {
            let dev_opt = event
                .data
                .get("device")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("default"))
                .map(|s| s.to_string());
            if let Ok(mut md) = state.shared_mic_device.write() {
                *md = dev_opt.clone();
            }
            if let Ok(mut cfg_g) = config.write() {
                cfg_g.mic_device = dev_opt.clone();
            }
            if let Some(ref s) = sink {
                s.emit(
                    "audio.mic_device_changed",
                    None,
                    json!({
                        "device": dev_opt.unwrap_or_else(|| "default".to_string()),
                    }),
                );
            }
        }
        "settings.privacy.share_anonymous_stats"
        | "privacy.share_anonymous_stats"
        | "telemetry.set_enabled"
        | "analytics.set_enabled" => {
            if let Some(en) = crate::stats::parse_privacy_toggle_event_value(&event.data) {
                if let Ok(mut cfg_g) = config.write() {
                    cfg_g.share_anonymous_stats = en;
                }
                let _ = crate::stats::persist_share_anonymous_stats(en);
                if let Some(ref s) = sink {
                    s.emit(
                        "settings.privacy.updated",
                        None,
                        json!({
                            "path": crate::stats::PRIVACY_SETTING_PATH,
                            "share_anonymous_stats": en,
                            "enabled": en,
                            "status": if en { "On" } else { "Off" },
                        }),
                    );
                }
            }
        }
        "settings.updated" | "recorder.reload_config" => {
            let cfg_snap = lock_config(config);
            let clamped_secs = cfg_snap.buffer_duration_secs.clamp(5, 7200);
            buffer.set_limits(
                Duration::from_secs(clamped_secs),
                (cfg_snap.fps.as_u32() as u64 * clamped_secs).min(32_768) as usize,
            );
            state
                .shared_audio_mode
                .store(cfg_snap.audio.to_usize(), Ordering::Release);
            state
                .shared_audio_kbps
                .store(cfg_snap.audio_bitrate_kbps as usize, Ordering::Release);
            state
                .shared_mic_muted
                .store(cfg_snap.mic_muted, Ordering::Release);
            state
                .shared_separate_tracks
                .store(cfg_snap.separate_audio_tracks, Ordering::Release);
            if let Ok(mut md) = state.shared_mic_device.write() {
                *md = cfg_snap.mic_device.clone();
            }
            state.reset_capture_signal.store(true, Ordering::Release);
        }
        other_kind if other_kind.starts_with("editor.") => {
            crate::editor::handle_editor_event(event, sink);
        }
        _ => {}
    }
}

#[cfg(not(windows))]
fn spawn_capture_workers(
    _config: Arc<RwLock<RecorderConfig>>,
    _buffer: buffer::RingBuffer<buffer::CompressedPacket>,
    _codec_info: Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    sink: Option<EventSink>,
    cancel: CancellationToken,
    _game_name: Option<String>,
    _target_pid: Option<u32>,
    _shared_target_pid: Arc<AtomicUsize>,
    _shared_audio_mode: Arc<AtomicUsize>,
    _shared_audio_kbps: Arc<AtomicUsize>,
    _shared_mic_muted: Arc<AtomicBool>,
    _shared_separate_tracks: Arc<AtomicBool>,
    _shared_mic_device: Arc<RwLock<Option<String>>>,
    _shared_game_name: Arc<RwLock<Option<String>>>,
    _captured_hwnd: Arc<AtomicUsize>,
    _reset_capture_signal: Arc<AtomicBool>,
    flush_signal: Arc<AtomicBool>,
    flush_done: Arc<AtomicBool>,
) -> (
    Option<std::thread::JoinHandle<()>>,
    Option<std::thread::JoinHandle<()>>,
) {
    let display_srv = linux_support::LinuxDisplayServer::detect();
    let backend = display_srv.recommended_backend();
    if let Some(ref s) = sink {
        s.emit(
            "recorder.linux_ready",
            None,
            json!({
                "display_server": format!("{:?}", display_srv),
                "recommended_backend": format!("{:?}", backend),
            }),
        );
    }
    let v_thread = std::thread::Builder::new()
        .name("clotch-video-linux".into())
        .spawn(move || {
            priority::set_low_priority();
            while !cancel.is_cancelled() {
                if flush_signal.swap(false, Ordering::AcqRel) {
                    flush_done.store(true, Ordering::Release);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if flush_signal.swap(false, Ordering::AcqRel) {
                flush_done.store(true, Ordering::Release);
            }
        })
        .ok();
    (v_thread, None)
}

#[cfg(windows)]
fn spawn_capture_workers(
    config: Arc<RwLock<RecorderConfig>>,
    buffer: buffer::RingBuffer<buffer::CompressedPacket>,
    codec_info: Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    sink: Option<EventSink>,
    cancel: CancellationToken,
    game_name: Option<String>,
    target_pid: Option<u32>,
    shared_target_pid: Arc<AtomicUsize>,
    shared_audio_mode: Arc<AtomicUsize>,
    shared_audio_kbps: Arc<AtomicUsize>,
    shared_mic_muted: Arc<AtomicBool>,
    shared_separate_tracks: Arc<AtomicBool>,
    shared_mic_device: Arc<RwLock<Option<String>>>,
    shared_game_name: Arc<RwLock<Option<String>>>,
    captured_hwnd: Arc<AtomicUsize>,
    reset_capture_signal: Arc<AtomicBool>,
    flush_signal: Arc<AtomicBool>,
    flush_done: Arc<AtomicBool>,
) -> (
    Option<std::thread::JoinHandle<()>>,
    Option<std::thread::JoinHandle<()>>,
) {
    let capture_start_ms = now_ms();
    let epoch_qpc = capture::query_qpc();
    let qpc_freq = capture::query_qpc_freq();

    let v_cancel = cancel.clone();
    let v_buffer = buffer.clone();
    let v_codec_info = codec_info;
    let v_sink = sink.clone();
    let v_config = config;
    let v_game = game_name;
    let v_pid = target_pid;
    let v_shared_pid = shared_target_pid.clone();
    let v_captured_hwnd = captured_hwnd;
    let v_start_ms = capture_start_ms;
    let v_shared_game = shared_game_name.clone();

    let audio_thread = crate::audio::spawn_audio_worker(
        cancel,
        buffer,
        sink,
        capture_start_ms,
        epoch_qpc,
        qpc_freq,
        shared_target_pid,
        shared_audio_mode,
        shared_audio_kbps,
        shared_mic_muted,
        shared_separate_tracks,
        shared_mic_device,
        shared_game_name,
    );

    let video_thread = std::thread::Builder::new()
        .name("clotch-video-dxgi".into())
        .spawn(move || {
            priority::set_video_priority();

            let init_cfg = lock_config(&v_config);
            let mut dst_w = (init_cfg.width.max(64)) & !1;
            let mut dst_h = (init_cfg.height.max(64)) & !1;
            let mut current_fps = init_cfg.fps.as_u32().max(1);
            let mut current_bitrate = (init_cfg.quality.bitrate_kbps(current_fps) * 1000) as u32;
            let mut keyframe_interval_frames = current_fps * 2;

            let mut resolved_pid = v_pid;
            let existing_hwnd = v_captured_hwnd.load(Ordering::Acquire);
            let mut target_hwnd = if existing_hwnd != 0 && existing_hwnd != usize::MAX {
                Some(windows::Win32::Foundation::HWND(existing_hwnd as *mut _))
            } else {
                resolved_pid.and_then(capture::find_window_by_pid)
            };

            if target_hwnd.is_none() {
                if let Some((hwnd, pid)) = capture::find_target_window(v_game.as_deref()) {
                    target_hwnd = Some(hwnd);
                    if pid != 0 {
                        resolved_pid = Some(pid);
                        v_shared_pid.store(pid as usize, Ordering::Relaxed);
                    }
                }
            }

            if let Some(hwnd) = target_hwnd {
                v_captured_hwnd.store(hwnd.0 as usize, Ordering::Release);
            } else {
                v_captured_hwnd.store(usize::MAX, Ordering::Release);
            }

            let mut duplicator_opt: Option<capture::DxgiDuplicator> = None;
            let mut converter_opt: Option<capture::GpuColorConverter> = None;
            let mut encoder_opt: Option<capture::HardwareH264Encoder> = None;

            let mut avcc_scratch: Vec<u8> = Vec::with_capacity(512 * 1024);
            let mut min_frame_qpc_step = (qpc_freq / current_fps as i64).max(1);
            let mut frame_dur_hns = (10_000_000 / current_fps as i64).max(1);
            let mut last_encoded_qpc: i64 = 0;
            let mut next_frame_qpc: i64 = 0;
            let mut has_pending_staged_frame = false;
            let mut has_valid_nv12_frame = false;
            let mut last_monitor_check = Instant::now();
            let mut last_full_window_scan = Instant::now();
            let mut last_switch_target: Option<windows::Win32::Graphics::Gdi::HMONITOR> = None;
            let mut consecutive_timeouts: u32 = 0;
            let mut next_reinit_timeout_threshold: u32 = 240;
            let mut force_next_idr = true;
            let is_low_end_cfg = init_cfg.low_end_mode;
            let mut last_capture_ctx = buffer::CaptureContext::Desktop;
            let mut last_init_error = String::new();

            while !v_cancel.is_cancelled() {
                let dyn_pid = v_shared_pid.load(Ordering::Relaxed) as u32;
                let active_pid_filter = if dyn_pid != 0 {
                    Some(dyn_pid)
                } else {
                    resolved_pid
                };
                let cur_game_for_ctx = v_shared_game
                    .tx_or_clone()
                    .or_else(|| v_game.clone());
                let cur_ctx = capture::detect_foreground_context(
                    active_pid_filter,
                    cur_game_for_ctx.as_deref(),
                );
                if cur_ctx != last_capture_ctx {
                    last_capture_ctx = cur_ctx;
                    v_buffer.set_active_context(cur_ctx);
                    force_next_idr = true;
                }

                if reset_capture_signal.swap(false, Ordering::AcqRel) {
                    let latest_cfg = lock_config(&v_config);
                    let new_w = (latest_cfg.width.max(64)) & !1;
                    let new_h = (latest_cfg.height.max(64)) & !1;
                    let new_fps = latest_cfg.fps.as_u32().max(1);
                    let new_bitrate = (latest_cfg.quality.bitrate_kbps(new_fps) * 1000) as u32;
                    if new_w != dst_w || new_h != dst_h || new_fps != current_fps || new_bitrate != current_bitrate {
                        dst_w = new_w;
                        dst_h = new_h;
                        current_fps = new_fps;
                        current_bitrate = new_bitrate;
                        keyframe_interval_frames = current_fps * 2;
                        min_frame_qpc_step = (qpc_freq / current_fps as i64).max(1);
                        frame_dur_hns = (10_000_000 / current_fps as i64).max(1);
                        converter_opt = None;
                        encoder_opt = None;
                    }
                    has_valid_nv12_frame = false;
                    has_pending_staged_frame = false;
                    next_frame_qpc = 0;
                    consecutive_timeouts = 0;
                    next_reinit_timeout_threshold = 240;
                    force_next_idr = true;
                    let cur_game_str = v_shared_game
                        .tx_or_clone()
                        .or_else(|| v_game.clone());
                    let new_hwnd = active_pid_filter
                        .and_then(capture::find_window_by_pid)
                        .or_else(|| {
                            capture::find_target_window(cur_game_str.as_deref()).map(
                                |(h, found_pid)| {
                                    if found_pid != 0 {
                                        resolved_pid = Some(found_pid);
                                        v_shared_pid.store(found_pid as usize, Ordering::Relaxed);
                                    }
                                    h
                                },
                            )
                        });
                    if let Some(hwnd) = new_hwnd {
                        target_hwnd = Some(hwnd);
                        v_captured_hwnd.store(hwnd.0 as usize, Ordering::Release);
                        if let Some(ref cur_dupl) = duplicator_opt {
                            let hmon = unsafe {
                                windows::Win32::Graphics::Gdi::MonitorFromWindow(
                                    hwnd,
                                    windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
                                )
                            };
                            if !hmon.0.is_null() && hmon != cur_dupl.monitor {
                                duplicator_opt = None;
                                converter_opt = None;
                                encoder_opt = None;
                            }
                        }
                    }
                    if let Some(ref mut d) = duplicator_opt {
                        let _ = d.reinitialize_duplication();
                    }
                }

                if flush_signal.swap(false, Ordering::AcqRel) {
                    if let (Some(dupl), Some(conv), Some(ref mut enc)) =
                        (duplicator_opt.as_ref(), converter_opt.as_ref(), &mut encoder_opt)
                    {
                        let cur_qpc = capture::query_qpc();
                        let qpc_delta = (cur_qpc - epoch_qpc).max(0) as u128;
                        let rel_ms = ((qpc_delta * 1000) / (qpc_freq as u128)) as u64;
                        let pts_ms = v_start_ms.wrapping_add(rel_ms);
                        let rel_hns = ((qpc_delta * 10_000_000) / (qpc_freq as u128)) as i64;
                        if has_pending_staged_frame {
                            if conv.convert_staged_to_nv12(&dupl.context, &dupl.video_context).is_ok() {
                                has_pending_staged_frame = false;
                                has_valid_nv12_frame = true;
                                last_encoded_qpc = cur_qpc;
                                let _ = enc.encode_nv12_frame(
                                    dupl,
                                    conv,
                                    pts_ms,
                                    rel_hns,
                                    frame_dur_hns,
                                    false,
                                    &mut avcc_scratch,
                                    &v_buffer,
                                    &v_codec_info,
                                );
                            }
                        } else if has_valid_nv12_frame {
                            let _ = enc.encode_repeat_frame(
                                pts_ms,
                                rel_hns,
                                frame_dur_hns,
                                &mut avcc_scratch,
                                &v_buffer,
                                &v_codec_info,
                            );
                        }
                        enc.drain_output_packets(
                            pts_ms,
                            &mut avcc_scratch,
                            &v_buffer,
                            &v_codec_info,
                        );
                    }
                    flush_done.store(true, Ordering::Release);
                }

                if last_monitor_check.elapsed() >= Duration::from_secs(2) {
                    last_monitor_check = Instant::now();
                    if let Ok(live_cfg) = v_config.try_read() {
                        let next_w = (live_cfg.width.max(64)) & !1;
                        let next_h = (live_cfg.height.max(64)) & !1;
                        let next_fps = live_cfg.fps.as_u32().max(1);
                        let next_bitrate = (live_cfg.quality.bitrate_kbps(next_fps) * 1000) as u32;
                        if next_w != dst_w || next_h != dst_h || next_fps != current_fps || next_bitrate != current_bitrate {
                            dst_w = next_w;
                            dst_h = next_h;
                            current_fps = next_fps;
                            current_bitrate = next_bitrate;
                            keyframe_interval_frames = current_fps * 2;
                            min_frame_qpc_step = (qpc_freq / current_fps as i64).max(1);
                            frame_dur_hns = (10_000_000 / current_fps as i64).max(1);
                            converter_opt = None;
                            encoder_opt = None;
                            has_pending_staged_frame = false;
                            next_frame_qpc = 0;
                            force_next_idr = true;
                        }
                    }
                    let valid_existing = target_hwnd
                        .filter(|&h| capture::is_valid_game_window(h));

                    let candidate_hwnd = if let Some(hwnd) = valid_existing {
                        Some(hwnd)
                    } else if last_full_window_scan.elapsed() >= Duration::from_secs(10) {
                        last_full_window_scan = Instant::now();
                        let cur_game_str = v_shared_game
                            .tx_or_clone()
                            .or_else(|| v_game.clone());
                        active_pid_filter
                            .and_then(capture::find_window_by_pid)
                            .or_else(|| {
                                capture::find_target_window(cur_game_str.as_deref()).map(
                                    |(h, found_pid)| {
                                        if found_pid != 0 {
                                            resolved_pid = Some(found_pid);
                                            v_shared_pid
                                                .store(found_pid as usize, Ordering::Relaxed);
                                        }
                                        h
                                    },
                                )
                            })
                    } else {
                        None
                    };

                    if let (Some(hwnd), Some(cur_mon)) =
                        (candidate_hwnd, duplicator_opt.as_ref().map(|d| d.monitor))
                    {
                        target_hwnd = Some(hwnd);
                        v_captured_hwnd.store(hwnd.0 as usize, Ordering::Release);
                        let hmon = unsafe {
                            windows::Win32::Graphics::Gdi::MonitorFromWindow(
                                hwnd,
                                windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
                            )
                        };
                        if hmon.0.is_null() || hmon == cur_mon {
                            last_switch_target = None;
                        } else if Some(hmon) != last_switch_target {
                            last_switch_target = Some(hmon);
                            duplicator_opt = None;
                            converter_opt = None;
                            encoder_opt = None;
                            has_valid_nv12_frame = false;
                            has_pending_staged_frame = false;
                            next_frame_qpc = 0;
                            force_next_idr = true;
                        }
                    }
                }

                if duplicator_opt.is_none() || converter_opt.is_none() || encoder_opt.is_none() {
                    match capture::DxgiDuplicator::new_for_window(target_hwnd) {
                        Ok(dupl) => {
                            let (eff_w, eff_h) =
                                capture::GpuColorConverter::clamp_resolution_for_vram(
                                    &dupl.device,
                                    dst_w,
                                    dst_h,
                                );
                            let conv_res = capture::GpuColorConverter::new(
                                &dupl.device,
                                &dupl.video_device,
                                dupl.src_width,
                                dupl.src_height,
                                eff_w,
                                eff_h,
                                current_fps,
                            );
                            let enc_res = capture::HardwareH264Encoder::new_with_sink(
                                &dupl,
                                eff_w,
                                eff_h,
                                current_fps,
                                current_bitrate,
                                keyframe_interval_frames,
                                v_start_ms,
                                &v_codec_info,
                                v_sink.clone(),
                            );
                            match (conv_res, enc_res) {
                                (Ok(conv), Ok(mut enc)) => {
                                    if !conv.has_hardware_processor() && enc.zero_copy_active {
                                        enc.disable_zero_copy("no hardware processor");
                                    }
                                    last_init_error.clear();
                                    duplicator_opt = Some(dupl);
                                    converter_opt = Some(conv);
                                    encoder_opt = Some(enc);
                                    has_pending_staged_frame = false;
                                    next_frame_qpc = 0;
                                    force_next_idr = true;
                                }
                                (Err(e), _) | (_, Err(e)) => {
                                    if last_init_error != e {
                                        last_init_error = e.clone();
                                        emit_error(&v_sink, "dxgi_pipeline_init_failed", &e);
                                    }
                                    std::thread::sleep(Duration::from_millis(500));
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            if last_init_error != e {
                                last_init_error = e.clone();
                                emit_error(&v_sink, "dxgi_duplicator_init_failed", &e);
                            }
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    }
                }

                let pre_qpc = capture::query_qpc();
                if next_frame_qpc <= 0 {
                    next_frame_qpc = pre_qpc;
                }
                if (has_valid_nv12_frame || has_pending_staged_frame)
                    && !flush_signal.load(Ordering::Relaxed)
                {
                    let rem_qpc = next_frame_qpc - pre_qpc;
                    if rem_qpc > 0 {
                        let rem_us = ((rem_qpc as u128 * 1_000_000) / (qpc_freq as u128)) as u64;
                        if has_pending_staged_frame {
                            if rem_us > 1800 {
                                std::thread::sleep(Duration::from_micros((rem_us - 1200).min(12_000)));
                            }
                            while capture::query_qpc() < next_frame_qpc {
                                std::hint::spin_loop();
                            }
                        }
                    }
                }

                let Some(dupl) = duplicator_opt.as_mut() else {
                    continue;
                };
                if dupl.duplication.is_none() && dupl.reinitialize_duplication().is_err() {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }

                let mut frame_info =
                    windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO::default();
                let mut desktop_resource: Option<windows::Win32::Graphics::Dxgi::IDXGIResource> =
                    None;

                let base_frame_ms = (1000 / current_fps.max(1)).max(1);
                let rem_to_grid_us = (((next_frame_qpc - capture::query_qpc()).max(0) as u128
                    * 1_000_000)
                    / (qpc_freq as u128)) as u64;
                let rem_to_grid_ms = (rem_to_grid_us / 1000) as u32;
                let wait_timeout_ms = if has_pending_staged_frame || (current_fps >= 90 && rem_to_grid_us < 1500) {
                    0
                } else if last_capture_ctx == buffer::CaptureContext::Game {
                    rem_to_grid_ms.clamp(1, base_frame_ms)
                } else if is_low_end_cfg {
                    (base_frame_ms * 2).clamp(16, 40)
                } else {
                    rem_to_grid_ms.clamp(2, base_frame_ms)
                };

                let Some(ref dupl_iface_active) = dupl.duplication else {
                    continue;
                };
                let acquire_res = unsafe {
                    dupl_iface_active.AcquireNextFrame(
                        wait_timeout_ms,
                        &mut frame_info,
                        &mut desktop_resource,
                    )
                };

                match acquire_res {
                    Ok(()) => {
                        if let Some(res) = desktop_resource.take() {
                            consecutive_timeouts = 0;
                            next_reinit_timeout_threshold = 240;
                            if let Ok(bgra_tex) = windows::core::Interface::cast::<
                                windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
                            >(&res)
                            {
                                let mut tex_desc =
                                    windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
                                unsafe {
                                    bgra_tex.GetDesc(&mut tex_desc);
                                }

                                if tex_desc.Width >= 64 && tex_desc.Height >= 64 {
                                    let (eff_w, eff_h) =
                                        capture::GpuColorConverter::clamp_resolution_for_vram(
                                            &dupl.device,
                                            dst_w,
                                            dst_h,
                                        );
                                    let need_recreate_conv = converter_opt
                                        .as_ref()
                                        .map(|c| {
                                            c.src_width != tex_desc.Width
                                                || c.src_height != tex_desc.Height
                                                || c.dst_width != eff_w
                                                || c.dst_height != eff_h
                                                || c.raw_src_format != tex_desc.Format
                                        })
                                        .unwrap_or(true);

                                    if need_recreate_conv {
                                        if let Ok(new_conv) =
                                            capture::GpuColorConverter::new_with_format(
                                                &dupl.device,
                                                &dupl.video_device,
                                                tex_desc.Width,
                                                tex_desc.Height,
                                                eff_w,
                                                eff_h,
                                                current_fps,
                                                tex_desc.Format,
                                            )
                                        {
                                            converter_opt = Some(new_conv);
                                        }
                                    }

                                    if let Some(ref conv) = converter_opt {
                                        let need_recreate_enc = encoder_opt
                                            .as_ref()
                                            .map(|e| {
                                                e.width != conv.dst_width
                                                    || e.height != conv.dst_height
                                                    || e.fps != current_fps
                                                    || e.bitrate_bps != current_bitrate
                                            })
                                            .unwrap_or(true);

                                        if need_recreate_enc {
                                            if let Ok(mut new_enc) = capture::HardwareH264Encoder::new_with_sink(
                                                dupl,
                                                conv.dst_width,
                                                conv.dst_height,
                                                current_fps,
                                                current_bitrate,
                                                keyframe_interval_frames,
                                                v_start_ms,
                                                &v_codec_info,
                                                v_sink.clone(),
                                            ) {
                                                if !conv.has_hardware_processor() && new_enc.zero_copy_active {
                                                    new_enc.disable_zero_copy("no hardware processor");
                                                }
                                                encoder_opt = Some(new_enc);
                                                force_next_idr = true;
                                            }
                                        }
                                    }

                                    if let (Some(ref conv), Some(_)) = (&converter_opt, &encoder_opt) {
                                        if conv
                                            .stage_desktop_frame(&dupl.context, &bgra_tex)
                                            .is_ok()
                                        {
                                            has_pending_staged_frame = true;
                                        }
                                    }
                                }
                            }
                            drop(res);
                        }

                        unsafe {
                            if let Some(ref dupl_iface) = dupl.duplication {
                                let _ = dupl_iface.ReleaseFrame();
                            }
                        }
                    }
                    Err(e)
                        if e.code()
                            == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT =>
                    {
                        if !has_pending_staged_frame {
                            consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                            if !has_valid_nv12_frame
                                && active_pid_filter.is_some()
                                && consecutive_timeouts >= next_reinit_timeout_threshold
                            {
                                next_reinit_timeout_threshold =
                                    (next_reinit_timeout_threshold.saturating_mul(2)).min(3840);
                                let _ = dupl.reinitialize_duplication();
                            }
                        }
                    }
                    Err(e)
                        if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST
                            || e.code()
                                == windows::Win32::Graphics::Dxgi::DXGI_ERROR_INVALID_CALL =>
                    {
                        let _ = dupl.reinitialize_duplication();
                        continue;
                    }
                    Err(e)
                        if e.code()
                            == windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED
                            || e.code()
                                == windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_RESET =>
                    {
                        duplicator_opt = None;
                        converter_opt = None;
                        encoder_opt = None;
                        continue;
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                }

                {
                    let cur_qpc = capture::query_qpc();
                    let tolerance_qpc = (min_frame_qpc_step / 16).max(1);
                    let is_first_frame = !has_valid_nv12_frame && has_pending_staged_frame;
                    let deadline_reached = cur_qpc >= (next_frame_qpc - tolerance_qpc);

                    if is_first_frame || deadline_reached {
                        let encode_qpc = if is_first_frame
                            || (cur_qpc - next_frame_qpc) > (min_frame_qpc_step * 2)
                        {
                            next_frame_qpc = cur_qpc + min_frame_qpc_step;
                            cur_qpc
                        } else {
                            let q = next_frame_qpc;
                            next_frame_qpc += min_frame_qpc_step;
                            q
                        };
                        let qpc_delta = (encode_qpc - epoch_qpc).max(0) as u128;
                        let rel_ms = ((qpc_delta * 1000) / (qpc_freq as u128)) as u64;
                        let pts_ms = v_start_ms.wrapping_add(rel_ms);
                        let rel_hns = ((qpc_delta * 10_000_000) / (qpc_freq as u128)) as i64;

                        if has_pending_staged_frame {
                            if let (Some(conv), Some(enc)) =
                                (converter_opt.as_ref(), encoder_opt.as_mut())
                            {
                                if conv.convert_staged_to_nv12(&dupl.context, &dupl.video_context).is_ok() {
                                    has_pending_staged_frame = false;
                                    has_valid_nv12_frame = true;
                                    last_encoded_qpc = encode_qpc;
                                    let is_idr = force_next_idr;
                                    force_next_idr = false;
                                    let _ = enc.encode_nv12_frame(
                                        dupl,
                                        conv,
                                        pts_ms,
                                        rel_hns,
                                        frame_dur_hns,
                                        is_idr,
                                        &mut avcc_scratch,
                                        &v_buffer,
                                        &v_codec_info,
                                    );
                                }
                            }
                        } else if has_valid_nv12_frame {
                            let repeat_mult = 1i64;
                            if (encode_qpc - last_encoded_qpc)
                                >= (min_frame_qpc_step * repeat_mult)
                            {
                                last_encoded_qpc = encode_qpc;
                                if let Some(enc) = encoder_opt.as_mut() {
                                    let _ = enc.encode_repeat_frame(
                                        pts_ms,
                                        rel_hns,
                                        frame_dur_hns,
                                        &mut avcc_scratch,
                                        &v_buffer,
                                        &v_codec_info,
                                    );
                                }
                            } else if let Some(ref mut enc) = encoder_opt {
                                enc.drain_output_packets(
                                    pts_ms,
                                    &mut avcc_scratch,
                                    &v_buffer,
                                    &v_codec_info,
                                );
                            }
                        }
                    } else if let Some(ref mut enc) = encoder_opt {
                        let qpc_delta = (cur_qpc - epoch_qpc).max(0) as u128;
                        let rel_ms = ((qpc_delta * 1000) / (qpc_freq as u128)) as u64;
                        let pts_ms = v_start_ms.wrapping_add(rel_ms);
                        enc.drain_output_packets(
                            pts_ms,
                            &mut avcc_scratch,
                            &v_buffer,
                            &v_codec_info,
                        );
                    }
                }
            }

            if flush_signal.swap(false, Ordering::AcqRel) {
                flush_done.store(true, Ordering::Release);
            }
        })
        .ok();

    (video_thread, audio_thread)
}

pub(crate) trait RwLockOptStringExt {
    fn tx_or_clone(&self) -> Option<String>;
}

impl RwLockOptStringExt for Arc<RwLock<Option<String>>> {
    fn tx_or_clone(&self) -> Option<String> {
        match self.read() {
            Ok(g) => g.clone(),
            Err(poison) => poison.into_inner().clone(),
        }
    }
}

async fn stop_capture(state: &mut SessionState) {
    if let Some(token) = state.capture_cancel.take() {
        token.cancel();
    }
    if let Some(v_handle) = state.video_thread.take() {
        let _ = tokio::time::timeout(
            Duration::from_secs(4),
            tokio::task::spawn_blocking(move || v_handle.join()),
        )
        .await;
    }
    if let Some(a_handle) = state.audio_thread.take() {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::task::spawn_blocking(move || a_handle.join()),
        )
        .await;
    }
    state.captured_hwnd.store(0, Ordering::Release);
}

pub use super::formats::format_clip_filename;

static SAVE_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_SAVES as usize);

#[derive(Clone, Debug, Default)]
struct ClipHighlightMeta {
    map: String,
    mode: String,
    weapon: String,
    kills: u32,
}

async fn save_clip_async(
    video_packets: Vec<Arc<buffer::CompressedPacket>>,
    audio_packets: Vec<Arc<buffer::CompressedPacket>>,
    config: RecorderConfig,
    codec_info: Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    game: &str,
    event_kind: &str,
    event_ms: u64,
    sink: Option<EventSink>,
    is_priority: bool,
) {
    let _ = save_clip_with_meta_async(
        video_packets,
        audio_packets,
        config,
        codec_info,
        game,
        event_kind,
        event_ms,
        sink,
        is_priority,
        ClipHighlightMeta::default(),
    )
    .await;
}

async fn save_clip_with_meta_async(
    video_packets: Vec<Arc<buffer::CompressedPacket>>,
    audio_packets: Vec<Arc<buffer::CompressedPacket>>,
    config: RecorderConfig,
    codec_info: Arc<Mutex<Option<encoder::CodecContextInfo>>>,
    game: &str,
    event_kind: &str,
    event_ms: u64,
    sink: Option<EventSink>,
    is_priority: bool,
    hl_meta: ClipHighlightMeta,
) -> Option<std::path::PathBuf> {
    if video_packets.is_empty() && audio_packets.is_empty() {
        return None;
    }

    let _permit = SAVE_PERMITS.acquire().await.ok();

    let clip_dir = match storage::StorageManager::resolve_game_clip_dir(&config.storage, game, None)
    {
        Ok(dir) => dir,
        Err(e) => {
            emit_error(&sink, "storage_resolution_failed", &e);
            return None;
        }
    };

    if let Err(e) =
        storage::StorageManager::ensure_free_space(&clip_dir, config.storage.min_free_gb)
    {
        emit_error(&sink, "storage_full", &e);
        return None;
    }

    let filename = format_clip_filename(game, event_kind, event_ms);
    let output_path = clip_dir.join(filename);

    let info_opt = match codec_info.lock() {
        Ok(g) => g.clone(),
        Err(poison) => poison.into_inner().clone(),
    };

    let info = match info_opt {
        Some(i) => i,
        None => {
            emit_error(
                &sink,
                "missing_codec_info",
                "cannot mux clip without video sps/pps headers",
            );
            return None;
        }
    };

    let game_owned = game.to_string();
    let event_kind_owned = event_kind.to_string();
    let output_path_clone = output_path.clone();
    let config_clone = config.clone();
    let separate_audio_tracks = config.separate_audio_tracks;
    let res = tokio::task::spawn_blocking(move || {
        priority::set_low_priority();
        encoder::EncoderPipeline::mux_clip(
            &video_packets,
            &audio_packets,
            &output_path_clone,
            &config_clone,
            &info,
        )
    })
    .await;

    match res {
        Ok(Ok(())) => {
            set_last_saved_clip(output_path.clone());
            let sidecar_path = output_path.with_extension("json");
            let created_epoch_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let now_local = chrono::Local::now().format("%b %d %H:%M").to_string();
            let clean_tag = match event_kind_owned.as_str() {
                "player.ace" | "ace" => "Ace",
                "player.clutch" | "clutch" => "Clutch",
                "player.multikill" | "player.round_multikill_win" | "multikill" => "Multikill",
                "player.headshot" | "headshot" => "Headshot",
                "player.kill" | "kill" => "Kill",
                "sport.goal" | "goal" => "Goal",
                "sport.epic_save" | "save" => "Save",
                "manual.direct_clip" | "direct" => "Direct",
                "manual.clip" | "replay" => "Replay",
                _ => "Clip",
            };
            let clean_game = if game_owned.trim().is_empty() || game_owned.eq_ignore_ascii_case("system") {
                "Desktop".to_string()
            } else {
                game_owned.clone()
            };
            let default_title = format!("{clean_game} {clean_tag}, {now_local}");
            let inferred_kills: u32 = if hl_meta.kills > 0 {
                hl_meta.kills
            } else {
                match clean_tag {
                    "Ace" => 5,
                    "Multikill" => 3,
                    "Clutch" | "Headshot" | "Kill" => 1,
                    _ => 0,
                }
            };
            let sidecar_json = json!({
                "display_title": default_title,
                "game": clean_game,
                "tag": clean_tag,
                "event_kind": event_kind_owned,
                "map": hl_meta.map,
                "mode": hl_meta.mode,
                "weapon": hl_meta.weapon,
                "kills": inferred_kills,
                "favorite": false,
                "created_epoch_secs": created_epoch_secs,
                "has_separate_stems": separate_audio_tracks,
            });
            if let Ok(serialized) = serde_json::to_string_pretty(&sidecar_json) {
                let _ = std::fs::write(&sidecar_path, serialized);
            }
            if let Some(ref s) = sink {
                s.emit(
                    "clip.saved",
                    None,
                    json!({
                        "path": output_path.to_string_lossy(),
                        "game": game_owned,
                        "event": event_kind_owned,
                        "priority": is_priority,
                    }),
                );
            }
            Some(output_path)
        }
        Ok(Err(e)) => {
            emit_error(&sink, "clip_mux_failed", &e);
            None
        }
        Err(join_err) => {
            emit_error(&sink, "mux_task_panicked", &format!("{join_err}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::buffer::{AudioStemPackets, CaptureContext, CompressedPacket, PacketKind, RingBuffer};
    use super::config::{RecorderConfig, Quality};
    use super::encoder::{CodecContextInfo, EncoderPipeline};
    use bytes::Bytes;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::time::Duration;

    fn make_video_pkt(pts_ms: u64, is_keyframe: bool) -> Arc<CompressedPacket> {
        let mut avcc = Vec::with_capacity(16);
        let nal_type: u8 = if is_keyframe { 0x65 } else { 0x41 };
        let payload = [nal_type, 0x88, 0x84, 0x21, 0xA0];
        avcc.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&payload);
        Arc::new(CompressedPacket {
            kind: PacketKind::Video,
            pts_ms,
            pts: pts_ms as i64,
            dts: pts_ms as i64,
            duration: 16,
            is_keyframe,
            data: Bytes::from(avcc),
            stems: None,
        })
    }

    fn make_audio_pkt(pts_ms: u64, with_stems: bool) -> Arc<CompressedPacket> {
        let raw_aac = Bytes::from_static(&[0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C]);
        let stems = if with_stems {
            Some(Arc::new(AudioStemPackets {
                game: raw_aac.clone(),
                discord: raw_aac.clone(),
                mic: raw_aac.clone(),
                other: raw_aac.clone(),
            }))
        } else {
            None
        };
        Arc::new(CompressedPacket {
            kind: PacketKind::Audio,
            pts_ms,
            pts: pts_ms as i64,
            dts: pts_ms as i64,
            duration: 1024,
            is_keyframe: true,
            data: raw_aac,
            stems,
        })
    }

    #[test]
    fn test_default_config_is_pure_and_low_end_disables_stems() {
        let mut cfg = RecorderConfig::default();
        assert!(!cfg.separate_audio_tracks);
        assert!(!cfg.low_end_mode);
        cfg.separate_audio_tracks = true;
        cfg.apply_low_end_optimizations();
        assert!(cfg.low_end_mode);
        assert!(!cfg.separate_audio_tracks);
        assert_eq!(cfg.width, 1280);
        assert_eq!(cfg.height, 720);
        assert_eq!(cfg.quality, Quality::Medium);
    }

    #[test]
    fn test_ring_buffer_snapshots_and_alt_tab_fallback() {
        let buf = RingBuffer::new(Duration::from_secs(30), 1800);
        buf.set_active_context(CaptureContext::Game);
        buf.push_video(1000, make_video_pkt(1000, true));
        buf.push_video(1016, make_video_pkt(1016, false));
        buf.push_audio(1000, make_audio_pkt(1000, false));

        buf.set_active_context(CaptureContext::Desktop);
        buf.push_video(2000, make_video_pkt(2000, true));
        buf.push_video(2016, make_video_pkt(2016, false));
        buf.push_audio(2000, make_audio_pkt(2000, false));

        let (game_v, game_a) = buf.snapshot_game_only_since(Some(1000));
        assert_eq!(game_v.len(), 2);
        assert_eq!(game_a.len(), 1);

        let (empty_game_v, _) = buf.snapshot_game_only_since(Some(1900));
        assert!(empty_game_v.is_empty());

        let (smart_v, smart_a, ctx) = buf.snapshot_smart_manual(Some(1900));
        assert_eq!(ctx, CaptureContext::Desktop);
        assert_eq!(smart_v.len(), 2);
        assert_eq!(smart_a.len(), 1);
    }

    #[test]
    fn test_mux_clip_av_sync_across_frame_gaps_under_25ms() {
        let mut v_pkts = Vec::new();
        let mut a_pkts = Vec::new();

        for i in 0..60u64 {
            let pts = 1000 + i * 16;
            v_pkts.push(make_video_pkt(pts, i % 30 == 0));
        }
        for i in 0..60u64 {
            let pts = 2500 + i * 16;
            v_pkts.push(make_video_pkt(pts, i % 30 == 0));
        }

        let video_end_pts = v_pkts.last().map(|p| p.pts_ms).unwrap_or(0) + 16;
        let mut a_pts = 1000u64;
        let mut a_idx = 0u64;
        while a_pts < video_end_pts {
            a_pkts.push(make_audio_pkt(a_pts, true));
            a_idx += 1;
            a_pts = 1000 + (a_idx * 1024 * 1000) / 48_000;
        }

        let tmp_dir = std::env::temp_dir().join("clotch_mux_unit_test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let out_path = tmp_dir.join("gap_sync_test.mp4");
        let _ = std::fs::remove_file(&out_path);

        let cfg = RecorderConfig::default();
        let info = CodecContextInfo {
            width: 1280,
            height: 720,
            fps: 60,
            sps: vec![0x67, 0x64, 0x00, 0x1F, 0xAC, 0xD9, 0x40, 0x50, 0x05, 0xBB, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xC0, 0xF1, 0x83, 0x19, 0x60],
            pps: vec![0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0],
        };

        let res = EncoderPipeline::mux_clip(&v_pkts, &a_pkts, &out_path, &cfg, &info);
        assert!(res.is_ok(), "mux_clip failed: {:?}", res.err());
        assert!(out_path.is_file());
        assert!(!out_path.with_extension("mp4.tmp").exists());

        let bytes = std::fs::read(&out_path).unwrap_or_default();
        let size = bytes.len() as u64;
        let reader = mp4::Mp4Reader::read_header(Cursor::new(bytes), size);
        assert!(reader.is_ok());
        if let Ok(mp4) = reader {
            let v_dur_ms = mp4
                .tracks()
                .get(&1)
                .map(|t| t.duration().as_millis() as i64)
                .unwrap_or(0);
            let a_dur_ms = mp4
                .tracks()
                .get(&2)
                .map(|t| t.duration().as_millis() as i64)
                .unwrap_or(0);
            let drift_ms = (v_dur_ms - a_dur_ms).abs();
            assert!(
                drift_ms < 25,
                "A/V sync drift across 540ms frame gap was {drift_ms}ms (v={v_dur_ms}ms, a={a_dur_ms}ms)"
            );
        }
        let _ = std::fs::remove_file(&out_path);
    }
}