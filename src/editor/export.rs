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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EditJob {
    pub input_path: PathBuf,
    pub output_path: Option<PathBuf>,
    pub trim_start_ms: Option<u64>,
    pub trim_end_ms: Option<u64>,
    pub cut_mode: CutMode,
    pub rescale: Option<(u32, u32)>,
    pub target_fps: Option<u32>,
    pub video_bitrate_kbps: Option<u32>,
    pub audio_mix: Option<AudioStemMixConfig>,
    pub mute_all_audio: bool,
    pub prefer_hardware: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditResult {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio_tracks_written: usize,
    pub backend: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SplitResult {
    pub input_path: PathBuf,
    pub parts: Vec<EditResult>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportedStemFile {
    pub role: AudioTrackRole,
    pub label: String,
    pub m4a_path: PathBuf,
    pub aac_path: PathBuf,
    pub bytes: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StemExportResult {
    pub input_path: PathBuf,
    pub stems: Vec<ExportedStemFile>,
    pub elapsed_ms: u64,
}


pub(crate) fn crc32_compute(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

pub(crate) fn write_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(payload);
    let crc = crc32_compute(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

pub fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let row_bytes = (width as usize) * 4;
    if rgba.len() < row_bytes * (height as usize) {
        return Err("insufficient RGBA buffer for PNG encoding".to_string());
    }
    let mut raw_scanlines = Vec::with_capacity((row_bytes + 1) * (height as usize));
    for y in 0..(height as usize) {
        raw_scanlines.push(0u8);
        let s = y * row_bytes;
        raw_scanlines.extend_from_slice(&rgba[s..s + row_bytes]);
    }

    let mut zlib_stream = Vec::with_capacity(raw_scanlines.len() + (raw_scanlines.len() / 65535 + 2) * 5 + 8);
    zlib_stream.push(0x78);
    zlib_stream.push(0x01);

    let total_len = raw_scanlines.len();
    let mut offset = 0usize;
    let mut s1 = 1u32;
    let mut s2 = 0u32;

    while offset < total_len {
        let chunk_len = (total_len - offset).min(65535);
        let is_last = offset + chunk_len >= total_len;
        zlib_stream.push(if is_last { 0x01 } else { 0x00 });
        let len_u16 = chunk_len as u16;
        let nlen_u16 = !len_u16;
        zlib_stream.extend_from_slice(&len_u16.to_le_bytes());
        zlib_stream.extend_from_slice(&nlen_u16.to_le_bytes());
        let slice = &raw_scanlines[offset..offset + chunk_len];
        zlib_stream.extend_from_slice(slice);
        for &b in slice {
            s1 = (s1 + (b as u32)) % 65521;
            s2 = (s2 + s1) % 65521;
        }
        offset += chunk_len;
    }
    let adler = (s2 << 16) | s1;
    zlib_stream.extend_from_slice(&adler.to_be_bytes());

    let mut png = Vec::with_capacity(zlib_stream.len() + 128);
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;
    ihdr[10] = 0;
    ihdr[11] = 0;
    ihdr[12] = 0;
    write_png_chunk(&mut png, b"IHDR", &ihdr);
    write_png_chunk(&mut png, b"IDAT", &zlib_stream);
    write_png_chunk(&mut png, b"IEND", &[]);
    Ok(png)
}

pub fn save_frame_screenshot_png(
    input_path: impl AsRef<Path>,
    target_ms: u64,
    output_path: Option<PathBuf>,
) -> Result<PathBuf, String> {
    let in_p = input_path.as_ref();
    let meta = probe_clip(in_p)?;
    let w = meta.width.max(320);
    let h = meta.height.max(180);
    let rgba = extract_frame_rgba(in_p, target_ms, w, h)?;
    let png_bytes = encode_rgba_to_png(w, h, &rgba)?;

    let out_p = output_path.unwrap_or_else(|| {
        let parent = in_p.parent().unwrap_or_else(|| Path::new("."));
        let stem = in_p.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
        let secs = target_ms / 1000;
        let ms_rem = target_ms % 1000;
        parent.join(format!("{stem}_shot_{secs}s{ms_rem:03}.png"))
    });
    std::fs::write(&out_p, png_bytes)
        .map_err(|e| format!("failed to write screenshot PNG: {e}"))?;
    let _ = copy_file_to_clipboard(&out_p);
    Ok(out_p)
}

pub fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("File does not exist on disk".to_string());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        extern "system" {
            fn OpenClipboard(h_wnd_new_owner: *mut std::ffi::c_void) -> i32;
            fn EmptyClipboard() -> i32;
            fn SetClipboardData(u_format: u32, h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
            fn CloseClipboard() -> i32;
            fn GlobalAlloc(u_flags: u32, dw_bytes: usize) -> *mut std::ffi::c_void;
            fn GlobalLock(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
            fn GlobalUnlock(h_mem: *mut std::ffi::c_void) -> i32;
        }
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let clean_str = canonical.to_string_lossy();
        let norm_str = clean_str.strip_prefix(r"\?").unwrap_or(&clean_str);
        let wide: Vec<u16> = std::ffi::OsStr::new(norm_str)
            .encode_wide()
            .chain(std::iter::once(0u16))
            .chain(std::iter::once(0u16))
            .collect();
        let dropfiles_header_size = 20usize;
        let total_bytes = dropfiles_header_size + wide.len() * 2;
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return Err("Could not open Windows Clipboard".to_string());
            }
            let _ = EmptyClipboard();
            let h_glob = GlobalAlloc(0x0042, total_bytes);
            if h_glob.is_null() {
                let _ = CloseClipboard();
                return Err("GlobalAlloc failed for clipboard".to_string());
            }
            let ptr = GlobalLock(h_glob) as *mut u8;
            if ptr.is_null() {
                let _ = CloseClipboard();
                return Err("GlobalLock failed for clipboard".to_string());
            }
            std::ptr::write_bytes(ptr, 0, total_bytes);
            *(ptr as *mut u32) = 20u32;
            *(ptr.add(16) as *mut i32) = 1i32;
            std::ptr::copy_nonoverlapping(
                wide.as_ptr() as *const u8,
                ptr.add(dropfiles_header_size),
                wide.len() * 2,
            );
            let _ = GlobalUnlock(h_glob);
            let set_res = SetClipboardData(15u32, h_glob);
            let _ = CloseClipboard();
            if set_res.is_null() {
                return Err("SetClipboardData failed".to_string());
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("Clipboard file copy requires Windows".to_string())
    }
}

pub fn extract_frame_rgba(
    input_path: impl AsRef<Path>,
    target_ms: u64,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>, String> {
    let path = input_path.as_ref();
    let dst_w = dst_w.max(32);
    let dst_h = dst_h.max(18);
    let expected_len = (dst_w as usize) * (dst_h as usize) * 4;

    if let Some(ffmpeg_bin) = locate_ffmpeg_binary() {
        let mut cmd = Command::new(ffmpeg_bin);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-ss")
            .arg(format!("{:.3}", target_ms as f64 / 1000.0))
            .arg("-i")
            .arg(path)
            .arg("-frames:v")
            .arg("1")
            .arg("-vf")
            .arg(format!("scale={dst_w}:{dst_h}:flags=fast_bilinear"))
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("rgba")
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000 | 0x00004000);
        }
        if let Ok(out) = cmd.output() {
            if out.status.success() && out.stdout.len() >= expected_len {
                let mut buf = out.stdout;
                buf.truncate(expected_len);
                return Ok(buf);
            }
        }
    }

    let demuxed = demux_clip_full(path)?;
    if demuxed.video_packets.is_empty() {
        return Err("clip contains no video packets".to_string());
    }

    let mut start_idx = 0usize;
    for (idx, pkt) in demuxed.video_packets.iter().enumerate() {
        if pkt.is_keyframe && pkt.pts_ms <= target_ms {
            start_idx = idx;
        }
        if pkt.pts_ms > target_ms {
            break;
        }
    }

    let mut annex_b_buf = Vec::with_capacity(256 * 1024);
    let mut rgba_out = vec![0u8; expected_len];
    let mut decoded_any = false;
    let max_decode_idx = (start_idx + 90).min(demuxed.video_packets.len());

    if let Ok(mut sw_dec) = OpenH264Decoder::new() {
        for pkt in &demuxed.video_packets[start_idx..max_decode_idx] {
            avcc_to_annex_b(
                &pkt.data,
                pkt.is_keyframe,
                &demuxed.sps,
                &demuxed.pps,
                &mut annex_b_buf,
            );
            if annex_b_buf.is_empty() {
                continue;
            }
            if let Ok(Some(yuv)) = sw_dec.decode(&annex_b_buf) {
                if pkt.pts_ms >= target_ms || !decoded_any {
                    convert_yuv_to_rgba_scaled(&yuv, dst_w, dst_h, &mut rgba_out);
                }
                decoded_any = true;
                if pkt.pts_ms >= target_ms {
                    break;
                }
            }
        }
    }

    #[cfg(windows)]
    if !decoded_any {
        if let Ok(mut mft_dec) = crate::recorder::capture::MftH264Decoder::new(
            demuxed.metadata.width,
            demuxed.metadata.height,
            &demuxed.sps,
            &demuxed.pps,
        ) {
            for pkt in &demuxed.video_packets[start_idx..max_decode_idx] {
                avcc_to_annex_b(
                    &pkt.data,
                    pkt.is_keyframe,
                    &demuxed.sps,
                    &demuxed.pps,
                    &mut annex_b_buf,
                );
                if annex_b_buf.is_empty() {
                    continue;
                }
                let _ = mft_dec.push_annex_b(&annex_b_buf, (pkt.pts_ms as i64) * 10_000);
                while mft_dec.pull_nv12_frame(|nv12, dec_w, dec_h| {
                    if pkt.pts_ms >= target_ms || !decoded_any {
                        let _ = convert_nv12_to_rgba_scaled(
                            nv12,
                            dec_w,
                            dec_h,
                            demuxed.metadata.width,
                            demuxed.metadata.height,
                            dst_w,
                            dst_h,
                            &mut rgba_out,
                        );
                    }
                    decoded_any = true;
                }) {}
                if decoded_any && pkt.pts_ms >= target_ms {
                    break;
                }
            }
            if !decoded_any {
                mft_dec.drain();
                while mft_dec.pull_nv12_frame(|nv12, dec_w, dec_h| {
                    if !decoded_any {
                        let _ = convert_nv12_to_rgba_scaled(
                            nv12,
                            dec_w,
                            dec_h,
                            demuxed.metadata.width,
                            demuxed.metadata.height,
                            dst_w,
                            dst_h,
                            &mut rgba_out,
                        );
                        decoded_any = true;
                    }
                }) {}
            }
        }
    }

    if decoded_any {
        Ok(rgba_out)
    } else {
        Err("failed to decode video frame at playhead".to_string())
    }
}


pub(crate) fn build_remixed_audio_packets(
    demuxed: &DemuxedClip,
    start_ms: u64,
    end_ms: u64,
    mix_opt: Option<&AudioStemMixConfig>,
    mute_all: bool,
) -> (Vec<Arc<CompressedPacket>>, bool) {
    if mute_all || demuxed.audio_tracks_packets.is_empty() {
        return (Vec::new(), false);
    }
    if let Some(mix) = mix_opt {
        if mix.is_all_muted() {
            return (Vec::new(), false);
        }
    }

    let has_stems = demuxed.audio_tracks_packets.len() >= 5;
    let keep_stems = mix_opt
        .map(|m| m.keep_stem_tracks_in_output && has_stems)
        .unwrap_or(false);

    if has_stems {
        if let Some(mix) = mix_opt {
            if let Some((solo_role, _gain)) = mix.single_solo_role() {
                let track_idx = match solo_role {
                    AudioTrackRole::MasterMix => 0,
                    AudioTrackRole::Game => 1,
                    AudioTrackRole::Discord => 2,
                    AudioTrackRole::Microphone => 3,
                    AudioTrackRole::OtherApps => 4,
                };
                if let Some((_, ref track_pkts)) = demuxed.audio_tracks_packets.get(track_idx) {
                    let master_pkts = &demuxed.audio_tracks_packets[0].1;
                    let mut out = Vec::new();
                    for (idx, p) in track_pkts.iter().enumerate() {
                        if p.pts_ms.saturating_add(22) < start_ms {
                            continue;
                        }
                        if p.pts_ms > end_ms.saturating_add(40) {
                            break;
                        }
                        let stems_clone = if keep_stems {
                            master_pkts.get(idx).and_then(|mp| mp.stems.clone())
                        } else {
                            None
                        };
                        out.push(Arc::new(CompressedPacket {
                            kind: PacketKind::Audio,
                            pts_ms: p.pts_ms,
                            pts: p.pts,
                            dts: p.dts,
                            duration: p.duration,
                            is_keyframe: true,
                            data: p.data.clone(),
                            stems: stems_clone,
                        }));
                    }
                    return (out, keep_stems);
                }
            }

            if !mix.is_unmodified_full_mix() {
                #[cfg(windows)]
                {
                    let master_pkts = &demuxed.audio_tracks_packets[0].1;
                    let game_pcm = if mix.game_enabled && mix.game_gain > 0.001 {
                        decode_aac_packets_via_mft(&demuxed.audio_tracks_packets[1].1)
                    } else {
                        Vec::new()
                    };
                    let discord_pcm = if mix.discord_enabled && mix.discord_gain > 0.001 {
                        decode_aac_packets_via_mft(&demuxed.audio_tracks_packets[2].1)
                    } else {
                        Vec::new()
                    };
                    let mic_pcm = if mix.mic_enabled && mix.mic_gain > 0.001 {
                        decode_aac_packets_via_mft(&demuxed.audio_tracks_packets[3].1)
                    } else {
                        Vec::new()
                    };
                    let other_pcm = if mix.other_enabled && mix.other_gain > 0.001 {
                        decode_aac_packets_via_mft(&demuxed.audio_tracks_packets[4].1)
                    } else {
                        Vec::new()
                    };
                    let max_samples = game_pcm
                        .len()
                        .max(discord_pcm.len())
                        .max(mic_pcm.len())
                        .max(other_pcm.len());
                    if max_samples >= 1024 {
                        if let Some(mut enc) = crate::audio::build_aac_encoder(128) {
                            let silent_pkt = build_silent_aac_packet(128);
                            let mut pcm_chunk = vec![0i16; 2048];
                            let mut aac_buf = vec![0u8; 2048];
                            let mut out = Vec::new();
                            for (pkt_idx, p) in master_pkts.iter().enumerate() {
                                if p.pts_ms.saturating_add(22) < start_ms {
                                    continue;
                                }
                                if p.pts_ms > end_ms.saturating_add(40) {
                                    break;
                                }
                                let base_sample = pkt_idx * 1024;
                                for s_idx in 0..1024 {
                                    let idx = base_sample + s_idx;
                                    let (gl, gr) = game_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    let (dl, dr) = discord_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    let (ml, mr) = mic_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    let (ol, or_) = other_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    let l = (gl * mix.game_gain
                                        + dl * mix.discord_gain
                                        + ml * mix.mic_gain
                                        + ol * mix.other_gain)
                                        .clamp(-1.0, 1.0);
                                    let r = (gr * mix.game_gain
                                        + dr * mix.discord_gain
                                        + mr * mix.mic_gain
                                        + or_ * mix.other_gain)
                                        .clamp(-1.0, 1.0);
                                    pcm_chunk[s_idx * 2] = (l * 32767.0) as i16;
                                    pcm_chunk[s_idx * 2 + 1] = (r * 32767.0) as i16;
                                }
                                let encoded_bytes = enc
                                    .encode(&pcm_chunk, &mut aac_buf)
                                    .ok()
                                    .filter(|r| r.out_bytes() > 0)
                                    .map(|r| Bytes::copy_from_slice(&aac_buf[..r.out_bytes()]))
                                    .unwrap_or_else(|| silent_pkt.clone());
                                let new_stems = if keep_stems {
                                    p.stems.as_ref().map(|st| {
                                        Arc::new(AudioStemPackets {
                                            game: if mix.game_enabled { st.game.clone() } else { silent_pkt.clone() },
                                            discord: if mix.discord_enabled { st.discord.clone() } else { silent_pkt.clone() },
                                            mic: if mix.mic_enabled { st.mic.clone() } else { silent_pkt.clone() },
                                            other: if mix.other_enabled { st.other.clone() } else { silent_pkt.clone() },
                                        })
                                    })
                                } else {
                                    None
                                };
                                out.push(Arc::new(CompressedPacket {
                                    kind: PacketKind::Audio,
                                    pts_ms: p.pts_ms,
                                    pts: p.pts,
                                    dts: p.dts,
                                    duration: p.duration,
                                    is_keyframe: true,
                                    data: encoded_bytes,
                                    stems: new_stems,
                                }));
                            }
                            if !out.is_empty() {
                                return (out, keep_stems);
                            }
                        }
                    }
                }
                let master_pkts = &demuxed.audio_tracks_packets[0].1;
                let silent_pkt = build_silent_aac_packet(128);
                let mut out = Vec::new();

                for p in master_pkts {
                    if p.pts_ms.saturating_add(22) < start_ms {
                        continue;
                    }
                    if p.pts_ms > end_ms.saturating_add(40) {
                        break;
                    }
                    let chosen_data = if let Some(ref st) = p.stems {
                        let g_on = mix.game_enabled
                            && mix.game_gain > 0.001
                            && st.game.as_ref() != silent_pkt.as_ref();
                        let d_on = mix.discord_enabled
                            && mix.discord_gain > 0.001
                            && st.discord.as_ref() != silent_pkt.as_ref();
                        let m_on = mix.mic_enabled
                            && mix.mic_gain > 0.001
                            && st.mic.as_ref() != silent_pkt.as_ref();
                        let o_on = mix.other_enabled
                            && mix.other_gain > 0.001
                            && st.other.as_ref() != silent_pkt.as_ref();

                        let muted_discord_was_active = !mix.discord_enabled
                            && st.discord.as_ref() != silent_pkt.as_ref();
                        let muted_mic_was_active =
                            !mix.mic_enabled && st.mic.as_ref() != silent_pkt.as_ref();
                        let muted_other_was_active =
                            !mix.other_enabled && st.other.as_ref() != silent_pkt.as_ref();
                        let muted_game_was_active =
                            !mix.game_enabled && st.game.as_ref() != silent_pkt.as_ref();

                        if !g_on && !d_on && !m_on && !o_on {
                            silent_pkt.clone()
                        } else if !muted_discord_was_active
                            && !muted_mic_was_active
                            && !muted_other_was_active
                            && !muted_game_was_active
                        {
                            p.data.clone()
                        } else if g_on && !d_on && !m_on && !o_on {
                            st.game.clone()
                        } else if d_on && !g_on && !m_on && !o_on {
                            st.discord.clone()
                        } else if m_on && !g_on && !d_on && !o_on {
                            st.mic.clone()
                        } else if o_on && !g_on && !d_on && !m_on {
                            st.other.clone()
                        } else if g_on {
                            st.game.clone()
                        } else if d_on {
                            st.discord.clone()
                        } else if m_on {
                            st.mic.clone()
                        } else {
                            st.other.clone()
                        }
                    } else {
                        p.data.clone()
                    };

                    let new_stems = if keep_stems {
                        p.stems.as_ref().map(|st| {
                            Arc::new(AudioStemPackets {
                                game: if mix.game_enabled {
                                    st.game.clone()
                                } else {
                                    silent_pkt.clone()
                                },
                                discord: if mix.discord_enabled {
                                    st.discord.clone()
                                } else {
                                    silent_pkt.clone()
                                },
                                mic: if mix.mic_enabled {
                                    st.mic.clone()
                                } else {
                                    silent_pkt.clone()
                                },
                                other: if mix.other_enabled {
                                    st.other.clone()
                                } else {
                                    silent_pkt.clone()
                                },
                            })
                        })
                    } else {
                        None
                    };

                    out.push(Arc::new(CompressedPacket {
                        kind: PacketKind::Audio,
                        pts_ms: p.pts_ms,
                        pts: p.pts,
                        dts: p.dts,
                        duration: p.duration,
                        is_keyframe: true,
                        data: chosen_data,
                        stems: new_stems,
                    }));
                }
                return (out, keep_stems);
            }
        }
    }

    let master_pkts = &demuxed.audio_tracks_packets[0].1;
    let mut out = Vec::new();
    for p in master_pkts {
        if p.pts_ms.saturating_add(22) < start_ms {
            continue;
        }
        if p.pts_ms > end_ms.saturating_add(40) {
            break;
        }
        out.push(Arc::new(CompressedPacket {
            kind: PacketKind::Audio,
            pts_ms: p.pts_ms,
            pts: p.pts,
            dts: p.dts,
            duration: p.duration,
            is_keyframe: true,
            data: p.data.clone(),
            stems: if keep_stems { p.stems.clone() } else { None },
        }));
    }
    (out, keep_stems)
}

pub(crate) fn locate_ffmpeg_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("ffmpeg.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
            let candidate_bin = dir.join("bin").join("ffmpeg.exe");
            if candidate_bin.is_file() {
                return Some(candidate_bin);
            }
        }
    }
    let local = PathBuf::from("ffmpeg.exe");
    if local.is_file() {
        return Some(local);
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000 | 0x00004000);
    }
    if cmd.status().map(|s| s.success()).unwrap_or(false) {
        return Some(PathBuf::from("ffmpeg"));
    }
    None
}

pub(crate) fn try_ffmpeg_edit_job(
    ffmpeg_bin: &Path,
    job: &EditJob,
    meta: &ClipMetadata,
    dst_w: u32,
    dst_h: u32,
    dst_fps: u32,
    video_kbps: u32,
    temp_output: &Path,
) -> Result<String, String> {
    let start_ms = job.trim_start_ms.unwrap_or(0);
    let trim_off_keyframe = start_ms > 0
        && !meta
            .keyframe_timestamps_ms
            .iter()
            .any(|&kf| kf.abs_diff(start_ms) <= 150);

    let need_reencode_video = job.cut_mode == CutMode::FrameExact
        || (job.cut_mode == CutMode::FastKeyframeCopy && trim_off_keyframe)
        || dst_w != meta.width
        || dst_h != meta.height
        || dst_fps != meta.fps
        || job.video_bitrate_kbps.is_some();

    let encoders: Vec<&str> = if !need_reencode_video {
        vec!["copy"]
    } else if job.prefer_hardware {
        vec!["h264_nvenc", "h264_amf", "h264_qsv", "libx264"]
    } else {
        vec!["libx264"]
    };

    for enc_name in encoders {
        let _ = std::fs::remove_file(temp_output);
        let mut cmd = Command::new(ffmpeg_bin);
        cmd.arg("-y").arg("-hide_banner").arg("-loglevel").arg("error");

        if let Some(s_ms) = job.trim_start_ms {
            if s_ms > 0 {
                cmd.arg("-ss").arg(format!("{:.3}", s_ms as f64 / 1000.0));
            }
        }
        if let Some(e_ms) = job.trim_end_ms {
            cmd.arg("-to").arg(format!("{:.3}", e_ms as f64 / 1000.0));
        }

        cmd.arg("-i").arg(&job.input_path);

        if enc_name == "copy" {
            cmd.arg("-map").arg("0:v:0").arg("-c:v").arg("copy");
        } else {
            let vf = if dst_w < dst_h && meta.width > meta.height {
                format!("crop=ih*{dst_w}/{dst_h}:ih,scale={dst_w}:{dst_h}:flags=lanczos,fps={dst_fps}")
            } else {
                format!("scale={dst_w}:{dst_h}:flags=lanczos,fps={dst_fps}")
            };
            cmd.arg("-map")
                .arg("0:v:0")
                .arg("-vf")
                .arg(vf)
                .arg("-c:v")
                .arg(enc_name)
                .arg("-b:v")
                .arg(format!("{video_kbps}k"))
                .arg("-pix_fmt")
                .arg("yuv420p");
        }

        if job.mute_all_audio
            || job
                .audio_mix
                .as_ref()
                .map(|m| m.is_all_muted())
                .unwrap_or(false)
        {
            cmd.arg("-an");
        } else if let Some(ref mix) = job.audio_mix {
            if meta.has_separated_stems && !mix.is_unmodified_full_mix() {
                if let Some((solo_role, gain)) = mix.single_solo_role() {
                    let a_idx = match solo_role {
                        AudioTrackRole::MasterMix => 0,
                        AudioTrackRole::Game => 1,
                        AudioTrackRole::Discord => 2,
                        AudioTrackRole::Microphone => 3,
                        AudioTrackRole::OtherApps => 4,
                    };
                    if (gain - 1.0).abs() < 0.01 {
                        cmd.arg("-map")
                            .arg(format!("0:a:{a_idx}"))
                            .arg("-c:a")
                            .arg("copy");
                    } else {
                        cmd.arg("-map")
                            .arg(format!("0:a:{a_idx}"))
                            .arg("-af")
                            .arg(format!("volume={gain:.3}"))
                            .arg("-c:a")
                            .arg("aac")
                            .arg("-b:a")
                            .arg("128k");
                    }
                } else {
                    let mut filter_parts = Vec::new();
                    let mut mix_labels = Vec::new();
                    let stem_cfgs = [
                        (1usize, mix.game_enabled, mix.game_gain, "g"),
                        (2usize, mix.discord_enabled, mix.discord_gain, "d"),
                        (3usize, mix.mic_enabled, mix.mic_gain, "m"),
                        (4usize, mix.other_enabled, mix.other_gain, "o"),
                    ];
                    for &(idx, enabled, gain, tag) in &stem_cfgs {
                        if enabled && gain > 0.001 {
                            filter_parts.push(format!("[0:a:{idx}]volume={gain:.3}[{tag}]"));
                            mix_labels.push(format!("[{tag}]"));
                        }
                    }
                    if !mix_labels.is_empty() {
                        let inputs_str = mix_labels.join("");
                        let n_in = mix_labels.len();
                        let fc = format!(
                            "{};{inputs_str}amix=inputs={n_in}:duration=longest:normalize=0[aout]",
                            filter_parts.join(";")
                        );
                        cmd.arg("-filter_complex")
                            .arg(fc)
                            .arg("-map")
                            .arg("[aout]")
                            .arg("-c:a")
                            .arg("aac")
                            .arg("-b:a")
                            .arg("128k");
                    } else {
                        cmd.arg("-an");
                    }
                }
            } else if mix.keep_stem_tracks_in_output && meta.has_separated_stems {
                cmd.arg("-map").arg("0:a?").arg("-c:a").arg("copy");
            } else {
                cmd.arg("-map").arg("0:a:0?").arg("-c:a").arg("copy");
            }
        } else {
            cmd.arg("-map").arg("0:a?").arg("-c:a").arg("copy");
        }

        cmd.arg("-movflags").arg("+faststart").arg(temp_output);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000 | 0x00004000);
        }

        if let Ok(st) = cmd.status() {
            if st.success() {
                if let Ok(m) = std::fs::metadata(temp_output) {
                    if m.len() > 512 {
                        return Ok(format!("FFmpeg ({enc_name})"));
                    }
                }
            }
        }
    }

    Err("FFmpeg edit pipeline not available or failed".to_string())
}

pub(crate) fn avcc_to_annex_b(
    sample_bytes: &[u8],
    is_sync: bool,
    sps: &[u8],
    pps: &[u8],
    out: &mut Vec<u8>,
) {
    out.clear();
    if is_sync && !sps.is_empty() && !pps.is_empty() {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(sps);
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(pps);
    }

    if sample_bytes.len() >= 4
        && ((sample_bytes[0] == 0
            && sample_bytes[1] == 0
            && sample_bytes[2] == 0
            && sample_bytes[3] == 1)
            || (sample_bytes[0] == 0 && sample_bytes[1] == 0 && sample_bytes[2] == 1))
    {
        out.extend_from_slice(sample_bytes);
        return;
    }

    let mut offset = 0usize;
    while offset + 4 <= sample_bytes.len() {
        let nal_len = u32::from_be_bytes([
            sample_bytes[offset],
            sample_bytes[offset + 1],
            sample_bytes[offset + 2],
            sample_bytes[offset + 3],
        ]) as usize;
        offset += 4;
        if nal_len == 0 || offset + nal_len > sample_bytes.len() {
            break;
        }
        let nal_type = sample_bytes[offset] & 0x1F;
        if nal_type != 9 {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&sample_bytes[offset..offset + nal_len]);
        }
        offset += nal_len;
    }
}

pub(crate) fn i420_to_scaled_nv12(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    src_h: usize,
    y_stride: usize,
    u_stride: usize,
    v_stride: usize,
    dst_w: usize,
    dst_h: usize,
    x_map_y: &[usize],
    x_map_uv: &[usize],
    dst_nv12: &mut [u8],
) {
    for y in 0..dst_h {
        let sy = (y * src_h) / dst_h.max(1);
        let s_row = sy * y_stride;
        let d_row = y * dst_w;
        for (dx, &sx) in x_map_y.iter().enumerate() {
            if s_row + sx < y_plane.len() {
                dst_nv12[d_row + dx] = y_plane[s_row + sx];
            }
        }
    }

    let dst_uv_base = dst_w * dst_h;
    let src_uv_h = (src_h / 2).max(1);
    let dst_uv_h = (dst_h / 2).max(1);
    for y in 0..dst_uv_h {
        let sy = (y * src_uv_h) / dst_uv_h;
        let u_row = sy * u_stride;
        let v_row = sy * v_stride;
        let d_row = dst_uv_base + y * dst_w;
        for (uv_idx, &sx_pair) in x_map_uv.iter().enumerate() {
            let su_x = sx_pair / 2;
            let dx = uv_idx * 2;
            if u_row + su_x < u_plane.len() && v_row + su_x < v_plane.len() {
                dst_nv12[d_row + dx] = u_plane[u_row + su_x];
                dst_nv12[d_row + dx + 1] = v_plane[v_row + su_x];
            }
        }
    }
}

#[cfg(windows)]
pub(crate) fn reencode_video_native_mft(
    video_packets: &[Arc<CompressedPacket>],
    src_w: u32,
    _src_h: u32,
    sps: &[u8],
    pps: &[u8],
    start_ms: u64,
    end_ms: u64,
    dst_w: u32,
    dst_h: u32,
    dst_fps: u32,
    video_bitrate_bps: u32,
) -> Result<(Vec<Arc<CompressedPacket>>, CodecContextInfo, String), String> {
    let dur_span_ms = end_ms.saturating_sub(start_ms).max(1000);
    let out_ring = RingBuffer::new_with_capacities(
        Duration::from_millis(dur_span_ms + 10_000),
        video_packets.len().max(128),
        64,
    );
    let out_codec_info: Arc<Mutex<Option<CodecContextInfo>>> = Arc::new(Mutex::new(None));

    let mut encoder = crate::recorder::capture::HardwareH264Encoder::new_standalone(
        dst_w,
        dst_h,
        dst_fps,
        video_bitrate_bps,
        (dst_fps * 2).max(30),
        start_ms,
        &out_codec_info,
    )?;

    let backend_label = format!("NativeMFT ({:?})", encoder.backend_kind);
    let (crop_x, crop_w) = if dst_w < dst_h && src_w > _src_h {
        let cw = ((_src_h as u64 * dst_w as u64) / dst_h.max(1) as u64) as usize;
        let cw_even = (cw.clamp(64, src_w as usize)) & !1;
        let cx = ((src_w as usize).saturating_sub(cw_even) / 2) & !1;
        (cx, cw_even.max(2))
    } else {
        (0usize, src_w.max(2) as usize)
    };
    let x_map_y: Vec<usize> = (0..dst_w as usize)
        .map(|dx| crop_x + (dx * crop_w) / dst_w.max(1) as usize)
        .collect();
    let x_map_uv: Vec<usize> = (0..(dst_w as usize / 2))
        .map(|uv_dx| crop_x + ((uv_dx * (crop_w / 2)) / (dst_w.max(2) as usize / 2)) * 2)
        .collect();

    let mut dst_nv12 = vec![0u8; ((dst_w * dst_h * 3) / 2) as usize];
    let mut annex_b_buf = Vec::with_capacity(256 * 1024);
    let mut avcc_scratch = Vec::with_capacity(256 * 1024);

    let min_step_ms = (1000 / dst_fps.max(1)).saturating_sub(2) as u64;
    let frame_dur_hns = (10_000_000 / dst_fps.max(1) as i64).max(1);
    let mut next_keep_ms = start_ms;
    let mut encoded_count = 0usize;

    let mut sw_dec = OpenH264Decoder::new()
        .map_err(|e| format!("OpenH264 decoder init failed: {e}"))?;

    for pkt in video_packets {
        if pkt.pts_ms > end_ms {
            break;
        }
        avcc_to_annex_b(&pkt.data, pkt.is_keyframe, sps, pps, &mut annex_b_buf);
        if annex_b_buf.is_empty() {
            continue;
        }
        if let Ok(Some(yuv)) = sw_dec.decode(&annex_b_buf) {
            if pkt.pts_ms >= start_ms && (pkt.pts_ms >= next_keep_ms || encoded_count == 0) {
                let (_, h) = yuv.dimensions();
                let (ys, us, vs) = yuv.strides();
                i420_to_scaled_nv12(
                    yuv.y(),
                    yuv.u(),
                    yuv.v(),
                    h,
                    ys,
                    us,
                    vs,
                    dst_w as usize,
                    dst_h as usize,
                    &x_map_y,
                    &x_map_uv,
                    &mut dst_nv12,
                );
                let rel_hns = (pkt.pts_ms.saturating_sub(start_ms) as i64) * 10_000;
                let force_idr = encoded_count == 0;
                let _ = encoder.encode_nv12_bytes(
                    &dst_nv12,
                    pkt.pts_ms,
                    rel_hns,
                    frame_dur_hns,
                    force_idr,
                    &mut avcc_scratch,
                    &out_ring,
                    &out_codec_info,
                );
                encoded_count += 1;
                next_keep_ms = pkt.pts_ms.saturating_add(min_step_ms);
            }
        }
    }

    encoder.drain_output_packets(end_ms, &mut avcc_scratch, &out_ring, &out_codec_info);
    let (v_pkts, _) = out_ring.snapshot_since(Some(0));
    if v_pkts.is_empty() {
        return Err("native video re-encode produced 0 packets".to_string());
    }
    let info = out_codec_info
        .lock()
        .map_err(|_| "codec_info lock poisoned".to_string())?
        .clone()
        .ok_or_else(|| "missing SPS/PPS headers after native re-encode".to_string())?;

    Ok((v_pkts, info, backend_label))
}

#[cfg(not(windows))]
pub(crate) fn reencode_video_native_mft(
    _video_packets: &[Arc<CompressedPacket>],
    _src_w: u32,
    _src_h: u32,
    _sps: &[u8],
    _pps: &[u8],
    _start_ms: u64,
    _end_ms: u64,
    _dst_w: u32,
    _dst_h: u32,
    _dst_fps: u32,
    _video_bitrate_bps: u32,
) -> Result<(Vec<Arc<CompressedPacket>>, CodecContextInfo, String), String> {
    Err("native MFT video re-encoding requires Windows".to_string())
}

pub fn run_edit_job(job: EditJob) -> Result<EditResult, String> {
    priority::set_low_priority();
    let started = Instant::now();

    let demuxed = demux_clip_full(&job.input_path)?;
    let meta = &demuxed.metadata;
    let start_ms = job.trim_start_ms.unwrap_or(0);
    let end_ms = job
        .trim_end_ms
        .unwrap_or(meta.duration_ms.max(1))
        .max(start_ms.saturating_add(100));

    let (dst_w, dst_h) = job
        .rescale
        .map(|(w, h)| ((w.clamp(64, 7680)) & !1, (h.clamp(64, 4320)) & !1))
        .unwrap_or((meta.width, meta.height));
    let dst_fps = job.target_fps.unwrap_or(meta.fps).clamp(15, 240);
    let default_v_kbps = if dst_h <= 480 {
        2500
    } else if dst_h <= 720 {
        5000
    } else if dst_h <= 1080 {
        9000
    } else {
        14000
    };
    let video_kbps = job.video_bitrate_kbps.unwrap_or(default_v_kbps).max(250);

    let out_path = job
        .output_path
        .clone()
        .unwrap_or_else(|| default_edited_output_path(&job.input_path, "edited"));

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    let trim_off_keyframe = start_ms > 0
        && !meta
            .keyframe_timestamps_ms
            .iter()
            .any(|&kf| kf.abs_diff(start_ms) <= 150);

    let need_reencode_video = job.cut_mode == CutMode::FrameExact
        || (job.cut_mode == CutMode::FastKeyframeCopy && trim_off_keyframe)
        || dst_w != meta.width
        || dst_h != meta.height
        || dst_fps != meta.fps
        || job.video_bitrate_kbps.is_some();

    let need_custom_gain_mix = job
        .audio_mix
        .as_ref()
        .map(|m| {
            !m.is_unmodified_full_mix()
                && !m.is_all_muted()
                && m.single_solo_role()
                    .map(|(_, g)| (g - 1.0).abs() > 0.01)
                    .unwrap_or(true)
        })
        .unwrap_or(false);

    if need_reencode_video || need_custom_gain_mix {
        if let Some(ffmpeg_bin) = locate_ffmpeg_binary() {
            let temp_out = out_path.with_extension("edit_ff_tmp.mp4");
            if let Ok(backend_name) = try_ffmpeg_edit_job(
                &ffmpeg_bin,
                &job,
                meta,
                dst_w,
                dst_h,
                dst_fps,
                video_kbps,
                &temp_out,
            ) {
                let out_bytes = std::fs::metadata(&temp_out).map(|m| m.len()).unwrap_or(0);
                if out_path.exists() {
                    let _ = std::fs::remove_file(&out_path);
                }
                std::fs::rename(&temp_out, &out_path)
                    .map_err(|e| format!("failed to finalize edited clip: {e}"))?;
                let out_dur = end_ms.saturating_sub(start_ms);
                return Ok(EditResult {
                    input_path: job.input_path,
                    output_path: out_path,
                    input_bytes: meta.file_size_bytes,
                    output_bytes: out_bytes,
                    duration_ms: out_dur,
                    width: dst_w,
                    height: dst_h,
                    fps: dst_fps,
                    audio_tracks_written: if job.mute_all_audio { 0 } else { 1 },
                    backend: backend_name,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
            }
            let _ = std::fs::remove_file(&temp_out);
        }
    }

    let (v_slice, actual_start_ms, actual_end_ms) =
        filter_video_slice_for_trim(&demuxed.video_packets, start_ms, end_ms);

    if v_slice.is_empty() {
        return Err("no video frames found inside the requested trim window".to_string());
    }

    let (final_v_pkts, final_info, backend_label) = if need_reencode_video {
        reencode_video_native_mft(
            &v_slice,
            meta.width,
            meta.height,
            &demuxed.sps,
            &demuxed.pps,
            start_ms,
            end_ms,
            dst_w,
            dst_h,
            dst_fps,
            video_kbps * 1000,
        )?
    } else {
        let info = CodecContextInfo {
            width: meta.width,
            height: meta.height,
            fps: meta.fps,
            sps: demuxed.sps.clone(),
            pps: demuxed.pps.clone(),
        };
        (
            v_slice,
            info,
            "NativeStreamCopy (Zero-Reencode)".to_string(),
        )
    };

    let a_win_start = if need_reencode_video {
        start_ms
    } else {
        actual_start_ms
    };
    let a_win_end = if need_reencode_video {
        end_ms
    } else {
        actual_end_ms
    };

    let (final_a_pkts, keep_stems_in_mux) = build_remixed_audio_packets(
        &demuxed,
        a_win_start,
        a_win_end,
        job.audio_mix.as_ref(),
        job.mute_all_audio,
    );

    let mut mux_cfg = RecorderConfig::default();
    mux_cfg.separate_audio_tracks = keep_stems_in_mux;

    EncoderPipeline::mux_clip(&final_v_pkts, &final_a_pkts, &out_path, &mux_cfg, &final_info)?;

    let output_bytes = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    let out_dur = a_win_end.saturating_sub(a_win_start);
    let tracks_written = if final_a_pkts.is_empty() {
        0
    } else if keep_stems_in_mux {
        5
    } else {
        1
    };

    Ok(EditResult {
        input_path: job.input_path,
        output_path: out_path,
        input_bytes: meta.file_size_bytes,
        output_bytes,
        duration_ms: out_dur,
        width: final_info.width,
        height: final_info.height,
        fps: final_info.fps,
        audio_tracks_written: tracks_written,
        backend: backend_label,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}


pub(crate) fn build_adts_header(aac_frame_len: usize) -> [u8; 7] {
    let full_len = (aac_frame_len + 7) & 0x1FFF;
    let profile_aac_lc = 1u8;
    let freq_idx_48k = 3u8;
    let chan_stereo = 2u8;
    let b0 = 0xFF;
    let b1 = 0xF1;
    let b2 = (profile_aac_lc << 6) | (freq_idx_48k << 2) | (chan_stereo >> 2);
    let b3 = ((chan_stereo & 3) << 6) | ((full_len >> 11) as u8 & 0x03);
    let b4 = ((full_len >> 3) & 0xFF) as u8;
    let b5 = (((full_len & 0x07) as u8) << 5) | 0x1F;
    let b6 = 0xFC;
    [b0, b1, b2, b3, b4, b5, b6]
}

pub(crate) fn write_single_audio_m4a(
    packets: &[Arc<CompressedPacket>],
    output_path: &Path,
) -> Result<u64, String> {
    let file = File::create(output_path)
        .map_err(|e| format!("failed to create audio file '{}': {e}", output_path.display()))?;
    let buf_writer = BufWriter::with_capacity(512 * 1024, file);

    let brand_m4a = "M4A ".parse().map_err(|e| format!("{e}"))?;
    let brand_isom = "isom".parse().map_err(|e| format!("{e}"))?;
    let brand_iso2 = "iso2".parse().map_err(|e| format!("{e}"))?;
    let brand_mp41 = "mp41".parse().map_err(|e| format!("{e}"))?;

    let mp4_cfg = Mp4Config {
        major_brand: brand_isom,
        minor_version: 512,
        compatible_brands: vec![brand_m4a, brand_isom, brand_iso2, brand_mp41],
        timescale: 48_000,
    };

    let mut writer = Mp4Writer::write_start(buf_writer, &mp4_cfg)
        .map_err(|e| format!("failed to start m4a writer: {e}"))?;

    let aac_conf = AacConfig {
        bitrate: 128_000,
        profile: AudioObjectType::AacLowComplexity,
        freq_index: SampleFreqIndex::Freq48000,
        chan_conf: ChannelConfig::Stereo,
    };
    let mut track_conf = TrackConfig::from(aac_conf);
    track_conf.timescale = 48_000;
    writer
        .add_track(&track_conf)
        .map_err(|e| format!("failed to add m4a track: {e}"))?;

    let mut cur_ticks = 0u64;
    for p in packets {
        let sample = Mp4Sample {
            start_time: cur_ticks,
            duration: 1024,
            rendering_offset: 0,
            is_sync: true,
            bytes: p.data.clone(),
        };
        writer
            .write_sample(1, &sample)
            .map_err(|e| format!("failed to write m4a sample: {e}"))?;
        cur_ticks += 1024;
    }

    writer
        .write_end()
        .map_err(|e| format!("failed to finalize m4a: {e}"))?;
    let mut bw = writer.into_writer();
    let _ = bw.flush();
    drop(bw);

    let sz = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    Ok(sz)
}

pub(crate) fn write_single_audio_adts_aac(
    packets: &[Arc<CompressedPacket>],
    output_path: &Path,
) -> Result<u64, String> {
    let file = File::create(output_path)
        .map_err(|e| format!("failed to create AAC file '{}': {e}", output_path.display()))?;
    let mut bw = BufWriter::with_capacity(512 * 1024, file);
    for p in packets {
        if p.data.is_empty() {
            continue;
        }
        let hdr = build_adts_header(p.data.len());
        bw.write_all(&hdr)
            .map_err(|e| format!("failed to write ADTS header: {e}"))?;
        bw.write_all(&p.data)
            .map_err(|e| format!("failed to write AAC frame: {e}"))?;
    }
    bw.flush()
        .map_err(|e| format!("failed to flush AAC file: {e}"))?;
    drop(bw);
    let sz = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    Ok(sz)
}

pub fn export_separated_stems(
    input_path: impl AsRef<Path>,
    output_dir: Option<&Path>,
) -> Result<StemExportResult, String> {
    priority::set_low_priority();
    let started = Instant::now();
    let in_p = input_path.as_ref().to_path_buf();
    let demuxed = demux_clip_full(&in_p)?;

    if demuxed.audio_tracks_packets.is_empty() {
        return Err("clip contains no audio tracks to export".to_string());
    }

    let parent = output_dir
        .map(Path::to_path_buf)
        .or_else(|| in_p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&parent);

    let file_stem = in_p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");

    let mut exported = Vec::with_capacity(demuxed.audio_tracks_packets.len());
    for (meta, pkts) in &demuxed.audio_tracks_packets {
        if pkts.is_empty() {
            continue;
        }
        let suffix = meta.role.short_suffix();
        let m4a_path = parent.join(format!("{file_stem}_{suffix}.m4a"));
        let aac_path = parent.join(format!("{file_stem}_{suffix}.aac"));

        let m4a_bytes = write_single_audio_m4a(pkts, &m4a_path)?;
        let _ = write_single_audio_adts_aac(pkts, &aac_path)?;

        exported.push(ExportedStemFile {
            role: meta.role,
            label: meta.label.clone(),
            m4a_path,
            aac_path,
            bytes: m4a_bytes,
            duration_ms: meta.duration_ms,
        });
    }

    Ok(StemExportResult {
        input_path: in_p,
        stems: exported,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub async fn run_edit_job_async(
    job: EditJob,
    sink: Option<EventSink>,
) -> Result<EditResult, String> {
    let in_path_str = job.input_path.to_string_lossy().to_string();
    if let Some(ref s) = sink {
        s.emit(
            "editor.job_started",
            None,
            json!({
                "input_path": in_path_str,
                "trim_start_ms": job.trim_start_ms,
                "trim_end_ms": job.trim_end_ms,
                "rescale": job.rescale,
                "target_fps": job.target_fps,
            }),
        );
    }

    let res = tokio::task::spawn_blocking(move || run_edit_job(job)).await;
    match res {
        Ok(Ok(info)) => {
            if let Some(ref s) = sink {
                s.emit(
                    "editor.job_done",
                    None,
                    serde_json::to_value(&info).unwrap_or_else(|_| json!({})),
                );
            }
            Ok(info)
        }
        Ok(Err(e)) => {
            if let Some(ref s) = sink {
                s.emit(
                    "editor.error",
                    None,
                    json!({
                        "input_path": in_path_str,
                        "message": e,
                    }),
                );
            }
            Err(e)
        }
        Err(join_err) => {
            let msg = format!("editor worker panicked: {join_err}");
            if let Some(ref s) = sink {
                s.emit(
                    "editor.error",
                    None,
                    json!({
                        "input_path": in_path_str,
                        "message": msg,
                    }),
                );
            }
            Err(msg)
        }
    }
}

pub fn handle_editor_event(event: &Event, sink: &Option<EventSink>) {
    let d = &event.data;
    let Some(path_str) = d
        .get("path")
        .or_else(|| d.get("input_path"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        if let Some(ref s) = sink {
            s.emit(
                "editor.error",
                None,
                json!({ "message": "editor event missing 'path' field" }),
            );
        }
        return;
    };

    let input_path = PathBuf::from(path_str);
    let output_path = d
        .get("output_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let sink_clone = sink.clone();
    let kind = event.kind.clone();
    let data_clone = d.clone();

    tokio::spawn(async move {
        match kind.as_str() {
            "editor.probe" => {
                let res = tokio::task::spawn_blocking(move || probe_clip(input_path)).await;
                match res {
                    Ok(Ok(meta)) => {
                        if let Some(ref s) = sink_clone {
                            s.emit(
                                "editor.probe_result",
                                None,
                                serde_json::to_value(&meta).unwrap_or_else(|_| json!({})),
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        if let Some(ref s) = sink_clone {
                            s.emit("editor.error", None, json!({ "message": e }));
                        }
                    }
                    Err(e) => {
                        if let Some(ref s) = sink_clone {
                            s.emit("editor.error", None, json!({ "message": format!("{e}") }));
                        }
                    }
                }
            }
            "editor.cut" | "editor.trim" => {
                let start_ms = data_clone
                    .get("start_ms")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        data_clone
                            .get("start")
                            .and_then(Value::as_str)
                            .and_then(parse_timestamp_ms)
                    })
                    .unwrap_or(0);
                let end_ms = data_clone
                    .get("end_ms")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        data_clone
                            .get("end")
                            .and_then(Value::as_str)
                            .and_then(parse_timestamp_ms)
                    });
                let exact = data_clone
                    .get("exact")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let job = EditJob {
                    input_path,
                    output_path,
                    trim_start_ms: Some(start_ms),
                    trim_end_ms: end_ms,
                    cut_mode: if exact {
                        CutMode::FrameExact
                    } else {
                        CutMode::FastKeyframeCopy
                    },
                    prefer_hardware: true,
                    ..EditJob::default()
                };
                let _ = run_edit_job_async(job, sink_clone).await;
            }
            "editor.split" => {
                let split_ms = data_clone
                    .get("at_ms")
                    .or_else(|| data_clone.get("split_at_ms"))
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        data_clone
                            .get("at")
                            .and_then(Value::as_str)
                            .and_then(parse_timestamp_ms)
                    })
                    .unwrap_or(5000);
                let res = tokio::task::spawn_blocking(move || {
                    split_clip(
                        input_path,
                        split_ms,
                        CutMode::FastKeyframeCopy,
                        output_path.as_deref(),
                    )
                })
                .await;
                if let Some(ref s) = sink_clone {
                    match res {
                        Ok(Ok(info)) => s.emit(
                            "editor.split_done",
                            None,
                            serde_json::to_value(&info).unwrap_or_else(|_| json!({})),
                        ),
                        Ok(Err(e)) => s.emit("editor.error", None, json!({ "message": e })),
                        Err(e) => {
                            s.emit("editor.error", None, json!({ "message": format!("{e}") }))
                        }
                    }
                }
            }
            "editor.rescale" => {
                let preset = data_clone
                    .get("resolution")
                    .or_else(|| data_clone.get("preset"))
                    .and_then(Value::as_str)
                    .and_then(ResolutionPreset::parse_str)
                    .unwrap_or(ResolutionPreset::P720);
                let fps = data_clone
                    .get("fps")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                let bitrate = data_clone
                    .get("bitrate_kbps")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                let res = tokio::task::spawn_blocking(move || {
                    rescale_clip(input_path, preset, fps, bitrate, output_path)
                })
                .await;
                if let Some(ref s) = sink_clone {
                    match res {
                        Ok(Ok(info)) => s.emit(
                            "editor.job_done",
                            None,
                            serde_json::to_value(&info).unwrap_or_else(|_| json!({})),
                        ),
                        Ok(Err(e)) => s.emit("editor.error", None, json!({ "message": e })),
                        Err(e) => {
                            s.emit("editor.error", None, json!({ "message": format!("{e}") }))
                        }
                    }
                }
            }
            "editor.remix_audio" | "editor.separate_audio" => {
                let mix = AudioStemMixConfig {
                    game_enabled: data_clone
                        .get("game_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    discord_enabled: data_clone
                        .get("discord_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    mic_enabled: data_clone
                        .get("mic_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    other_enabled: data_clone
                        .get("other_enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    game_gain: data_clone
                        .get("game_gain")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0) as f32,
                    discord_gain: data_clone
                        .get("discord_gain")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0) as f32,
                    mic_gain: data_clone
                        .get("mic_gain")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0) as f32,
                    other_gain: data_clone
                        .get("other_gain")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0) as f32,
                    keep_stem_tracks_in_output: data_clone
                        .get("keep_stems")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                let job = EditJob {
                    input_path,
                    output_path,
                    audio_mix: Some(mix),
                    prefer_hardware: true,
                    ..EditJob::default()
                };
                let _ = run_edit_job_async(job, sink_clone).await;
            }
            "editor.export_stems" => {
                let res = tokio::task::spawn_blocking(move || {
                    export_separated_stems(input_path, output_path.as_deref())
                })
                .await;
                if let Some(ref s) = sink_clone {
                    match res {
                        Ok(Ok(info)) => s.emit(
                            "editor.stems_exported",
                            None,
                            serde_json::to_value(&info).unwrap_or_else(|_| json!({})),
                        ),
                        Ok(Err(e)) => s.emit("editor.error", None, json!({ "message": e })),
                        Err(e) => {
                            s.emit("editor.error", None, json!({ "message": format!("{e}") }))
                        }
                    }
                }
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::config::RecorderConfig;
    use crate::recorder::encoder::{CodecContextInfo, EncoderPipeline};

    fn create_sample_multi_stem_mp4(path: &Path) {
        let mut v_pkts = Vec::new();
        for i in 0..90u64 {
            let pts_ms = i * 16;
            let is_key = i % 30 == 0;
            let nal_type: u8 = if is_key { 0x65 } else { 0x41 };
            let payload = [nal_type, 0x88, 0x84, 0x21, 0xA0];
            let mut avcc = Vec::with_capacity(9);
            avcc.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            avcc.extend_from_slice(&payload);
            v_pkts.push(Arc::new(CompressedPacket {
                kind: PacketKind::Video,
                pts_ms,
                pts: pts_ms as i64,
                dts: pts_ms as i64,
                duration: 16,
                is_keyframe: is_key,
                data: Bytes::from(avcc),
                stems: None,
            }));
        }

        let mut a_pkts = Vec::new();
        let raw_aac = Bytes::from_static(&[0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C]);
        for i in 0..68u64 {
            let pts_ms = (i * 1024 * 1000) / 48_000;
            a_pkts.push(Arc::new(CompressedPacket {
                kind: PacketKind::Audio,
                pts_ms,
                pts: (i * 1024) as i64,
                dts: (i * 1024) as i64,
                duration: 1024,
                is_keyframe: true,
                data: raw_aac.clone(),
                stems: Some(Arc::new(AudioStemPackets {
                    game: raw_aac.clone(),
                    discord: raw_aac.clone(),
                    mic: raw_aac.clone(),
                    other: raw_aac.clone(),
                })),
            }));
        }

        let mut cfg = RecorderConfig::default();
        cfg.separate_audio_tracks = true;
        let info = CodecContextInfo {
            width: 1280,
            height: 720,
            fps: 60,
            sps: vec![
                0x67, 0x64, 0x00, 0x1F, 0xAC, 0xD9, 0x40, 0x50, 0x05, 0xBB, 0x01, 0x10, 0x00,
                0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xC0, 0xF1, 0x83, 0x19, 0x60,
            ],
            pps: vec![0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0],
        };
        let _ = EncoderPipeline::mux_clip(&v_pkts, &a_pkts, path, &cfg, &info);
    }

    #[test]
    fn test_probe_cut_split_and_stem_isolation() {
        let dir = std::env::temp_dir().join("clotch_editor_unit_test");
        let _ = std::fs::create_dir_all(&dir);
        let src_mp4 = dir.join("source_multi.mp4");
        create_sample_multi_stem_mp4(&src_mp4);

        let meta = probe_clip(&src_mp4).unwrap();
        assert!(meta.has_separated_stems);
        assert_eq!(meta.audio_tracks.len(), 5);
        assert_eq!(meta.audio_tracks[1].role, AudioTrackRole::Game);
        assert_eq!(meta.audio_tracks[2].role, AudioTrackRole::Discord);
        assert_eq!(meta.audio_tracks[3].role, AudioTrackRole::Microphone);
        assert_eq!(meta.audio_tracks[4].role, AudioTrackRole::OtherApps);

        let cut_out = dir.join("cut_clip.mp4");
        let cut_res = cut_clip(&src_mp4, 0, 800, CutMode::FastKeyframeCopy, Some(cut_out.clone())).unwrap();
        assert!(cut_out.is_file());
        assert!(cut_res.output_bytes > 100);

        let split_res = split_clip(&src_mp4, 600, CutMode::FastKeyframeCopy, Some(&dir)).unwrap();
        assert_eq!(split_res.parts.len(), 2);

        let game_only_out = dir.join("game_only.mp4");
        let iso_res = isolate_single_stem(&src_mp4, AudioTrackRole::Game, Some(game_only_out.clone())).unwrap();
        assert_eq!(iso_res.audio_tracks_written, 1);
        let iso_meta = probe_clip(&game_only_out).unwrap();
        assert_eq!(iso_meta.audio_tracks.len(), 1);

        let stems_res = export_separated_stems(&src_mp4, Some(&dir)).unwrap();
        assert_eq!(stems_res.stems.len(), 5);

        let _ = std::fs::remove_dir_all(&dir);
    }
}