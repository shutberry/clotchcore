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

pub fn parse_timestamp_ms(raw: &str) -> Option<u64> {
    let s = raw.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Some(ms_str) = s.strip_suffix("ms") {
        let val = ms_str.trim().parse::<f64>().ok()?;
        if val >= 0.0 {
            return Some(val.round() as u64);
        }
    }
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 2 {
            let mins = parts[0].trim().parse::<u64>().ok()?;
            let secs = parts[1].trim().parse::<f64>().ok()?;
            if secs >= 0.0 {
                return Some(mins * 60_000 + (secs * 1000.0).round() as u64);
            }
        } else if parts.len() == 3 {
            let hrs = parts[0].trim().parse::<u64>().ok()?;
            let mins = parts[1].trim().parse::<u64>().ok()?;
            let secs = parts[2].trim().parse::<f64>().ok()?;
            if secs >= 0.0 {
                return Some(hrs * 3_600_000 + mins * 60_000 + (secs * 1000.0).round() as u64);
            }
        }
    }
    let clean_sec = s
        .strip_suffix("secs")
        .or_else(|| s.strip_suffix("sec"))
        .or_else(|| s.strip_suffix('s'))
        .unwrap_or(&s)
        .trim();
    if let Ok(sec_f) = clean_sec.parse::<f64>() {
        if sec_f >= 0.0 {
            return Some((sec_f * 1000.0).round() as u64);
        }
    }
    parse_duration_secs(&s).map(|secs| secs * 1000)
}

pub fn default_edited_output_path(input_path: &Path, suffix: &str) -> PathBuf {
    let parent = input_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");
    let ext = input_path
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| if e.eq_ignore_ascii_case("mkv") { "mkv" } else { "mp4" })
        .unwrap_or("mp4");
    let clean_suffix = suffix.trim_matches('_').trim();
    let label = if clean_suffix.is_empty() || clean_suffix.eq_ignore_ascii_case("edited") || clean_suffix.eq_ignore_ascii_case("edit") {
        "edited".to_string()
    } else {
        clean_suffix.to_string()
    };
    let first = parent.join(format!("{stem} ({label}).{ext}"));
    if !first.exists() {
        return first;
    }
    for n in 2..=9999u32 {
        let candidate = parent.join(format!("{stem} ({label} {n}).{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    parent.join(format!("{stem} ({label} {ts}).{ext}"))
}

pub(crate) fn convert_nv12_to_rgba_scaled(
    nv12: &[u8],
    dec_w: u32,
    dec_h: u32,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    rgba_out: &mut [u8],
) -> bool {
    let stride = dec_w.max(src_w).max(2) as usize;
    let slice_h = dec_h.max(src_h).max(2) as usize;
    let y_plane_len = stride * slice_h;
    let uv_h = (src_h.max(2) as usize) / 2;
    let min_required = y_plane_len + stride * uv_h;
    if nv12.len() < min_required {
        return false;
    }
    let y_plane = &nv12[..y_plane_len];
    let uv_plane = &nv12[y_plane_len..];

    let sw = src_w.max(2) as usize;
    let sh = src_h.max(2) as usize;
    let dw = dst_w.max(1) as usize;
    let dh = dst_h.max(1) as usize;
    let sw_max = sw.saturating_sub(1);
    let sh_max = sh.saturating_sub(1);
    let uv_w_max = (sw / 2).saturating_sub(1);
    let uv_h_max = (sh / 2).saturating_sub(1);

    for dy in 0..dh {
        let sy_fp = (dy * sh * 256) / dh;
        let sy0 = (sy_fp >> 8).min(sh_max);
        let sy1 = (sy0 + 1).min(sh_max);
        let wy = (sy_fp & 0xFF) as i32;
        let inv_wy = 256 - wy;

        let sy_uv = (sy0 / 2).min(uv_h_max);
        let y_row0 = sy0 * stride;
        let y_row1 = sy1 * stride;
        let uv_row = sy_uv * stride;

        for dx in 0..dw {
            let sx_fp = (dx * sw * 256) / dw;
            let sx0 = (sx_fp >> 8).min(sw_max);
            let sx1 = (sx0 + 1).min(sw_max);
            let wx = (sx_fp & 0xFF) as i32;
            let inv_wx = 256 - wx;

            let y00 = y_plane.get(y_row0 + sx0).copied().unwrap_or(16) as i32;
            let y01 = y_plane.get(y_row0 + sx1).copied().unwrap_or(y00 as u8) as i32;
            let y10 = y_plane.get(y_row1 + sx0).copied().unwrap_or(y00 as u8) as i32;
            let y11 = y_plane.get(y_row1 + sx1).copied().unwrap_or(y00 as u8) as i32;
            let y_top = (y00 * inv_wx + y01 * wx) >> 8;
            let y_bot = (y10 * inv_wx + y11 * wx) >> 8;
            let y_val = (y_top * inv_wy + y_bot * wy) >> 8;

            let sx_uv = (sx0 / 2).min(uv_w_max) * 2;
            let u_val = uv_plane.get(uv_row + sx_uv).copied().unwrap_or(128) as i32;
            let v_val = uv_plane.get(uv_row + sx_uv + 1).copied().unwrap_or(128) as i32;

            let c = (y_val - 16).max(0);
            let d = u_val - 128;
            let e = v_val - 128;
            let r = ((298 * c + 459 * e + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 55 * d - 136 * e + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 541 * d + 128) >> 8).clamp(0, 255) as u8;
            let out_off = (dy * dw + dx) * 4;
            if out_off + 3 < rgba_out.len() {
                rgba_out[out_off] = r;
                rgba_out[out_off + 1] = g;
                rgba_out[out_off + 2] = b;
                rgba_out[out_off + 3] = 255;
            }
        }
    }
    true
}

pub(crate) fn convert_yuv_to_rgba_scaled(
    yuv: &openh264::decoder::DecodedYUV<'_>,
    dst_w: u32,
    dst_h: u32,
    rgba_out: &mut [u8],
) {
    let (src_w, src_h) = yuv.dimensions();
    let (ys, us, vs) = yuv.strides();
    let y_plane = yuv.y();
    let u_plane = yuv.u();
    let v_plane = yuv.v();
    let dw = dst_w.max(1) as usize;
    let dh = dst_h.max(1) as usize;
    let sw_max = src_w.saturating_sub(1);
    let sh_max = src_h.saturating_sub(1);
    let uv_w_max = (src_w / 2).saturating_sub(1);
    let uv_h_max = (src_h / 2).saturating_sub(1);

    for dy in 0..dh {
        let sy_fp = (dy * src_h * 256) / dh;
        let sy0 = (sy_fp >> 8).min(sh_max);
        let sy1 = (sy0 + 1).min(sh_max);
        let wy = (sy_fp & 0xFF) as i32;
        let inv_wy = 256 - wy;

        let sy_uv = (sy0 / 2).min(uv_h_max);
        let y_row0 = sy0 * ys;
        let y_row1 = sy1 * ys;
        let u_row = sy_uv * us;
        let v_row = sy_uv * vs;

        for dx in 0..dw {
            let sx_fp = (dx * src_w * 256) / dw;
            let sx0 = (sx_fp >> 8).min(sw_max);
            let sx1 = (sx0 + 1).min(sw_max);
            let wx = (sx_fp & 0xFF) as i32;
            let inv_wx = 256 - wx;

            let y00 = y_plane.get(y_row0 + sx0).copied().unwrap_or(16) as i32;
            let y01 = y_plane.get(y_row0 + sx1).copied().unwrap_or(y00 as u8) as i32;
            let y10 = y_plane.get(y_row1 + sx0).copied().unwrap_or(y00 as u8) as i32;
            let y11 = y_plane.get(y_row1 + sx1).copied().unwrap_or(y00 as u8) as i32;
            let y_top = (y00 * inv_wx + y01 * wx) >> 8;
            let y_bot = (y10 * inv_wx + y11 * wx) >> 8;
            let y_val = (y_top * inv_wy + y_bot * wy) >> 8;

            let sx_uv = (sx0 / 2).min(uv_w_max);
            let u_val = u_plane.get(u_row + sx_uv).copied().unwrap_or(128) as i32;
            let v_val = v_plane.get(v_row + sx_uv).copied().unwrap_or(128) as i32;

            let c = (y_val - 16).max(0);
            let d = u_val - 128;
            let e = v_val - 128;
            let r = ((298 * c + 459 * e + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 55 * d - 136 * e + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 541 * d + 128) >> 8).clamp(0, 255) as u8;
            let out_off = (dy * dw + dx) * 4;
            if out_off + 3 < rgba_out.len() {
                rgba_out[out_off] = r;
                rgba_out[out_off + 1] = g;
                rgba_out[out_off + 2] = b;
                rgba_out[out_off + 3] = 255;
            }
        }
    }
}

#[cfg(windows)]
pub(crate) struct PreviewAudioPlayer {
    active_mix: Arc<Mutex<AudioStemMixConfig>>,
    target_seek_ms: Arc<std::sync::atomic::AtomicU64>,
    playback_rate_x100: Arc<std::sync::atomic::AtomicU32>,
    playing_flag: Arc<std::sync::atomic::AtomicBool>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl PreviewAudioPlayer {
    fn spawn(
        input_path: PathBuf,
        audio_tracks: Vec<(AudioTrackMetadata, Vec<Arc<CompressedPacket>>)>,
    ) -> Option<Self> {
        let active_mix = Arc::new(Mutex::new(AudioStemMixConfig::default()));
        let target_seek_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let playback_rate_x100 = Arc::new(std::sync::atomic::AtomicU32::new(100));
        let playing_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mix_c = active_mix.clone();
        let seek_c = target_seek_ms.clone();
        let rate_c = playback_rate_x100.clone();
        let play_c = playing_flag.clone();
        let stop_c = stop_flag.clone();

        let handle = std::thread::Builder::new()
            .name("clotch-preview-audio".into())
            .spawn(move || {
                run_preview_audio_thread(
                    input_path,
                    audio_tracks,
                    mix_c,
                    seek_c,
                    rate_c,
                    play_c,
                    stop_c,
                );
            })
            .ok();

        Some(Self {
            active_mix,
            target_seek_ms,
            playback_rate_x100,
            playing_flag,
            stop_flag,
            thread_handle: handle,
        })
    }

    fn notify_video_playhead(&self, target_ms: u64) {
        let cur = self.target_seek_ms.load(std::sync::atomic::Ordering::Acquire);
        if (cur & (1u64 << 62)) == 0 {
            self.target_seek_ms
                .store(target_ms, std::sync::atomic::Ordering::Release);
        }
    }

    fn seek_to_ms(&self, target_ms: u64) {
        self.target_seek_ms
            .store(target_ms | (1u64 << 62), std::sync::atomic::Ordering::Release);
    }

    fn set_mix(&self, mix: AudioStemMixConfig) {
        if let Ok(mut g) = self.active_mix.lock() {
            *g = mix;
        }
    }

    fn set_playback_rate(&self, rate: f32) {
        let r = (rate.clamp(0.25, 2.0) * 100.0).round() as u32;
        self.playback_rate_x100
            .store(r, std::sync::atomic::Ordering::Release);
    }

    fn set_playing(&self, playing: bool, position_ms: Option<u64>) {
        if let Some(ms) = position_ms {
            self.target_seek_ms
                .store(ms | (1u64 << 62), std::sync::atomic::Ordering::Release);
        }
        self.playing_flag
            .store(playing, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(windows)]
impl Drop for PreviewAudioPlayer {
    fn drop(&mut self) {
        self.stop_flag
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(h) = self.thread_handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(windows)]
pub(crate) fn try_decode_aac_mft_mode(
    packets: &[Arc<CompressedPacket>],
    use_adts: bool,
) -> Vec<(f32, f32)> {
    use std::mem::ManuallyDrop;
    use windows::core::GUID;
    use windows::Win32::Media::MediaFoundation::{
        IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType,
        MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio, MFT_MESSAGE_COMMAND_DRAIN,
        MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
        MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
        MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
        MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
        MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_USER_DATA,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    const CLSID_CMS_AAC_DEC_MFT: GUID = GUID::from_u128(0x32d186a7_218f_4c75_8876_dd77273a8999);
    const MF_AUDIO_FORMAT_AAC_GUID: GUID = GUID::from_u128(0x00001610_0000_0010_8000_00aa00389b71);
    const MEDIASUBTYPE_RAW_AAC1_GUID: GUID = GUID::from_u128(0x000000ff_0000_0010_8000_00aa00389b71);
    const MF_AUDIO_FORMAT_FLOAT_GUID: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    const MF_AUDIO_FORMAT_PCM_GUID: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
    const MF_MT_AAC_PAYLOAD_TYPE_GUID: GUID = GUID::from_u128(0xbfbab799_c9df_4c10_8935_07228f43a8ef);

    let _ = crate::recorder::ensure_mf_initialized();
    let mut pcm_out = Vec::with_capacity(packets.len() * 1024);

    unsafe {
        let Ok(mft) =
            CoCreateInstance::<_, IMFTransform>(&CLSID_CMS_AAC_DEC_MFT, None, CLSCTX_INPROC_SERVER)
        else {
            return pcm_out;
        };
        let Ok(in_type) = MFCreateMediaType() else {
            return pcm_out;
        };
        let _ = in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
        let _ = in_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000);
        let _ = in_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2);

        if use_adts {
            let _ = in_type.SetGUID(&MF_MT_SUBTYPE, &MF_AUDIO_FORMAT_AAC_GUID);
            let _ = in_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE_GUID, 1);
            let user_data: [u8; 14] = [0x01, 0x00, 0xFE, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x90];
            let _ = in_type.SetBlob(&MF_MT_USER_DATA, &user_data);
            if mft.SetInputType(0, &in_type, 0).is_err() {
                return pcm_out;
            }
        } else {
            let _ = in_type.SetGUID(&MF_MT_SUBTYPE, &MEDIASUBTYPE_RAW_AAC1_GUID);
            let asc: [u8; 2] = [0x11, 0x90];
            let _ = in_type.SetBlob(&MF_MT_USER_DATA, &asc);
            if mft.SetInputType(0, &in_type, 0).is_err() {
                let _ = in_type.SetGUID(&MF_MT_SUBTYPE, &MF_AUDIO_FORMAT_AAC_GUID);
                let _ = in_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE_GUID, 0);
                let user_data: [u8; 14] = [0x00, 0x00, 0xFE, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x90];
                let _ = in_type.SetBlob(&MF_MT_USER_DATA, &user_data);
                if mft.SetInputType(0, &in_type, 0).is_err() {
                    return pcm_out;
                }
            }
        }

        let select_out_type = |mft_ref: &IMFTransform| -> Option<(bool, usize)> {
            for desired_sub in [MF_AUDIO_FORMAT_FLOAT_GUID, MF_AUDIO_FORMAT_PCM_GUID] {
                for idx in 0..32 {
                    let Ok(avail) = mft_ref.GetOutputAvailableType(0, idx) else {
                        break;
                    };
                    if avail.GetGUID(&MF_MT_SUBTYPE) == Ok(desired_sub) {
                        let ch = avail.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2).max(1) as usize;
                        if mft_ref.SetOutputType(0, &avail, 0).is_ok() {
                            return Some((desired_sub == MF_AUDIO_FORMAT_PCM_GUID, ch));
                        }
                    }
                }
            }
            if let Ok(out_type) = MFCreateMediaType() {
                let _ = out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
                let _ = out_type.SetGUID(&MF_MT_SUBTYPE, &MF_AUDIO_FORMAT_FLOAT_GUID);
                let _ = out_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000);
                let _ = out_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2);
                let _ = out_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32);
                let _ = out_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 8);
                if mft_ref.SetOutputType(0, &out_type, 0).is_ok() {
                    return Some((false, 2));
                }
            }
            None
        };

        let Some((mut is_pcm16, mut out_channels)) = select_out_type(&mft) else {
            return pcm_out;
        };

        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

        let Ok(out_sample) = MFCreateSample() else {
            return pcm_out;
        };
        let Ok(out_buf) = MFCreateMemoryBuffer(65536) else {
            return pcm_out;
        };
        let _ = out_sample.AddBuffer(&out_buf);

        let drain_mft_output = |mft_ref: &IMFTransform,
                                is_pcm16_flag: &mut bool,
                                channels_cnt: &mut usize,
                                pcm_dst: &mut Vec<(f32, f32)>| {
            loop {
                let _ = out_buf.SetCurrentLength(0);
                let _ = out_sample.RemoveAllBuffers();
                let _ = out_sample.AddBuffer(&out_buf);
                let mut out_data = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(out_sample.clone())),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let hr = mft_ref.ProcessOutput(0, &mut out_data, &mut status);
                let _ = ManuallyDrop::take(&mut out_data[0].pEvents);
                let prod = ManuallyDrop::take(&mut out_data[0].pSample);
                match hr {
                    Ok(()) => {
                        if let Some(s) = prod {
                            if let Ok(mb) = s.ConvertToContiguousBuffer() {
                                let mut rptr: *mut u8 = std::ptr::null_mut();
                                let mut clen = 0u32;
                                if mb.Lock(&mut rptr, None, Some(&mut clen)).is_ok() {
                                    let ch = (*channels_cnt).max(1);
                                    if !rptr.is_null() && clen > 0 {
                                        if *is_pcm16_flag {
                                            let i_slice = std::slice::from_raw_parts(
                                                rptr as *const i16,
                                                (clen as usize) / 2,
                                            );
                                            for frame in i_slice.chunks_exact(ch) {
                                                let l = (frame[0] as f32 / 32768.0).clamp(-1.0, 1.0);
                                                let r = if ch >= 2 {
                                                    (frame[1] as f32 / 32768.0).clamp(-1.0, 1.0)
                                                } else {
                                                    l
                                                };
                                                pcm_dst.push((l, r));
                                            }
                                        } else {
                                            let f_slice = std::slice::from_raw_parts(
                                                rptr as *const f32,
                                                (clen as usize) / 4,
                                            );
                                            for frame in f_slice.chunks_exact(ch) {
                                                let l = if frame[0].is_finite() {
                                                    frame[0].clamp(-1.0, 1.0)
                                                } else {
                                                    0.0
                                                };
                                                let r = if ch >= 2 {
                                                    if frame[1].is_finite() {
                                                        frame[1].clamp(-1.0, 1.0)
                                                    } else {
                                                        0.0
                                                    }
                                                } else {
                                                    l
                                                };
                                                pcm_dst.push((l, r));
                                            }
                                        }
                                    }
                                    let _ = mb.Unlock();
                                }
                            }
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        if let Some((new_pcm16, new_ch)) = select_out_type(mft_ref) {
                            *is_pcm16_flag = new_pcm16;
                            *channels_cnt = new_ch;
                            continue;
                        }
                        break;
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(_) => break,
                }
            }
        };

        for (pkt_idx, pkt) in packets.iter().enumerate() {
            if pkt.data.is_empty() {
                continue;
            }
            let has_adts = pkt.data.len() > 7 && pkt.data[0] == 0xFF && (pkt.data[1] & 0xF0) == 0xF0;
            let payload_len = if use_adts && !has_adts {
                pkt.data.len() + 7
            } else {
                pkt.data.len()
            };
            let Ok(in_s) = MFCreateSample() else {
                continue;
            };
            let Ok(in_b) = MFCreateMemoryBuffer(payload_len as u32) else {
                continue;
            };
            let mut ptr: *mut u8 = std::ptr::null_mut();
            if in_b.Lock(&mut ptr, None, None).is_ok() {
                if !ptr.is_null() {
                    if use_adts && !has_adts {
                        let hdr = build_adts_header(pkt.data.len());
                        std::ptr::copy_nonoverlapping(hdr.as_ptr(), ptr, 7);
                        std::ptr::copy_nonoverlapping(pkt.data.as_ptr(), ptr.add(7), pkt.data.len());
                    } else {
                        std::ptr::copy_nonoverlapping(pkt.data.as_ptr(), ptr, pkt.data.len());
                    }
                    let _ = in_b.SetCurrentLength(payload_len as u32);
                }
                let _ = in_b.Unlock();
            }
            let _ = in_s.AddBuffer(&in_b);
            let sample_hns = ((pkt_idx as i64) * 1024 * 10_000_000) / 48_000;
            let _ = in_s.SetSampleTime(sample_hns);
            let _ = in_s.SetSampleDuration(213_333);
            let _ = mft.ProcessInput(0, &in_s, 0);

            drain_mft_output(&mft, &mut is_pcm16, &mut out_channels, &mut pcm_out);
        }

        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        drain_mft_output(&mft, &mut is_pcm16, &mut out_channels, &mut pcm_out);
    }
    pcm_out
}

#[cfg(windows)]
pub(crate) fn decode_aac_packets_via_mft(packets: &[Arc<CompressedPacket>]) -> Vec<(f32, f32)> {
    if packets.is_empty() {
        return Vec::new();
    }
    let pcm_adts = try_decode_aac_mft_mode(packets, true);
    if !pcm_adts.is_empty() {
        return pcm_adts;
    }
    try_decode_aac_mft_mode(packets, false)
}

pub(crate) fn decode_audio_via_ffmpeg_fallback(input_path: &Path, track_index: usize) -> Vec<(f32, f32)> {
    let Some(ffmpeg_bin) = locate_ffmpeg_binary() else {
        return Vec::new();
    };
    let mut cmd = Command::new(ffmpeg_bin);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(input_path)
        .arg("-map")
        .arg(format!("0:a:{track_index}?"))
        .arg("-vn")
        .arg("-f")
        .arg("f32le")
        .arg("-ac")
        .arg("2")
        .arg("-ar")
        .arg("48000")
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
        if out.status.success() && out.stdout.len() >= 8 {
            let mut res = Vec::with_capacity(out.stdout.len() / 8);
            for chunk in out.stdout.chunks_exact(8) {
                let l = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let r = f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                res.push((
                    if l.is_finite() { l.clamp(-1.0, 1.0) } else { 0.0 },
                    if r.is_finite() { r.clamp(-1.0, 1.0) } else { 0.0 },
                ));
            }
            return res;
        }
    }
    Vec::new()
}

#[cfg(windows)]
pub(crate) fn run_preview_audio_thread(
    input_path: PathBuf,
    audio_tracks: Vec<(AudioTrackMetadata, Vec<Arc<CompressedPacket>>)>,
    active_mix: Arc<Mutex<AudioStemMixConfig>>,
    target_seek_ms: Arc<std::sync::atomic::AtomicU64>,
    playback_rate_x100: Arc<std::sync::atomic::AtomicU32>,
    playing_flag: Arc<std::sync::atomic::AtomicBool>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, WAVEFORMATEX,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let mut master_pcm = if !audio_tracks.is_empty() {
        decode_aac_packets_via_mft(&audio_tracks[0].1)
    } else {
        Vec::new()
    };
    if master_pcm.is_empty() {
        master_pcm = decode_audio_via_ffmpeg_fallback(&input_path, 0);
    }
    if master_pcm.is_empty() {
        return;
    }

    let has_stems = audio_tracks.len() >= 5;
    let game_pcm = if has_stems {
        let v = decode_aac_packets_via_mft(&audio_tracks[1].1);
        if v.is_empty() {
            decode_audio_via_ffmpeg_fallback(&input_path, 1)
        } else {
            v
        }
    } else {
        Vec::new()
    };
    let discord_pcm = if has_stems {
        let v = decode_aac_packets_via_mft(&audio_tracks[2].1);
        if v.is_empty() {
            decode_audio_via_ffmpeg_fallback(&input_path, 2)
        } else {
            v
        }
    } else {
        Vec::new()
    };
    let mic_pcm = if has_stems {
        let v = decode_aac_packets_via_mft(&audio_tracks[3].1);
        if v.is_empty() {
            decode_audio_via_ffmpeg_fallback(&input_path, 3)
        } else {
            v
        }
    } else {
        Vec::new()
    };
    let other_pcm = if has_stems {
        let v = decode_aac_packets_via_mft(&audio_tracks[4].1);
        if v.is_empty() {
            decode_audio_via_ffmpeg_fallback(&input_path, 4)
        } else {
            v
        }
    } else {
        Vec::new()
    };

    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return;
        };
        let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return;
        };
        let Ok(client) = device.Activate::<IAudioClient>(CLSCTX_ALL, None) else {
            return;
        };

        let wf_desired = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };

        let mut out_sample_rate = 48_000u32;
        let mut out_channels = 2usize;
        let mut out_is_pcm16 = false;

        if client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0x8000_0000 | 0x0800_0000,
                1_000_000,
                0,
                &wf_desired,
                None,
            )
            .is_err()
        {
            let Ok(mix_ptr) = client.GetMixFormat() else {
                return;
            };
            if mix_ptr.is_null() {
                return;
            }
            out_sample_rate = (*mix_ptr).nSamplesPerSec.max(8_000);
            out_channels = (*mix_ptr).nChannels.max(1) as usize;
            out_is_pcm16 = (*mix_ptr).wBitsPerSample == 16;
            let init_res = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                1_000_000,
                0,
                mix_ptr,
                None,
            );
            CoTaskMemFree(Some(mix_ptr as *const std::ffi::c_void));
            if init_res.is_err() {
                return;
            }
        }

        let Ok(buf_frames) = client.GetBufferSize() else {
            return;
        };
        let Ok(render) = client.GetService::<IAudioRenderClient>() else {
            return;
        };
        let _ = client.Start();

        let mut cursor_sample_fp: f64 = 0.0;
        let mut last_target_ms: u64 = 0;
        let resample_ratio = 48_000.0 / (out_sample_rate as f64);

        while !stop_flag.load(std::sync::atomic::Ordering::Acquire) {
            let is_playing = playing_flag.load(std::sync::atomic::Ordering::Acquire);

            let raw_req = target_seek_ms.load(std::sync::atomic::Ordering::Acquire);
            let force_seek = (raw_req & (1u64 << 62)) != 0;
            let req_ms = raw_req & !(1u64 << 62);
            if force_seek {
                target_seek_ms.store(req_ms, std::sync::atomic::Ordering::Release);
                cursor_sample_fp = ((req_ms as usize) * 48).min(master_pcm.len()) as f64;
                last_target_ms = req_ms;
            } else if req_ms != u64::MAX {
                let target_sample = ((req_ms as usize) * 48).min(master_pcm.len());
                let diff = (target_sample as isize - (cursor_sample_fp as isize)).abs();
                if diff > 9_600 || (req_ms != last_target_ms && diff > 6_400) {
                    cursor_sample_fp = target_sample as f64;
                    last_target_ms = req_ms;
                }
            }

            if !is_playing || (cursor_sample_fp as usize) >= master_pcm.len() {
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }

            let user_rate = (playback_rate_x100.load(std::sync::atomic::Ordering::Acquire) as f64
                / 100.0)
                .clamp(0.25, 2.0);
            let step_per_out_frame = user_rate * resample_ratio;

            if let Ok(padding) = client.GetCurrentPadding() {
                let avail = buf_frames.saturating_sub(padding) as usize;
                let to_write = avail.min((out_sample_rate as usize) / 25);
                if to_write > 0 {
                    let mix_cfg = active_mix.lock().map(|g| g.clone()).unwrap_or_default();
                    if let Ok(buf_ptr) = render.GetBuffer(to_write as u32) {
                        for i in 0..to_write {
                            let idx = (cursor_sample_fp + (i as f64) * step_per_out_frame) as usize;
                            let (l, r) = if mix_cfg.is_all_muted() || idx >= master_pcm.len() {
                                (0.0f32, 0.0f32)
                            } else if has_stems && !mix_cfg.is_unmodified_full_mix() {
                                let (gl, gr) = if mix_cfg.game_enabled {
                                    let s = game_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    (s.0 * mix_cfg.game_gain, s.1 * mix_cfg.game_gain)
                                } else {
                                    (0.0, 0.0)
                                };
                                let (dl, dr) = if mix_cfg.discord_enabled {
                                    let s = discord_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    (s.0 * mix_cfg.discord_gain, s.1 * mix_cfg.discord_gain)
                                } else {
                                    (0.0, 0.0)
                                };
                                let (ml, mr) = if mix_cfg.mic_enabled {
                                    let s = mic_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    (s.0 * mix_cfg.mic_gain, s.1 * mix_cfg.mic_gain)
                                } else {
                                    (0.0, 0.0)
                                };
                                let (ol, or_) = if mix_cfg.other_enabled {
                                    let s = other_pcm.get(idx).copied().unwrap_or((0.0, 0.0));
                                    (s.0 * mix_cfg.other_gain, s.1 * mix_cfg.other_gain)
                                } else {
                                    (0.0, 0.0)
                                };
                                (
                                    (gl + dl + ml + ol).clamp(-1.0, 1.0),
                                    (gr + dr + mr + or_).clamp(-1.0, 1.0),
                                )
                            } else {
                                master_pcm.get(idx).copied().unwrap_or((0.0, 0.0))
                            };

                            if out_is_pcm16 {
                                let out_s = std::slice::from_raw_parts_mut(
                                    buf_ptr as *mut i16,
                                    to_write * out_channels,
                                );
                                let base = i * out_channels;
                                out_s[base] = (l * 32767.0) as i16;
                                if out_channels >= 2 {
                                    out_s[base + 1] = (r * 32767.0) as i16;
                                    for c_idx in 2..out_channels {
                                        out_s[base + c_idx] = 0;
                                    }
                                }
                            } else {
                                let out_s = std::slice::from_raw_parts_mut(
                                    buf_ptr as *mut f32,
                                    to_write * out_channels,
                                );
                                let base = i * out_channels;
                                out_s[base] = l;
                                if out_channels >= 2 {
                                    out_s[base + 1] = r;
                                    for c_idx in 2..out_channels {
                                        out_s[base + c_idx] = 0.0;
                                    }
                                }
                            }
                        }
                        let _ = render.ReleaseBuffer(to_write as u32, 0);
                        cursor_sample_fp = (cursor_sample_fp
                            + (to_write as f64) * step_per_out_frame)
                            .min(master_pcm.len() as f64);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = client.Stop();
    }
}

pub struct VideoPreviewSession {
    input_path: PathBuf,
    metadata: ClipMetadata,
    sps: Vec<u8>,
    pps: Vec<u8>,
    video_packets: Vec<Arc<CompressedPacket>>,
    #[cfg(windows)]
    hw_dec: Option<crate::recorder::capture::MftH264Decoder>,
    sw_dec: Option<OpenH264Decoder>,
    last_decoded_idx: Option<usize>,
    annex_b_buf: Vec<u8>,
    cached_rgba: Vec<u8>,
    cached_w: u32,
    cached_h: u32,
    cached_pts_ms: u64,
    #[cfg(windows)]
    audio_player: Option<PreviewAudioPlayer>,
}

impl VideoPreviewSession {
    pub fn open(input_path: impl AsRef<Path>) -> Result<Self, String> {
        let in_p = input_path.as_ref().to_path_buf();
        let demuxed = demux_clip_full(&in_p)?;
        if demuxed.video_packets.is_empty() {
            return Err("clip contains no video packets".to_string());
        }
        #[cfg(windows)]
        let hw_dec = crate::recorder::capture::MftH264Decoder::new(
            demuxed.metadata.width,
            demuxed.metadata.height,
            &demuxed.sps,
            &demuxed.pps,
        )
        .ok();
        let sw_dec = OpenH264Decoder::new().ok();
        #[cfg(windows)]
        let audio_player = PreviewAudioPlayer::spawn(in_p.clone(), demuxed.audio_tracks_packets);
        Ok(Self {
            input_path: in_p,
            metadata: demuxed.metadata,
            sps: demuxed.sps,
            pps: demuxed.pps,
            video_packets: demuxed.video_packets,
            #[cfg(windows)]
            hw_dec,
            sw_dec,
            last_decoded_idx: None,
            annex_b_buf: Vec::with_capacity(256 * 1024),
            cached_rgba: Vec::new(),
            cached_w: 0,
            cached_h: 0,
            cached_pts_ms: 0,
            #[cfg(windows)]
            audio_player,
        })
    }

    pub fn metadata(&self) -> &ClipMetadata {
        &self.metadata
    }

    pub fn set_preview_audio_mix(&self, mix: AudioStemMixConfig) {
        #[cfg(windows)]
        if let Some(ref ap) = self.audio_player {
            ap.set_mix(mix);
        }
        #[cfg(not(windows))]
        let _ = mix;
    }

    pub fn set_preview_playing(&self, playing: bool, position_ms: Option<u64>) {
        #[cfg(windows)]
        if let Some(ref ap) = self.audio_player {
            ap.set_playing(playing, position_ms);
        }
        #[cfg(not(windows))]
        let _ = (playing, position_ms);
    }

    pub fn seek_preview_audio(&self, target_ms: u64) {
        #[cfg(windows)]
        if let Some(ref ap) = self.audio_player {
            ap.seek_to_ms(target_ms);
        }
        #[cfg(not(windows))]
        let _ = target_ms;
    }

    pub fn set_preview_playback_rate(&self, rate: f32) {
        #[cfg(windows)]
        if let Some(ref ap) = self.audio_player {
            ap.set_playback_rate(rate);
        }
        #[cfg(not(windows))]
        let _ = rate;
    }

    pub fn decode_frame_rgba(
        &mut self,
        target_ms: u64,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<(u64, Vec<u8>), String> {
        #[cfg(windows)]
        if let Some(ref ap) = self.audio_player {
            ap.notify_video_playhead(target_ms);
        }
        if self.video_packets.is_empty() {
            return Err("clip contains no video packets".to_string());
        }
        let dst_w = dst_w.max(32);
        let dst_h = dst_h.max(18);
        let expected_len = (dst_w as usize) * (dst_h as usize) * 4;

        let mut target_idx = 0usize;
        let mut kf_idx = 0usize;
        for (idx, pkt) in self.video_packets.iter().enumerate() {
            if pkt.pts_ms <= target_ms {
                target_idx = idx;
                if pkt.is_keyframe {
                    kf_idx = idx;
                }
            } else {
                break;
            }
        }

        if self.last_decoded_idx == Some(target_idx)
            && self.cached_w == dst_w
            && self.cached_h == dst_h
            && self.cached_rgba.len() == expected_len
        {
            return Ok((self.cached_pts_ms, self.cached_rgba.clone()));
        }

        let (decode_start, jumped) = match self.last_decoded_idx {
            Some(last) if last >= kf_idx && last < target_idx && (target_idx - last) <= 180 => {
                (last + 1, false)
            }
            _ => (kf_idx, true),
        };

        if self.cached_rgba.len() != expected_len {
            self.cached_rgba.resize(expected_len, 0);
        }
        self.cached_w = dst_w;
        self.cached_h = dst_h;
        let src_w = self.metadata.width;
        let src_h = self.metadata.height;

        let mut decoded_any = false;

        if let Some(ref mut sw) = self.sw_dec {
            for idx in decode_start..=target_idx {
                let pkt = &self.video_packets[idx];
                avcc_to_annex_b(
                    &pkt.data,
                    pkt.is_keyframe,
                    &self.sps,
                    &self.pps,
                    &mut self.annex_b_buf,
                );
                if self.annex_b_buf.is_empty() {
                    continue;
                }
                if let Ok(Some(yuv)) = sw.decode(&self.annex_b_buf) {
                    if idx == target_idx || !decoded_any {
                        convert_yuv_to_rgba_scaled(&yuv, dst_w, dst_h, &mut self.cached_rgba);
                    }
                    self.last_decoded_idx = Some(idx);
                    self.cached_pts_ms = pkt.pts_ms;
                    decoded_any = true;
                }
            }
        }

        #[cfg(windows)]
        if !decoded_any {
            if let Some(ref mut mft) = self.hw_dec {
                if jumped {
                    mft.flush();
                }
                let start_for_mft = if jumped { kf_idx } else { decode_start };
                for idx in start_for_mft..=target_idx {
                    let pkt = &self.video_packets[idx];
                    avcc_to_annex_b(
                        &pkt.data,
                        pkt.is_keyframe,
                        &self.sps,
                        &self.pps,
                        &mut self.annex_b_buf,
                    );
                    if self.annex_b_buf.is_empty() {
                        continue;
                    }
                    let sample_hns = (pkt.pts_ms as i64) * 10_000;
                    let _ = mft.push_annex_b(&self.annex_b_buf, sample_hns);
                    let is_last = idx == target_idx;
                    while mft.pull_nv12_frame(|nv12, dec_w, dec_h| {
                        if is_last || !decoded_any {
                            let _ = convert_nv12_to_rgba_scaled(
                                nv12,
                                dec_w,
                                dec_h,
                                src_w,
                                src_h,
                                dst_w,
                                dst_h,
                                &mut self.cached_rgba,
                            );
                        }
                        decoded_any = true;
                    }) {}
                    self.last_decoded_idx = Some(idx);
                    self.cached_pts_ms = pkt.pts_ms;
                }
                if !decoded_any {
                    let lookahead_end = (target_idx + 4).min(self.video_packets.len().saturating_sub(1));
                    for idx in (target_idx + 1)..=lookahead_end {
                        let pkt = &self.video_packets[idx];
                        avcc_to_annex_b(
                            &pkt.data,
                            pkt.is_keyframe,
                            &self.sps,
                            &self.pps,
                            &mut self.annex_b_buf,
                        );
                        if !self.annex_b_buf.is_empty() {
                            let _ = mft.push_annex_b(&self.annex_b_buf, (pkt.pts_ms as i64) * 10_000);
                        }
                        while mft.pull_nv12_frame(|nv12, dec_w, dec_h| {
                            if !decoded_any {
                                let _ = convert_nv12_to_rgba_scaled(
                                    nv12,
                                    dec_w,
                                    dec_h,
                                    src_w,
                                    src_h,
                                    dst_w,
                                    dst_h,
                                    &mut self.cached_rgba,
                                );
                                decoded_any = true;
                            }
                        }) {}
                        if decoded_any {
                            self.last_decoded_idx = None;
                            break;
                        }
                    }
                }
                if !decoded_any {
                    mft.drain();
                    while mft.pull_nv12_frame(|nv12, dec_w, dec_h| {
                        if !decoded_any {
                            let _ = convert_nv12_to_rgba_scaled(
                                nv12,
                                dec_w,
                                dec_h,
                                src_w,
                                src_h,
                                dst_w,
                                dst_h,
                                &mut self.cached_rgba,
                            );
                            decoded_any = true;
                        }
                    }) {}
                    self.last_decoded_idx = None;
                }
            }
        }

        if !decoded_any {
            if let Ok(fallback_rgba) = extract_frame_rgba(&self.input_path, target_ms, dst_w, dst_h) {
                if fallback_rgba.len() == expected_len {
                    self.cached_rgba = fallback_rgba;
                    self.cached_pts_ms = target_ms;
                    decoded_any = true;
                }
            }
        }

        if decoded_any {
            Ok((self.cached_pts_ms, self.cached_rgba.clone()))
        } else {
            Err("failed to decode preview frame".to_string())
        }
    }
}


