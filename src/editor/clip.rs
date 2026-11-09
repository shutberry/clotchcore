#![allow(dead_code, unused_imports)]
use bytes::Bytes;
use mp4::{
    AacConfig, AudioObjectType, AvcConfig, ChannelConfig, Mp4Config, Mp4Reader, Mp4Sample,
    Mp4Writer, SampleFreqIndex, TrackConfig, TrackType,
};
use openh264::decoder::Decoder as OpenH264Decoder;
use openh264::formats::YUVSource;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::audio::build_silent_aac_packet;
use crate::tracker::{Event, EventSink};
use crate::recorder::buffer::{AudioStemPackets, CompressedPacket, PacketKind, RingBuffer};
use crate::recorder::config::{parse_duration_secs, parse_resolution_pair, RecorderConfig};
use crate::recorder::encoder::{CodecContextInfo, EncoderPipeline};
use crate::recorder::priority;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioTrackRole {
    MasterMix,
    Game,
    Discord,
    Microphone,
    OtherApps,
}

impl AudioTrackRole {
    pub fn from_audio_index(idx: usize) -> Self {
        match idx {
            0 => Self::MasterMix,
            1 => Self::Game,
            2 => Self::Discord,
            3 => Self::Microphone,
            _ => Self::OtherApps,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MasterMix => "Master Mix",
            Self::Game => "Game Audio",
            Self::Discord => "Discord / Voice Chat",
            Self::Microphone => "Microphone",
            Self::OtherApps => "Other Apps",
        }
    }

    pub fn short_suffix(self) -> &'static str {
        match self {
            Self::MasterMix => "master",
            Self::Game => "game",
            Self::Discord => "discord",
            Self::Microphone => "mic",
            Self::OtherApps => "other",
        }
    }

    pub fn parse_str(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "master" | "mix" | "master_mix" | "all" => Some(Self::MasterMix),
            "game" | "gameplay" | "game_audio" => Some(Self::Game),
            "discord" | "voice" | "chat" | "comms" => Some(Self::Discord),
            "mic" | "microphone" => Some(Self::Microphone),
            "other" | "apps" | "other_apps" | "browser" | "music" => Some(Self::OtherApps),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioTrackMetadata {
    pub track_id: u32,
    pub audio_index: usize,
    pub role: AudioTrackRole,
    pub label: String,
    pub timescale: u32,
    pub sample_count: u32,
    pub duration_ms: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipMetadata {
    pub path: PathBuf,
    pub file_size_bytes: u64,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_track_id: Option<u32>,
    pub video_sample_count: u32,
    pub keyframe_timestamps_ms: Vec<u64>,
    pub audio_tracks: Vec<AudioTrackMetadata>,
    pub has_separated_stems: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutMode {
    FastKeyframeCopy,
    FrameExact,
    KeyframeSnap,
}

impl Default for CutMode {
    fn default() -> Self {
        Self::FastKeyframeCopy
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionPreset {
    Original,
    P480,
    P720,
    P900,
    P1080,
    P1440,
    Custom { width: u32, height: u32 },
}

impl ResolutionPreset {
    pub fn resolve_dimensions(self, src_w: u32, src_h: u32) -> (u32, u32) {
        let target_h = match self {
            Self::Original => return ((src_w.max(64)) & !1, (src_h.max(64)) & !1),
            Self::P480 => 480u32,
            Self::P720 => 720u32,
            Self::P900 => 900u32,
            Self::P1080 => 1080u32,
            Self::P1440 => 1440u32,
            Self::Custom { width, height } => {
                return ((width.clamp(64, 7680)) & !1, (height.clamp(64, 4320)) & !1)
            }
        };
        if src_h == 0 || src_w == 0 {
            return (1280, 720);
        }
        let scaled_w = ((src_w as u64 * target_h as u64) / src_h as u64) as u32;
        ((scaled_w.clamp(64, 7680)) & !1, (target_h.clamp(64, 4320)) & !1)
    }

    pub fn parse_str(raw: &str) -> Option<Self> {
        let clean = raw.trim().to_lowercase();
        match clean.as_str() {
            "original" | "source" | "native" | "same" => Some(Self::Original),
            "480p" | "sd" => Some(Self::P480),
            "720p" | "hd" => Some(Self::P720),
            "900p" => Some(Self::P900),
            "1080p" | "fhd" => Some(Self::P1080),
            "1440p" | "2k" | "qhd" => Some(Self::P1440),
            "9:16" | "9:16 vertical" | "vertical" | "shorts" | "tiktok" => Some(Self::Custom {
                width: 1080,
                height: 1920,
            }),
            _ => {
                let (w, h) = parse_resolution_pair(&clean)?;
                Some(Self::Custom {
                    width: w,
                    height: h,
                })
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioStemMixConfig {
    pub game_enabled: bool,
    pub discord_enabled: bool,
    pub mic_enabled: bool,
    pub other_enabled: bool,
    pub game_gain: f32,
    pub discord_gain: f32,
    pub mic_gain: f32,
    pub other_gain: f32,
    pub keep_stem_tracks_in_output: bool,
}

impl Default for AudioStemMixConfig {
    fn default() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: true,
            mic_enabled: true,
            other_enabled: true,
            game_gain: 1.0,
            discord_gain: 1.0,
            mic_gain: 1.0,
            other_gain: 1.0,
            keep_stem_tracks_in_output: false,
        }
    }
}

impl AudioStemMixConfig {
    pub fn game_only() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: false,
            mic_enabled: false,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn discord_only() -> Self {
        Self {
            game_enabled: false,
            discord_enabled: true,
            mic_enabled: false,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn mic_only() -> Self {
        Self {
            game_enabled: false,
            discord_enabled: false,
            mic_enabled: true,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn other_only() -> Self {
        Self {
            game_enabled: false,
            discord_enabled: false,
            mic_enabled: false,
            other_enabled: true,
            ..Self::default()
        }
    }

    pub fn no_discord() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: false,
            mic_enabled: true,
            other_enabled: true,
            ..Self::default()
        }
    }

    pub fn no_mic() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: true,
            mic_enabled: false,
            other_enabled: true,
            ..Self::default()
        }
    }

    pub fn no_other_apps() -> Self {
        Self {
            game_enabled: true,
            discord_enabled: true,
            mic_enabled: true,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn mute_all() -> Self {
        Self {
            game_enabled: false,
            discord_enabled: false,
            mic_enabled: false,
            other_enabled: false,
            ..Self::default()
        }
    }

    pub fn is_all_muted(&self) -> bool {
        (!self.game_enabled || self.game_gain <= 0.001)
            && (!self.discord_enabled || self.discord_gain <= 0.001)
            && (!self.mic_enabled || self.mic_gain <= 0.001)
            && (!self.other_enabled || self.other_gain <= 0.001)
    }

    pub fn is_unmodified_full_mix(&self) -> bool {
        self.game_enabled
            && self.discord_enabled
            && self.mic_enabled
            && self.other_enabled
            && (self.game_gain - 1.0).abs() < 0.01
            && (self.discord_gain - 1.0).abs() < 0.01
            && (self.mic_gain - 1.0).abs() < 0.01
            && (self.other_gain - 1.0).abs() < 0.01
    }

    pub fn single_solo_role(&self) -> Option<(AudioTrackRole, f32)> {
        let mut active = Vec::with_capacity(4);
        if self.game_enabled && self.game_gain > 0.001 {
            active.push((AudioTrackRole::Game, self.game_gain));
        }
        if self.discord_enabled && self.discord_gain > 0.001 {
            active.push((AudioTrackRole::Discord, self.discord_gain));
        }
        if self.mic_enabled && self.mic_gain > 0.001 {
            active.push((AudioTrackRole::Microphone, self.mic_gain));
        }
        if self.other_enabled && self.other_gain > 0.001 {
            active.push((AudioTrackRole::OtherApps, self.other_gain));
        }
        if active.len() == 1 {
            Some(active[0])
        } else {
            None
        }
    }
}




pub(crate) struct DemuxedClip {
    pub(crate) metadata: ClipMetadata,
    pub(crate) sps: Vec<u8>,
    pub(crate) pps: Vec<u8>,
    pub(crate) video_packets: Vec<Arc<CompressedPacket>>,
    pub(crate) audio_tracks_packets: Vec<(AudioTrackMetadata, Vec<Arc<CompressedPacket>>)>,
}

fn ebml_read_id(data: &[u8], pos: usize) -> Option<(u32, usize)> {
    let b0 = *data.get(pos)?;
    if b0 & 0x80 != 0 {
        Some((b0 as u32, 1))
    } else if b0 & 0x40 != 0 {
        let b1 = *data.get(pos + 1)?;
        Some((((b0 as u32) << 8) | (b1 as u32), 2))
    } else if b0 & 0x20 != 0 {
        let b1 = *data.get(pos + 1)?;
        let b2 = *data.get(pos + 2)?;
        Some((((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32), 3))
    } else if b0 & 0x10 != 0 {
        let b1 = *data.get(pos + 1)?;
        let b2 = *data.get(pos + 2)?;
        let b3 = *data.get(pos + 3)?;
        Some((
            ((b0 as u32) << 24) | ((b1 as u32) << 16) | ((b2 as u32) << 8) | (b3 as u32),
            4,
        ))
    } else {
        None
    }
}

fn ebml_read_size(data: &[u8], pos: usize) -> Option<(u64, usize, bool)> {
    let b0 = *data.get(pos)?;
    if b0 == 0 {
        return None;
    }
    let lz = (b0.leading_zeros() as usize).min(7);
    let len = lz + 1;
    if pos + len > data.len() {
        return None;
    }
    let mask = (1u8 << (7 - lz)) - 1;
    let mut val = (b0 & mask) as u64;
    let mut all_ones = (b0 & mask) == mask;
    for i in 1..len {
        let b = data[pos + i];
        if b != 0xFF {
            all_ones = false;
        }
        val = (val << 8) | (b as u64);
    }
    Some((val, len, all_ones))
}

fn ebml_read_uint(payload: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in payload.iter().take(8) {
        v = (v << 8) | (b as u64);
    }
    v
}

fn ebml_read_float(payload: &[u8]) -> f64 {
    if payload.len() == 4 {
        let mut arr = [0u8; 4];
        arr.copy_from_slice(payload);
        f32::from_be_bytes(arr) as f64
    } else if payload.len() == 8 {
        let mut arr = [0u8; 8];
        arr.copy_from_slice(payload);
        f64::from_be_bytes(arr)
    } else {
        0.0
    }
}

fn parse_avcc_or_annexb_codec_private(priv_bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    if priv_bytes.len() >= 8 && priv_bytes[0] == 1 {
        let num_sps = (priv_bytes[5] & 0x1F) as usize;
        let mut pos = 6usize;
        for _ in 0..num_sps {
            if pos + 2 > priv_bytes.len() {
                break;
            }
            let slen = u16::from_be_bytes([priv_bytes[pos], priv_bytes[pos + 1]]) as usize;
            pos += 2;
            if pos + slen <= priv_bytes.len() {
                if sps.is_empty() {
                    sps = priv_bytes[pos..pos + slen].to_vec();
                }
                pos += slen;
            } else {
                break;
            }
        }
        if pos < priv_bytes.len() {
            let num_pps = priv_bytes[pos] as usize;
            pos += 1;
            for _ in 0..num_pps {
                if pos + 2 > priv_bytes.len() {
                    break;
                }
                let plen = u16::from_be_bytes([priv_bytes[pos], priv_bytes[pos + 1]]) as usize;
                pos += 2;
                if pos + plen <= priv_bytes.len() {
                    if pps.is_empty() {
                        pps = priv_bytes[pos..pos + plen].to_vec();
                    }
                    pos += plen;
                } else {
                    break;
                }
            }
        }
    } else {
        extract_sps_pps_from_annexb(priv_bytes, &mut sps, &mut pps);
    }
    (sps, pps)
}

fn extract_sps_pps_from_annexb(data: &[u8], sps: &mut Vec<u8>, pps: &mut Vec<u8>) {
    let mut nal_starts = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                nal_starts.push((i, i + 3));
                i += 3;
                continue;
            } else if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                nal_starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    for idx in 0..nal_starts.len() {
        let payload_start = nal_starts[idx].1;
        let payload_end = if idx + 1 < nal_starts.len() {
            nal_starts[idx + 1].0
        } else {
            data.len()
        };
        if payload_start < payload_end {
            let nal = &data[payload_start..payload_end];
            let nal_type = nal[0] & 0x1F;
            if nal_type == 7 && sps.is_empty() {
                *sps = nal.to_vec();
            } else if nal_type == 8 && pps.is_empty() {
                *pps = nal.to_vec();
            }
        }
    }
}

fn normalize_h264_packet_to_avcc(
    raw: &[u8],
    sps: &mut Vec<u8>,
    pps: &mut Vec<u8>,
) -> Bytes {
    let is_annexb = (raw.len() >= 4 && raw[0] == 0 && raw[1] == 0 && raw[2] == 0 && raw[3] == 1)
        || (raw.len() >= 3 && raw[0] == 0 && raw[1] == 0 && raw[2] == 1);
    if !is_annexb {
        if sps.is_empty() || pps.is_empty() {
            let mut pos = 0usize;
            while pos + 4 <= raw.len() {
                let nlen = u32::from_be_bytes([raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]])
                    as usize;
                pos += 4;
                if nlen == 0 || pos + nlen > raw.len() {
                    break;
                }
                let ntype = raw[pos] & 0x1F;
                if ntype == 7 && sps.is_empty() {
                    *sps = raw[pos..pos + nlen].to_vec();
                } else if ntype == 8 && pps.is_empty() {
                    *pps = raw[pos..pos + nlen].to_vec();
                }
                pos += nlen;
            }
        }
        return Bytes::copy_from_slice(raw);
    }

    if sps.is_empty() || pps.is_empty() {
        extract_sps_pps_from_annexb(raw, sps, pps);
    }

    let mut nal_starts = Vec::new();
    let mut i = 0usize;
    while i + 3 < raw.len() {
        if raw[i] == 0 && raw[i + 1] == 0 {
            if raw[i + 2] == 1 {
                nal_starts.push((i, i + 3));
                i += 3;
                continue;
            } else if i + 4 <= raw.len() && raw[i + 2] == 0 && raw[i + 3] == 1 {
                nal_starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    let mut avcc = Vec::with_capacity(raw.len() + 16);
    for idx in 0..nal_starts.len() {
        let p_start = nal_starts[idx].1;
        let p_end = if idx + 1 < nal_starts.len() {
            nal_starts[idx + 1].0
        } else {
            raw.len()
        };
        if p_start < p_end {
            let nal = &raw[p_start..p_end];
            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
        }
    }
    Bytes::from(avcc)
}

fn split_mkv_block_frames<'a>(block: & [u8], header_end: usize, lacing_mode: u8) -> Vec<& [u8]> {
    if header_end >= block.len() {
        return Vec::new();
    }
    let payload = &block[header_end..];
    if lacing_mode == 0 || payload.is_empty() {
        return vec![payload];
    }
    let frame_count = (payload[0] as usize) + 1;
    let mut pos = 1usize;
    let mut sizes: Vec<usize> = Vec::with_capacity(frame_count);

    match lacing_mode {
        1 => {
            for _ in 0..(frame_count.saturating_sub(1)) {
                let mut sz = 0usize;
                while pos < payload.len() {
                    let b = payload[pos];
                    pos += 1;
                    sz += b as usize;
                    if b < 255 {
                        break;
                    }
                }
                sizes.push(sz);
            }
        }
        2 => {
            let rem = payload.len().saturating_sub(1);
            let each = rem / frame_count.max(1);
            for _ in 0..(frame_count.saturating_sub(1)) {
                sizes.push(each);
            }
        }
        3 => {
            let mut last_sz: i64 = 0;
            for f_idx in 0..(frame_count.saturating_sub(1)) {
                let Some((val, vlen, _)) = ebml_read_size(payload, pos) else {
                    break;
                };
                pos += vlen;
                let sz = if f_idx == 0 {
                    val as i64
                } else {
                    let bias = (1i64 << (7 * vlen - 1)) - 1;
                    last_sz + ((val as i64) - bias)
                };
                last_sz = sz.max(0);
                sizes.push(last_sz as usize);
            }
        }
        _ => return vec![payload],
    }

    let mut out = Vec::with_capacity(frame_count);
    for sz in sizes {
        if pos + sz <= payload.len() {
            out.push(&payload[pos..pos + sz]);
            pos += sz;
        } else {
            break;
        }
    }
    if pos < payload.len() {
        out.push(&payload[pos..]);
    }
    out
}

pub fn probe_clip_header_fast(input_path: impl AsRef<Path>) -> Result<(f64, bool), String> {
    use std::io::Read;
    let path = input_path.as_ref();
    let mut file = File::open(path)
        .map_err(|e| format!("failed to open clip '{}': {e}", path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|e| format!("failed to read metadata '{}': {e}", path.display()))?
        .len();
    if file_size == 0 {
        return Err("clip file is empty".to_string());
    }

    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok() && magic == [0x1A, 0x45, 0xDF, 0xA3] {
        let mut head_buf = vec![0u8; (file_size.min(256 * 1024)) as usize];
        head_buf[..4].copy_from_slice(&magic);
        let n = file.read(&mut head_buf[4..]).unwrap_or(0);
        head_buf.truncate(4 + n);

        let mut tc_scale_ns: u64 = 1_000_000;
        let mut dur_scaled: f64 = 0.0;
        let mut audio_track_count = 0usize;

        let mut pos = 0usize;
        while pos < head_buf.len() {
            let Some((id, id_len)) = ebml_read_id(&head_buf, pos) else {
                break;
            };
            let Some((sz, sz_len, is_unk)) = ebml_read_size(&head_buf, pos + id_len) else {
                break;
            };
            let body_start = pos + id_len + sz_len;
            if id == 0x18538067 {
                pos = body_start;
                continue;
            }
            if id == 0x1F43B675 {
                break;
            }
            let body_end = if is_unk {
                head_buf.len()
            } else {
                (body_start + (sz as usize)).min(head_buf.len())
            };
            let body = &head_buf[body_start..body_end];
            if id == 0x1549A966 {
                let mut ip = 0usize;
                while ip < body.len() {
                    let Some((iid, ilen)) = ebml_read_id(body, ip) else {
                        break;
                    };
                    let Some((isz, islen, _)) = ebml_read_size(body, ip + ilen) else {
                        break;
                    };
                    let ib_start = ip + ilen + islen;
                    let ib_end = (ib_start + (isz as usize)).min(body.len());
                    if iid == 0x2AD7B1 {
                        tc_scale_ns = ebml_read_uint(&body[ib_start..ib_end]).max(1);
                    } else if iid == 0x4489 {
                        dur_scaled = ebml_read_float(&body[ib_start..ib_end]);
                    }
                    ip = ib_end;
                }
            } else if id == 0x1654AE6B {
                let mut tp = 0usize;
                while tp < body.len() {
                    let Some((tid, tlen)) = ebml_read_id(body, tp) else {
                        break;
                    };
                    let Some((tsz, tslen, _)) = ebml_read_size(body, tp + tlen) else {
                        break;
                    };
                    let tb_start = tp + tlen + tslen;
                    let tb_end = (tb_start + (tsz as usize)).min(body.len());
                    if tid == 0xAE {
                        let entry = &body[tb_start..tb_end];
                        let mut ep = 0usize;
                        while ep < entry.len() {
                            let Some((eid, elen)) = ebml_read_id(entry, ep) else {
                                break;
                            };
                            let Some((esz, eslen, _)) = ebml_read_size(entry, ep + elen) else {
                                break;
                            };
                            let eb_start = ep + elen + eslen;
                            let eb_end = (eb_start + (esz as usize)).min(entry.len());
                            if eid == 0x83 && ebml_read_uint(&entry[eb_start..eb_end]) == 2 {
                                audio_track_count += 1;
                            }
                            ep = eb_end;
                        }
                    }
                    tp = tb_end;
                }
            }
            pos = body_end;
        }

        let dur_ms = ((dur_scaled * (tc_scale_ns as f64)) / 1_000_000.0).round() as u64;
        let dur_secs = if dur_ms > 100 {
            dur_ms as f64 / 1000.0
        } else {
            15.0
        };
        return Ok((dur_secs.max(1.0), audio_track_count >= 5));
    }

    let file = File::open(path)
        .map_err(|e| format!("failed to reopen clip '{}': {e}", path.display()))?;
    let reader = BufReader::with_capacity(128 * 1024, file);
    let mp4 = Mp4Reader::read_header(reader, file_size)
        .map_err(|e| format!("invalid MP4 header '{}': {e}", path.display()))?;

    let mut v_dur_ms = mp4.duration().as_millis() as u64;
    let mut audio_track_count = 0usize;
    for track in mp4.tracks().values() {
        match track.track_type() {
            Ok(TrackType::Video) => {
                let td = track.duration().as_millis() as u64;
                if td > 0 {
                    v_dur_ms = td;
                }
            }
            Ok(TrackType::Audio) => {
                audio_track_count += 1;
            }
            _ => {}
        }
    }
    let dur_secs = (v_dur_ms as f64 / 1000.0).max(1.0);
    Ok((dur_secs, audio_track_count >= 5))
}

pub(crate) fn demux_mkv_full(input_path: &Path) -> Result<DemuxedClip, String> {
    let raw_bytes = std::fs::read(input_path)
        .map_err(|e| format!("failed to read MKV file '{}': {e}", input_path.display()))?;
    let file_size = raw_bytes.len() as u64;
    if file_size < 16 {
        return Err("MKV file is too small".to_string());
    }

    let mut tc_scale_ns: u64 = 1_000_000;
    let mut duration_scaled: f64 = 0.0;
    let mut video_track_num: Option<u64> = None;
    let mut src_w: u32 = 1920;
    let mut src_h: u32 = 1080;
    let mut default_frame_ns: u64 = 0;
    let mut sps: Vec<u8> = Vec::new();
    let mut pps: Vec<u8> = Vec::new();

    struct MkvAudioTrackDef {
        track_num: u64,
        sample_rate: u32,
        packets: Vec<Arc<CompressedPacket>>,
        total_bytes: u64,
    }
    let mut audio_tracks: Vec<MkvAudioTrackDef> = Vec::new();
    let mut video_packets: Vec<Arc<CompressedPacket>> = Vec::new();
    let mut keyframe_timestamps_ms: Vec<u64> = Vec::new();

    let mut pos = 0usize;
    while pos < raw_bytes.len() {
        let Some((id, id_len)) = ebml_read_id(&raw_bytes, pos) else {
            break;
        };
        let Some((sz, sz_len, is_unk)) = ebml_read_size(&raw_bytes, pos + id_len) else {
            break;
        };
        let body_start = pos + id_len + sz_len;
        if id == 0x18538067 {
            pos = body_start;
            continue;
        }
        let body_end = if is_unk {
            raw_bytes.len()
        } else {
            (body_start + (sz as usize)).min(raw_bytes.len())
        };
        let body = &raw_bytes[body_start..body_end];

        match id {
            0x1549A966 => {
                let mut ip = 0usize;
                while ip < body.len() {
                    let Some((iid, ilen)) = ebml_read_id(body, ip) else {
                        break;
                    };
                    let Some((isz, islen, _)) = ebml_read_size(body, ip + ilen) else {
                        break;
                    };
                    let ib_start = ip + ilen + islen;
                    let ib_end = (ib_start + (isz as usize)).min(body.len());
                    if iid == 0x2AD7B1 {
                        tc_scale_ns = ebml_read_uint(&body[ib_start..ib_end]).max(1);
                    } else if iid == 0x4489 {
                        duration_scaled = ebml_read_float(&body[ib_start..ib_end]);
                    }
                    ip = ib_end;
                }
            }
            0x1654AE6B => {
                let mut tp = 0usize;
                while tp < body.len() {
                    let Some((tid, tlen)) = ebml_read_id(body, tp) else {
                        break;
                    };
                    let Some((tsz, tslen, _)) = ebml_read_size(body, tp + tlen) else {
                        break;
                    };
                    let tb_start = tp + tlen + tslen;
                    let tb_end = (tb_start + (tsz as usize)).min(body.len());
                    if tid == 0xAE {
                        let entry = &body[tb_start..tb_end];
                        let mut t_num = 0u64;
                        let mut t_type = 0u64;
                        let mut codec_priv: &[u8] = &[];
                        let mut def_dur = 0u64;
                        let mut vw = 1920u32;
                        let mut vh = 1080u32;
                        let mut srate = 48_000u32;

                        let mut ep = 0usize;
                        while ep < entry.len() {
                            let Some((eid, elen)) = ebml_read_id(entry, ep) else {
                                break;
                            };
                            let Some((esz, eslen, _)) = ebml_read_size(entry, ep + elen) else {
                                break;
                            };
                            let eb_start = ep + elen + eslen;
                            let eb_end = (eb_start + (esz as usize)).min(entry.len());
                            let epay = &entry[eb_start..eb_end];
                            match eid {
                                0xD7 => t_num = ebml_read_uint(epay),
                                0x83 => t_type = ebml_read_uint(epay),
                                0x63A2 => codec_priv = epay,
                                0x23E383 => def_dur = ebml_read_uint(epay),
                                0xE0 => {
                                    let mut vp = 0usize;
                                    while vp < epay.len() {
                                        let Some((vid, vlen)) = ebml_read_id(epay, vp) else {
                                            break;
                                        };
                                        let Some((vsz, vslen, _)) =
                                            ebml_read_size(epay, vp + vlen)
                                        else {
                                            break;
                                        };
                                        let vb_s = vp + vlen + vslen;
                                        let vb_e = (vb_s + (vsz as usize)).min(epay.len());
                                        if vid == 0xB0 {
                                            vw = ebml_read_uint(&epay[vb_s..vb_e]).max(64) as u32;
                                        } else if vid == 0xBA {
                                            vh = ebml_read_uint(&epay[vb_s..vb_e]).max(64) as u32;
                                        }
                                        vp = vb_e;
                                    }
                                }
                                0xE1 => {
                                    let mut ap = 0usize;
                                    while ap < epay.len() {
                                        let Some((aid, alen)) = ebml_read_id(epay, ap) else {
                                            break;
                                        };
                                        let Some((asz, aslen, _)) =
                                            ebml_read_size(epay, ap + alen)
                                        else {
                                            break;
                                        };
                                        let ab_s = ap + alen + aslen;
                                        let ab_e = (ab_s + (asz as usize)).min(epay.len());
                                        if aid == 0xB5 {
                                            let f = ebml_read_float(&epay[ab_s..ab_e]);
                                            if f >= 8000.0 {
                                                srate = f.round() as u32;
                                            }
                                        }
                                        ap = ab_e;
                                    }
                                }
                                _ => {}
                            }
                            ep = eb_end;
                        }

                        if t_type == 1 && video_track_num.is_none() && t_num > 0 {
                            video_track_num = Some(t_num);
                            src_w = vw;
                            src_h = vh;
                            default_frame_ns = def_dur;
                            if !codec_priv.is_empty() {
                                let (s, p) = parse_avcc_or_annexb_codec_private(codec_priv);
                                sps = s;
                                pps = p;
                            }
                        } else if t_type == 2 && t_num > 0 {
                            audio_tracks.push(MkvAudioTrackDef {
                                track_num: t_num,
                                sample_rate: srate,
                                packets: Vec::new(),
                                total_bytes: 0,
                            });
                        }
                    }
                    tp = tb_end;
                }
            }
            0x1F43B675 => {
                let mut cluster_tc: u64 = 0;
                let mut cp = 0usize;
                while cp < body.len() {
                    let Some((cid, clen)) = ebml_read_id(body, cp) else {
                        break;
                    };
                    let Some((csz, cslen, c_unk)) = ebml_read_size(body, cp + clen) else {
                        break;
                    };
                    let cb_start = cp + clen + cslen;
                    if c_unk && cid == 0x1F43B675 {
                        break;
                    }
                    let cb_end = if c_unk {
                        body.len()
                    } else {
                        (cb_start + (csz as usize)).min(body.len())
                    };
                    let cpay = &body[cb_start..cb_end];

                    if cid == 0xE7 {
                        cluster_tc = ebml_read_uint(cpay);
                    } else if cid == 0xA3 || cid == 0xA0 {
                        let (block_slice, is_simple, has_ref_block) = if cid == 0xA3 {
                            (Some(cpay), true, false)
                        } else {
                            let mut blk_opt: Option<&[u8]> = None;
                            let mut has_ref = false;
                            let mut bgp = 0usize;
                            while bgp < cpay.len() {
                                let Some((bid, blen)) = ebml_read_id(cpay, bgp) else {
                                    break;
                                };
                                let Some((bsz, bslen, _)) = ebml_read_size(cpay, bgp + blen) else {
                                    break;
                                };
                                let bb_s = bgp + blen + bslen;
                                let bb_e = (bb_s + (bsz as usize)).min(cpay.len());
                                if bid == 0xA1 {
                                    blk_opt = Some(&cpay[bb_s..bb_e]);
                                } else if bid == 0xFB {
                                    has_ref = true;
                                }
                                bgp = bb_e;
                            }
                            (blk_opt, false, has_ref)
                        };

                        if let Some(blk) = block_slice {
                            if let Some((t_num, vlen, _)) = ebml_read_size(blk, 0) {
                                if blk.len() >= vlen + 3 {
                                    let rel_tc =
                                        i16::from_be_bytes([blk[vlen], blk[vlen + 1]]) as i64;
                                    let flags = blk[vlen + 2];
                                    let is_kf = if is_simple {
                                        (flags & 0x80) != 0
                                    } else {
                                        !has_ref_block
                                    };
                                    let lacing = (flags >> 1) & 0x03;
                                    let abs_tc = ((cluster_tc as i64) + rel_tc).max(0) as u128;
                                    let pts_ms =
                                        ((abs_tc * (tc_scale_ns as u128)) / 1_000_000) as u64;

                                    let frames = split_mkv_block_frames(blk, vlen + 3, lacing);
                                    if Some(t_num) == video_track_num {
                                        let nom_dur_ms = if default_frame_ns > 0 {
                                            (default_frame_ns / 1_000_000).max(1) as i64
                                        } else {
                                            16
                                        };
                                        for (f_idx, f_bytes) in frames.iter().enumerate() {
                                            if f_bytes.is_empty() {
                                                continue;
                                            }
                                            let f_pts =
                                                pts_ms + (f_idx as u64) * (nom_dur_ms as u64);
                                            let avcc_bytes = normalize_h264_packet_to_avcc(
                                                f_bytes, &mut sps, &mut pps,
                                            );
                                            if is_kf && f_idx == 0 {
                                                keyframe_timestamps_ms.push(f_pts);
                                            }
                                            video_packets.push(Arc::new(CompressedPacket {
                                                kind: PacketKind::Video,
                                                pts_ms: f_pts,
                                                pts: f_pts as i64,
                                                dts: f_pts as i64,
                                                duration: nom_dur_ms,
                                                is_keyframe: is_kf && f_idx == 0,
                                                data: avcc_bytes,
                                                stems: None,
                                            }));
                                        }
                                    } else if let Some(a_def) =
                                        audio_tracks.iter_mut().find(|a| a.track_num == t_num)
                                    {
                                        for (f_idx, f_bytes) in frames.iter().enumerate() {
                                            if f_bytes.is_empty() {
                                                continue;
                                            }
                                            let clean_aac = if f_bytes.len() > 7
                                                && f_bytes[0] == 0xFF
                                                && (f_bytes[1] & 0xF0) == 0xF0
                                            {
                                                let hdr_len = if (f_bytes[1] & 0x01) == 0 {
                                                    9
                                                } else {
                                                    7
                                                };
                                                if f_bytes.len() > hdr_len {
                                                    &f_bytes[hdr_len..]
                                                } else {
                                                    *f_bytes
                                                }
                                            } else {
                                                *f_bytes
                                            };
                                            let f_pts = pts_ms + (f_idx as u64) * 21;
                                            let start_ticks = (f_pts * 48_000) / 1000;
                                            a_def.total_bytes += clean_aac.len() as u64;
                                            a_def.packets.push(Arc::new(CompressedPacket {
                                                kind: PacketKind::Audio,
                                                pts_ms: f_pts,
                                                pts: start_ticks as i64,
                                                dts: start_ticks as i64,
                                                duration: 1024,
                                                is_keyframe: true,
                                                data: Bytes::copy_from_slice(clean_aac),
                                                stems: None,
                                            }));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    cp = cb_end;
                }
            }
            _ => {}
        }
        pos = body_end;
    }

    if video_packets.is_empty() {
        if let Some(ffmpeg_bin) = locate_ffmpeg_binary() {
            let tmp_mp4 = std::env::temp_dir().join(format!(
                "clotch_mkv_remux_{}.mp4",
                std::process::id()
            ));
            let mut cmd = Command::new(ffmpeg_bin);
            cmd.arg("-y")
                .arg("-hide_banner")
                .arg("-loglevel")
                .arg("error")
                .arg("-i")
                .arg(input_path)
                .arg("-map")
                .arg("0:v:0")
                .arg("-map")
                .arg("0:a?")
                .arg("-c:v")
                .arg("libx264")
                .arg("-preset")
                .arg("ultrafast")
                .arg("-crf")
                .arg("18")
                .arg("-c:a")
                .arg("aac")
                .arg(&tmp_mp4)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000 | 0x00004000);
            }
            if let Ok(st) = cmd.status() {
                if st.success() && tmp_mp4.exists() {
                    let res = demux_mp4_full(&tmp_mp4, Some(input_path));
                    let _ = std::fs::remove_file(&tmp_mp4);
                    return res;
                }
            }
            let _ = std::fs::remove_file(&tmp_mp4);
        }
        return Err("MKV clip contains no readable H.264 video packets".to_string());
    }

    video_packets.sort_by_key(|p| p.pts_ms);
    for i in 0..video_packets.len() {
        if i + 1 < video_packets.len() {
            let d = video_packets[i + 1]
                .pts_ms
                .saturating_sub(video_packets[i].pts_ms)
                .clamp(1, 1000) as i64;
            if let Some(pkt) = Arc::get_mut(&mut video_packets[i]) {
                pkt.duration = d;
            }
        }
    }

    let mut v_dur_ms = ((duration_scaled * (tc_scale_ns as f64)) / 1_000_000.0).round() as u64;
    if let Some(last_v) = video_packets.last() {
        let last_end = last_v.pts_ms.saturating_add(last_v.duration.max(16) as u64);
        if v_dur_ms < last_end {
            v_dur_ms = last_end;
        }
    }

    let fps = if !video_packets.is_empty() && v_dur_ms > 0 {
        (((video_packets.len() as u64 * 1000) + (v_dur_ms / 2)) / v_dur_ms.max(1)) as u32
    } else {
        60
    }
    .clamp(15, 240);

    let mut audio_tracks_meta = Vec::with_capacity(audio_tracks.len());
    let mut audio_tracks_packets = Vec::with_capacity(audio_tracks.len());

    for (audio_idx, mut a_def) in audio_tracks.into_iter().enumerate() {
        a_def.packets.sort_by_key(|p| p.pts_ms);
        let role = AudioTrackRole::from_audio_index(audio_idx);
        let eff_dur = a_def
            .packets
            .last()
            .map(|p| p.pts_ms + 21)
            .unwrap_or(v_dur_ms);
        let meta = AudioTrackMetadata {
            track_id: a_def.track_num as u32,
            audio_index: audio_idx,
            role,
            label: role.label().to_string(),
            timescale: a_def.sample_rate.max(8000),
            sample_count: a_def.packets.len() as u32,
            duration_ms: eff_dur,
            total_bytes: a_def.total_bytes,
        };
        audio_tracks_meta.push(meta.clone());
        audio_tracks_packets.push((meta, a_def.packets));
    }

    if audio_tracks_packets.len() >= 5 {
        let master_len = audio_tracks_packets[0].1.len();
        let mut enriched_master = Vec::with_capacity(master_len);
        for i in 0..master_len {
            let base_pkt = &audio_tracks_packets[0].1[i];
            let game_b = audio_tracks_packets[1]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| base_pkt.data.clone());
            let discord_b = audio_tracks_packets[2]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));
            let mic_b = audio_tracks_packets[3]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));
            let other_b = audio_tracks_packets[4]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));

            enriched_master.push(Arc::new(CompressedPacket {
                kind: PacketKind::Audio,
                pts_ms: base_pkt.pts_ms,
                pts: base_pkt.pts,
                dts: base_pkt.dts,
                duration: base_pkt.duration,
                is_keyframe: true,
                data: base_pkt.data.clone(),
                stems: Some(Arc::new(AudioStemPackets {
                    game: game_b,
                    discord: discord_b,
                    mic: mic_b,
                    other: other_b,
                })),
            }));
        }
        audio_tracks_packets[0].1 = enriched_master;
    }

    let has_separated_stems = audio_tracks_meta.len() >= 5;
    let metadata = ClipMetadata {
        path: input_path.to_path_buf(),
        file_size_bytes: file_size,
        duration_ms: v_dur_ms,
        width: src_w,
        height: src_h,
        fps,
        video_track_id: video_track_num.map(|n| n as u32),
        video_sample_count: video_packets.len() as u32,
        keyframe_timestamps_ms,
        audio_tracks: audio_tracks_meta,
        has_separated_stems,
    };

    Ok(DemuxedClip {
        metadata,
        sps,
        pps,
        video_packets,
        audio_tracks_packets,
    })
}

pub fn demux_mkv_for_discord(
    input_path: &Path,
) -> Result<
    (
        u32,
        u32,
        u32,
        u64,
        Vec<u8>,
        Vec<u8>,
        Vec<(u64, bool, Bytes)>,
        Vec<Arc<CompressedPacket>>,
        u64,
    ),
    String,
> {
    let demuxed = demux_mkv_full(input_path)?;
    let video_samples: Vec<(u64, bool, Bytes)> = demuxed
        .video_packets
        .iter()
        .map(|p| (p.pts_ms, p.is_keyframe, p.data.clone()))
        .collect();
    let (audio_packets, audio_total_bytes) = if let Some((meta, pkts)) =
        demuxed.audio_tracks_packets.into_iter().next()
    {
        (pkts, meta.total_bytes)
    } else {
        (Vec::new(), 0)
    };
    Ok((
        demuxed.metadata.width,
        demuxed.metadata.height,
        demuxed.metadata.fps,
        demuxed.metadata.duration_ms,
        demuxed.sps,
        demuxed.pps,
        video_samples,
        audio_packets,
        audio_total_bytes,
    ))
}

pub(crate) fn demux_clip_full(input_path: &Path) -> Result<DemuxedClip, String> {
    if let Ok(mut f) = File::open(input_path) {
        use std::io::Read;
        let mut magic = [0u8; 4];
        if f.read_exact(&mut magic).is_ok() && magic == [0x1A, 0x45, 0xDF, 0xA3] {
            return demux_mkv_full(input_path);
        }
    }
    demux_mp4_full(input_path, None)
}

pub(crate) fn demux_mp4_full(input_path: &Path, _logical_path: Option<&Path>) -> Result<DemuxedClip, String> {
    let file = File::open(input_path)
        .map_err(|e| format!("failed to open clip '{}': {e}", input_path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|e| format!("failed to read metadata '{}': {e}", input_path.display()))?
        .len();
    if file_size == 0 {
        return Err("clip file is empty (0 bytes)".to_string());
    }
    let reader = BufReader::with_capacity(2 * 1024 * 1024, file);
    let mut mp4 = Mp4Reader::read_header(reader, file_size)
        .map_err(|e| format!("invalid MP4 header '{}': {e}", input_path.display()))?;

    let mut video_track_id: Option<u32> = None;
    let mut src_w = 1920u32;
    let mut src_h = 1080u32;
    let mut v_timescale = 90_000u32;
    let mut v_sample_count = 0u32;
    let mut v_dur_ms = mp4.duration().as_millis() as u64;
    let mut sps = Vec::new();
    let mut pps = Vec::new();

    let mut raw_audio_tracks: Vec<(u32, u32, u32, u64)> = Vec::new();
    let mut sorted_track_ids: Vec<u32> = mp4.tracks().keys().copied().collect();
    sorted_track_ids.sort_unstable();

    for tid in sorted_track_ids {
        let Some(track) = mp4.tracks().get(&tid) else {
            continue;
        };
        match track.track_type() {
            Ok(TrackType::Video) if video_track_id.is_none() => {
                video_track_id = Some(track.track_id());
                src_w = (track.width() as u32).max(64);
                src_h = (track.height() as u32).max(64);
                v_timescale = track.timescale().max(1);
                v_sample_count = track.sample_count();
                let td = track.duration().as_millis() as u64;
                if td > 0 {
                    v_dur_ms = td;
                }
                if let Ok(s) = track.sequence_parameter_set() {
                    sps = s.to_vec();
                }
                if let Ok(p) = track.picture_parameter_set() {
                    pps = p.to_vec();
                }
            }
            Ok(TrackType::Audio) => {
                let ts = track.timescale().max(1);
                let sc = track.sample_count();
                let dur = track.duration().as_millis() as u64;
                raw_audio_tracks.push((track.track_id(), ts, sc, dur));
            }
            _ => {}
        }
    }

    let mut video_packets = Vec::with_capacity(v_sample_count as usize);
    let mut keyframe_timestamps_ms = Vec::new();

    if let Some(v_id) = video_track_id {
        for sample_idx in 1..=v_sample_count {
            if let Ok(Some(s)) = mp4.read_sample(v_id, sample_idx) {
                let pts_ms = (s.start_time.saturating_mul(1000)) / (v_timescale as u64);
                let dur_ms = ((s.duration as u64).saturating_mul(1000) / (v_timescale as u64)).max(1);
                let ticks_90k = if v_timescale == 90_000 {
                    (s.start_time as i64).saturating_add(s.rendering_offset as i64)
                } else {
                    (((s.start_time as i128 + s.rendering_offset as i128) * 90_000)
                        / (v_timescale as i128)) as i64
                };
                if s.is_sync {
                    keyframe_timestamps_ms.push(pts_ms);
                }
                video_packets.push(Arc::new(CompressedPacket {
                    kind: PacketKind::Video,
                    pts_ms,
                    pts: ticks_90k.max(0),
                    dts: ticks_90k.max(0),
                    duration: dur_ms as i64,
                    is_keyframe: s.is_sync,
                    data: s.bytes,
                    stems: None,
                }));
            }
        }
    }

    if v_dur_ms == 0 {
        if let Some(last_v) = video_packets.last() {
            v_dur_ms = last_v.pts_ms.saturating_add(last_v.duration.max(16) as u64);
        }
    }

    let fps = if !video_packets.is_empty() && v_dur_ms > 0 {
        (((video_packets.len() as u64 * 1000) + (v_dur_ms / 2)) / v_dur_ms.max(1)) as u32
    } else {
        60
    }
    .clamp(15, 240);

    let mut audio_tracks_meta = Vec::with_capacity(raw_audio_tracks.len());
    let mut audio_tracks_packets = Vec::with_capacity(raw_audio_tracks.len());

    for (audio_idx, (a_id, a_ts, a_sc, a_dur)) in raw_audio_tracks.into_iter().enumerate() {
        let role = AudioTrackRole::from_audio_index(audio_idx);
        let mut pkts = Vec::with_capacity(a_sc as usize);
        let mut total_bytes = 0u64;

        for sample_idx in 1..=a_sc {
            if let Ok(Some(s)) = mp4.read_sample(a_id, sample_idx) {
                let pts_ms = (s.start_time.saturating_mul(1000)) / (a_ts as u64);
                let dur_ticks = if a_ts == 48_000 {
                    s.duration as i64
                } else {
                    ((s.duration as u64 * 48_000) / a_ts as u64) as i64
                };
                total_bytes += s.bytes.len() as u64;
                pkts.push(Arc::new(CompressedPacket {
                    kind: PacketKind::Audio,
                    pts_ms,
                    pts: s.start_time as i64,
                    dts: s.start_time as i64,
                    duration: dur_ticks.max(1024),
                    is_keyframe: true,
                    data: s.bytes,
                    stems: None,
                }));
            }
        }

        let eff_dur = if a_dur > 0 {
            a_dur
        } else {
            pkts.last().map(|p| p.pts_ms + 21).unwrap_or(v_dur_ms)
        };

        let meta = AudioTrackMetadata {
            track_id: a_id,
            audio_index: audio_idx,
            role,
            label: role.label().to_string(),
            timescale: a_ts,
            sample_count: pkts.len() as u32,
            duration_ms: eff_dur,
            total_bytes,
        };
        audio_tracks_meta.push(meta.clone());
        audio_tracks_packets.push((meta, pkts));
    }

    if audio_tracks_packets.len() >= 5 {
        let master_len = audio_tracks_packets[0].1.len();
        let mut enriched_master = Vec::with_capacity(master_len);
        for i in 0..master_len {
            let base_pkt = &audio_tracks_packets[0].1[i];
            let game_b = audio_tracks_packets[1]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| base_pkt.data.clone());
            let discord_b = audio_tracks_packets[2]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));
            let mic_b = audio_tracks_packets[3]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));
            let other_b = audio_tracks_packets[4]
                .1
                .get(i)
                .map(|p| p.data.clone())
                .unwrap_or_else(|| build_silent_aac_packet(128));

            enriched_master.push(Arc::new(CompressedPacket {
                kind: PacketKind::Audio,
                pts_ms: base_pkt.pts_ms,
                pts: base_pkt.pts,
                dts: base_pkt.dts,
                duration: base_pkt.duration,
                is_keyframe: true,
                data: base_pkt.data.clone(),
                stems: Some(Arc::new(AudioStemPackets {
                    game: game_b,
                    discord: discord_b,
                    mic: mic_b,
                    other: other_b,
                })),
            }));
        }
        audio_tracks_packets[0].1 = enriched_master;
    }

    let has_separated_stems = audio_tracks_meta.len() >= 5;
    let metadata = ClipMetadata {
        path: input_path.to_path_buf(),
        file_size_bytes: file_size,
        duration_ms: v_dur_ms,
        width: src_w,
        height: src_h,
        fps,
        video_track_id,
        video_sample_count: video_packets.len() as u32,
        keyframe_timestamps_ms,
        audio_tracks: audio_tracks_meta,
        has_separated_stems,
    };

    Ok(DemuxedClip {
        metadata,
        sps,
        pps,
        video_packets,
        audio_tracks_packets,
    })
}

pub fn probe_clip(input_path: impl AsRef<Path>) -> Result<ClipMetadata, String> {
    let demuxed = demux_clip_full(input_path.as_ref())?;
    Ok(demuxed.metadata)
}


pub(crate) fn filter_video_slice_for_trim(
    video_packets: &[Arc<CompressedPacket>],
    start_ms: u64,
    end_ms: u64,
) -> (Vec<Arc<CompressedPacket>>, u64, u64) {
    if video_packets.is_empty() {
        return (Vec::new(), start_ms, end_ms);
    }
    let first_key_idx = video_packets
        .iter()
        .rposition(|p| p.pts_ms <= start_ms && p.is_keyframe)
        .or_else(|| {
            video_packets
                .iter()
                .position(|p| p.pts_ms >= start_ms && p.is_keyframe)
        })
        .unwrap_or(0);

    let mut out = Vec::new();
    for pkt in video_packets.iter().skip(first_key_idx) {
        if pkt.pts_ms > end_ms {
            break;
        }
        out.push(Arc::clone(pkt));
    }

    let actual_start = out.first().map(|p| p.pts_ms).unwrap_or(start_ms);
    let actual_end = out
        .last()
        .map(|p| p.pts_ms.saturating_add(p.duration.max(16) as u64))
        .unwrap_or(end_ms);
    (out, actual_start, actual_end)
}


pub fn cut_clip(
    input_path: impl AsRef<Path>,
    start_ms: u64,
    end_ms: u64,
    mode: CutMode,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    let in_p = input_path.as_ref().to_path_buf();
    let out_p = output_path.or_else(|| Some(default_edited_output_path(&in_p, "cut")));
    run_edit_job(EditJob {
        input_path: in_p,
        output_path: out_p,
        trim_start_ms: Some(start_ms),
        trim_end_ms: Some(end_ms),
        cut_mode: mode,
        prefer_hardware: true,
        ..EditJob::default()
    })
}

pub fn split_clip(
    input_path: impl AsRef<Path>,
    split_at_ms: u64,
    mode: CutMode,
    output_dir: Option<&Path>,
) -> Result<SplitResult, String> {
    split_multi_clip(input_path, &[split_at_ms], mode, output_dir)
}

pub fn split_multi_clip(
    input_path: impl AsRef<Path>,
    split_points_ms: &[u64],
    mode: CutMode,
    output_dir: Option<&Path>,
) -> Result<SplitResult, String> {
    let started = Instant::now();
    let in_p = input_path.as_ref().to_path_buf();
    let meta = probe_clip(&in_p)?;
    if meta.duration_ms < 200 {
        return Err("clip is too short to split".to_string());
    }

    let mut sorted_pts: Vec<u64> = split_points_ms
        .iter()
        .copied()
        .filter(|&t| t > 100 && t + 100 < meta.duration_ms)
        .collect();
    sorted_pts.sort_unstable();
    sorted_pts.dedup();

    if sorted_pts.is_empty() {
        return Err(format!(
            "split timestamp must be between 100ms and {}ms",
            meta.duration_ms.saturating_sub(100)
        ));
    }

    let parent = output_dir
        .map(Path::to_path_buf)
        .or_else(|| in_p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&parent);
    let stem = in_p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");

    let mut boundaries = Vec::with_capacity(sorted_pts.len() + 2);
    boundaries.push(0u64);
    boundaries.extend(sorted_pts);
    boundaries.push(meta.duration_ms);

    let mut parts = Vec::with_capacity(boundaries.len() - 1);
    for idx in 0..(boundaries.len() - 1) {
        let seg_start = boundaries[idx];
        let seg_end = boundaries[idx + 1];
        let ext = in_p
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| if e.eq_ignore_ascii_case("mkv") { "mkv" } else { "mp4" })
            .unwrap_or("mp4");
        let part_path = parent.join(format!("{stem}_part{}.{ext}", idx + 1));
        let effective_mode = if mode == CutMode::KeyframeSnap {
            CutMode::KeyframeSnap
        } else {
            CutMode::FrameExact
        };
        let res = cut_clip(&in_p, seg_start, seg_end, effective_mode, Some(part_path))?;
        parts.push(res);
    }

    Ok(SplitResult {
        input_path: in_p,
        parts,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn remove_middle_segment(
    input_path: impl AsRef<Path>,
    remove_start_ms: u64,
    remove_end_ms: u64,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    priority::set_low_priority();
    let started = Instant::now();
    let in_p = input_path.as_ref().to_path_buf();
    let demuxed = demux_clip_full(&in_p)?;
    let meta = &demuxed.metadata;

    if remove_start_ms >= remove_end_ms || remove_end_ms >= meta.duration_ms {
        return Err("invalid middle segment bounds to remove".to_string());
    }

    let gap_removed_ms = remove_end_ms.saturating_sub(remove_start_ms);
    let (v_part1, _, _) =
        filter_video_slice_for_trim(&demuxed.video_packets, 0, remove_start_ms);
    let (v_part2_raw, actual_p2_start, _) =
        filter_video_slice_for_trim(&demuxed.video_packets, remove_end_ms, meta.duration_ms);

    let shift_ms = actual_p2_start.saturating_sub(remove_start_ms).max(gap_removed_ms);
    let mut joined_v = Vec::with_capacity(v_part1.len() + v_part2_raw.len());
    joined_v.extend(v_part1);
    let shift_90k = (shift_ms as i64).saturating_mul(90);
    for p in v_part2_raw {
        let shifted_pts = p.pts_ms.saturating_sub(shift_ms);
        let shifted_90k = if p.pts > p.pts_ms as i64 {
            p.pts.saturating_sub(shift_90k).max(0)
        } else {
            (shifted_pts as i64).saturating_mul(90)
        };
        joined_v.push(Arc::new(CompressedPacket {
            kind: PacketKind::Video,
            pts_ms: shifted_pts,
            pts: shifted_90k,
            dts: shifted_90k,
            duration: p.duration,
            is_keyframe: p.is_keyframe,
            data: p.data.clone(),
            stems: None,
        }));
    }

    let keep_stems_cfg = AudioStemMixConfig {
        keep_stem_tracks_in_output: meta.has_separated_stems,
        ..AudioStemMixConfig::default()
    };
    let (a_part1, keep_stems_1) =
        build_remixed_audio_packets(&demuxed, 0, remove_start_ms, Some(&keep_stems_cfg), false);
    let (a_part2_raw, _) = build_remixed_audio_packets(
        &demuxed,
        actual_p2_start,
        meta.duration_ms,
        Some(&keep_stems_cfg),
        false,
    );

    let shift_48k = (shift_ms as i64).saturating_mul(48);
    let mut joined_a = Vec::with_capacity(a_part1.len() + a_part2_raw.len());
    joined_a.extend(a_part1);
    for p in a_part2_raw {
        let shifted_pts = p.pts_ms.saturating_sub(shift_ms);
        let shifted_48k = if p.pts > p.pts_ms as i64 {
            p.pts.saturating_sub(shift_48k).max(0)
        } else {
            (shifted_pts as i64).saturating_mul(48)
        };
        joined_a.push(Arc::new(CompressedPacket {
            kind: PacketKind::Audio,
            pts_ms: shifted_pts,
            pts: shifted_48k,
            dts: shifted_48k,
            duration: p.duration,
            is_keyframe: true,
            data: p.data.clone(),
            stems: p.stems.clone(),
        }));
    }

    let out_p = output_path.unwrap_or_else(|| default_edited_output_path(&in_p, "spliced"));
    let info = CodecContextInfo {
        width: meta.width,
        height: meta.height,
        fps: meta.fps,
        sps: demuxed.sps,
        pps: demuxed.pps,
    };
    let mut cfg = RecorderConfig::default();
    cfg.separate_audio_tracks = keep_stems_1;
    EncoderPipeline::mux_clip(&joined_v, &joined_a, &out_p, &cfg, &info)?;

    let out_bytes = std::fs::metadata(&out_p).map(|m| m.len()).unwrap_or(0);
    Ok(EditResult {
        input_path: in_p,
        output_path: out_p,
        input_bytes: meta.file_size_bytes,
        output_bytes: out_bytes,
        duration_ms: meta.duration_ms.saturating_sub(shift_ms),
        width: meta.width,
        height: meta.height,
        fps: meta.fps,
        audio_tracks_written: if joined_a.is_empty() { 0 } else { 1 },
        backend: "NativeSplice (Zero-Reencode)".to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn splice_kept_segments(
    input_path: impl AsRef<Path>,
    segments_ms: &[(u64, u64)],
    template_job: &EditJob,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    priority::set_low_priority();
    let started = Instant::now();
    let in_p = input_path.as_ref().to_path_buf();
    let valid_segs: Vec<(u64, u64)> = segments_ms
        .iter()
        .copied()
        .filter(|&(s, e)| e > s.saturating_add(80))
        .collect();
    if valid_segs.is_empty() {
        return Err("No active video segments left to save (all parts were deleted).".to_string());
    }
    let out_p = output_path
        .or_else(|| template_job.output_path.clone())
        .unwrap_or_else(|| default_edited_output_path(&in_p, "edited"));

    if valid_segs.len() == 1 {
        let mut single_job = template_job.clone();
        single_job.input_path = in_p;
        single_job.output_path = Some(out_p);
        single_job.trim_start_ms = Some(valid_segs[0].0);
        single_job.trim_end_ms = Some(valid_segs[0].1);
        return run_edit_job(single_job);
    }

    let demuxed = demux_clip_full(&in_p)?;
    let meta = &demuxed.metadata;
    let mut joined_v: Vec<Arc<CompressedPacket>> = Vec::new();
    let mut joined_a: Vec<Arc<CompressedPacket>> = Vec::new();
    let mut running_timeline_ms = 0u64;
    let mut keep_stems_any = false;

    for &(seg_start, seg_end) in &valid_segs {
        let (v_slice, actual_start, actual_end) =
            filter_video_slice_for_trim(&demuxed.video_packets, seg_start, seg_end);
        if v_slice.is_empty() {
            continue;
        }
        let seg_span_ms = actual_end.saturating_sub(actual_start).max(16);
        for p in v_slice {
            let rel_ms = p.pts_ms.saturating_sub(actual_start);
            let new_pts_ms = running_timeline_ms.saturating_add(rel_ms);
            let new_90k = (new_pts_ms as i64).saturating_mul(90);
            joined_v.push(Arc::new(CompressedPacket {
                kind: PacketKind::Video,
                pts_ms: new_pts_ms,
                pts: new_90k,
                dts: new_90k,
                duration: p.duration,
                is_keyframe: p.is_keyframe,
                data: p.data.clone(),
                stems: None,
            }));
        }

        let (a_slice, ks) = build_remixed_audio_packets(
            &demuxed,
            actual_start,
            actual_end,
            template_job.audio_mix.as_ref(),
            template_job.mute_all_audio,
        );
        if ks {
            keep_stems_any = true;
        }
        for p in a_slice {
            let rel_ms = p.pts_ms.saturating_sub(actual_start);
            let new_pts_ms = running_timeline_ms.saturating_add(rel_ms);
            let new_48k = (new_pts_ms as i64).saturating_mul(48);
            joined_a.push(Arc::new(CompressedPacket {
                kind: PacketKind::Audio,
                pts_ms: new_pts_ms,
                pts: new_48k,
                dts: new_48k,
                duration: p.duration,
                is_keyframe: true,
                data: p.data.clone(),
                stems: p.stems.clone(),
            }));
        }
        running_timeline_ms = running_timeline_ms.saturating_add(seg_span_ms);
    }

    if joined_v.is_empty() {
        return Err("No readable video frames found in the selected segments.".to_string());
    }

    let needs_post_reencode = template_job.rescale.is_some()
        || template_job.target_fps.is_some()
        || template_job.video_bitrate_kbps.is_some();

    let mux_target = if needs_post_reencode {
        out_p.with_extension("splice_stage_tmp.mp4")
    } else {
        out_p.clone()
    };

    let info = CodecContextInfo {
        width: meta.width,
        height: meta.height,
        fps: meta.fps,
        sps: demuxed.sps,
        pps: demuxed.pps,
    };
    let mut cfg = RecorderConfig::default();
    cfg.separate_audio_tracks = keep_stems_any;
    EncoderPipeline::mux_clip(&joined_v, &joined_a, &mux_target, &cfg, &info)?;

    if needs_post_reencode {
        let mut post_job = template_job.clone();
        post_job.input_path = mux_target.clone();
        post_job.output_path = Some(out_p);
        post_job.trim_start_ms = None;
        post_job.trim_end_ms = None;
        let res = run_edit_job(post_job);
        let _ = std::fs::remove_file(&mux_target);
        return res;
    }

    let out_bytes = std::fs::metadata(&out_p).map(|m| m.len()).unwrap_or(0);
    Ok(EditResult {
        input_path: in_p,
        output_path: out_p,
        input_bytes: meta.file_size_bytes,
        output_bytes: out_bytes,
        duration_ms: running_timeline_ms,
        width: meta.width,
        height: meta.height,
        fps: meta.fps,
        audio_tracks_written: if joined_a.is_empty() { 0 } else if keep_stems_any { 5 } else { 1 },
        backend: "NativeMultiSplice (Zero-Reencode)".to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn rescale_clip(
    input_path: impl AsRef<Path>,
    preset: ResolutionPreset,
    target_fps: Option<u32>,
    video_bitrate_kbps: Option<u32>,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    let in_p = input_path.as_ref().to_path_buf();
    let meta = probe_clip(&in_p)?;
    let (dst_w, dst_h) = preset.resolve_dimensions(meta.width, meta.height);
    let suffix = format!("{dst_w}x{dst_h}");
    let out_p = output_path.or_else(|| Some(default_edited_output_path(&in_p, &suffix)));

    run_edit_job(EditJob {
        input_path: in_p,
        output_path: out_p,
        rescale: Some((dst_w, dst_h)),
        target_fps,
        video_bitrate_kbps,
        prefer_hardware: true,
        ..EditJob::default()
    })
}

pub fn remix_clip_audio(
    input_path: impl AsRef<Path>,
    mix_config: AudioStemMixConfig,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    let in_p = input_path.as_ref().to_path_buf();
    let suffix = if mix_config.is_all_muted() {
        "muted"
    } else if let Some((role, _)) = mix_config.single_solo_role() {
        role.short_suffix()
    } else if !mix_config.discord_enabled {
        "no_discord"
    } else if !mix_config.mic_enabled {
        "no_mic"
    } else if !mix_config.other_enabled {
        "no_other"
    } else {
        "remixed"
    };
    let out_p = output_path.or_else(|| Some(default_edited_output_path(&in_p, suffix)));

    run_edit_job(EditJob {
        input_path: in_p,
        output_path: out_p,
        audio_mix: Some(mix_config),
        prefer_hardware: true,
        ..EditJob::default()
    })
}

pub fn isolate_single_stem(
    input_path: impl AsRef<Path>,
    role: AudioTrackRole,
    output_path: Option<PathBuf>,
) -> Result<EditResult, String> {
    let mix = match role {
        AudioTrackRole::MasterMix => AudioStemMixConfig::default(),
        AudioTrackRole::Game => AudioStemMixConfig::game_only(),
        AudioTrackRole::Discord => AudioStemMixConfig::discord_only(),
        AudioTrackRole::Microphone => AudioStemMixConfig::mic_only(),
        AudioTrackRole::OtherApps => AudioStemMixConfig::other_only(),
    };
    remix_clip_audio(input_path, mix, output_path)
}

