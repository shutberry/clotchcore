#![allow(dead_code, unused_imports)]
use std::io;
use std::path::{Path, PathBuf};
use regex::Regex;
use serde_json::json;
use crate::games::definitions::ROCKET_LEAGUE_WEB_PORT;
use crate::games::event_mapping::upsert_ini;

fn compile_re(pat: &str) -> Regex {
    Regex::new(pat).unwrap_or_else(|e| panic!("invalid internal regex '{pat}': {e}"))
}

pub fn newest_file_in_dir(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
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

pub fn steam_libraries() -> Vec<PathBuf> {
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

pub fn format_valorant_shipping_version(raw: &str) -> String {
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

pub fn extract_valorant_log_metadata() -> (String, String, Option<String>, Vec<String>) {
    let mut shard = String::new();
    let mut ver = String::new();
    let mut puuid: Option<String> = None;
    let mut match_ids: Vec<String> = Vec::new();

    let Some(primary_log) = locate_valorant_log_path() else {
        return (shard, ver, puuid, match_ids);
    };
    let mut log_files = vec![primary_log.clone()];
    if let Some(parent) = primary_log.parent() {
        if let Ok(rd) = std::fs::read_dir(parent) {
            let mut backups: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p != &primary_log
                        && p.file_name()
                            .and_then(|s| s.to_str())
                            .map_or(false, |n| n.starts_with("ShooterGame") && n.ends_with(".log"))
                })
                .collect();
            backups.sort();
            backups.reverse();
            log_files.extend(backups.into_iter().take(4));
        }
    }

    let shard_re = compile_re(r"https://(?:pd|glz-[a-z0-9-]+-1)\.([a-z0-9]+)\.a\.pvp\.net");
    let ver_re = compile_re(r"CI server version:\s*([^\r\n]+)");
    let puuid_url_re = compile_re(
        r"a\.pvp\.net/(?:match-history/v1/history|mmr/v1/players|core-game/v1/players|pregame/v1/players|session/v1/sessions|contracts/v1/contracts|store/v[0-9]+/storefront)/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})",
    );
    let match_url_re = compile_re(
        r"a\.pvp\.net/(?:core-game/v1/matches|match-details/v1/matches)/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})",
    );

    for lf in log_files {
        let Ok(content) = std::fs::read_to_string(&lf) else {
            continue;
        };
        if shard.is_empty() {
            if let Some(s) = shard_re.captures(&content).and_then(|c| c.get(1)) {
                shard = s.as_str().to_string();
            }
        }
        if ver.is_empty() {
            if let Some(v) = ver_re.captures(&content).and_then(|c| c.get(1)) {
                ver = format_valorant_shipping_version(v.as_str());
            }
        }
        if puuid.is_none() {
            if let Some(p) = puuid_url_re.captures(&content).and_then(|c| c.get(1)) {
                puuid = Some(p.as_str().to_string());
            }
        }
        for caps in match_url_re.captures_iter(&content) {
            if let Some(m) = caps.get(1) {
                let mid = m.as_str().to_string();
                if !match_ids.contains(&mid) {
                    match_ids.push(mid);
                }
            }
        }
    }

    (shard, ver, puuid, match_ids)
}

pub fn extract_valorant_shard_and_version_from_log() -> (String, String) {
    let (s, v, _, _) = extract_valorant_log_metadata();
    (s, v)
}

pub fn read_riot_lockfile_credentials() -> Option<(u16, String)> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    let lockfile_path = PathBuf::from(local_app_data).join("Riot Games/Riot Client/Config/lockfile");
    let content = std::fs::read_to_string(lockfile_path).ok()?;
    let parts: Vec<&str> = content.trim().split(':').collect();
    if parts.len() < 4 {
        return None;
    }
    let port = parts[2].parse::<u16>().ok()?;
    let password = parts[3].to_string();
    if port == 0 || password.is_empty() {
        return None;
    }
    Some((port, password))
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
        return Some(
            PathBuf::from(user_profile).join("AppData/Local/Marvel/Saved/Logs/Marvel.log"),
        );
    }
    None
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
