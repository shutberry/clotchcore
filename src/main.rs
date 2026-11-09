#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(dead_code, unused_imports)]
use std::path::{Path, PathBuf};
use serde_json::json;
use tokio_util::sync::CancellationToken;

mod achievements;
mod analytics;
mod bridge;
mod discord;
mod events;
pub mod audio_engine;
pub mod editor;
pub mod gui;
pub mod recorder;

use recorder::{
    list_input_devices, make_it_for_discord, parse_duration_secs, parse_resolution_pair,
    AudioMode, DiscordCompressOptions, Fps, Keybind, KeybindsConfig, Quality, Recorder,
    RecorderConfig, StorageConfig, StorageManager, StorageMode, SystemHardwareProfile,
    DEFAULT_DISCORD_TARGET_MB,
};

struct CliArgs {
    fps: Option<Fps>,
    quality: Option<Quality>,
    resolution: Option<(u32, u32)>,
    audio: Option<AudioMode>,
    audio_bitrate_kbps: Option<u32>,
    mic_device: Option<String>,
    mic_muted: Option<bool>,
    separate_audio_tracks: Option<bool>,
    list_mics: bool,
    self_test: bool,
    buffer_secs: Option<u64>,
    direct_clip_secs: Option<u64>,
    pre_roll_secs: Option<u64>,
    post_roll_secs: Option<u64>,
    hotkey_replay: Option<Keybind>,
    hotkey_direct: Option<Keybind>,
    hotkey_discord: Option<Keybind>,
    hotkey_mic_mute: Option<Keybind>,
    output_dir: Option<PathBuf>,
    regenerate_token: bool,
    discord_input: Option<PathBuf>,
    discord_output: Option<PathBuf>,
    discord_target_mb: f64,
    discord_app_id: Option<String>,
    low_end_mode: Option<bool>,
    telemetry_enabled: Option<bool>,
    headless: bool,
}

fn print_help() {
    println!(
        r#"Clotch / Skirmish Game Replay & Telemetry Engine

USAGE:
    clotch [OPTIONS]
    clotch --list-mics
    clotch --self-test
    clotch --make-it-for-discord <VIDEO_PATH> [--target-mb <MB>] [--discord-out <OUTPUT_PATH>]
    clotch make-it-for-discord <VIDEO_PATH> [--target-mb <MB>] [--discord-out <OUTPUT_PATH>]

OPTIONS:
    --fps <30|60|120|15..240>            Capture frame rate (default: 60)
    --quality <low|medium|high|lossless> Encoding quality preset (default: high)
    --resolution <720p|1080p|1440p|WxH>  Capture resolution preset or custom size (default: 1080p)
    --audio <none|system|mic|both>       Audio capture source (default: both)
    --audio-bitrate <64..192>            Audio AAC bitrate in kbps (default: 128, low-end: 96)
    --mic-device <default|NAME|INDEX>    Microphone device name, index, or ID (default: system default)
    --list-mics                          List all connected microphones on this PC and exit
    --self-test, --test-games            Run offline simulation test across all 17 games and exit
    --mute-mic                           Start with microphone muted (can toggle anytime with F7)
    --separate-tracks                    Enable multi-track audio separation (Game/Discord/Mic/Other)
    --single-track                       Disable multi-track separation (Master Mix track only)
    --buffer-secs <TIME>                 Replay buffer length, e.g. 45s, 2m, 1h (default: 60s)
    --direct-clip-secs <TIME>            Direct foreground clip duration, e.g. 15s, 2m (default: 15s)
    --pre-roll <TIME>                    Highlight pre-roll before event, e.g. 10s, 30s (default: 10s)
    --post-roll <TIME>                   Highlight post-roll after event, e.g. 8s, 15s (default: 8s)
    --hotkey-replay <KEY>                Custom replay hotkey, e.g. F8, Alt+F8 (default: F8)
    --hotkey-direct <KEY>                Custom direct clip hotkey, e.g. F9, Alt+F9 (default: F9)
    --hotkey-discord <KEY>               Custom Discord compress hotkey, e.g. F10 (default: F10)
    --hotkey-mic-mute <KEY>              Custom mic mute/unmute toggle hotkey, e.g. F7, Alt+M (default: F7)
    --output-dir <PATH>                  Custom clip output directory (default: auto partition)
    --low-end, --eco                     Force Eco Low-End hardware mode (ultra-low CPU/RAM/GPU overhead)
    --no-low-end                         Disable automatic Eco Low-End hardware mode
    --share-anonymous-stats <on|off>     Toggle Settings -> Privacy -> Share anonymous stats (default: on)
    --no-telemetry                       Alias for --share-anonymous-stats off
    --regenerate-token                   Generate and save a new secure GSI authentication token
    --make-it-for-discord <FILE>         Compress a video clip for non-Nitro Discord (~7.5 MB) using GPU
    --discord <FILE>                     Alias for --make-it-for-discord <FILE>
    --target-mb <MB>                     Target size in MB for Discord compression (default: 7.5)
    --discord-out, -o <PATH>             Custom output path for Discord-compressed video
    -h, --help                           Print help information
"#
    );
}

fn parse_cli_args() -> Result<Option<CliArgs>, String> {
    let mut args = std::env::args().skip(1);
    let mut fps = None;
    let mut quality = None;
    let mut resolution = None;
    let mut audio = None;
    let mut audio_bitrate_kbps = None;
    let mut mic_device = None;
    let mut mic_muted = None;
    let mut separate_audio_tracks = None;
    let mut list_mics = false;
    let mut self_test = false;
    let mut buffer_secs = None;
    let mut direct_clip_secs = None;
    let mut pre_roll_secs = None;
    let mut post_roll_secs = None;
    let mut hotkey_replay = None;
    let mut hotkey_direct = None;
    let mut hotkey_discord = None;
    let mut hotkey_mic_mute = None;
    let mut output_dir = None;
    let mut regenerate_token = false;
    let mut discord_input = None;
    let mut discord_output = None;
    let mut discord_target_mb = DEFAULT_DISCORD_TARGET_MB;
    let mut discord_app_id = None;
    let mut low_end_mode = None;
    let mut telemetry_enabled = None;
    let mut headless = false;

    while let Some(arg) = args.next() {
        let (key, inline_val) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg, None),
        };

        match key.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            "--list-mics" | "--list-microphones" => {
                list_mics = true;
            }
            "--self-test" | "--test-games" | "self-test" | "test-games" => {
                self_test = true;
            }
            "--headless" | "--cli" | "--no-gui" => {
                headless = true;
            }
            "--fps" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--fps requires a value (e.g. 30, 60, 120)".to_string())?;
                fps = Some(Fps::parse_str(&val).ok_or_else(|| {
                    format!("unsupported fps: '{val}' (allowed: 15..=240)")
                })?);
            }
            "--quality" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--quality requires a value".to_string())?;
                quality = Some(Quality::parse_str(&val).ok_or_else(|| {
                    format!("unsupported quality: '{val}' (allowed: low, medium, high, lossless)")
                })?);
            }
            "--resolution" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--resolution requires a value".to_string())?;
                resolution = Some(parse_resolution_pair(&val).ok_or_else(|| {
                    format!("invalid resolution '{val}', expected 720p, 1080p, 1440p, 4k or WxH")
                })?);
            }
            "--audio" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--audio requires a value".to_string())?;
                audio = Some(AudioMode::parse_str(&val).ok_or_else(|| {
                    format!("unsupported audio mode: '{val}' (allowed: none, system, mic, both)")
                })?);
            }
            "--audio-bitrate" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--audio-bitrate requires a numeric value in kbps".to_string())?;
                let clean = val.trim_end_matches("kbps").trim_end_matches('k').trim();
                let kbps = clean
                    .parse::<u32>()
                    .map_err(|_| format!("invalid audio bitrate: '{val}'"))?;
                audio_bitrate_kbps = Some(kbps.clamp(64, 192));
            }
            "--mic-device" | "--microphone" | "--mic" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--mic-device requires a device name, index, or 'default'".to_string())?;
                mic_device = Some(val);
            }
            "--mute-mic" | "--mic-muted" => {
                mic_muted = Some(true);
            }
            "--unmute-mic" => {
                mic_muted = Some(false);
            }
            "--separate-tracks" | "--multi-track-audio" => {
                separate_audio_tracks = Some(true);
            }
            "--single-track" | "--no-separate-tracks" => {
                separate_audio_tracks = Some(false);
            }
            "--buffer-secs" | "--buffer-time" | "--recording-time" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--buffer-secs requires a duration (e.g. 60, 2m, 1h)".to_string())?;
                let secs = parse_duration_secs(&val)
                    .ok_or_else(|| format!("invalid buffer duration: '{val}'"))?;
                buffer_secs = Some(secs.clamp(5, 7200));
            }
            "--direct-clip-secs" | "--direct-clip-time" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--direct-clip-secs requires a duration (e.g. 15, 2m)".to_string())?;
                let secs = parse_duration_secs(&val)
                    .ok_or_else(|| format!("invalid direct-clip duration: '{val}'"))?;
                direct_clip_secs = Some(secs.clamp(3, 3600));
            }
            "--pre-roll" | "--pre-roll-secs" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--pre-roll requires a duration (e.g. 10s, 30s)".to_string())?;
                let secs = parse_duration_secs(&val)
                    .ok_or_else(|| format!("invalid pre-roll duration: '{val}'"))?;
                pre_roll_secs = Some(secs.clamp(1, 3600));
            }
            "--post-roll" | "--post-roll-secs" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--post-roll requires a duration (e.g. 8s, 15s)".to_string())?;
                let secs = parse_duration_secs(&val)
                    .ok_or_else(|| format!("invalid post-roll duration: '{val}'"))?;
                post_roll_secs = Some(secs.clamp(1, 3600));
            }
            "--hotkey-replay" | "--key-replay" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--hotkey-replay requires a keybind (e.g. F8, Alt+F8)".to_string())?;
                hotkey_replay = Some(
                    Keybind::parse(&val)
                        .ok_or_else(|| format!("invalid keybind for --hotkey-replay: '{val}'"))?,
                );
            }
            "--hotkey-direct" | "--key-direct" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--hotkey-direct requires a keybind (e.g. F9, Alt+F9)".to_string())?;
                hotkey_direct = Some(
                    Keybind::parse(&val)
                        .ok_or_else(|| format!("invalid keybind for --hotkey-direct: '{val}'"))?,
                );
            }
            "--hotkey-discord" | "--key-discord" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--hotkey-discord requires a keybind (e.g. F10)".to_string())?;
                hotkey_discord = Some(
                    Keybind::parse(&val)
                        .ok_or_else(|| format!("invalid keybind for --hotkey-discord: '{val}'"))?,
                );
            }
            "--hotkey-mic-mute" | "--key-mic-mute" | "--hotkey-mute" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--hotkey-mic-mute requires a keybind (e.g. F7, Alt+M)".to_string())?;
                hotkey_mic_mute = Some(
                    Keybind::parse(&val)
                        .ok_or_else(|| format!("invalid keybind for --hotkey-mic-mute: '{val}'"))?,
                );
            }
            "--output-dir" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--output-dir requires a path".to_string())?;
                output_dir = Some(PathBuf::from(val));
            }
            "--low-end" | "--eco" => {
                low_end_mode = Some(true);
            }
            "--no-low-end" => {
                low_end_mode = Some(false);
            }
            "--telemetry" | "--analytics" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--telemetry requires on or off".to_string())?;
                telemetry_enabled = Some(!matches!(
                    val.trim().to_lowercase().as_str(),
                    "off" | "false" | "0" | "no" | "disable" | "disabled"
                ));
            }
            "--no-telemetry" | "--disable-telemetry" => {
                telemetry_enabled = Some(false);
                let _ = analytics::persist_share_anonymous_stats(false);
            }
            "--share-anonymous-stats" | "--privacy-share-anonymous-stats" => {
                let raw = match inline_val {
                    Some(v) => v,
                    None => args.next().ok_or_else(|| {
                        "--share-anonymous-stats requires a value (on|off|true|false)".to_string()
                    })?,
                };
                let en = !matches!(
                    raw.trim().to_lowercase().as_str(),
                    "0" | "false" | "no" | "off" | "disabled"
                );
                telemetry_enabled = Some(en);
                let _ = analytics::persist_share_anonymous_stats(en);
            }
            "--regenerate-token" | "--new-token" => {
                regenerate_token = true;
            }
            "make-it-for-discord" | "--make-it-for-discord" | "--discord" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--make-it-for-discord requires a video file path".to_string())?;
                discord_input = Some(PathBuf::from(val));
            }
            "--target-mb" | "--discord-mb" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--target-mb requires a size in MB (e.g. 7.5)".to_string())?;
                let parsed = val
                    .parse::<f64>()
                    .map_err(|_| format!("invalid --target-mb value: '{val}'"))?;
                if parsed <= 0.5 || parsed > 500.0 {
                    return Err("--target-mb must be between 0.5 and 500.0".to_string());
                }
                discord_target_mb = parsed;
            }
            "--discord-out" | "-o" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--discord-out requires an output file path".to_string())?;
                discord_output = Some(PathBuf::from(val));
            }
            "--discord-app-id" | "--discord-client-id" => {
                let val = inline_val
                    .or_else(|| args.next())
                    .ok_or_else(|| "--discord-app-id requires a Discord Application ID".to_string())?;
                discord_app_id = Some(val);
            }
            unknown => {
                return Err(format!("unknown argument: '{unknown}'. Use --help for usage."));
            }
        }
    }

    Ok(Some(CliArgs {
        fps,
        quality,
        resolution,
        audio,
        audio_bitrate_kbps,
        mic_device,
        mic_muted,
        separate_audio_tracks,
        list_mics,
        self_test,
        buffer_secs,
        direct_clip_secs,
        pre_roll_secs,
        post_roll_secs,
        hotkey_replay,
        hotkey_direct,
        hotkey_discord,
        hotkey_mic_mute,
        output_dir,
        regenerate_token,
        discord_input,
        discord_output,
        discord_target_mb,
        discord_app_id,
        low_end_mode,
        telemetry_enabled,
        headless,
    }))
}

fn load_config_low_end_override() -> Option<bool> {
    let cfg_path = discord::locate_config_file()?;
    let content = std::fs::read_to_string(cfg_path).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim().to_lowercase();
            let val = v.trim().trim_matches('"').trim_matches('\'').trim().to_lowercase();
            if matches!(key.as_str(), "low_end_mode" | "eco_mode" | "potato_mode") {
                return Some(matches!(val.as_str(), "true" | "1" | "yes" | "on"));
            }
        }
    }
    None
}

fn load_or_generate_gsi_token(force_regenerate: bool) -> String {
    let config_path = discord::locate_config_file()
        .unwrap_or_else(|| PathBuf::from("clotch_config.toml"));
    let mut existing_lines: Vec<String> = Vec::new();
    if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if !force_regenerate {
                    if let Some(rest) = trimmed.strip_prefix("gsi_token") {
                        let rest = rest.trim();
                        if let Some(val) = rest.strip_prefix('=') {
                            let token = val.trim().trim_matches('"').trim_matches('\'').trim();
                            if !token.is_empty() {
                                return token.to_string();
                            }
                        }
                    }
                }
                if !trimmed.starts_with("gsi_token") {
                    existing_lines.push(line.to_string());
                }
            }
        }
    }
    let new_token = uuid::Uuid::new_v4().to_string();
    if config_path.exists() {
        let bak = config_path.with_extension("toml.bak");
        let _ = std::fs::copy(&config_path, &bak);
    }
    let mut out_lines = Vec::with_capacity(existing_lines.len() + 1);
    out_lines.push(format!("gsi_token = \"{new_token}\""));
    out_lines.extend(existing_lines);
    let body = out_lines.join("\n") + "\n";
    let tmp = config_path.with_extension("toml.tmp");
    if std::fs::write(&tmp, &body)
        .and_then(|_| std::fs::rename(&tmp, &config_path))
        .is_ok()
    {
        println!(
            "[CONFIG] GSI security token initialized: {}",
            config_path.display()
        );
    }
    new_token
}

fn auto_install_game_integrations(token: &str) {
    if let Some(cs2_cfg) = events::locate_cs2_cfg_dir() {
        let gsi = events::GsiSource::new(events::presets::CS2_GSI_PORT, token);
        match gsi.install(&cs2_cfg) {
            Ok(p) => println!("[CONFIG] CS2 Gamestate Integration installed: {}", p.display()),
            Err(e) => eprintln!("[CONFIG WARN] CS2 GSI configuration could not be written: {e}"),
        }
    }

    if let Some(dota2_cfg) = events::locate_dota2_cfg_dir() {
        let cfg_body = format!(
            "\"Dota 2 Integration Configuration\"\n{{\n    \"uri\" \"http://127.0.0.1:{}/gsi\"\n    \"timeout\" \"5.0\"\n    \"buffer\" \"0.1\"\n    \"throttle\" \"0.1\"\n    \"heartbeat\" \"30.0\"\n    \"data\"\n    {{\n        \"provider\" \"1\"\n        \"map\" \"1\"\n        \"player\" \"1\"\n        \"hero\" \"1\"\n        \"abilities\" \"1\"\n        \"items\" \"1\"\n    }}\n    \"auth\"\n    {{\n        \"token\" \"{}\"\n    }}\n}}\n",
            events::presets::DOTA2_GSI_PORT,
            token
        );
        let path = dota2_cfg.join("gamestate_integration_clotch.cfg");
        if path.exists() {
            let bak = dota2_cfg.join("gamestate_integration_clotch.cfg.bak");
            let _ = std::fs::copy(&path, &bak);
        }
        let tmp = dota2_cfg.join("gamestate_integration_clotch.cfg.tmp");
        if std::fs::write(&tmp, cfg_body)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .is_ok()
        {
            println!(
                "[CONFIG] Dota 2 Gamestate Integration installed: {}",
                path.display()
            );
        }
    }

    if let Some(rl_cfg) = events::locate_rocket_league_config_dir() {
        match events::enable_rocket_league_stats_api(
            &rl_cfg,
            events::presets::ROCKET_LEAGUE_PORT,
            10,
        ) {
            Ok(p) => println!(
                "[CONFIG] Rocket League Match Stats API configured: {}",
                p.display()
            ),
            Err(e) => {
                eprintln!("[CONFIG WARN] Rocket League Stats API configuration failed: {e}")
            }
        }
    }

    if let Some(apex_dir) = events::locate_apex_cfg_dir() {
        match events::install_apex_liveapi_config(&apex_dir, events::APEX_LIVEAPI_PORT) {
            Ok(p) => println!(
                "[CONFIG] Apex Legends LiveAPI telemetry configured: {}",
                p.display()
            ),
            Err(e) => {
                eprintln!("[CONFIG WARN] Apex Legends LiveAPI configuration failed: {e}")
            }
        }
    }
}

#[cfg(windows)]
fn spawn_hotkey_listener(
    hotkey_sink: events::EventSink,
    shared_config: std::sync::Arc<std::sync::RwLock<RecorderConfig>>,
    mut event_rx: tokio::sync::broadcast::Receiver<events::Event>,
    cancel: CancellationToken,
) -> std::thread::JoinHandle<()> {
    let rt_handle = tokio::runtime::Handle::try_current().ok();
    std::thread::Builder::new()
        .name("clotch-hotkey".into())
        .spawn(move || {
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::System::Threading::GetCurrentThreadId;
            use windows::Win32::UI::Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
            };
            use windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage,
                MSG, PM_NOREMOVE, WM_HOTKEY, WM_QUIT, WM_USER,
            };

            let thread_id = unsafe { GetCurrentThreadId() };
            let cancel_clone = cancel.clone();
            let hotkey_replay_id = 1;
            let hotkey_direct_id = 2;
            let hotkey_discord_id = 3;
            let hotkey_mic_mute_id = 4;
            let wm_reload_keybinds = WM_USER + 42;
            let wm_pause_hotkeys = WM_USER + 43;
            let wm_resume_hotkeys = WM_USER + 44;

            let read_kb = || -> KeybindsConfig {
                shared_config
                    .read()
                    .map(|g| g.keybinds.clone())
                    .unwrap_or_default()
            };

            let mut keybinds = read_kb();

            unsafe {
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0, PM_NOREMOVE);
                let _ = RegisterHotKey(
                    HWND(std::ptr::null_mut()),
                    hotkey_replay_id,
                    HOT_KEY_MODIFIERS(keybinds.replay_clip.modifiers),
                    keybinds.replay_clip.vk,
                );
                let _ = RegisterHotKey(
                    HWND(std::ptr::null_mut()),
                    hotkey_direct_id,
                    HOT_KEY_MODIFIERS(keybinds.direct_clip.modifiers),
                    keybinds.direct_clip.vk,
                );
                let _ = RegisterHotKey(
                    HWND(std::ptr::null_mut()),
                    hotkey_discord_id,
                    HOT_KEY_MODIFIERS(keybinds.discord_clip.modifiers),
                    keybinds.discord_clip.vk,
                );
                let _ = RegisterHotKey(
                    HWND(std::ptr::null_mut()),
                    hotkey_mic_mute_id,
                    HOT_KEY_MODIFIERS(keybinds.mic_mute.modifiers),
                    keybinds.mic_mute.vk,
                );
            }

            if let Some(handle) = rt_handle {
                let c_quit = cancel_clone.clone();
                handle.spawn(async move {
                    c_quit.cancelled().await;
                    unsafe {
                        let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
                    }
                });
                handle.spawn(async move {
                    while let Ok(ev) = event_rx.recv().await {
                        if ev.kind == "settings.keybinds.updated" {
                            unsafe {
                                let _ = PostThreadMessageW(
                                    thread_id,
                                    wm_reload_keybinds,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                            }
                        } else if ev.kind == "settings.hotkeys.pause" {
                            let paused = ev
                                .data
                                .get("paused")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let msg_id = if paused {
                                wm_pause_hotkeys
                            } else {
                                wm_resume_hotkeys
                            };
                            unsafe {
                                let _ = PostThreadMessageW(
                                    thread_id,
                                    msg_id,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                            }
                        }
                    }
                });
            }

            unsafe {
                let mut msg = MSG::default();
                while !cancel.is_cancelled()
                    && GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool()
                {
                    if msg.message == wm_pause_hotkeys {
                        let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_replay_id);
                        let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_direct_id);
                        let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_discord_id);
                        let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_mic_mute_id);
                    } else if msg.message == wm_reload_keybinds || msg.message == wm_resume_hotkeys {
                        let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_replay_id);
                        let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_direct_id);
                        let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_discord_id);
                        let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_mic_mute_id);
                        keybinds = read_kb();
                        let _ = RegisterHotKey(
                            HWND(std::ptr::null_mut()),
                            hotkey_replay_id,
                            HOT_KEY_MODIFIERS(keybinds.replay_clip.modifiers),
                            keybinds.replay_clip.vk,
                        );
                        let _ = RegisterHotKey(
                            HWND(std::ptr::null_mut()),
                            hotkey_direct_id,
                            HOT_KEY_MODIFIERS(keybinds.direct_clip.modifiers),
                            keybinds.direct_clip.vk,
                        );
                        let _ = RegisterHotKey(
                            HWND(std::ptr::null_mut()),
                            hotkey_discord_id,
                            HOT_KEY_MODIFIERS(keybinds.discord_clip.modifiers),
                            keybinds.discord_clip.vk,
                        );
                        let _ = RegisterHotKey(
                            HWND(std::ptr::null_mut()),
                            hotkey_mic_mute_id,
                            HOT_KEY_MODIFIERS(keybinds.mic_mute.modifiers),
                            keybinds.mic_mute.vk,
                        );
                    } else if msg.message == WM_HOTKEY {
                        let id = msg.wParam.0 as i32;
                        let (fg_hwnd, fg_title) = {
                            let hwnd =
                                windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
                            if hwnd.0.is_null() {
                                (0usize, String::new())
                            } else {
                                let mut title_buf = [0u16; 256];
                                let len = windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(
                                    hwnd,
                                    &mut title_buf,
                                );
                                let title = if len > 0 {
                                    String::from_utf16_lossy(&title_buf[..len as usize])
                                } else {
                                    String::new()
                                };
                                (hwnd.0 as usize, title)
                            }
                        };
                        if id == hotkey_replay_id {
                            hotkey_sink.emit(
                                "manual.clip",
                                None,
                                json!({
                                    "hotkey": keybinds.replay_clip.label,
                                    "window_hwnd": fg_hwnd,
                                    "window_title": fg_title,
                                }),
                            );
                        } else if id == hotkey_direct_id {
                            hotkey_sink.emit(
                                "manual.direct_clip",
                                None,
                                json!({
                                    "hotkey": keybinds.direct_clip.label,
                                    "window_hwnd": fg_hwnd,
                                    "window_title": fg_title,
                                }),
                            );
                        } else if id == hotkey_discord_id {
                            hotkey_sink.emit(
                                "clip.make_for_discord",
                                None,
                                json!({
                                    "hotkey": keybinds.discord_clip.label,
                                    "target_mb": DEFAULT_DISCORD_TARGET_MB,
                                }),
                            );
                        } else if id == hotkey_mic_mute_id {
                            hotkey_sink.emit(
                                "audio.toggle_mic_mute",
                                None,
                                json!({
                                    "hotkey": keybinds.mic_mute.label,
                                }),
                            );
                        }
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_replay_id);
                let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_direct_id);
                let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_discord_id);
                let _ = UnregisterHotKey(HWND(std::ptr::null_mut()), hotkey_mic_mute_id);
            }
        })
        .unwrap_or_else(|_| std::thread::spawn(|| {}))
}

#[cfg(not(windows))]
fn spawn_hotkey_listener(
    _hotkey_sink: events::EventSink,
    _shared_config: std::sync::Arc<std::sync::RwLock<RecorderConfig>>,
    _event_rx: tokio::sync::broadcast::Receiver<events::Event>,
    _cancel: CancellationToken,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| {})
}

fn format_human_duration(secs: u64) -> String {
    if secs >= 3600 && secs % 3600 == 0 {
        format!("{}h ({} seconds)", secs / 3600, secs)
    } else if secs >= 60 && secs % 60 == 0 {
        format!("{}m ({} seconds)", secs / 60, secs)
    } else {
        format!("{secs} seconds")
    }
}

fn print_banner(
    config: &RecorderConfig,
    hw: &SystemHardwareProfile,
    active_storage: &Path,
    games: &[&str],
) {
    let actual_bitrate = config.quality.bitrate_kbps(config.fps.as_u32()) as f64 / 1000.0;
    let quality_name = format!("{:?} ({} Mbps)", config.quality, actual_bitrate);
    let audio_name = match config.audio {
        AudioMode::None => "Disabled".to_string(),
        AudioMode::SystemOnly => format!("System Audio Only ({} kbps)", config.audio_bitrate_kbps),
        AudioMode::MicOnly => format!("Microphone Only ({} kbps)", config.audio_bitrate_kbps),
        AudioMode::Both => format!(
            "System + Microphone (AAC 48kHz @ {} kbps)",
            config.audio_bitrate_kbps
        ),
    };
    let mic_label = config
        .mic_device
        .as_deref()
        .unwrap_or("System Default Microphone");
    let mic_state_str = if config.mic_muted { "MUTED" } else { "ACTIVE" };
    let stem_mode_str = if config.separate_audio_tracks {
        "Multi-Track Stem Separation (Master + Game + Discord + Mic + Other)"
    } else {
        "Single Master Track"
    };
    let eco_label = if config.low_end_mode {
        "ECO LOW-END ACTIVE (Zero-Alloc GPU / Minimal CPU)"
    } else {
        "Standard Hardware Accelerated"
    };

    println!(
        r#"
========================================================================
   ____ _       ___ _____ ____ _   _   ____  _____ ____ _        _  ____ 
  / ___| |     / _ \_   _/ ___| | | | |  _ \| ____|  _ \ |      / \|  _ \
 | |   | |    | | | || || |   | |_| | | |_) |  _| | |_) | |     / _ \ |_) |
 | |___| |___ | |_| || || |___|  _  | |  _ <| |___|  __/| |___ / ___ \  __/ 
  \____|_____| \___/ |_| \____|_| |_| |_| \_\_____|_|   |_____/_/   \_\_|    
========================================================================
 Clotch / Skirmish Replay & Telemetry Engine
 Status: RUNNING & PROCESS WATCHER ACTIVE
========================================================================
 Hardware Check:      {} Logical Cores | {} MB RAM ({})
 Target Resolution:   {}x{}
 Frame Rate:          {} FPS
 Quality Preset:      {}
 Audio Mode:          {}
 Microphone Input:    {} [{}]
 Editor Audio Tracks: {}
 Replay Duration:     {} (Pre: {}s / Post: {}s)
 Direct Clip Length:  {}
 Storage Partition:   {}
 Hotkeys:             {} (Clip), {} (Direct), {} (Discord), {} (Mic Mute)
 Auto-Detected Games: {}
========================================================================
"#,
        hw.logical_cores,
        hw.total_ram_mb,
        eco_label,
        config.width,
        config.height,
        config.fps.as_u32(),
        quality_name,
        audio_name,
        mic_label,
        mic_state_str,
        stem_mode_str,
        format_human_duration(config.buffer_duration_secs),
        config.pre_roll_secs,
        config.post_roll_secs,
        format_human_duration(config.direct_clip_duration_secs),
        active_storage.display(),
        config.keybinds.replay_clip.label,
        config.keybinds.direct_clip.label,
        config.keybinds.discord_clip.label,
        config.keybinds.mic_mute.label,
        games.join(", ")
    );
}

fn attach_parent_console_if_cli() {
    #[cfg(windows)]
    {
        if std::env::args().len() > 1 {
            use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
            unsafe {
                let _ = AttachConsole(ATTACH_PARENT_PROCESS);
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    attach_parent_console_if_cli();
    let worker_count = std::thread::available_parallelism()
        .map(|n| n.get().clamp(4, 8))
        .unwrap_or(4);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_count)
        .enable_all()
        .build()?;
    let _rt_guard = rt.enter();
    let args = match parse_cli_args() {
        Ok(Some(parsed)) => parsed,
        Ok(None) => return Ok(()),
        Err(err_msg) => {
            eprintln!("[CLI ERROR] {err_msg}");
            std::process::exit(1);
        }
    };

    if args.self_test {
        println!("========================================================================");
        println!(" Clotch Multi-Game Telemetry & Highlight Self-Test Suite (17 Games)");
        println!("========================================================================");
        match events::run_all_games_self_test() {
            Ok(results) => {
                for (idx, (game_name, kinds)) in results.iter().enumerate() {
                    println!(
                        " [{:02}/{}] {:<20} PASS ({} events: {})",
                        idx + 1,
                        results.len(),
                        game_name,
                        kinds.len(),
                        kinds.join(", ")
                    );
                }
                println!("------------------------------------------------------------------------");
                println!(
                    " [SELF-TEST SUCCESS] All {} game pipelines verified without hardware dependencies.",
                    results.len()
                );
                println!("========================================================================");
                return Ok(());
            }
            Err(err) => {
                eprintln!("[SELF-TEST FAILED] {err}");
                std::process::exit(1);
            }
        }
    }

    if args.list_mics {
        let mics = list_input_devices();
        println!("========================================================================");
        println!(" Connected Microphone / Audio Input Devices ({} found)", mics.len());
        println!("========================================================================");
        if mics.is_empty() {
            println!(" No active microphone input devices detected.");
        } else {
            for (idx, dev) in mics.iter().enumerate() {
                let tag = if dev.is_default { " [SYSTEM DEFAULT]" } else { "" };
                println!(" [{idx}] {}{tag}", dev.name);
                println!("     ID: {}", dev.id);
            }
        }
        println!("========================================================================");
        return Ok(());
    }

    if let Some(ref input_video) = args.discord_input {
        println!("========================================================================");
        println!(" Clotch — Make It For Discord (Fast Hardware-Accelerated Compressor)");
        println!("========================================================================");
        println!(" Input Video:  {}", input_video.display());
        println!(
            " Target Size:  {:.2} MB (Non-Nitro Safe)",
            args.discord_target_mb
        );
        println!(" Priority:     Below-Normal CPU / Hardware GPU Preferred");
        println!("------------------------------------------------------------------------");

        let opts = DiscordCompressOptions {
            target_mb: args.discord_target_mb,
            output_path: args.discord_output.clone(),
            max_fps: None,
            prefer_hardware: true,
        };

        let in_clone = input_video.clone();
        let res = rt.block_on(async move {
            tokio::task::spawn_blocking(move || make_it_for_discord(in_clone, opts)).await
        })?;
        match res {
            Ok(info) => {
                let in_mb = info.input_bytes as f64 / (1024.0 * 1024.0);
                let out_mb = info.output_bytes as f64 / (1024.0 * 1024.0);
                let saved_pct = if info.input_bytes > 0 && info.output_bytes <= info.input_bytes {
                    (1.0 - (info.output_bytes as f64 / info.input_bytes as f64)) * 100.0
                } else {
                    0.0
                };
                println!(
                    "[DISCORD READY] Output saved to: {}",
                    info.output_path.display()
                );
                println!(
                    " Size:         {:.2} MB -> {:.2} MB ({:.1}% reduction)",
                    in_mb, out_mb, saved_pct
                );
                println!(
                    " Stream:       {}x{} @ {} FPS (~{} kbps video)",
                    info.width, info.height, info.fps, info.video_bitrate_kbps
                );
                println!(
                    " Engine:       {} (completed in {:.2}s)",
                    info.backend,
                    info.elapsed_ms as f64 / 1000.0
                );
                println!("========================================================================");
                return Ok(());
            }
            Err(e) => {
                eprintln!("[DISCORD ERROR] Failed to compress video: {e}");
                std::process::exit(1);
            }
        }
    }

    let hw_profile = SystemHardwareProfile::detect();
    let effective_low_end = args
        .low_end_mode
        .or_else(load_config_low_end_override)
        .unwrap_or(hw_profile.is_low_end);

    let gsi_token = load_or_generate_gsi_token(args.regenerate_token);
    auto_install_game_integrations(&gsi_token);

    let mut reader = events::presets::all(&gsi_token)?;
    if effective_low_end {
        reader.scan_interval(std::time::Duration::from_secs(3));
    }

    let registered_game_names: Vec<String> = reader
        .registrations()
        .iter()
        .map(|r| r.game().to_string())
        .collect();
    let registered_game_refs: Vec<&str> =
        registered_game_names.iter().map(String::as_str).collect();

    let mut recorder_config = RecorderConfig::load();
    recorder_config.low_end_mode = effective_low_end;
    if effective_low_end {
        recorder_config.apply_low_end_optimizations();
    }
    recorder_config.load_overrides_from_config_files();

    if let Some(f) = args.fps {
        recorder_config.fps = f;
    }
    if let Some(q) = args.quality {
        recorder_config.quality = q;
    }
    if let Some((w, h)) = args.resolution {
        recorder_config.width = w;
        recorder_config.height = h;
    }
    if let Some(a) = args.audio {
        recorder_config.audio = a;
    }
    if let Some(kbps) = args.audio_bitrate_kbps {
        recorder_config.audio_bitrate_kbps = kbps;
    }
    if let Some(dev) = args.mic_device {
        let trimmed = dev.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("default")
            || trimmed.eq_ignore_ascii_case("auto")
            || trimmed.eq_ignore_ascii_case("system")
        {
            recorder_config.mic_device = None;
        } else {
            recorder_config.mic_device = Some(trimmed.to_string());
        }
    }
    if let Some(muted) = args.mic_muted {
        recorder_config.mic_muted = muted;
    }
    if let Some(sep) = args.separate_audio_tracks {
        recorder_config.separate_audio_tracks = sep;
    }
    if let Some(b_secs) = args.buffer_secs {
        recorder_config.buffer_duration_secs = b_secs;
    }
    if let Some(d_secs) = args.direct_clip_secs {
        recorder_config.direct_clip_duration_secs = d_secs;
    }
    if let Some(pre) = args.pre_roll_secs {
        recorder_config.pre_roll_secs = pre;
    }
    if let Some(post) = args.post_roll_secs {
        recorder_config.post_roll_secs = post;
    }
    if let Some(kb) = args.hotkey_replay {
        recorder_config.keybinds.replay_clip = kb;
    }
    if let Some(kb) = args.hotkey_direct {
        recorder_config.keybinds.direct_clip = kb;
    }
    if let Some(kb) = args.hotkey_discord {
        recorder_config.keybinds.discord_clip = kb;
    }
    if let Some(kb) = args.hotkey_mic_mute {
        recorder_config.keybinds.mic_mute = kb;
    }
    if let Some(path) = args.output_dir {
        recorder_config.storage = StorageConfig {
            mode: StorageMode::Custom(path),
            min_free_gb: 10,
        };
    }

    let active_storage_path = StorageManager::resolve_clip_dir(&recorder_config.storage, None)
        .unwrap_or_else(|_| PathBuf::from("ClotchClips"));

    print_banner(
        &recorder_config,
        &hw_profile,
        &active_storage_path,
        &registered_game_refs,
    );

    let active_keybinds = recorder_config.keybinds.clone();

    if let Some(en) = args.telemetry_enabled {
        recorder_config.share_anonymous_stats = en;
    }

    let analytics_rx = reader.subscribe();
    let analytics_handle = analytics::start_analytics_with_config(
        analytics_rx,
        &recorder_config,
        args.telemetry_enabled,
    );
    recorder_config.share_anonymous_stats = analytics_handle.share_anonymous_stats();

    let mut discord_cfg = discord::DiscordConfig::from_env_and_file();
    if let Some(ref custom_id) = args.discord_app_id {
        if !custom_id.trim().is_empty() {
            discord_cfg.app_id = custom_id.trim().to_string();
        }
    }
    let discord_rx = reader.subscribe();
    let discord_handle = discord::start_discord_presence(discord_rx, discord_cfg);

    let recorder_rx = reader.subscribe();
    let recorder_sink = reader.sink("system", "engine");
    let recorder = Recorder::with_sink(recorder_config, recorder_rx, recorder_sink);

    let mut console_rx = reader.subscribe();
    let console_listener = tokio::spawn(async move {
        while let Ok(event) = console_rx.recv().await {
            match event.kind.as_str() {
                "clip.saved" => {
                    let path = event
                        .data
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    println!("[SAVED] Clip written to: {path}");
                }
                "clip.direct_started" => {
                    let win = event
                        .data
                        .get("window_title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("App");
                    let dur = event
                        .data
                        .get("duration_secs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(15);
                    println!(
                        "[DIRECT CLIP START] Recording foreground window: {win} ({dur}s)..."
                    );
                }
                "manual.clip" => {
                    let hotkey = event
                        .data
                        .get("hotkey")
                        .and_then(|v| v.as_str())
                        .unwrap_or("F8");
                    println!("[REPLAY HOTKEY] {hotkey} pressed! Saving instant replay clip...");
                }
                "manual.direct_clip" => {
                    let hotkey = event
                        .data
                        .get("hotkey")
                        .and_then(|v| v.as_str())
                        .unwrap_or("F9");
                    println!(
                        "[DIRECT HOTKEY] {hotkey} pressed! Direct foreground capture triggered!"
                    );
                }
                "audio.mic_ready" => {
                    let dev_name = event
                        .data
                        .get("device_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("System Default Microphone");
                    let is_muted = event
                        .data
                        .get("muted")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let status = if is_muted { "MUTED" } else { "ACTIVE" };
                    println!("[MIC READY] Connected to microphone: {dev_name} [{status}]");
                }
                "audio.mic_mute_changed" => {
                    let is_muted = event
                        .data
                        .get("muted")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if is_muted {
                        println!("[MIC MUTE] Microphone is now MUTED.");
                    } else {
                        println!("[MIC UNMUTE] Microphone is now ACTIVE.");
                    }
                }
                "game.started" => {
                    println!(
                        "[GAME DETECTED] {} has launched. Replay buffer armed.",
                        event.game
                    );
                    let g_lower = event.game.to_lowercase();
                    if g_lower == "cs2" || g_lower.contains("counter-strike") {
                        println!("[CS2 NOTICE] If CS2 uses Fullscreen Exclusive or displays a black screen, add -allow_third_party_software to CS2 Steam launch options.");
                    }
                }
                "game.pid_updated" => {
                    let pid = event.pid.unwrap_or(0);
                    println!(
                        "[GAME WINDOW LOCKED] {} attached to active game window process (PID {pid}).",
                        event.game
                    );
                }
                "game.stopped" => {
                    println!(
                        "[GAME EXITED] {} closed. Post-game replay buffer flushed.",
                        event.game
                    );
                }
                "match.start" => {
                    println!("[MATCH START] {} match begun.", event.game);
                }
                "round.end" => {
                    let round = event
                        .data
                        .get("round")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let round_str = if round.is_empty() {
                        String::new()
                    } else {
                        format!(" #{round}")
                    };
                    println!(
                        "[ROUND END] {} round{round_str} ended. Saving round replay...",
                        event.game
                    );
                }
                "match.end" | "match.leave" => {
                    println!("[MATCH END] {} match ended.", event.game);
                }
                "player.kill"
                | "player.knockdown"
                | "player.headshot"
                | "player.ace"
                | "player.clutch"
                | "player.round_multikill_win"
                | "player.mvp"
                | "match.mvp"
                | "player.multikill"
                | "sport.goal"
                | "sport.hat_trick"
                | "sport.epic_save"
                | "bomb.planted"
                | "bomb.defused"
                | "objective.first_blood"
                | "objective.secured" => {
                    let weapon = event
                        .data
                        .get("weapon")
                        .or_else(|| event.data.get("weapon_0"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let weapon_label = if !weapon.is_empty() {
                        format!(" ({weapon})")
                    } else {
                        String::new()
                    };
                    println!(
                        "[CLUTCH EVENT] {}: {}{}",
                        event.game, event.kind, weapon_label
                    );
                }
                "discord.compress_started" => {
                    let input_p = event
                        .data
                        .get("input_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("clip");
                    let t_mb = event
                        .data
                        .get("target_mb")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(DEFAULT_DISCORD_TARGET_MB);
                    println!(
                        "[DISCORD START] Compressing '{}' to <= {:.1} MB for Discord...",
                        input_p, t_mb
                    );
                }
                "discord.compress_done" => {
                    let out_p = event
                        .data
                        .get("output_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let out_b = event
                        .data
                        .get("output_bytes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let backend = event
                        .data
                        .get("backend")
                        .and_then(|v| v.as_str())
                        .unwrap_or("GPU");
                    let elapsed = event
                        .data
                        .get("elapsed_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    println!(
                        "[DISCORD READY] Saved {} ({:.2} MB via {} in {:.2}s)",
                        out_p,
                        out_b as f64 / (1024.0 * 1024.0),
                        backend,
                        elapsed as f64 / 1000.0
                    );
                }
                "discord.compress_error" => {
                    let msg = event
                        .data
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    eprintln!("[DISCORD ERROR] {msg}");
                }
                "recorder.encoder_fallback" => {
                    let kind = event
                        .data
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("fallback");
                    let reason = event
                        .data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    eprintln!("[ENCODER FALLBACK] {kind}: {reason}");
                }
                "settings.privacy.updated" | "settings.privacy.share_anonymous_stats" => {
                    let status = event
                        .data
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Updated");
                    println!(
                        "[PRIVACY] Settings -> Privacy -> Share anonymous stats -> {status}"
                    );
                }
                "stats.match_recorded" => {
                    if let Some(m) = event.data.get("match") {
                        let g = m.get("game").and_then(|v| v.as_str()).unwrap_or("game");
                        let mp = m.get("map").and_then(|v| v.as_str()).unwrap_or("Unknown");
                        let k = m.get("kills").and_then(|v| v.as_u64()).unwrap_or(0);
                        let d = m.get("deaths").and_then(|v| v.as_u64()).unwrap_or(0);
                        let cl = m.get("clutches").and_then(|v| v.as_u64()).unwrap_or(0);
                        println!(
                            "[LOCAL STATS] Match saved for {g} on {mp} (K: {k} | D: {d} | Clutches: {cl})"
                        );
                    }
                }
                "recorder.error" | "reader.error" => {
                    let msg = event
                        .data
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    eprintln!("[ERROR] {}: {} {msg}", event.game, event.kind);
                }
                _ => {}
            }
        }
    });

    let _ = active_keybinds;
    let shared_cfg = recorder.shared_config();
    let ui_sink = reader.sink("system", "gui");
    let ui_bridge = bridge::UiBridge::from_shared(shared_cfg.clone(), Some(ui_sink));
    let gui_event_rx = reader.subscribe();
    let hotkey_event_rx = reader.subscribe();

    let reader_cancel = CancellationToken::new();
    let hotkey_sink = reader.sink("system", "hotkey");
    let _hotkey_handle = spawn_hotkey_listener(
        hotkey_sink,
        shared_cfg,
        hotkey_event_rx,
        reader_cancel.clone(),
    );
    let reader_handle = reader.spawn(reader_cancel.clone());

    if args.headless {
        rt.block_on(async {
            if let Err(e) = tokio::signal::ctrl_c().await {
                eprintln!("[SHUTDOWN] Interrupt signal listener encountered error: {e}");
            } else {
                println!("\n[SHUTDOWN] Interrupt signal received. Stopping Clotch engine safely...");
            }
        });
    } else if let Err(e) = gui::run_gui(ui_bridge, gui_event_rx) {
        eprintln!("[GUI NOTICE] GUI exited ({e}). Falling back to console wait...");
        let _ = rt.block_on(tokio::signal::ctrl_c());
    }

    rt.block_on(async {
        if let Err(e) = analytics_handle.shutdown().await {
            eprintln!("[ANALYTICS] Telemetry flush encountered notice: {e}");
        }

        reader_cancel.cancel();
        tokio::task::yield_now().await;
        let _ = reader_handle.await;
        discord_handle.shutdown().await;

        recorder.shutdown().await;
        console_listener.abort();
    });
    recorder::shutdown_media_foundation();

    println!("[SHUTDOWN] Engine successfully stopped. Goodbye!");
    Ok(())
}