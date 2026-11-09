#![allow(dead_code, unused_imports)]
pub use crate::recorder::engine::config::Container;

pub fn format_clip_filename(game: &str, event_kind: &str, timestamp_ms: u64) -> String {
    let forbidden = [
        '<', '>', ':', '"', '/', '\\', '|', '?', '*', ',', ';', '=', '[',
        ']', '@', '#', '!', '$', '%', '^', '&', '(', ')', '+', '~', '`',
    ];
    let sanitize_short = |raw: &str, max_chars: usize, fallback: &str| -> String {
        let mut out = String::with_capacity(max_chars);
        let mut prev_underscore = false;
        for c in raw.chars() {
            if forbidden.contains(&c) || c.is_control() {
                continue;
            }
            if c.is_whitespace() || c == '-' || c == '.' || c == '_' {
                if !out.is_empty() && !prev_underscore {
                    out.push('_');
                    prev_underscore = true;
                }
            } else if c.is_alphanumeric() {
                out.push(c);
                prev_underscore = false;
            }
            if out.chars().count() >= max_chars {
                break;
            }
        }
        let trimmed = out.trim_matches(|c: char| c == '_' || c == '.' || c.is_whitespace());
        if trimmed.is_empty() {
            fallback.to_string()
        } else {
            trimmed.to_string()
        }
    };

    let g_low = game.trim().to_lowercase();
    let game_part = if g_low == "valorant" {
        "Val".to_string()
    } else if g_low == "cs2" || g_low.contains("counter-strike") || g_low.contains("counter_strike") {
        "CS2".to_string()
    } else if g_low == "dota2" || g_low.contains("dota") {
        "Dota".to_string()
    } else if g_low == "lol" || g_low.contains("league") {
        "LoL".to_string()
    } else if g_low == "rocket_league" || g_low.contains("rocket") {
        "RL".to_string()
    } else if g_low == "overwatch2" || g_low.contains("overwatch") {
        "OW2".to_string()
    } else if g_low == "warzone" || g_low == "cod" || g_low.contains("call_of_duty") || g_low.contains("call of duty") {
        "COD".to_string()
    } else if g_low.contains("battlefield") {
        "BF".to_string()
    } else if g_low == "apex_legends" || g_low == "apex" || g_low.contains("apex") {
        "Apex".to_string()
    } else if g_low.contains("fortnite") {
        "FN".to_string()
    } else if g_low == "rainbow_six_siege" || g_low == "r6" || g_low.contains("rainbow") {
        "R6".to_string()
    } else if g_low == "the_finals" || g_low.contains("finals") {
        "Finals".to_string()
    } else if g_low == "arc_raiders" || g_low.contains("arc") {
        "ARC".to_string()
    } else if g_low == "marvel_rivals" || g_low.contains("marvel") {
        "Rivals".to_string()
    } else if g_low == "fc26" || g_low.contains("fc 26") || g_low.contains("easportsfc26") {
        "FC26".to_string()
    } else if g_low == "fc27" || g_low.contains("fc 27") || g_low.contains("easportsfc27") {
        "FC27".to_string()
    } else if g_low == "efootball" || g_low.contains("efootball") {
        "eFootball".to_string()
    } else if g_low.contains("minecraft") {
        "MC".to_string()
    } else if g_low.contains("discord") {
        "Discord".to_string()
    } else if g_low.starts_with("desktop") {
        "Desktop".to_string()
    } else {
        sanitize_short(game, 10, "Game")
    };

    let e_low = event_kind.trim().to_lowercase();
    let event_part = match e_low.as_str() {
        "manual.clip" | "replay.hotkey" | "direct.clip" | "manual.direct_clip" | "clip" => "Clip".to_string(),
        "player.kill" | "kill" | "workshop.kill" | "champion_kill" => "Kill".to_string(),
        "player.headshot" | "headshot" => "HS".to_string(),
        "player.multikill" | "multikill" => "Multi".to_string(),
        "player.ace" | "ace" => "Ace".to_string(),
        "player.clutch" | "player.round_multikill_win" | "clutch" => "Clutch".to_string(),
        "player.knockdown" | "knockdown" => "Knock".to_string(),
        "player.mvp" | "match.mvp" | "statfeed.mvp" | "mvp" => "MVP".to_string(),
        "sport.goal" | "goal_scored" | "statfeed.goal" | "statfeed.aerial_goal" | "statfeed.long_goal" | "statfeed.overtime_goal" | "statfeed.double_tap" => "Goal".to_string(),
        "statfeed.hat_trick" | "sport.hat_trick" => "HatTrick".to_string(),
        "sport.epic_save" | "statfeed.epic_save" | "statfeed.save" | "statfeed.savior" => "Save".to_string(),
        "bomb.planted" => "Plant".to_string(),
        "bomb.defused" => "Defuse".to_string(),
        "objective.first_blood" | "first_blood" => "1stBlood".to_string(),
        "objective.secured" | "dragon_kill" | "baron_kill" | "herald_kill" | "horde_kill" | "atakhan_kill" => "Obj".to_string(),
        _ => {
            let tail = event_kind.rsplit('.').next().unwrap_or(event_kind);
            sanitize_short(tail, 8, "Clip")
        }
    };

    let ts_tag = if timestamp_ms >= 1_000_000 {
        format!("{:06}", (timestamp_ms / 100) % 1_000_000)
    } else {
        format!("{timestamp_ms}")
    };

    format!("{game_part}_{event_part}_{ts_tag}.mp4")
}


pub mod discord_compress {
    use crate::recorder::engine::buffer::{CompressedPacket, PacketKind, RingBuffer};
    use crate::recorder::engine::config::RecorderConfig;
    use crate::recorder::engine::encoder::{CodecContextInfo, EncoderPipeline};
    use crate::recorder::engine::priority;
    use bytes::Bytes;
    use mp4::{Mp4Reader, TrackType};
    use openh264::decoder::Decoder as OpenH264Decoder;
    use openh264::formats::YUVSource;
    use serde::{Deserialize, Serialize};
    use std::fs::File;
    use std::io::BufReader;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    pub const DEFAULT_DISCORD_TARGET_MB: f64 = 7.5;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct DiscordCompressOptions {
        pub target_mb: f64,
        pub output_path: Option<PathBuf>,
        pub max_fps: Option<u32>,
        pub prefer_hardware: bool,
    }

    impl Default for DiscordCompressOptions {
        fn default() -> Self {
            Self {
                target_mb: DEFAULT_DISCORD_TARGET_MB,
                output_path: None,
                max_fps: None,
                prefer_hardware: true,
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct DiscordCompressResult {
        pub input_path: PathBuf,
        pub output_path: PathBuf,
        pub input_bytes: u64,
        pub output_bytes: u64,
        pub duration_ms: u64,
        pub width: u32,
        pub height: u32,
        pub fps: u32,
        pub video_bitrate_kbps: u32,
        pub backend: String,
        pub elapsed_ms: u64,
    }

    pub fn default_discord_output_path(input_path: &Path) -> PathBuf {
        let parent = input_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = input_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("clip");
        let clean_stem = stem.strip_suffix("_discord").unwrap_or(stem);
        parent.join(format!("{clean_stem}_discord.mp4"))
    }

    fn choose_target_profile(
        src_w: u32,
        src_h: u32,
        src_fps: u32,
        target_video_bps: u32,
        max_fps_override: Option<u32>,
    ) -> (u32, u32, u32) {
        let capped_src_fps = max_fps_override
            .map(|m| src_fps.min(m.max(15)))
            .unwrap_or(src_fps)
            .clamp(15, 60);

        let (max_h, target_fps) = if target_video_bps >= 2_200_000 {
            (1080u32, capped_src_fps)
        } else if target_video_bps >= 1_350_000 {
            (720u32, capped_src_fps)
        } else if target_video_bps >= 750_000 {
            (720u32, capped_src_fps.min(30))
        } else if target_video_bps >= 420_000 {
            (540u32, capped_src_fps.min(30))
        } else {
            (480u32, capped_src_fps.min(24))
        };

        let (dst_w, dst_h) = if src_h > max_h && src_h > 0 {
            let scaled_w = ((src_w as u64 * max_h as u64) / src_h as u64) as u32;
            ((scaled_w.max(64)) & !1, (max_h.max(64)) & !1)
        } else {
            ((src_w.max(64)) & !1, (src_h.max(64)) & !1)
        };

        (dst_w, dst_h, target_fps.max(15))
    }

    fn locate_ffmpeg_binary() -> Option<PathBuf> {
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

    fn probe_via_ffmpeg_stderr(
        ffmpeg_bin: &Path,
        input_path: &Path,
    ) -> Option<(u64, u32, u32, u32)> {
        let mut cmd = Command::new(ffmpeg_bin);
        cmd.arg("-hide_banner")
            .arg("-i")
            .arg(input_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000 | 0x00004000);
        }
        let output = cmd.output().ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut duration_ms = 0u64;
        let mut width = 1920u32;
        let mut height = 1080u32;
        let mut fps = 60u32;

        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(idx) = trimmed.find("Duration:") {
                let after = &trimmed[idx + 9..];
                if let Some(time_part) = after.split(',').next() {
                    let parts: Vec<&str> = time_part.trim().split(':').collect();
                    if parts.len() == 3 {
                        let hrs: f64 = parts[0].parse().unwrap_or(0.0);
                        let mins: f64 = parts[1].parse().unwrap_or(0.0);
                        let secs: f64 = parts[2].parse().unwrap_or(0.0);
                        let total_secs = hrs * 3600.0 + mins * 60.0 + secs;
                        if total_secs > 0.0 {
                            duration_ms = (total_secs * 1000.0) as u64;
                        }
                    }
                }
            }
            if trimmed.contains("Video:") {
                for token in trimmed.split(|c: char| c == ',' || c.is_whitespace()) {
                    if let Some((w_s, h_s)) = token.split_once('x') {
                        if let (Ok(w), Ok(h)) = (w_s.parse::<u32>(), h_s.parse::<u32>()) {
                            if (64..=7680).contains(&w) && (64..=4320).contains(&h) {
                                width = w;
                                height = h;
                                break;
                            }
                        }
                    }
                }
                let tokens: Vec<&str> = trimmed.split_whitespace().collect();
                for win in tokens.windows(2) {
                    if win[1].starts_with("fps") {
                        if let Ok(f) = win[0].parse::<f64>() {
                            if (1.0..=240.0).contains(&f) {
                                fps = f.round() as u32;
                                break;
                            }
                        }
                    }
                }
            }
        }

        if duration_ms > 0 {
            Some((duration_ms, width, height, fps))
        } else {
            None
        }
    }

    fn try_ffmpeg_transcode(
        ffmpeg_bin: &Path,
        input_path: &Path,
        temp_output: &Path,
        dst_w: u32,
        dst_h: u32,
        dst_fps: u32,
        video_kbps: u32,
        audio_kbps: u32,
        prefer_hardware: bool,
    ) -> Result<String, String> {
        let maxrate_kbps = (video_kbps as f64 * 1.08) as u32;
        let bufsize_kbps = (video_kbps * 2).max(256);
        let vf_filter = format!("scale={dst_w}:{dst_h}:flags=fast_bilinear,fps={dst_fps}");

        let encoders: Vec<(&str, Vec<&str>)> = if prefer_hardware {
            vec![
                (
                    "h264_nvenc",
                    vec!["-preset", "p4", "-tune", "hq", "-rc", "vbr"],
                ),
                (
                    "h264_amf",
                    vec!["-quality", "balanced", "-rc", "vbr_peak"],
                ),
                ("h264_qsv", vec!["-preset", "medium"]),
                (
                    "libx264",
                    vec!["-preset", "veryfast", "-threads", "2"],
                ),
            ]
        } else {
            vec![(
                "libx264",
                vec!["-preset", "veryfast", "-threads", "2"],
            )]
        };

        for (enc_name, extra_args) in encoders {
            let _ = std::fs::remove_file(temp_output);
            let mut cmd = Command::new(ffmpeg_bin);
            cmd.arg("-y")
                .arg("-hide_banner")
                .arg("-loglevel")
                .arg("error");

            if enc_name != "libx264" {
                cmd.arg("-hwaccel").arg("auto");
            }

            cmd.arg("-i")
                .arg(input_path)
                .arg("-vf")
                .arg(&vf_filter)
                .arg("-c:v")
                .arg(enc_name);

            for a in extra_args {
                cmd.arg(a);
            }

            cmd.arg("-b:v")
                .arg(format!("{video_kbps}k"))
                .arg("-maxrate")
                .arg(format!("{maxrate_kbps}k"))
                .arg("-bufsize")
                .arg(format!("{bufsize_kbps}k"))
                .arg("-pix_fmt")
                .arg("yuv420p")
                .arg("-c:a")
                .arg("aac")
                .arg("-b:a")
                .arg(format!("{audio_kbps}k"))
                .arg("-ac")
                .arg("2")
                .arg("-ar")
                .arg("48000")
                .arg("-movflags")
                .arg("+faststart")
                .arg(temp_output)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());

            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000 | 0x00004000);
            }

            if let Ok(status) = cmd.status() {
                if status.success() {
                    if let Ok(meta) = std::fs::metadata(temp_output) {
                        if meta.len() > 1024 {
                            return Ok(format!("FFmpeg ({enc_name})"));
                        }
                    }
                }
            }
        }

        Err("all FFmpeg encoder backends failed".to_string())
    }

    fn avcc_to_annex_b(
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

    fn scale_nv12_frame(
        raw_nv12: &[u8],
        dec_w: u32,
        dec_h: u32,
        active_w: u32,
        active_h: u32,
        dst_w: u32,
        dst_h: u32,
        x_map_y: &[usize],
        x_map_uv: &[usize],
        dst_nv12: &mut [u8],
    ) -> bool {
        let cand_strides = [
            dec_w as usize,
            ((dec_w + 15) & !15) as usize,
            ((dec_w + 31) & !31) as usize,
            ((dec_w + 63) & !63) as usize,
            ((dec_w + 127) & !127) as usize,
        ];
        let cand_heights = [
            dec_h as usize,
            ((dec_h + 15) & !15) as usize,
            active_h as usize,
            ((active_h + 15) & !15) as usize,
        ];

        let mut stride = dec_w.max(active_w) as usize;
        let mut buf_h = dec_h.max(active_h) as usize;
        let mut matched = false;

        for &s in &cand_strides {
            if s == 0 {
                continue;
            }
            for &h in &cand_heights {
                if h == 0 {
                    continue;
                }
                if (s * h * 3) / 2 == raw_nv12.len() {
                    stride = s;
                    buf_h = h;
                    matched = true;
                    break;
                }
            }
            if matched {
                break;
            }
        }

        if !matched && stride > 0 {
            let inferred_h = (raw_nv12.len() * 2) / (stride * 3);
            if inferred_h >= active_h as usize {
                buf_h = inferred_h;
            }
        }

        let uv_offset = stride * buf_h;
        let src_w = (active_w as usize).min(stride);
        let src_h = (active_h as usize).min(buf_h);
        let dw = dst_w as usize;
        let dh = dst_h as usize;

        if src_w == 0 || src_h == 0 || uv_offset + stride * (src_h / 2) > raw_nv12.len() {
            return false;
        }

        if src_w == dw && src_h == dh {
            for y in 0..dh {
                let s_row = y * stride;
                let d_row = y * dw;
                dst_nv12[d_row..d_row + dw].copy_from_slice(&raw_nv12[s_row..s_row + dw]);
            }
            let dst_uv_base = dw * dh;
            for y in 0..(dh / 2) {
                let s_row = uv_offset + y * stride;
                let d_row = dst_uv_base + y * dw;
                dst_nv12[d_row..d_row + dw].copy_from_slice(&raw_nv12[s_row..s_row + dw]);
            }
            return true;
        }

        for y in 0..dh {
            let sy = (y * src_h) / dh;
            let s_row = sy * stride;
            let d_row = y * dw;
            for (dx, &sx) in x_map_y.iter().enumerate() {
                dst_nv12[d_row + dx] = raw_nv12[s_row + sx];
            }
        }

        let dst_uv_base = dw * dh;
        let src_uv_h = (src_h / 2).max(1);
        let dst_uv_h = (dh / 2).max(1);
        for y in 0..dst_uv_h {
            let sy = (y * src_uv_h) / dst_uv_h;
            let s_row = uv_offset + sy * stride;
            let d_row = dst_uv_base + y * dw;
            for (uv_idx, &sx_pair) in x_map_uv.iter().enumerate() {
                let dx = uv_idx * 2;
                dst_nv12[d_row + dx] = raw_nv12[s_row + sx_pair];
                dst_nv12[d_row + dx + 1] = raw_nv12[s_row + sx_pair + 1];
            }
        }

        true
    }

    fn i420_to_scaled_nv12(
        y_plane: &[u8],
        u_plane: &[u8],
        v_plane: &[u8],
        _src_w: usize,
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
            let sy = (y * src_h) / dst_h;
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

    struct DemuxedMp4Data {
        src_w: u32,
        src_h: u32,
        src_fps: u32,
        duration_ms: u64,
        sps: Vec<u8>,
        pps: Vec<u8>,
        video_samples: Vec<(u64, bool, Bytes)>,
        audio_packets: Vec<Arc<CompressedPacket>>,
        audio_total_bytes: u64,
    }

    fn demux_mp4_file(input_path: &Path) -> Result<DemuxedMp4Data, String> {
        if let Ok(mut probe_f) = File::open(input_path) {
            use std::io::Read;
            let mut magic = [0u8; 4];
            if probe_f.read_exact(&mut magic).is_ok() && magic == [0x1A, 0x45, 0xDF, 0xA3] {
                let (src_w, src_h, src_fps, duration_ms, sps, pps, video_samples, audio_packets, audio_total_bytes) =
                    crate::editor::demux_mkv_for_discord(input_path)?;
                return Ok(DemuxedMp4Data {
                    src_w,
                    src_h,
                    src_fps,
                    duration_ms,
                    sps,
                    pps,
                    video_samples,
                    audio_packets,
                    audio_total_bytes,
                });
            }
        }
        let file = File::open(input_path).map_err(|e| format!("open mp4 failed: {e}"))?;
        let file_size = file
            .metadata()
            .map_err(|e| format!("metadata failed: {e}"))?
            .len();
        let reader = BufReader::with_capacity(2 * 1024 * 1024, file);
        let mut mp4 =
            Mp4Reader::read_header(reader, file_size).map_err(|e| format!("invalid mp4: {e}"))?;

        let mut video_track_id: Option<u32> = None;
        let mut audio_track_id: Option<u32> = None;
        let mut src_w = 1920u32;
        let mut src_h = 1080u32;
        let mut v_timescale = 1000u32;
        let mut v_sample_count = 0u32;
        let mut v_dur_ms = mp4.duration().as_millis() as u64;
        let mut sps = Vec::new();
        let mut pps = Vec::new();
        let mut a_timescale = 48_000u32;
        let mut a_sample_count = 0u32;

        for track in mp4.tracks().values() {
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
                Ok(TrackType::Audio) if audio_track_id.is_none() => {
                    audio_track_id = Some(track.track_id());
                    a_timescale = track.timescale().max(1);
                    a_sample_count = track.sample_count();
                }
                _ => {}
            }
        }

        let v_id = video_track_id.ok_or_else(|| "no video track found in MP4".to_string())?;
        let mut video_samples = Vec::with_capacity(v_sample_count as usize);

        for sample_idx in 1..=v_sample_count {
            if let Ok(Some(s)) = mp4.read_sample(v_id, sample_idx) {
                let pts_ms = (s.start_time.saturating_mul(1000)) / (v_timescale as u64);
                video_samples.push((pts_ms, s.is_sync, s.bytes));
            }
        }

        if video_samples.is_empty() {
            return Err("video track contains 0 readable samples".to_string());
        }

        if v_dur_ms == 0 {
            v_dur_ms = video_samples
                .last()
                .map(|(pts, _, _)| (*pts).max(1000))
                .unwrap_or(1000);
        }

        let src_fps =
            (((video_samples.len() as u64 * 1000) / v_dur_ms.max(1)) as u32).clamp(15, 120);

        let mut audio_packets = Vec::new();
        let mut audio_total_bytes = 0u64;
        if let Some(a_id) = audio_track_id {
            for sample_idx in 1..=a_sample_count {
                if let Ok(Some(s)) = mp4.read_sample(a_id, sample_idx) {
                    let pts_ms = (s.start_time.saturating_mul(1000)) / (a_timescale as u64);
                    let dur_ticks = if a_timescale == 48_000 {
                        s.duration as i64
                    } else {
                        ((s.duration as u64 * 48_000) / a_timescale as u64) as i64
                    };
                    audio_total_bytes += s.bytes.len() as u64;
                    audio_packets.push(Arc::new(CompressedPacket {
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
        }

        Ok(DemuxedMp4Data {
            src_w,
            src_h,
            src_fps,
            duration_ms: v_dur_ms,
            sps,
            pps,
            video_samples,
            audio_packets,
            audio_total_bytes,
        })
    }

    #[cfg(not(windows))]
    fn transcode_native_pipeline(
        _demuxed: &DemuxedMp4Data,
        _temp_output: &Path,
        _dst_w: u32,
        _dst_h: u32,
        _dst_fps: u32,
        _video_bitrate_bps: u32,
        _keep_audio_ratio: f64,
    ) -> Result<String, String> {
        Err("native MFT pipeline requires Windows; install ffmpeg on Linux".to_string())
    }

    #[cfg(windows)]
    fn transcode_native_pipeline(
        demuxed: &DemuxedMp4Data,
        temp_output: &Path,
        dst_w: u32,
        dst_h: u32,
        dst_fps: u32,
        video_bitrate_bps: u32,
        keep_audio_ratio: f64,
    ) -> Result<String, String> {
        let max_dur = std::time::Duration::from_millis(demuxed.duration_ms.saturating_add(10_000));
        let out_ring = RingBuffer::new_with_capacities(
            max_dur,
            demuxed.video_samples.len().max(128),
            64,
        );
        let out_codec_info: Arc<Mutex<Option<CodecContextInfo>>> = Arc::new(Mutex::new(None));

        let mut encoder = crate::recorder::engine::capture::HardwareH264Encoder::new_standalone(
            dst_w,
            dst_h,
            dst_fps,
            video_bitrate_bps,
            (dst_fps * 2).max(30),
            0,
            &out_codec_info,
        )?;

        let backend_label = format!("NativeMFT ({:?})", encoder.backend_kind);

        let x_map_y: Vec<usize> = (0..dst_w as usize)
            .map(|dx| (dx * demuxed.src_w as usize) / dst_w as usize)
            .collect();
        let x_map_uv: Vec<usize> = (0..(dst_w as usize / 2))
            .map(|uv_dx| ((uv_dx * (demuxed.src_w as usize / 2)) / (dst_w as usize / 2)) * 2)
            .collect();

        let mut dst_nv12 = vec![0u8; ((dst_w * dst_h * 3) / 2) as usize];
        let mut annex_b_buf = Vec::with_capacity(256 * 1024);
        let mut avcc_scratch = Vec::with_capacity(256 * 1024);

        let min_step_ms = (1000 / dst_fps.max(1)).saturating_sub(2) as u64;
        let frame_dur_hns = (10_000_000 / dst_fps.max(1) as i64).max(1);
        let mut next_keep_ms = 0u64;
        let mut encoded_count = 0usize;

        if let Ok(mut mft_dec) = crate::recorder::engine::capture::MftH264Decoder::new(
            demuxed.src_w,
            demuxed.src_h,
            &demuxed.sps,
            &demuxed.pps,
        ) {
            for (pts_ms, is_sync, ref sample_bytes) in &demuxed.video_samples {
                avcc_to_annex_b(
                    sample_bytes,
                    *is_sync,
                    &demuxed.sps,
                    &demuxed.pps,
                    &mut annex_b_buf,
                );
                if annex_b_buf.is_empty() {
                    continue;
                }
                let sample_hns = (*pts_ms as i64) * 10_000;
                let _ = mft_dec.push_annex_b(&annex_b_buf, sample_hns);

                while mft_dec.pull_nv12_frame(|raw_nv12, dec_w, dec_h| {
                    if *pts_ms >= next_keep_ms || encoded_count == 0 {
                        if scale_nv12_frame(
                            raw_nv12,
                            dec_w,
                            dec_h,
                            demuxed.src_w,
                            demuxed.src_h,
                            dst_w,
                            dst_h,
                            &x_map_y,
                            &x_map_uv,
                            &mut dst_nv12,
                        ) {
                            let force_idr = encoded_count == 0;
                            let _ = encoder.encode_nv12_bytes(
                                &dst_nv12,
                                *pts_ms,
                                sample_hns,
                                frame_dur_hns,
                                force_idr,
                                &mut avcc_scratch,
                                &out_ring,
                                &out_codec_info,
                            );
                            encoded_count += 1;
                            next_keep_ms = pts_ms.saturating_add(min_step_ms);
                        }
                    }
                }) {}
            }
        }

        if encoded_count == 0 {
            let mut sw_dec = OpenH264Decoder::new()
                .map_err(|e| format!("OpenH264 fallback decoder init failed: {e}"))?;
            next_keep_ms = 0;

            for (pts_ms, is_sync, ref sample_bytes) in &demuxed.video_samples {
                avcc_to_annex_b(
                    sample_bytes,
                    *is_sync,
                    &demuxed.sps,
                    &demuxed.pps,
                    &mut annex_b_buf,
                );
                if annex_b_buf.is_empty() {
                    continue;
                }
                if let Ok(Some(yuv)) = sw_dec.decode(&annex_b_buf) {
                    if *pts_ms >= next_keep_ms || encoded_count == 0 {
                        let (w, h) = yuv.dimensions();
                        let (ys, us, vs) = yuv.strides();
                        i420_to_scaled_nv12(
                            yuv.y(),
                            yuv.u(),
                            yuv.v(),
                            w,
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
                        let sample_hns = (*pts_ms as i64) * 10_000;
                        let force_idr = encoded_count == 0;
                        let _ = encoder.encode_nv12_bytes(
                            &dst_nv12,
                            *pts_ms,
                            sample_hns,
                            frame_dur_hns,
                            force_idr,
                            &mut avcc_scratch,
                            &out_ring,
                            &out_codec_info,
                        );
                        encoded_count += 1;
                        next_keep_ms = pts_ms.saturating_add(min_step_ms);
                    }
                }
            }
        }

        let last_pts = demuxed
            .video_samples
            .last()
            .map(|(p, _, _)| *p)
            .unwrap_or(0);
        encoder.drain_output_packets(last_pts, &mut avcc_scratch, &out_ring, &out_codec_info);

        let (v_pkts, _) = out_ring.snapshot_since(Some(0));
        if v_pkts.is_empty() {
            return Err("native transcoder produced 0 video packets".to_string());
        }

        let info = out_codec_info
            .lock()
            .map_err(|_| "codec_info lock poisoned".to_string())?
            .clone()
            .ok_or_else(|| "missing SPS/PPS headers after native encode".to_string())?;

        let audio_slice: &[Arc<CompressedPacket>] = if keep_audio_ratio <= 0.0 {
            &[]
        } else {
            &demuxed.audio_packets
        };

        let dummy_cfg = RecorderConfig::default();
        EncoderPipeline::mux_clip(&v_pkts, audio_slice, temp_output, &dummy_cfg, &info)?;
        Ok(backend_label)
    }

    pub fn make_it_for_discord(
        input_path: impl AsRef<Path>,
        options: DiscordCompressOptions,
    ) -> Result<DiscordCompressResult, String> {
        priority::set_low_priority();
        let started = Instant::now();
        let in_path = input_path.as_ref().to_path_buf();

        let in_meta = std::fs::metadata(&in_path)
            .map_err(|e| format!("input video not found '{}': {e}", in_path.display()))?;
        let input_bytes = in_meta.len();
        if input_bytes == 0 {
            return Err("input video file is empty (0 bytes)".to_string());
        }

        let target_mb = options.target_mb.clamp(1.0, 500.0);
        let max_target_bytes = (target_mb * 1024.0 * 1024.0) as u64;
        let out_path = options
            .output_path
            .clone()
            .unwrap_or_else(|| default_discord_output_path(&in_path));

        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }

        let demuxed_opt = demux_mp4_file(&in_path).ok();
        let ffmpeg_bin = locate_ffmpeg_binary();

        let (duration_ms, src_w, src_h, src_fps, audio_bytes) = if let Some(ref d) = demuxed_opt {
            (
                d.duration_ms.max(500),
                d.src_w,
                d.src_h,
                d.src_fps,
                d.audio_total_bytes,
            )
        } else if let Some(ref bin) = ffmpeg_bin {
            if let Some((dur, w, h, f)) = probe_via_ffmpeg_stderr(bin, &in_path) {
                let est_audio = (dur / 1000).saturating_mul(16_000);
                (dur.max(500), w, h, f, est_audio)
            } else {
                return Err("failed to read video metadata from input file".to_string());
            }
        } else {
            return Err("unsupported container and ffmpeg is not installed".to_string());
        };

        if input_bytes <= max_target_bytes {
            if in_path != out_path {
                std::fs::copy(&in_path, &out_path)
                    .map_err(|e| format!("failed to copy already-fitting video: {e}"))?;
            }
            return Ok(DiscordCompressResult {
                input_path: in_path,
                output_path: out_path,
                input_bytes,
                output_bytes: input_bytes,
                duration_ms,
                width: src_w,
                height: src_h,
                fps: src_fps,
                video_bitrate_kbps: ((input_bytes * 8) / duration_ms.max(1)) as u32,
                backend: "DirectCopy (Already Fits Discord)".to_string(),
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }

        let duration_secs = (duration_ms as f64 / 1000.0).max(0.5);
        let safe_budget_bytes = (max_target_bytes as f64 * 0.93) as u64;

        let target_audio_kbps: u32 = if duration_secs > 90.0 {
            64
        } else if duration_secs > 40.0 {
            96
        } else {
            128
        };

        let temp_output = out_path.with_extension(format!("discord_tmp_{}.mp4", crate::recorder::engine::now_ms()));

        let mut scale_factor = 1.0f64;
        let mut final_backend = String::new();
        let mut final_w = src_w;
        let mut final_h = src_h;
        let mut final_fps = src_fps;
        let mut final_v_kbps = 1000u32;

        for attempt in 0..2 {
            let eff_budget = (safe_budget_bytes as f64 * scale_factor) as u64;
            let ff_audio_bytes = ((target_audio_kbps as f64 * 1000.0 / 8.0) * duration_secs) as u64;
            let video_budget_bytes = eff_budget.saturating_sub(ff_audio_bytes.min(eff_budget / 3));
            let target_video_bps =
                (((video_budget_bytes as f64 * 8.0) / duration_secs) as u32).max(150_000);
            let (dst_w, dst_h, dst_fps) = choose_target_profile(
                src_w,
                src_h,
                src_fps,
                target_video_bps,
                options.max_fps,
            );
            let video_kbps = (target_video_bps / 1000).max(150);

            final_w = dst_w;
            final_h = dst_h;
            final_fps = dst_fps;
            final_v_kbps = video_kbps;

            let mut succeeded = false;

            if let Some(ref bin) = ffmpeg_bin {
                if let Ok(b_name) = try_ffmpeg_transcode(
                    bin,
                    &in_path,
                    &temp_output,
                    dst_w,
                    dst_h,
                    dst_fps,
                    video_kbps,
                    target_audio_kbps,
                    options.prefer_hardware,
                ) {
                    final_backend = b_name;
                    succeeded = true;
                }
            }

            if !succeeded {
                if let Some(ref d) = demuxed_opt {
                    let native_keep_audio = if audio_bytes < (eff_budget * 2) / 5 {
                        1.0
                    } else {
                        0.0
                    };
                    let native_v_budget = if native_keep_audio > 0.0 {
                        eff_budget.saturating_sub(audio_bytes)
                    } else {
                        eff_budget
                    };
                    let native_v_bps =
                        (((native_v_budget as f64 * 8.0) / duration_secs) as u32).max(150_000);
                    let (nw, nh, nfps) = choose_target_profile(
                        src_w,
                        src_h,
                        src_fps,
                        native_v_bps,
                        options.max_fps,
                    );
                    final_w = nw;
                    final_h = nh;
                    final_fps = nfps;
                    final_v_kbps = (native_v_bps / 1000).max(150);

                    let b_name = transcode_native_pipeline(
                        d,
                        &temp_output,
                        nw,
                        nh,
                        nfps,
                        native_v_bps,
                        native_keep_audio,
                    )?;
                    final_backend = b_name;
                    succeeded = true;
                }
            }

            if !succeeded {
                let _ = std::fs::remove_file(&temp_output);
                return Err("failed to compress video for Discord across all hardware/software backends".to_string());
            }

            let produced_size = std::fs::metadata(&temp_output)
                .map(|m| m.len())
                .unwrap_or(0);

            if produced_size > 0 && (produced_size <= max_target_bytes || attempt == 1) {
                break;
            }

            if produced_size > max_target_bytes {
                let ratio = (max_target_bytes as f64 / produced_size as f64) * 0.85;
                scale_factor = (scale_factor * ratio).clamp(0.25, 0.85);
            }
        }

        let output_bytes = std::fs::metadata(&temp_output)
            .map_err(|e| format!("failed to inspect compressed output: {e}"))?
            .len();

        if out_path.exists() {
            let _ = std::fs::remove_file(&out_path);
        }
        std::fs::rename(&temp_output, &out_path).map_err(|e| {
            let _ = std::fs::remove_file(&temp_output);
            format!(
                "failed to move compressed video to '{}': {e}",
                out_path.display()
            )
        })?;

        Ok(DiscordCompressResult {
            input_path: in_path,
            output_path: out_path,
            input_bytes,
            output_bytes,
            duration_ms,
            width: final_w,
            height: final_h,
            fps: final_fps,
            video_bitrate_kbps: final_v_kbps,
            backend: final_backend,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}


pub use discord_compress::{
    default_discord_output_path, make_it_for_discord, DiscordCompressOptions, DiscordCompressResult,
    DEFAULT_DISCORD_TARGET_MB,
};
