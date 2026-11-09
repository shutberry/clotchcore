#![allow(dead_code, unused_imports)]
use crate::tracker::event_bus::SourceError;

pub const CS2_GSI_PORT: u16 = 3000;
pub const DOTA2_GSI_PORT: u16 = 3001;
pub const APEX_LIVEAPI_PORT: u16 = 3002;
pub const ROCKET_LEAGUE_PORT: u16 = 49123;
pub const ROCKET_LEAGUE_WEB_PORT: u16 = 49124;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

pub const GSI_TEMPLATE: &str = r#""Skirrmish"
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

pub fn validate_token(token: &str) -> Result<String, SourceError> {
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
