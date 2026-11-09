#![allow(dead_code, unused_imports)]
use std::path::PathBuf;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalLauncherIdentity {
    pub launcher: String,
    pub display_id: String,
    pub account_id: String,
    pub region_or_tag: String,
    pub online_status: String,
}

pub fn resolve_local_identities_path() -> PathBuf {
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let dir = PathBuf::from(local_appdata).join("Clotch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("local_identities.json");
    }
    PathBuf::from("local_identities.json")
}

pub fn persist_cached_launcher_identity(identity: LocalLauncherIdentity) {
    if identity.display_id.trim().is_empty() {
        return;
    }
    let path = resolve_local_identities_path();
    let mut list: Vec<LocalLauncherIdentity> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if let Some(existing) = list.iter_mut().find(|x| x.launcher.eq_ignore_ascii_case(&identity.launcher)) {
        *existing = identity;
    } else {
        list.push(identity);
    }
    if let Ok(body) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(path, body);
    }
}

pub fn discover_steam_local_identity_sync() -> Option<LocalLauncherIdentity> {
    let mut candidates = Vec::new();
    for key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(p) = std::env::var_os(key) {
            candidates.push(PathBuf::from(p).join("Steam/config/loginusers.vdf"));
        }
    }
    for root in crate::games::detection::steam_libraries() {
        candidates.push(root.join("config/loginusers.vdf"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let h = PathBuf::from(home);
        candidates.push(h.join(".local/share/Steam/config/loginusers.vdf"));
        candidates.push(h.join(".steam/steam/config/loginusers.vdf"));
    }
    candidates.push(PathBuf::from(r"C:\Program Files (x86)\Steam\config\loginusers.vdf"));
    candidates.push(PathBuf::from(r"D:\Steam\config\loginusers.vdf"));

    let block_re = Regex::new(r#""(7656[0-9]{13})"\s*\{([^}]+)\}"#).ok()?;
    let kv_re = Regex::new(r#""([A-Za-z0-9_]+)"\s+"([^"]*)""#).ok()?;

    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut best_persona = String::new();
        let mut best_steamid = String::new();
        let mut best_account = String::new();
        let mut best_score = -1i64;

        for caps in block_re.captures_iter(&content) {
            let sid = caps.get(1).map_or("", |m| m.as_str());
            let body = caps.get(2).map_or("", |m| m.as_str());
            let mut persona = String::new();
            let mut account = String::new();
            let mut most_recent = false;
            let mut auto_login = false;
            let mut ts = 0i64;
            for kv in kv_re.captures_iter(body) {
                let k = kv.get(1).map_or("", |m| m.as_str());
                let v = kv.get(2).map_or("", |m| m.as_str());
                if k.eq_ignore_ascii_case("PersonaName") {
                    persona = v.to_string();
                } else if k.eq_ignore_ascii_case("AccountName") {
                    account = v.to_string();
                } else if k.eq_ignore_ascii_case("MostRecent") && v == "1" {
                    most_recent = true;
                } else if k.eq_ignore_ascii_case("AllowAutoLogin") && v == "1" {
                    auto_login = true;
                } else if k.eq_ignore_ascii_case("Timestamp") {
                    ts = v.parse::<i64>().unwrap_or(0);
                }
            }
            let score = (if most_recent { 1_000_000_000_000 } else { 0 })
                + (if auto_login { 500_000_000_000 } else { 0 })
                + ts;
            if !persona.is_empty() && score > best_score {
                best_score = score;
                best_persona = persona;
                best_steamid = sid.to_string();
                best_account = account;
            }
        }
        if !best_persona.is_empty() {
            return Some(LocalLauncherIdentity {
                launcher: "Steam".to_string(),
                display_id: best_persona,
                account_id: best_steamid,
                region_or_tag: if best_account.is_empty() {
                    "Local VDF".to_string()
                } else {
                    best_account
                },
                online_status: "Verified Local".to_string(),
            });
        }
    }
    None
}

pub fn discover_riot_local_identity_sync() -> Option<LocalLauncherIdentity> {
    let cached_path = resolve_local_identities_path();
    if let Ok(raw) = std::fs::read_to_string(&cached_path) {
        if let Ok(list) = serde_json::from_str::<Vec<LocalLauncherIdentity>>(&raw) {
            if let Some(found) = list.into_iter().find(|x| {
                x.launcher.eq_ignore_ascii_case("Riot Games")
                    && !x.display_id.is_empty()
                    && !x.display_id.starts_with("Valorant#")
            }) {
                return Some(found);
            }
        }
    }
    None
}

pub fn discover_epic_local_identity_sync() -> Option<LocalLauncherIdentity> {
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let rl_log = PathBuf::from(&user_profile)
            .join("Documents/My Games/Rocket League/TAGame/Logs/Launch.log");
        if let Ok(content) = std::fs::read_to_string(&rl_log) {
            let name_re = Regex::new(r#"PlayerName="([^"]+)""#).ok();
            if let Some(name) = name_re
                .as_ref()
                .and_then(|re| re.captures(&content))
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty())
            {
                return Some(LocalLauncherIdentity {
                    launcher: "Epic / RL".to_string(),
                    display_id: name,
                    account_id: String::new(),
                    region_or_tag: "Launch.log".to_string(),
                    online_status: "Verified Local".to_string(),
                });
            }
        }
    }
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let fn_log = PathBuf::from(&local_appdata).join("FortniteGame/Saved/Logs/FortniteGame.log");
        if let Ok(content) = std::fs::read_to_string(&fn_log) {
            let name_re = Regex::new(r"DisplayName=\[?([A-Za-z0-9_.-]{3,24})\]?").ok();
            if let Some(name) = name_re
                .as_ref()
                .and_then(|re| re.captures(&content))
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty())
            {
                return Some(LocalLauncherIdentity {
                    launcher: "Epic Games".to_string(),
                    display_id: name,
                    account_id: String::new(),
                    region_or_tag: "FortniteGame.log".to_string(),
                    online_status: "Verified Local".to_string(),
                });
            }
        }
    }
    None
}

pub fn discover_battlenet_local_identity_sync() -> Option<LocalLauncherIdentity> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let bnet_cfg = PathBuf::from(appdata).join("Battle.net/Battle.net.config");
        if let Ok(content) = std::fs::read_to_string(&bnet_cfg) {
            if let Ok(val) = serde_json::from_str::<Value>(&content) {
                if let Some(saved) = val
                    .pointer("/Client/SavedAccountNames")
                    .and_then(Value::as_str)
                    .and_then(|s| s.split(',').next())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let display = saved.split('@').next().unwrap_or(saved).to_string();
                    return Some(LocalLauncherIdentity {
                        launcher: "Battle.net".to_string(),
                        display_id: display,
                        account_id: String::new(),
                        region_or_tag: "Local Config".to_string(),
                        online_status: "Verified Local".to_string(),
                    });
                }
            }
        }
    }
    None
}

pub fn discover_all_local_identities() -> Vec<LocalLauncherIdentity> {
    let mut out = Vec::new();
    if let Some(riot) = discover_riot_local_identity_sync() {
        out.push(riot);
    }
    if let Some(steam) = discover_steam_local_identity_sync() {
        out.push(steam);
    }
    if let Some(epic) = discover_epic_local_identity_sync() {
        out.push(epic);
    }
    if let Some(bnet) = discover_battlenet_local_identity_sync() {
        out.push(bnet);
    }
    out
}
