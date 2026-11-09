use std::path::PathBuf;
use super::*;

pub const CS2_GSI_PORT: u16 = 3000;
pub const DOTA2_GSI_PORT: u16 = 3001;
pub const APEX_LIVEAPI_PORT: u16 = 3002;
pub const ROCKET_LEAGUE_PORT: u16 = 49123;
pub const ROCKET_LEAGUE_WEB_PORT: u16 = 49124;

pub fn cs2(token: impl Into<String>) -> Registration {
    Registration::new("cs2", GsiSource::new(CS2_GSI_PORT, token)).when_running(["cs2.exe"])
}

pub fn try_cs2(token: impl Into<String>) -> Result<Registration, SourceError> {
    Ok(
        Registration::new("cs2", GsiSource::try_new(CS2_GSI_PORT, token)?)
            .when_running(["cs2.exe"]),
    )
}

pub fn dota2(token: impl Into<String>) -> Registration {
    Registration::new("dota2", Dota2GsiSource::new(DOTA2_GSI_PORT, token))
        .when_running(["dota2.exe"])
}

pub fn try_dota2(token: impl Into<String>) -> Result<Registration, SourceError> {
    Ok(
        Registration::new("dota2", Dota2GsiSource::try_new(DOTA2_GSI_PORT, token)?)
            .when_running(["dota2.exe"]),
    )
}

pub fn league_of_legends() -> Result<Registration, SourceError> {
    Ok(Registration::new("lol", LeagueLiveClient::new()?)
        .when_running(["league of legends.exe"]))
}

pub fn rocket_league() -> Registration {
    Registration::new("rocket_league", RocketLeagueStats::new(ROCKET_LEAGUE_PORT))
        .when_running(["rocketleague.exe"])
}

pub fn valorant() -> Result<Registration, SourceError> {
    let path = locate_valorant_log_path()
        .unwrap_or_else(|| PathBuf::from("VALORANT/Saved/Logs/ShooterGame.log"));
    valorant_with_log(path)
}

pub fn valorant_with_log(log_path: impl Into<PathBuf>) -> Result<Registration, SourceError> {
    let src = ValorantSource::new(log_path);
    Ok(Registration::new("valorant", src)
        .when_running(["valorant-win64-shipping.exe", "valorant.exe"]))
}

pub fn overwatch2_with_workshop_log(
    log_path: impl Into<PathBuf>,
) -> Result<Registration, SourceError> {
    let tail = TailSource::new("overwatch_workshop_log", log_path)
        .rule(
            "workshop.match_start",
            r"\[(?P<time>[0-9:.]+)\]\s+MATCH_START\s+map=(?P<map>.+)",
        )?
        .rule(
            "match.start",
            r"\[(?P<time>[0-9:.]+)\]\s+MATCH_START\s+map=(?P<map>.+)",
        )?
        .rule(
            "workshop.kill",
            r"\[(?P<time>[0-9:.]+)\]\s+KILL\s+attacker=(?P<attacker>[^;]+);victim=(?P<victim>[^;]+);ability=(?P<ability>.+)",
        )?
        .rule(
            "player.kill",
            r"\[(?P<time>[0-9:.]+)\]\s+KILL\s+attacker=(?P<attacker>[^;]+);victim=(?P<victim>[^;]+);ability=(?P<ability>.+)",
        )?
        .rule(
            "workshop.match_end",
            r"\[(?P<time>[0-9:.]+)\]\s+MATCH_END\s+winner=(?P<winner>.+)",
        )?
        .rule(
            "match.end",
            r"\[(?P<time>[0-9:.]+)\]\s+MATCH_END\s+winner=(?P<winner>.+)",
        )?;
    Ok(Registration::new("overwatch2", tail).when_running(["overwatch.exe"]))
}

pub fn overwatch2() -> Registration {
    Registration::new("overwatch2", Overwatch2Source::new()).when_running(["overwatch.exe"])
}

pub fn warzone() -> Registration {
    Registration::new("warzone", CallOfDutySource::new()).when_running([
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
    ])
}

pub fn call_of_duty() -> Registration {
    warzone()
}

pub fn battlefield() -> Registration {
    Registration::new("battlefield", BattlefieldSource::new()).when_running([
        "bf2042.exe",
        "bf6.exe",
        "bfv.exe",
        "bf1.exe",
        "bf4.exe",
        "battlefield2042.exe",
        "battlefieldv.exe",
        "battlefield1.exe",
    ])
}

pub fn apex_legends() -> Registration {
    Registration::new("apex_legends", ApexLegendsSource::new(APEX_LIVEAPI_PORT))
        .when_running(["r5apex.exe", "r5apex_dx12.exe"])
}

pub fn fortnite() -> Registration {
    Registration::new("fortnite", FortniteSource::default()).when_running([
        "fortniteclient-win64-shipping.exe",
        "fortniteclient-win64-shipping_eac.exe",
        "fortniteclient-win64-shipping_be.exe",
    ])
}

pub fn fortnite_with_log(log_path: impl Into<PathBuf>) -> Registration {
    Registration::new("fortnite", FortniteSource::new(log_path)).when_running([
        "fortniteclient-win64-shipping.exe",
        "fortniteclient-win64-shipping_eac.exe",
        "fortniteclient-win64-shipping_be.exe",
    ])
}

pub fn rainbow_six_siege() -> Registration {
    Registration::new("rainbow_six_siege", RainbowSixSiegeSource::new()).when_running([
        "rainbowsix.exe",
        "rainbowsix_vulkan.exe",
        "rainbowsix_dx11.exe",
    ])
}

pub fn the_finals() -> Registration {
    Registration::new("the_finals", TheFinalsSource::default()).when_running([
        "discovery.exe",
        "discovery-d.exe",
        "discovery-win64-shipping.exe",
        "thefinals.exe",
    ])
}

pub fn arc_raiders() -> Registration {
    Registration::new("arc_raiders", ArcRaidersSource::default()).when_running([
        "pioneergame.exe",
        "pioneergame-win64-shipping.exe",
        "arcraiders.exe",
        "arcraiders-win64-shipping.exe",
    ])
}

pub fn marvel_rivals() -> Registration {
    Registration::new("marvel_rivals", MarvelRivalsSource::default()).when_running([
        "marvel-win64-shipping.exe",
        "marvel.exe",
        "marvelrivals.exe",
    ])
}

pub fn fc26() -> Registration {
    Registration::new("fc26", EaSportsFc26Source::default()).when_running(FC26_EXECUTABLES.iter().copied())
}

pub fn fc27() -> Registration {
    Registration::new("fc27", EaSportsFc27Source::default()).when_running(FC27_EXECUTABLES.iter().copied())
}

pub fn efootball() -> Registration {
    Registration::new("efootball", EFootballSource::default())
        .when_running(EFOOTBALL_EXECUTABLES.iter().copied())
}

pub fn all(cs2_token: impl Into<String>) -> Result<EventReader, SourceError> {
    let token_str = validate_token(&cs2_token.into())?;
    let mut reader = EventReader::new(4096);
    reader
        .register(try_cs2(token_str.clone())?)
        .register(try_dota2(token_str)?)
        .register(league_of_legends()?)
        .register(rocket_league())
        .register(valorant()?)
        .register(overwatch2())
        .register(warzone())
        .register(battlefield())
        .register(apex_legends())
        .register(fortnite())
        .register(rainbow_six_siege())
        .register(the_finals())
        .register(arc_raiders())
        .register(marvel_rivals())
        .register(fc26())
        .register(fc27())
        .register(efootball());
    Ok(reader)
}