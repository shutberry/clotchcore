#![allow(dead_code, unused_imports)]
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::metadata::{
    compute_stable_clip_id, persist_clip_sidecar, resolve_clips_index_path,
    resolve_thumbs_cache_dir, CachedClipIndexEntry, ClipItem, ClipSidecarMeta,
};

pub fn scan_clips_directory_worker(clips_dir: &Path) -> Vec<ClipItem> {
    let index_path = resolve_clips_index_path();
    let mut cached_index: HashMap<String, CachedClipIndexEntry> =
        std::fs::read_to_string(&index_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<CachedClipIndexEntry>>(&s).ok())
            .map(|list| list.into_iter().map(|e| (e.path_str.clone(), e)).collect())
            .unwrap_or_default();

    let mut search_roots: Vec<PathBuf> = Vec::new();
    search_roots.push(clips_dir.to_path_buf());
    if let Some(parent) = clips_dir.parent() {
        if parent
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("ClotchClips"))
            .unwrap_or(false)
            && !search_roots.iter().any(|r| r == parent)
        {
            search_roots.push(parent.to_path_buf());
        }
    }

    let mut raw_paths: Vec<PathBuf> = Vec::new();
    for root in &search_roots {
        collect_video_files_recursive(root, 0, &mut raw_paths);
    }
    for key in cached_index.keys() {
        let p = PathBuf::from(key);
        if p.exists() {
            raw_paths.push(p);
        }
    }

    let thumbs_dir = resolve_thumbs_cache_dir();
    let mut discovered: Vec<ClipItem> = Vec::with_capacity(raw_paths.len());
    let mut updated_index: HashMap<String, CachedClipIndexEntry> =
        HashMap::with_capacity(raw_paths.len());
    let mut seen_ids: HashSet<u64> = HashSet::with_capacity(raw_paths.len());

    for path in raw_paths {
        let cid = compute_stable_clip_id(&path);
        if !seen_ids.insert(cid) {
            continue;
        }
        if let Some(item) =
            build_clip_item_from_path(&path, &thumbs_dir, &mut cached_index)
        {
            let key = path.to_string_lossy().to_string();
            if let Some(entry) = cached_index.get(&key) {
                updated_index.insert(key, entry.clone());
            }
            discovered.push(item);
        }
    }

    let entries_vec: Vec<CachedClipIndexEntry> = updated_index.into_values().collect();
    if let Ok(json_str) = serde_json::to_string(&entries_vec) {
        let _ = std::fs::write(&index_path, json_str);
    }

    discovered.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    discovered
}

pub fn build_clip_item_from_path(
    path: &Path,
    thumbs_dir: &Path,
    index_map: &mut HashMap<String, CachedClipIndexEntry>,
) -> Option<ClipItem> {
    let meta = std::fs::metadata(path).ok()?;
    let file_size_bytes = meta.len();
    if file_size_bytes == 0 {
        return None;
    }
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let clip_id = compute_stable_clip_id(path);
    let thumb_path = thumbs_dir.join(format!("{clip_id:016x}.rgba"));
    let sidecar_path = path.with_extension("json");
    let sidecar_opt: Option<ClipSidecarMeta> = std::fs::read_to_string(&sidecar_path)
        .ok()
        .and_then(|s| serde_json::from_str::<ClipSidecarMeta>(&s).ok());

    let path_key = path.to_string_lossy().to_string();
    let cached_hit = index_map.get(&path_key).filter(|e| {
        e.file_size_bytes == file_size_bytes && e.mtime_secs == mtime_secs && e.duration_secs > 0.1
    });

    let (dur_secs, has_stems) = if let Some(hit) = cached_hit {
        (hit.duration_secs, hit.has_separate_stems)
    } else if let Some(ref sc) = sidecar_opt {
        if let (Some(d), Some(st)) = (sc.duration_secs, sc.has_separate_stems) {
            (d.max(1.0), st)
        } else {
            probe_duration_and_stems(path)
        }
    } else {
        probe_duration_and_stems(path)
    };

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Clip");
    let parent_folder = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let (parsed_game, parsed_tag) = parse_legacy_filename_tokens(stem, parent_folder);
    let date_label = format_relative_mtime(mtime_secs);
    let clock_label = format_local_date_time(mtime_secs);

    let (display_title, game, tag, map, mode, weapon, kills, favorite) =
        if let Some(sc) = sidecar_opt {
            let g = if sc.game.trim().is_empty() {
                parsed_game
            } else {
                sc.game
            };
            let t = if sc.tag.trim().is_empty() {
                parsed_tag
            } else {
                sc.tag
            };
            let m = if sc.map.trim().is_empty() {
                detect_known_map_in_text(&sc.display_title)
                    .or_else(|| detect_known_map_in_text(stem))
                    .unwrap_or("")
                    .to_string()
            } else {
                sc.map
            };
            let title = if sc.display_title.trim().is_empty() {
                if m.is_empty() {
                    format!("{g} {t}, {clock_label}")
                } else {
                    format!("{g} {t} on {m}, {clock_label}")
                }
            } else {
                sc.display_title
            };
            (
                title,
                g,
                t,
                m,
                sc.mode,
                sc.weapon,
                sc.kills,
                sc.favorite,
            )
        } else {
            let m = detect_known_map_in_text(stem).unwrap_or("").to_string();
            let default_title = if m.is_empty() {
                format!("{parsed_game} {parsed_tag}, {clock_label}")
            } else {
                format!("{parsed_game} {parsed_tag} on {m}, {clock_label}")
            };
            (
                default_title,
                parsed_game,
                parsed_tag,
                m,
                String::new(),
                String::new(),
                None,
                false,
            )
        };

    index_map.insert(
        path_key.clone(),
        CachedClipIndexEntry {
            path_str: path_key,
            file_size_bytes,
            mtime_secs,
            duration_secs: dur_secs,
            has_separate_stems: has_stems,
            display_title: display_title.clone(),
            game: game.clone(),
            tag: tag.clone(),
            map: map.clone(),
            mode: mode.clone(),
            weapon: weapon.clone(),
            kills,
            favorite,
        },
    );

    let item = ClipItem {
        id: clip_id,
        display_title,
        game,
        tag,
        map,
        mode,
        weapon,
        kills,
        date_label,
        duration_secs: dur_secs,
        duration_label: format_mm_ss(dur_secs),
        favorite,
        has_separate_stems: has_stems,
        file_path: path.to_path_buf(),
        thumb_path,
        mtime_secs,
    };

    if !sidecar_path.exists() {
        persist_clip_sidecar(&item);
    }

    Some(item)
}

pub fn probe_duration_and_stems(path: &Path) -> (f64, bool) {
    match crate::editor::probe_clip_header_fast(path) {
        Ok((dur_secs, has_stems)) => (dur_secs.max(1.0), has_stems),
        Err(_) => (15.0, false),
    }
}

pub fn collect_video_files_recursive(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 2 || !dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_video_files_recursive(&p, depth + 1, out);
        } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            let fname = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !fname.ends_with(".splicing_tmp.mp4")
                && (ext.eq_ignore_ascii_case("mp4") || ext.eq_ignore_ascii_case("mkv"))
            {
                out.push(p);
            }
        }
    }
}

pub fn detect_known_map_in_text(text: &str) -> Option<&'static str> {
    const KNOWN_MAPS: &[(&str, &str)] = &[
        ("ascent", "Ascent"),
        ("bind", "Bind"),
        ("haven", "Haven"),
        ("split", "Split"),
        ("lotus", "Lotus"),
        ("sunset", "Sunset"),
        ("icebox", "Icebox"),
        ("breeze", "Breeze"),
        ("fracture", "Fracture"),
        ("pearl", "Pearl"),
        ("abyss", "Abyss"),
        ("corrode", "Corrode"),
        ("mirage", "Mirage"),
        ("dust2", "Dust2"),
        ("de_dust2", "Dust2"),
        ("inferno", "Inferno"),
        ("nuke", "Nuke"),
        ("ancient", "Ancient"),
        ("anubis", "Anubis"),
        ("vertigo", "Vertigo"),
        ("overpass", "Overpass"),
        ("train", "Train"),
        ("cache", "Cache"),
        ("kingsrow", "Kings Row"),
        ("ilios", "Ilios"),
        ("lijang", "Lijiang Tower"),
        ("dorado", "Dorado"),
        ("eichenwalde", "Eichenwalde"),
        ("worldsedge", "Worlds Edge"),
        ("olympus", "Olympus"),
        ("kings_canyon", "Kings Canyon"),
        ("erangel", "Erangel"),
        ("miramar", "Miramar"),
        ("sanhok", "Sanhok"),
        ("clubhouse", "Clubhouse"),
        ("oregon", "Oregon"),
        ("chalet", "Chalet"),
        ("kafe", "Kafe"),
        ("border", "Border"),
    ];
    for tok in text.split(|c: char| !c.is_alphanumeric()) {
        if tok.is_empty() {
            continue;
        }
        let lower = tok.to_lowercase();
        for &(needle, canonical) in KNOWN_MAPS {
            if lower == needle {
                return Some(canonical);
            }
        }
    }
    None
}

pub fn compute_clip_search_score(clip: &ClipItem, raw_query: &str) -> Option<u32> {
    let tokens: Vec<String> = raw_query
        .split_whitespace()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if tokens.is_empty() {
        return Some(1);
    }

    let title_l = clip.display_title.to_lowercase();
    let game_l = clip.game.to_lowercase();
    let tag_l = clip.tag.to_lowercase();
    let map_l = clip.map.to_lowercase();
    let mode_l = clip.mode.to_lowercase();
    let weapon_l = clip.weapon.to_lowercase();
    let date_l = clip.date_label.to_lowercase();
    let stem_l = clip
        .file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    let mut total_score = 0u32;

    for tok in &tokens {
        let mut tok_score = 0u32;

        if tag_l == *tok || map_l == *tok || game_l == *tok {
            tok_score = tok_score.max(100);
        } else if tag_l.contains(tok) || (!map_l.is_empty() && map_l.contains(tok)) {
            tok_score = tok_score.max(85);
        }

        if title_l.split(|c: char| !c.is_alphanumeric()).any(|w| w == tok) {
            tok_score = tok_score.max(90);
        } else if title_l.contains(tok) {
            tok_score = tok_score.max(60);
        }

        if game_l.contains(tok)
            || (!mode_l.is_empty() && mode_l.contains(tok))
            || (!weapon_l.is_empty() && weapon_l.contains(tok))
            || date_l.contains(tok)
            || stem_l.contains(tok)
        {
            tok_score = tok_score.max(55);
        }

        if let Some(canon_game) = canonical_game_from_token(tok) {
            if clip.game.eq_ignore_ascii_case(canon_game) {
                tok_score = tok_score.max(90);
            }
        }

        if let Some(canon_tag) = canonical_tag_from_token(tok) {
            if clip.tag.eq_ignore_ascii_case(canon_tag) {
                tok_score = tok_score.max(92);
            }
        }

        if let Some(canon_map) = detect_known_map_in_text(tok) {
            if clip.map.eq_ignore_ascii_case(canon_map)
                || title_l.contains(&canon_map.to_lowercase())
                || stem_l.contains(&canon_map.to_lowercase())
            {
                tok_score = tok_score.max(95);
            }
        }

        match tok.as_str() {
            "ace" | "5k" | "penta" | "pentakill" | "teamwipe" => {
                if clip.tag.eq_ignore_ascii_case("Ace")
                    || clip.kills.unwrap_or(0) >= 5
                    || title_l.contains("ace")
                    || title_l.contains("5k")
                    || title_l.contains("penta")
                {
                    tok_score = tok_score.max(98);
                }
            }
            "4k" | "quad" | "quadra" | "quadrakill" => {
                if clip.kills == Some(4)
                    || title_l.contains("4k")
                    || title_l.contains("quad")
                    || (clip.tag.eq_ignore_ascii_case("Multikill") && clip.kills.is_none())
                {
                    tok_score = tok_score.max(90);
                }
            }
            "3k" | "triple" | "triplekill" => {
                if clip.kills == Some(3)
                    || title_l.contains("3k")
                    || title_l.contains("triple")
                    || (clip.tag.eq_ignore_ascii_case("Multikill") && clip.kills.is_none())
                {
                    tok_score = tok_score.max(88);
                }
            }
            "2k" | "double" | "doublekill" => {
                if clip.kills == Some(2)
                    || title_l.contains("2k")
                    || title_l.contains("double")
                {
                    tok_score = tok_score.max(85);
                }
            }
            "multikill" | "multi" => {
                if clip.tag.eq_ignore_ascii_case("Multikill")
                    || clip.tag.eq_ignore_ascii_case("Ace")
                    || clip.kills.unwrap_or(0) >= 2
                {
                    tok_score = tok_score.max(90);
                }
            }
            "clutch" | "1v1" | "1v2" | "1v3" | "1v4" | "1v5" => {
                if clip.tag.eq_ignore_ascii_case("Clutch")
                    || title_l.contains("clutch")
                    || title_l.contains("1v")
                {
                    tok_score = tok_score.max(94);
                }
            }
            "hs" | "headshot" | "onetap" | "1tap" => {
                if clip.tag.eq_ignore_ascii_case("Headshot")
                    || title_l.contains("headshot")
                    || title_l.contains("hs")
                {
                    tok_score = tok_score.max(92);
                }
            }
            "kill" | "kills" | "frag" | "elim" => {
                if matches!(
                    clip.tag.as_str(),
                    "Kill" | "Headshot" | "Multikill" | "Ace" | "Clutch"
                ) || clip.kills.unwrap_or(0) >= 1
                {
                    tok_score = tok_score.max(75);
                }
            }
            "map" | "maps" => {
                if !clip.map.is_empty() || detect_known_map_in_text(&clip.display_title).is_some() {
                    tok_score = tok_score.max(80);
                }
            }
            "fav" | "favorite" | "favorites" | "star" | "starred" => {
                if clip.favorite {
                    tok_score = tok_score.max(95);
                }
            }
            "stem" | "stems" | "tracks" | "multitrack" => {
                if clip.has_separate_stems {
                    tok_score = tok_score.max(85);
                }
            }
            _ => {}
        }

        if tok_score == 0 {
            return None;
        }
        total_score += tok_score;
    }

    Some(total_score)
}

pub fn canonical_game_from_token(token: &str) -> Option<&'static str> {
    match token.trim().to_lowercase().as_str() {
        "cs" | "cs2" | "csgo" | "counterstrike" | "counter-strike" => Some("CS2"),
        "val" | "valo" | "valorant" => Some("Valorant"),
        "apex" | "apexlegends" => Some("Apex"),
        "rocketleague" | "rocket" | "rl" => Some("Rocket League"),
        "overwatch" | "overwatch2" | "ow" | "ow2" => Some("Overwatch 2"),
        "dota2" | "dota" => Some("Dota 2"),
        "lol" | "league" | "leagueoflegends" => Some("League of Legends"),
        "fn" | "fortnite" => Some("Fortnite"),
        "wz" | "warzone" | "cod" => Some("Warzone"),
        "pubg" => Some("PUBG"),
        "r6" | "siege" | "rainbowsix" => Some("Rainbow Six"),
        "mc" | "minecraft" => Some("Minecraft"),
        "gta" | "gtav" | "fivem" => Some("GTA V"),
        "rust" => Some("Rust"),
        "desktop" | "app" | "screen" => Some("Desktop"),
        _ => None,
    }
}

pub fn canonical_tag_from_token(token: &str) -> Option<&'static str> {
    match token.trim().to_lowercase().as_str() {
        "ace" => Some("Ace"),
        "clutch" => Some("Clutch"),
        "multikill" | "quadra" | "penta" | "triple" | "3k" | "4k" | "5k" => Some("Multikill"),
        "headshot" | "hs" => Some("Headshot"),
        "kill" | "knockdown" => Some("Kill"),
        "goal" => Some("Goal"),
        "save" | "epicsave" => Some("Save"),
        "direct" => Some("Direct"),
        "discord" => Some("Discord"),
        "edited" | "edit" | "cut" | "spliced" => Some("Edited"),
        "replay" | "manual" => Some("Replay"),
        _ => None,
    }
}

pub fn parse_legacy_filename_tokens(stem: &str, parent_folder: &str) -> (String, String) {
    let tokens: Vec<&str> = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();

    let mut game: Option<&'static str> = canonical_game_from_token(parent_folder);
    if game.is_none() {
        for &tok in &tokens {
            if let Some(g) = canonical_game_from_token(tok) {
                game = Some(g);
                break;
            }
        }
    }

    let mut tag: Option<&'static str> = None;
    for &tok in &tokens {
        if let Some(t) = canonical_tag_from_token(tok) {
            tag = Some(t);
            break;
        }
    }

    (
        game.unwrap_or("Desktop").to_string(),
        tag.unwrap_or("Clip").to_string(),
    )
}

pub fn format_mm_ss(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let m = total / 60;
    let s = total % 60;
    format!("{m}:{s:02}")
}

pub fn format_local_date_time(mtime_secs: u64) -> String {
    if mtime_secs == 0 {
        return chrono::Local::now().format("%b %d %H:%M").to_string();
    }
    use chrono::TimeZone;
    if let chrono::LocalResult::Single(dt) = chrono::Local.timestamp_opt(mtime_secs as i64, 0) {
        dt.format("%b %d %H:%M").to_string()
    } else {
        chrono::Local::now().format("%b %d %H:%M").to_string()
    }
}

pub fn format_relative_mtime(mtime_secs: u64) -> String {
    if mtime_secs == 0 {
        return "Today".to_string();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(mtime_secs);
    let diff = now.saturating_sub(mtime_secs);
    if diff < 86_400 {
        "Today".to_string()
    } else if diff < 172_800 {
        "Yesterday".to_string()
    } else {
        format!("{}d ago", diff / 86_400)
    }
}
