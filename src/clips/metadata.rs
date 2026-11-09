#![allow(dead_code, unused_imports)]
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClipSidecarMeta {
    #[serde(default)]
    pub display_title: String,
    #[serde(default)]
    pub game: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub event_kind: String,
    #[serde(default)]
    pub map: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub weapon: String,
    #[serde(default)]
    pub kills: Option<u32>,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub created_epoch_secs: u64,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub has_separate_stems: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedClipIndexEntry {
    pub path_str: String,
    pub file_size_bytes: u64,
    pub mtime_secs: u64,
    pub duration_secs: f64,
    pub has_separate_stems: bool,
    pub display_title: String,
    pub game: String,
    pub tag: String,
    #[serde(default)]
    pub map: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub weapon: String,
    #[serde(default)]
    pub kills: Option<u32>,
    pub favorite: bool,
}

#[derive(Clone, Debug)]
pub struct ClipItem {
    pub id: u64,
    pub display_title: String,
    pub game: String,
    pub tag: String,
    pub map: String,
    pub mode: String,
    pub weapon: String,
    pub kills: Option<u32>,
    pub date_label: String,
    pub duration_secs: f64,
    pub duration_label: String,
    pub favorite: bool,
    pub has_separate_stems: bool,
    pub file_path: PathBuf,
    pub thumb_path: PathBuf,
    pub mtime_secs: u64,
}

pub fn resolve_app_cache_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data).join("Clotch");
        let _ = std::fs::create_dir_all(&p);
        return p;
    }
    let fallback = PathBuf::from("ClotchCache");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

pub fn resolve_thumbs_cache_dir() -> PathBuf {
    let p = resolve_app_cache_dir().join("thumbs");
    let _ = std::fs::create_dir_all(&p);
    p
}

pub fn resolve_clips_index_path() -> PathBuf {
    resolve_app_cache_dir().join("clips_index.json")
}

pub fn compute_stable_clip_id(path: &Path) -> u64 {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let norm = canonical.to_string_lossy().to_lowercase();
    let mut hasher = DefaultHasher::new();
    norm.hash(&mut hasher);
    hasher.finish()
}

pub fn load_cached_clips_from_index() -> Vec<ClipItem> {
    let index_path = resolve_clips_index_path();
    let Ok(raw) = std::fs::read_to_string(&index_path) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<CachedClipIndexEntry>>(&raw) else {
        return Vec::new();
    };
    let thumbs_dir = resolve_thumbs_cache_dir();
    let mut items = Vec::with_capacity(entries.len());
    let mut seen_ids = HashSet::with_capacity(entries.len());

    for e in entries {
        let p = PathBuf::from(&e.path_str);
        if !p.exists() {
            continue;
        }
        let cid = compute_stable_clip_id(&p);
        if !seen_ids.insert(cid) {
            continue;
        }
        let thumb_path = thumbs_dir.join(format!("{cid:016x}.rgba"));
        let dur_secs = e.duration_secs.max(1.0);
        items.push(ClipItem {
            id: cid,
            display_title: e.display_title,
            game: e.game,
            tag: e.tag,
            map: e.map,
            mode: e.mode,
            weapon: e.weapon,
            kills: e.kills,
            date_label: super::library::format_relative_mtime(e.mtime_secs),
            duration_secs: dur_secs,
            duration_label: super::library::format_mm_ss(dur_secs),
            favorite: e.favorite,
            has_separate_stems: e.has_separate_stems,
            file_path: p,
            thumb_path,
            mtime_secs: e.mtime_secs,
        });
    }
    items.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    items
}

pub fn persist_clip_sidecar(clip: &ClipItem) {
    let sidecar_path = clip.file_path.with_extension("json");
    let mut meta = std::fs::read_to_string(&sidecar_path)
        .ok()
        .and_then(|s| serde_json::from_str::<ClipSidecarMeta>(&s).ok())
        .unwrap_or_default();
    meta.display_title = clip.display_title.clone();
    meta.game = clip.game.clone();
    meta.tag = clip.tag.clone();
    if !clip.map.is_empty() {
        meta.map = clip.map.clone();
    }
    if !clip.mode.is_empty() {
        meta.mode = clip.mode.clone();
    }
    if !clip.weapon.is_empty() {
        meta.weapon = clip.weapon.clone();
    }
    if clip.kills.is_some() {
        meta.kills = clip.kills;
    }
    meta.favorite = clip.favorite;
    meta.duration_secs = Some(clip.duration_secs);
    meta.has_separate_stems = Some(clip.has_separate_stems);
    if meta.created_epoch_secs == 0 {
        meta.created_epoch_secs = clip.mtime_secs;
    }
    if let Ok(serialized) = serde_json::to_string_pretty(&meta) {
        let _ = std::fs::write(&sidecar_path, serialized);
    }
}
