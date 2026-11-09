#![allow(dead_code, unused_imports)]
use std::collections::{HashMap, VecDeque};
use eframe::egui::TextureHandle;
use crate::clips::ClipItem;

pub fn is_highlight_clip(clip: &ClipItem) -> bool {
    let t = clip.tag.trim().to_lowercase();
    matches!(
        t.as_str(),
        "ace" | "clutch" | "multikill" | "kill" | "headshot" | "goal" | "hattrick" | "save" | "mvp" | "1stblood" | "obj" | "plant" | "defuse"
    ) || clip.kills.unwrap_or(0) > 0
}

pub fn inferred_clip_kills(clip: &ClipItem) -> u32 {
    if let Some(k) = clip.kills {
        if k <= 10 {
            return k;
        }
    }
    match clip.tag.trim().to_lowercase().as_str() {
        "ace" => 5,
        "multikill" => 3,
        "clutch" | "kill" | "headshot" | "1stblood" => 1,
        _ => 0,
    }
}

pub struct ThumbnailLruCache {
    pub(crate) map: HashMap<u64, TextureHandle>,
    pub(crate) order: VecDeque<u64>,
    pub(crate) capacity: usize,
}

impl ThumbnailLruCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity: capacity.max(8),
        }
    }

    pub fn get(&mut self, clip_id: u64) -> Option<&TextureHandle> {
        if self.map.contains_key(&clip_id) {
            self.order.retain(|&k| k != clip_id);
            self.order.push_back(clip_id);
            self.map.get(&clip_id)
        } else {
            None
        }
    }

    pub fn insert(&mut self, clip_id: u64, handle: TextureHandle) {
        if self.map.contains_key(&clip_id) {
            self.order.retain(|&k| k != clip_id);
        } else if self.map.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
        self.order.push_back(clip_id);
        self.map.insert(clip_id, handle);
    }

    pub fn remove(&mut self, clip_id: u64) {
        self.map.remove(&clip_id);
        self.order.retain(|&k| k != clip_id);
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

