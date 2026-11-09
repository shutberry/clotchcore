#![allow(dead_code, unused_imports)]
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::events::EventSink;
use crate::recorder::{
    buffer, capture, config::AudioMode, diag, emit_error, priority, RwLockOptStringExt,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioInputDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[cfg(windows)]
mod win {
    use super::AudioInputDeviceInfo;
    use std::ffi::c_void;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use windows::core::{Interface, GUID};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
    use windows::Win32::Media::Audio::{
        eCapture, eCommunications, eConsole, eRender, EDataFlow, IAudioCaptureClient,
        IAudioClient, IAudioRenderClient, IAudioSessionControl2, IAudioSessionEnumerator,
        IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const SHARED_BUFFER_TIME_100NS: i64 = 5_000_000;
    const FALLBACK_BUFFER_TIME_100NS: i64 = 1_000_000;
    const KEEPALIVE_BUFFER_TIME_100NS: i64 = 2_000_000;
    const TARGET_SAMPLE_RATE: u32 = 48_000;

    const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM_BITS: u32 = 0x8000_0000;
    const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY_BITS: u32 = 0x0800_0000;
    const AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY_BITS: u32 = 0x0000_0001;
    const AUDCLNT_BUFFERFLAGS_SILENT_BITS: u32 = 0x0000_0002;

    const WAVE_FORMAT_PCM_TAG: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

    #[repr(C)]
    struct PropertyKeyRaw {
        fmtid: GUID,
        pid: u32,
    }

    #[repr(C)]
    struct PropVariantRaw {
        vt: u16,
        r1: u16,
        r2: u16,
        r3: u16,
        data_ptr: *mut u16,
        data2: usize,
    }

    const PKEY_DEVICE_FRIENDLY_NAME: PropertyKeyRaw = PropertyKeyRaw {
        fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };

    const PKEY_DEVICE_INTERFACE_FRIENDLY_NAME: PropertyKeyRaw = PropertyKeyRaw {
        fmtid: GUID::from_u128(0x026e516e_b814_414b_83cd_856d6fef4822),
        pid: 2,
    };

    unsafe fn com_release(ptr: *mut c_void) {
        if !ptr.is_null() {
            let vtbl = *(ptr as *const *const usize);
            let release_fn: unsafe extern "system" fn(*mut c_void) -> u32 =
                std::mem::transmute(*vtbl.add(2));
            release_fn(ptr);
        }
    }

    unsafe fn pwstr_to_string(ptr: *const u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 && len < 4096 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }

    fn get_device_id(device: &IMMDevice) -> Option<String> {
        unsafe {
            let raw = device.as_raw();
            if raw.is_null() {
                return None;
            }
            let vtbl = *(raw as *const *const usize);
            let get_id_fn: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32 =
                std::mem::transmute(*vtbl.add(5));
            let mut id_ptr: *mut u16 = std::ptr::null_mut();
            let hr = get_id_fn(raw, &mut id_ptr);
            if hr >= 0 && !id_ptr.is_null() {
                let s = pwstr_to_string(id_ptr);
                CoTaskMemFree(Some(id_ptr as *const c_void));
                if !s.is_empty() {
                    return Some(s);
                }
            }
            None
        }
    }

    unsafe fn read_prop_string(
        store_raw: *mut c_void,
        key: &PropertyKeyRaw,
    ) -> Option<String> {
        let vtbl = *(store_raw as *const *const usize);
        let get_val_fn: unsafe extern "system" fn(
            *mut c_void,
            *const PropertyKeyRaw,
            *mut PropVariantRaw,
        ) -> i32 = std::mem::transmute(*vtbl.add(5));
        let mut pv: PropVariantRaw = std::mem::zeroed();
        let hr = get_val_fn(store_raw, key as *const PropertyKeyRaw, &mut pv);
        if hr >= 0 {
            let res = if pv.vt == 31 && !pv.data_ptr.is_null() {
                let s = pwstr_to_string(pv.data_ptr).trim().to_string();
                CoTaskMemFree(Some(pv.data_ptr as *const c_void));
                if !s.is_empty() {
                    Some(s)
                } else {
                    None
                }
            } else {
                None
            };
            return res;
        }
        None
    }

    fn get_device_friendly_name(device: &IMMDevice) -> Option<String> {
        unsafe {
            let raw = device.as_raw();
            if raw.is_null() {
                return None;
            }
            let vtbl = *(raw as *const *const usize);
            let open_store_fn: unsafe extern "system" fn(
                *mut c_void,
                u32,
                *mut *mut c_void,
            ) -> i32 = std::mem::transmute(*vtbl.add(4));
            let mut store_raw: *mut c_void = std::ptr::null_mut();
            let hr = open_store_fn(raw, 0, &mut store_raw);
            if hr < 0 || store_raw.is_null() {
                return None;
            }
            let name = read_prop_string(store_raw, &PKEY_DEVICE_FRIENDLY_NAME)
                .or_else(|| read_prop_string(store_raw, &PKEY_DEVICE_INTERFACE_FRIENDLY_NAME));
            com_release(store_raw);
            name
        }
    }

    fn enumerate_active_devices(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
    ) -> Vec<IMMDevice> {
        let mut out = Vec::new();
        unsafe {
            let raw = enumerator.as_raw();
            if raw.is_null() {
                return out;
            }
            let vtbl = *(raw as *const *const usize);
            let enum_fn: unsafe extern "system" fn(
                *mut c_void,
                i32,
                u32,
                *mut *mut c_void,
            ) -> i32 = std::mem::transmute(*vtbl.add(3));
            let mut coll_raw: *mut c_void = std::ptr::null_mut();
            let hr = enum_fn(raw, flow.0, 1u32, &mut coll_raw);
            if hr < 0 || coll_raw.is_null() {
                return out;
            }
            let coll_vtbl = *(coll_raw as *const *const usize);
            let count_fn: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 =
                std::mem::transmute(*coll_vtbl.add(3));
            let item_fn: unsafe extern "system" fn(
                *mut c_void,
                u32,
                *mut *mut c_void,
            ) -> i32 = std::mem::transmute(*coll_vtbl.add(4));

            let mut count: u32 = 0;
            if count_fn(coll_raw, &mut count) >= 0 {
                for idx in 0..count.min(64) {
                    let mut dev_raw: *mut c_void = std::ptr::null_mut();
                    if item_fn(coll_raw, idx, &mut dev_raw) >= 0 && !dev_raw.is_null() {
                        out.push(IMMDevice::from_raw(dev_raw));
                    }
                }
            }
            com_release(coll_raw);
        }
        out
    }

    fn open_device_by_id(
        enumerator: &IMMDeviceEnumerator,
        id_str: &str,
    ) -> Result<IMMDevice, String> {
        unsafe {
            let raw = enumerator.as_raw();
            if raw.is_null() {
                return Err("null IMMDeviceEnumerator".to_string());
            }
            let vtbl = *(raw as *const *const usize);
            let get_dev_fn: unsafe extern "system" fn(
                *mut c_void,
                *const u16,
                *mut *mut c_void,
            ) -> i32 = std::mem::transmute(*vtbl.add(5));
            let mut wide: Vec<u16> = id_str.encode_utf16().collect();
            wide.push(0);
            let mut dev_raw: *mut c_void = std::ptr::null_mut();
            let hr = get_dev_fn(raw, wide.as_ptr(), &mut dev_raw);
            if hr >= 0 && !dev_raw.is_null() {
                Ok(IMMDevice::from_raw(dev_raw))
            } else {
                Err(format!("GetDevice({id_str}) failed: {:#010x}", hr as u32))
            }
        }
    }

    pub fn list_input_devices() -> Vec<AudioInputDeviceInfo> {
        ensure_coinit();
        let Ok(enumerator) = create_enumerator() else {
            return Vec::new();
        };
        let default_id = get_default_device(&enumerator, eCapture)
            .ok()
            .and_then(|d| get_device_id(&d))
            .unwrap_or_default();

        let devices = enumerate_active_devices(&enumerator, eCapture);
        let mut list = Vec::with_capacity(devices.len());
        for (idx, dev) in devices.into_iter().enumerate() {
            let id = get_device_id(&dev).unwrap_or_else(|| format!("capture_{idx}"));
            let name = get_device_friendly_name(&dev)
                .unwrap_or_else(|| format!("Microphone {}", idx + 1));
            let is_default = !default_id.is_empty() && id.eq_ignore_ascii_case(&default_id);
            list.push(AudioInputDeviceInfo {
                id,
                name,
                is_default,
            });
        }
        list.sort_by(|a, b| b.is_default.cmp(&a.is_default));
        list
    }

    fn query_pid_exe_lower(pid: u32) -> String {
        if pid == 0 {
            return String::new();
        }
        unsafe {
            if let Ok(proc_handle) =
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            {
                let mut exe_buf = [0u16; 512];
                let mut len = exe_buf.len() as u32;
                let ok = QueryFullProcessImageNameW(
                    proc_handle,
                    PROCESS_NAME_FORMAT(0),
                    windows::core::PWSTR(exe_buf.as_mut_ptr()),
                    &mut len,
                )
                .is_ok();
                let _ = CloseHandle(proc_handle);
                if ok && len > 0 {
                    let path_str = String::from_utf16_lossy(&exe_buf[..len as usize]);
                    if let Some(fname) = Path::new(&path_str)
                        .file_name()
                        .and_then(|n| n.to_str())
                    {
                        return fname.to_lowercase();
                    }
                }
            }
        }
        String::new()
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SampleEncoding {
        Float32,
        Pcm16,
        Pcm24,
        Pcm32,
    }

    struct NativeMixFormat {
        channels: usize,
        sample_rate: u32,
        block_align: usize,
        encoding: SampleEncoding,
        channel_weights: Vec<(f32, f32)>,
    }

    struct LinearStereoResampler {
        src_rate: u32,
        dst_rate: u32,
        frac_pos: f64,
        last_frame: Option<(f32, f32)>,
    }

    impl LinearStereoResampler {
        fn new(src_rate: u32, dst_rate: u32) -> Self {
            Self {
                src_rate: src_rate.max(8_000),
                dst_rate: dst_rate.max(8_000),
                frac_pos: 0.0,
                last_frame: None,
            }
        }

        fn process(&mut self, input: &[(f32, f32)], out: &mut Vec<(f32, f32)>) {
            if input.is_empty() {
                return;
            }
            if self.src_rate == self.dst_rate {
                out.extend_from_slice(input);
                self.last_frame = input.last().copied();
                return;
            }

            let step = self.src_rate as f64 / self.dst_rate as f64;
            let prev = self.last_frame.unwrap_or(input[0]);

            while self.frac_pos < input.len() as f64 {
                let idx_f = self.frac_pos.floor();
                let frac = (self.frac_pos - idx_f) as f32;
                let idx = idx_f as isize;

                let (s0, s1) = if idx < 0 {
                    (prev, input[0])
                } else {
                    let u = idx as usize;
                    let a = input[u];
                    let b = if u + 1 < input.len() {
                        input[u + 1]
                    } else {
                        a
                    };
                    (a, b)
                };

                let l = (s0.0 + (s1.0 - s0.0) * frac).clamp(-1.0, 1.0);
                let r = (s0.1 + (s1.1 - s0.1) * frac).clamp(-1.0, 1.0);
                out.push((l, r));
                self.frac_pos += step;
            }

            self.frac_pos -= input.len() as f64;
            if self.frac_pos < 0.0 {
                self.frac_pos = 0.0;
            }
            self.last_frame = input.last().copied();
        }
    }

    fn ensure_coinit() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
    }

    fn create_enumerator() -> Result<IMMDeviceEnumerator, String> {
        unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("CoCreateInstance IMMDeviceEnumerator failed: {e}"))
        }
    }

    fn get_default_device(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
    ) -> Result<IMMDevice, String> {
        unsafe {
            enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .or_else(|_| enumerator.GetDefaultAudioEndpoint(flow, eCommunications))
                .map_err(|e| format!("GetDefaultAudioEndpoint failed: {e}"))
        }
    }

    fn speaker_bit_weight(bit: u32) -> (f32, f32) {
        match bit {
            0x0000_0001 => (1.0, 0.0),
            0x0000_0002 => (0.0, 1.0),
            0x0000_0004 => (0.70710677, 0.70710677),
            0x0000_0008 => (0.0, 0.0),
            0x0000_0010 => (0.70710677, 0.0),
            0x0000_0020 => (0.0, 0.70710677),
            0x0000_0040 => (0.85, 0.15),
            0x0000_0080 => (0.15, 0.85),
            0x0000_0100 => (0.5, 0.5),
            0x0000_0200 => (0.70710677, 0.0),
            0x0000_0400 => (0.0, 0.70710677),
            0x0000_1000 => (0.70710677, 0.0),
            0x0000_2000 => (0.5, 0.5),
            0x0000_4000 => (0.0, 0.70710677),
            _ => (0.5, 0.5),
        }
    }

    fn normalize_channel_weights(mut weights: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
        if weights.len() <= 2 {
            return weights;
        }
        let sum_l: f32 = weights.iter().map(|w| w.0.abs()).sum();
        let sum_r: f32 = weights.iter().map(|w| w.1.abs()).sum();
        let max_sum = sum_l.max(sum_r);
        if max_sum > 1.0 {
            let inv = 1.0 / max_sum;
            for w in &mut weights {
                w.0 *= inv;
                w.1 *= inv;
            }
        }
        weights
    }

    fn default_channel_weights(channels: usize) -> Vec<(f32, f32)> {
        let raw = match channels {
            0 | 1 => vec![(1.0, 1.0)],
            2 => vec![(1.0, 0.0), (0.0, 1.0)],
            3 => vec![(1.0, 0.0), (0.0, 1.0), (0.70710677, 0.70710677)],
            4 => vec![
                (1.0, 0.0),
                (0.0, 1.0),
                (0.70710677, 0.0),
                (0.0, 0.70710677),
            ],
            5 => vec![
                (1.0, 0.0),
                (0.0, 1.0),
                (0.70710677, 0.70710677),
                (0.70710677, 0.0),
                (0.0, 0.70710677),
            ],
            6 => vec![
                (1.0, 0.0),
                (0.0, 1.0),
                (0.70710677, 0.70710677),
                (0.0, 0.0),
                (0.70710677, 0.0),
                (0.0, 0.70710677),
            ],
            7 => vec![
                (1.0, 0.0),
                (0.0, 1.0),
                (0.70710677, 0.70710677),
                (0.0, 0.0),
                (0.70710677, 0.0),
                (0.0, 0.70710677),
                (0.5, 0.5),
            ],
            _ => {
                let base = [
                    (1.0, 0.0),
                    (0.0, 1.0),
                    (0.70710677, 0.70710677),
                    (0.0, 0.0),
                    (0.70710677, 0.0),
                    (0.0, 0.70710677),
                    (0.70710677, 0.0),
                    (0.0, 0.70710677),
                ];
                (0..channels)
                    .map(|i| {
                        if i < base.len() {
                            base[i]
                        } else if i % 2 == 0 {
                            (0.5, 0.0)
                        } else {
                            (0.0, 0.5)
                        }
                    })
                    .collect()
            }
        };
        normalize_channel_weights(raw)
    }

    fn build_channel_weights(channels: usize, mask: u32) -> Vec<(f32, f32)> {
        if channels <= 2 || mask == 0 {
            return default_channel_weights(channels);
        }
        let mut weights = Vec::with_capacity(channels);
        for bit_idx in 0..18u32 {
            let bit = 1u32 << bit_idx;
            if (mask & bit) != 0 {
                weights.push(speaker_bit_weight(bit));
                if weights.len() == channels {
                    break;
                }
            }
        }
        if weights.len() < channels {
            let fallback = default_channel_weights(channels);
            for i in weights.len()..channels {
                weights.push(fallback[i]);
            }
        }
        normalize_channel_weights(weights)
    }

    unsafe fn parse_mix_format(wf_ptr: *const WAVEFORMATEX) -> Result<NativeMixFormat, String> {
        if wf_ptr.is_null() {
            return Err("null WAVEFORMATEX pointer from GetMixFormat".to_string());
        }
        let raw = wf_ptr as *const u8;
        let format_tag = u16::from_le_bytes(unsafe { [*raw, *raw.add(1)] });
        let channels =
            (u16::from_le_bytes(unsafe { [*raw.add(2), *raw.add(3)] }) as usize).max(1);
        let sample_rate = u32::from_le_bytes(unsafe {
            [*raw.add(4), *raw.add(5), *raw.add(6), *raw.add(7)]
        })
        .max(8_000);
        let block_align_raw =
            u16::from_le_bytes(unsafe { [*raw.add(12), *raw.add(13)] }) as usize;
        let bits = u16::from_le_bytes(unsafe { [*raw.add(14), *raw.add(15)] });
        let cb_size = u16::from_le_bytes(unsafe { [*raw.add(16), *raw.add(17)] });
        let block_align = block_align_raw.max(channels * (bits as usize / 8).max(1));

        let mut mask = 0u32;
        let encoding = if format_tag == WAVE_FORMAT_IEEE_FLOAT_TAG {
            SampleEncoding::Float32
        } else if format_tag == WAVE_FORMAT_PCM_TAG {
            match bits {
                16 => SampleEncoding::Pcm16,
                24 => SampleEncoding::Pcm24,
                32 => SampleEncoding::Pcm32,
                _ => return Err(format!("unsupported PCM bit depth: {bits}")),
            }
        } else if format_tag == WAVE_FORMAT_EXTENSIBLE_TAG && cb_size >= 22 {
            mask = u32::from_le_bytes(unsafe {
                [*raw.add(20), *raw.add(21), *raw.add(22), *raw.add(23)]
            });
            let sub_data1 = u32::from_le_bytes(unsafe {
                [*raw.add(24), *raw.add(25), *raw.add(26), *raw.add(27)]
            });
            if sub_data1 == 3 {
                SampleEncoding::Float32
            } else if sub_data1 == 1 {
                match bits {
                    16 => SampleEncoding::Pcm16,
                    24 => SampleEncoding::Pcm24,
                    32 => SampleEncoding::Pcm32,
                    _ => return Err(format!("unsupported extensible PCM bit depth: {bits}")),
                }
            } else {
                SampleEncoding::Float32
            }
        } else {
            SampleEncoding::Float32
        };

        let channel_weights = build_channel_weights(channels, mask);
        Ok(NativeMixFormat {
            channels,
            sample_rate,
            block_align,
            encoding,
            channel_weights,
        })
    }

    struct SilentLoopbackKeepAlive {
        client: IAudioClient,
        render: IAudioRenderClient,
        buffer_frames: u32,
    }

    impl SilentLoopbackKeepAlive {
        fn new(device: &IMMDevice) -> Option<Self> {
            unsafe {
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
                let wf_ptr = client.GetMixFormat().ok()?;
                let raw = wf_ptr as *const u8;
                let block_align =
                    (u16::from_le_bytes([*raw.add(12), *raw.add(13)]) as usize).max(4);
                let init_ok = client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        0,
                        KEEPALIVE_BUFFER_TIME_100NS,
                        0,
                        wf_ptr,
                        None,
                    )
                    .is_ok();
                CoTaskMemFree(Some(wf_ptr as *const c_void));
                if !init_ok {
                    return None;
                }
                let buffer_frames = client.GetBufferSize().ok()?;
                if buffer_frames == 0 {
                    return None;
                }
                let render: IAudioRenderClient = client.GetService().ok()?;
                if let Ok(buf_ptr) = render.GetBuffer(buffer_frames) {
                    if !buf_ptr.is_null() {
                        std::ptr::write_bytes(
                            buf_ptr,
                            0,
                            (buffer_frames as usize).saturating_mul(block_align),
                        );
                    }
                    let _ = render.ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT_BITS);
                }
                client.Start().ok()?;
                Some(Self {
                    client,
                    render,
                    buffer_frames,
                })
            }
        }

        fn top_up(&self) {
            unsafe {
                if let Ok(padding) = self.client.GetCurrentPadding() {
                    let avail = self.buffer_frames.saturating_sub(padding);
                    if avail >= self.buffer_frames / 2 && avail > 0 {
                        if let Ok(_buf_ptr) = self.render.GetBuffer(avail) {
                            let _ =
                                self.render.ReleaseBuffer(avail, AUDCLNT_BUFFERFLAGS_SILENT_BITS);
                        }
                    }
                }
            }
        }
    }

    impl Drop for SilentLoopbackKeepAlive {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }

    fn open_capture_client(
        device: &IMMDevice,
        loopback: bool,
    ) -> Result<(IAudioClient, IAudioCaptureClient, NativeMixFormat), String> {
        unsafe {
            let base_flags = if loopback {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            } else {
                0
            };

            let target_wf = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT_TAG,
                nChannels: 2,
                nSamplesPerSec: TARGET_SAMPLE_RATE,
                nAvgBytesPerSec: TARGET_SAMPLE_RATE * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let autoconvert_flags = base_flags
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM_BITS
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY_BITS;

            if let Ok(auto_client) = device.Activate::<IAudioClient>(CLSCTX_ALL, None) {
                let auto_ok = auto_client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        autoconvert_flags,
                        SHARED_BUFFER_TIME_100NS,
                        0,
                        &target_wf,
                        None,
                    )
                    .is_ok()
                    || auto_client
                        .Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            autoconvert_flags,
                            FALLBACK_BUFFER_TIME_100NS,
                            0,
                            &target_wf,
                            None,
                        )
                        .is_ok();

                if auto_ok {
                    if let Ok(capture_client) = auto_client.GetService::<IAudioCaptureClient>() {
                        if auto_client.Start().is_ok() {
                            return Ok((
                                auto_client,
                                capture_client,
                                NativeMixFormat {
                                    channels: 2,
                                    sample_rate: TARGET_SAMPLE_RATE,
                                    block_align: 8,
                                    encoding: SampleEncoding::Float32,
                                    channel_weights: vec![(1.0, 0.0), (0.0, 1.0)],
                                },
                            ));
                        }
                    }
                }
            }

            let audio_client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("IMMDevice::Activate failed: {e}"))?;

            let mix_ptr = audio_client
                .GetMixFormat()
                .map_err(|e| format!("IAudioClient::GetMixFormat failed: {e}"))?;

            let parsed_res = parse_mix_format(mix_ptr);

            let mut init_res = audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                base_flags,
                SHARED_BUFFER_TIME_100NS,
                0,
                mix_ptr,
                None,
            );
            if init_res.is_err() {
                init_res = audio_client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    base_flags,
                    FALLBACK_BUFFER_TIME_100NS,
                    0,
                    mix_ptr,
                    None,
                );
            }

            CoTaskMemFree(Some(mix_ptr as *const c_void));

            init_res.map_err(|e| format!("IAudioClient::Initialize failed: {e}"))?;
            let mix_format = parsed_res?;

            let capture_client: IAudioCaptureClient = audio_client
                .GetService()
                .map_err(|e| format!("GetService IAudioCaptureClient failed: {e}"))?;

            audio_client
                .Start()
                .map_err(|e| format!("IAudioClient::Start failed: {e}"))?;

            Ok((audio_client, capture_client, mix_format))
        }
    }

    #[inline]
    unsafe fn decode_channel_sample(
        frame_ptr: *const u8,
        ch: usize,
        encoding: SampleEncoding,
    ) -> f32 {
        match encoding {
            SampleEncoding::Float32 => {
                let p = unsafe { (frame_ptr as *const f32).add(ch) };
                let v = unsafe { p.read_unaligned() };
                if v.is_finite() {
                    v
                } else {
                    0.0
                }
            }
            SampleEncoding::Pcm16 => {
                let p = unsafe { (frame_ptr as *const i16).add(ch) };
                let v = unsafe { p.read_unaligned() };
                (v as f32) / 32768.0
            }
            SampleEncoding::Pcm24 => {
                let p = unsafe { frame_ptr.add(ch * 3) };
                let b0 = unsafe { *p } as i32;
                let b1 = (unsafe { *p.add(1) } as i32) << 8;
                let b2 = (unsafe { *p.add(2) } as i8 as i32) << 16;
                let val = b0 | b1 | b2;
                (val as f32) / 8_388_608.0
            }
            SampleEncoding::Pcm32 => {
                let p = unsafe { (frame_ptr as *const i32).add(ch) };
                let v = unsafe { p.read_unaligned() };
                (v as f64 / 2_147_483_648.0) as f32
            }
        }
    }

    fn read_capture(
        capture_client: &IAudioCaptureClient,
        mix_format: &NativeMixFormat,
        resampler: &mut LinearStereoResampler,
        scratch_native: &mut Vec<(f32, f32)>,
        last_qpc_100ns: &mut Option<u64>,
    ) -> Result<Vec<(f32, f32)>, String> {
        scratch_native.clear();
        unsafe {
            loop {
                let packet_len = match capture_client.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(e) => {
                        return Err(format!("GetNextPacketSize failed: {e}"));
                    }
                };
                if packet_len == 0 {
                    break;
                }
                let mut pdata: *mut u8 = std::ptr::null_mut();
                let mut frames_available: u32 = 0;
                let mut flags: u32 = 0;
                let mut dev_pos: u64 = 0;
                let mut qpc_pos_100ns: u64 = 0;
                let hr = capture_client.GetBuffer(
                    &mut pdata,
                    &mut frames_available,
                    &mut flags,
                    Some(&mut dev_pos),
                    Some(&mut qpc_pos_100ns),
                );
                if let Err(e) = hr {
                    return Err(format!("GetBuffer failed: {:#010x}", e.code().0 as u32));
                }
                if frames_available == 0 {
                    let _ = capture_client.ReleaseBuffer(0);
                    break;
                }
                if qpc_pos_100ns > 0 {
                    if let Some(prev_qpc) = *last_qpc_100ns {
                        let expected_step_100ns = ((frames_available as u64) * 10_000_000)
                            / (mix_format.sample_rate.max(8_000) as u64);
                        if (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY_BITS) != 0
                            && qpc_pos_100ns > prev_qpc + expected_step_100ns + 150_000
                        {
                            let gap_100ns = (qpc_pos_100ns - prev_qpc).saturating_sub(expected_step_100ns);
                            let missing_native = (((gap_100ns as u128)
                                * (mix_format.sample_rate.max(8_000) as u128))
                                / 10_000_000u128)
                                .min((mix_format.sample_rate as u128) / 2)
                                as usize;
                            if missing_native > 0 {
                                scratch_native.extend(
                                    std::iter::repeat((0.0f32, 0.0f32)).take(missing_native),
                                );
                            }
                        }
                    }
                    let pkt_dur_100ns = ((frames_available as u64) * 10_000_000)
                        / (mix_format.sample_rate.max(8_000) as u64);
                    *last_qpc_100ns = Some(qpc_pos_100ns.saturating_add(pkt_dur_100ns));
                }

                let frames = frames_available as usize;
                let is_silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT_BITS) != 0 || pdata.is_null();

                if is_silent {
                    scratch_native.extend(std::iter::repeat((0.0f32, 0.0f32)).take(frames));
                } else {
                    let ch_count = mix_format.channels;
                    let stride_bytes = mix_format.block_align;
                    let weights = &mix_format.channel_weights;
                    let enc = mix_format.encoding;

                    if ch_count == 2 && enc == SampleEncoding::Float32 && stride_bytes == 8 {
                        let slice =
                            std::slice::from_raw_parts(pdata as *const f32, frames * 2);
                        for pair in slice.chunks_exact(2) {
                            let l = if pair[0].is_finite() {
                                pair[0].clamp(-1.0, 1.0)
                            } else {
                                0.0
                            };
                            let r = if pair[1].is_finite() {
                                pair[1].clamp(-1.0, 1.0)
                            } else {
                                0.0
                            };
                            scratch_native.push((l, r));
                        }
                    } else if ch_count == 1
                        && enc == SampleEncoding::Float32
                        && stride_bytes == 4
                    {
                        let slice = std::slice::from_raw_parts(pdata as *const f32, frames);
                        for &m in slice {
                            let v = if m.is_finite() {
                                m.clamp(-1.0, 1.0)
                            } else {
                                0.0
                            };
                            scratch_native.push((v, v));
                        }
                    } else {
                        for f_idx in 0..frames {
                            let frame_ptr = pdata.add(f_idx * stride_bytes);
                            let mut left = 0.0f32;
                            let mut right = 0.0f32;
                            for (ch, &(wl, wr)) in weights.iter().enumerate().take(ch_count) {
                                let s = decode_channel_sample(frame_ptr, ch, enc);
                                left += s * wl;
                                right += s * wr;
                            }
                            scratch_native.push((left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0)));
                        }
                    }
                }

                capture_client
                    .ReleaseBuffer(frames_available)
                    .map_err(|e| format!("ReleaseBuffer failed: {e}"))?;
            }
        }

        let mut out_48k = Vec::with_capacity(scratch_native.len());
        resampler.process(scratch_native, &mut out_48k);
        Ok(out_48k)
    }

    pub struct SystemAudioCapture {
        _client: IAudioClient,
        keepalive: Option<SilentLoopbackKeepAlive>,
        capture_client: IAudioCaptureClient,
        mix_format: NativeMixFormat,
        resampler: LinearStereoResampler,
        scratch: Vec<(f32, f32)>,
        last_qpc_100ns: Option<u64>,
        device_id: String,
    }

    unsafe impl Send for SystemAudioCapture {}
    unsafe impl Sync for SystemAudioCapture {}

    impl SystemAudioCapture {
        pub fn new() -> Result<Self, String> {
            ensure_coinit();
            let enumerator = create_enumerator()?;
            let device = get_default_device(&enumerator, eRender)?;
            let device_id = get_device_id(&device).unwrap_or_default();
            let keepalive = SilentLoopbackKeepAlive::new(&device);
            let (client, capture_client, mix_format) = open_capture_client(&device, true)?;
            let resampler =
                LinearStereoResampler::new(mix_format.sample_rate, TARGET_SAMPLE_RATE);
            Ok(Self {
                _client: client,
                keepalive,
                capture_client,
                mix_format,
                resampler,
                scratch: Vec::with_capacity(2048),
                last_qpc_100ns: None,
                device_id,
            })
        }

        pub fn device_id(&self) -> &str {
            &self.device_id
        }

        pub fn read_frames(&mut self) -> Result<Vec<(f32, f32)>, String> {
            if let Some(ref ka) = self.keepalive {
                ka.top_up();
            }
            read_capture(
                &self.capture_client,
                &self.mix_format,
                &mut self.resampler,
                &mut self.scratch,
                &mut self.last_qpc_100ns,
            )
        }
    }

    pub struct CommsAudioCapture {
        _client: IAudioClient,
        capture_client: IAudioCaptureClient,
        mix_format: NativeMixFormat,
        resampler: LinearStereoResampler,
        scratch: Vec<(f32, f32)>,
        last_qpc_100ns: Option<u64>,
    }

    unsafe impl Send for CommsAudioCapture {}
    unsafe impl Sync for CommsAudioCapture {}

    impl CommsAudioCapture {
        pub fn new_if_distinct() -> Option<Self> {
            ensure_coinit();
            let enumerator = create_enumerator().ok()?;
            let console_dev = unsafe {
                enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?
            };
            let comms_dev = unsafe {
                enumerator
                    .GetDefaultAudioEndpoint(eRender, eCommunications)
                    .ok()?
            };
            let console_id = get_device_id(&console_dev)?;
            let comms_id = get_device_id(&comms_dev)?;
            if console_id.eq_ignore_ascii_case(&comms_id) {
                return None;
            }
            let (client, capture_client, mix_format) =
                open_capture_client(&comms_dev, true).ok()?;
            let resampler =
                LinearStereoResampler::new(mix_format.sample_rate, TARGET_SAMPLE_RATE);
            Some(Self {
                _client: client,
                capture_client,
                mix_format,
                resampler,
                scratch: Vec::with_capacity(2048),
                last_qpc_100ns: None,
            })
        }

        pub fn read_frames(&mut self) -> Result<Vec<(f32, f32)>, String> {
            read_capture(
                &self.capture_client,
                &self.mix_format,
                &mut self.resampler,
                &mut self.scratch,
                &mut self.last_qpc_100ns,
            )
        }
    }

    pub struct MicAudioCapture {
        _client: IAudioClient,
        capture_client: IAudioCaptureClient,
        mix_format: NativeMixFormat,
        resampler: LinearStereoResampler,
        scratch: Vec<(f32, f32)>,
        last_qpc_100ns: Option<u64>,
        device_id: String,
        device_name: String,
    }

    unsafe impl Send for MicAudioCapture {}
    unsafe impl Sync for MicAudioCapture {}

    impl MicAudioCapture {
        pub fn new() -> Result<Self, String> {
            Self::new_with_device(None)
        }

        pub fn new_with_device(selector: Option<&str>) -> Result<Self, String> {
            ensure_coinit();
            let enumerator = create_enumerator()?;
            let clean_sel = selector
                .map(|s| s.trim())
                .filter(|s| {
                    !s.is_empty()
                        && !s.eq_ignore_ascii_case("default")
                        && !s.eq_ignore_ascii_case("auto")
                        && !s.eq_ignore_ascii_case("system")
                });

            let mut chosen_device: Option<IMMDevice> = None;
            if let Some(sel) = clean_sel {
                let available = list_input_devices();
                let sel_lower = sel.to_lowercase();
                let matched = available
                    .iter()
                    .find(|d| {
                        d.id.eq_ignore_ascii_case(sel) || d.name.eq_ignore_ascii_case(sel)
                    })
                    .or_else(|| {
                        sel.parse::<usize>()
                            .ok()
                            .and_then(|idx| available.get(idx))
                    })
                    .or_else(|| {
                        available
                            .iter()
                            .find(|d| d.name.to_lowercase().contains(&sel_lower))
                    });
                if let Some(info) = matched {
                    chosen_device = open_device_by_id(&enumerator, &info.id).ok();
                }
            }

            let device = match chosen_device {
                Some(d) => d,
                None => get_default_device(&enumerator, eCapture)?,
            };
            let device_id =
                get_device_id(&device).unwrap_or_else(|| "default".to_string());
            let device_name = get_device_friendly_name(&device)
                .unwrap_or_else(|| "System Default Microphone".to_string());

            let (client, capture_client, mix_format) = open_capture_client(&device, false)?;
            let resampler =
                LinearStereoResampler::new(mix_format.sample_rate, TARGET_SAMPLE_RATE);
            Ok(Self {
                _client: client,
                capture_client,
                mix_format,
                resampler,
                scratch: Vec::with_capacity(2048),
                last_qpc_100ns: None,
                device_id,
                device_name,
            })
        }

        pub fn device_id(&self) -> &str {
            &self.device_id
        }

        pub fn device_name(&self) -> &str {
            &self.device_name
        }

        pub fn read_frames(&mut self) -> Result<Vec<(f32, f32)>, String> {
            read_capture(
                &self.capture_client,
                &self.mix_format,
                &mut self.resampler,
                &mut self.scratch,
                &mut self.last_qpc_100ns,
            )
        }
    }

    pub struct AudioSessionSeparator {
        session_mgr: Option<IAudioSessionManager2>,
        cached_meters: Vec<(u32, String, IAudioMeterInformation)>,
        last_refresh: Instant,
        smooth_game: f32,
        smooth_discord: f32,
        smooth_other: f32,
    }

    unsafe impl Send for AudioSessionSeparator {}
    unsafe impl Sync for AudioSessionSeparator {}

    impl AudioSessionSeparator {
        pub fn new() -> Self {
            Self {
                session_mgr: None,
                cached_meters: Vec::new(),
                last_refresh: Instant::now() - Duration::from_secs(10),
                smooth_game: 1.0,
                smooth_discord: 0.0,
                smooth_other: 0.0,
            }
        }

        fn refresh_sessions(&mut self) {
            self.cached_meters.clear();
            unsafe {
                if self.session_mgr.is_none() {
                    ensure_coinit();
                    let Ok(enumerator) = create_enumerator() else {
                        return;
                    };
                    let Ok(dev) = get_default_device(&enumerator, eRender) else {
                        return;
                    };
                    let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                        return;
                    };
                    self.session_mgr = Some(mgr);
                }

                let Some(ref mgr) = self.session_mgr else {
                    return;
                };
                let Ok(session_enum): Result<IAudioSessionEnumerator, _> =
                    mgr.GetSessionEnumerator()
                else {
                    self.session_mgr = None;
                    return;
                };

                let Ok(count) = session_enum.GetCount() else {
                    return;
                };
                for idx in 0..count.min(48) {
                    let Ok(ctrl) = session_enum.GetSession(idx) else {
                        continue;
                    };
                    let pid = ctrl
                        .cast::<IAudioSessionControl2>()
                        .ok()
                        .and_then(|c2| c2.GetProcessId().ok())
                        .unwrap_or(0);
                    if pid != 0 {
                        if let Ok(meter) = ctrl.cast::<IAudioMeterInformation>() {
                            let exe = query_pid_exe_lower(pid);
                            self.cached_meters.push((pid, exe, meter));
                        }
                    }
                }
            }
        }

        pub fn sample_stem_weights(
            &mut self,
            active_pid_filter: Option<u32>,
            active_game_name: Option<&str>,
        ) -> (f32, f32, f32) {
            if self.last_refresh.elapsed() >= Duration::from_secs(2) {
                self.last_refresh = Instant::now();
                self.refresh_sessions();
            }

            let mut game_peak = 0.0f32;
            let mut discord_peak = 0.0f32;
            let mut other_peak = 0.0f32;

            unsafe {
                for (pid, exe, meter) in &self.cached_meters {
                    if let Ok(peak) = meter.GetPeakValue() {
                        if peak.is_finite() && peak > 0.0005 {
                            let ctx = crate::recorder::capture::classify_executable_context(
                                exe,
                                *pid,
                                active_pid_filter,
                                active_game_name,
                            );
                            match ctx {
                                crate::recorder::buffer::CaptureContext::Game => {
                                    if peak > game_peak {
                                        game_peak = peak;
                                    }
                                }
                                crate::recorder::buffer::CaptureContext::Social => {
                                    if peak > discord_peak {
                                        discord_peak = peak;
                                    }
                                }
                                _ => {
                                    if peak > other_peak {
                                        other_peak = peak;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let is_game_running = active_pid_filter.unwrap_or(0) != 0 || active_game_name.is_some();
            let tg = if game_peak > 0.0005 || is_game_running {
                1.0f32
            } else {
                0.0f32
            };
            let td = if discord_peak > 0.0005 {
                (discord_peak / (game_peak + discord_peak).max(0.001)).clamp(0.15, 1.0)
            } else {
                0.0f32
            };
            let to = if other_peak > 0.0005 {
                (other_peak / (game_peak + other_peak).max(0.001)).clamp(0.15, 1.0)
            } else if !is_game_running {
                1.0f32
            } else {
                0.0f32
            };

            self.smooth_game = self.smooth_game * 0.70 + tg * 0.30;
            self.smooth_discord = self.smooth_discord * 0.70 + td * 0.30;
            self.smooth_other = self.smooth_other * 0.70 + to * 0.30;

            (
                self.smooth_game.clamp(0.0, 1.0),
                self.smooth_discord.clamp(0.0, 1.0),
                self.smooth_other.clamp(0.0, 1.0),
            )
        }
    }
}

#[cfg(windows)]
pub use win::{
    list_input_devices, AudioSessionSeparator, CommsAudioCapture, MicAudioCapture,
    SystemAudioCapture,
};

#[cfg(not(windows))]
pub fn list_input_devices() -> Vec<AudioInputDeviceInfo> {
    vec![AudioInputDeviceInfo {
        id: "default".to_string(),
        name: "System Default Microphone".to_string(),
        is_default: true,
    }]
}

#[cfg(not(windows))]
pub struct SystemAudioCapture;

#[cfg(not(windows))]
impl SystemAudioCapture {
    pub fn new() -> Result<Self, String> {
        Err("audio capture not supported on non-windows platform".to_string())
    }
    pub fn read_frames(&mut self) -> Result<Vec<(f32, f32)>, String> {
        Ok(Vec::new())
    }
}

#[cfg(not(windows))]
pub struct CommsAudioCapture;

#[cfg(not(windows))]
impl CommsAudioCapture {
    pub fn new_if_distinct() -> Option<Self> {
        None
    }
    pub fn read_frames(&mut self) -> Result<Vec<(f32, f32)>, String> {
        Ok(Vec::new())
    }
}

#[cfg(not(windows))]
pub struct MicAudioCapture;

#[cfg(not(windows))]
impl MicAudioCapture {
    pub fn new() -> Result<Self, String> {
        Err("mic capture not supported on non-windows platform".to_string())
    }
    pub fn new_with_device(_selector: Option<&str>) -> Result<Self, String> {
        Err("mic capture not supported on non-windows platform".to_string())
    }
    pub fn device_id(&self) -> &str {
        "default"
    }
    pub fn device_name(&self) -> &str {
        "Default"
    }
    pub fn read_frames(&mut self) -> Result<Vec<(f32, f32)>, String> {
        Ok(Vec::new())
    }
}

#[cfg(not(windows))]
pub struct AudioSessionSeparator;

#[cfg(not(windows))]
impl AudioSessionSeparator {
    pub fn new() -> Self {
        Self
    }
    pub fn sample_stem_weights(
        &mut self,
        _active_pid_filter: Option<u32>,
        _active_game_name: Option<&str>,
    ) -> (f32, f32, f32) {
        (1.0, 0.0, 0.0)
    }
}

pub fn build_aac_encoder(kbps: u32) -> Option<fdk_aac_rs::encoder::Encoder> {
    fdk_aac_rs::encoder::Encoder::builder()
        .aot(fdk_aac_rs::encoder::Aot::AacLc)
        .sample_rate(48000)
        .channel_mode(fdk_aac_rs::encoder::ChannelMode::Stereo)
        .bitrate(fdk_aac_rs::encoder::Bitrate::Cbr(kbps.clamp(64, 192) * 1000))
        .afterburner(false)
        .transport(fdk_aac_rs::encoder::Transport::Raw)
        .build()
        .ok()
}

pub fn build_silent_aac_packet(kbps: u32) -> Bytes {
    if let Some(mut enc) = build_aac_encoder(kbps) {
        let zero_pcm = [0i16; 2048];
        let mut out_buf = [0u8; 2048];
        let mut last_bytes = Bytes::new();
        for _ in 0..3 {
            if let Ok(res) = enc.encode(&zero_pcm, &mut out_buf) {
                let n = res.out_bytes();
                if n > 0 {
                    last_bytes = Bytes::copy_from_slice(&out_buf[..n]);
                }
            }
        }
        if !last_bytes.is_empty() {
            return last_bytes;
        }
    }
    Bytes::from_static(&[0x21, 0x00, 0x49, 0x90, 0x02, 0x19, 0x00, 0x23, 0x80])
}

#[cfg(windows)]
pub fn spawn_audio_worker(
    a_cancel: CancellationToken,
    a_buffer: buffer::RingBuffer<buffer::CompressedPacket>,
    a_sink: Option<EventSink>,
    a_start_ms: u64,
    epoch_qpc: i64,
    qpc_freq: i64,
    a_shared_pid: Arc<AtomicUsize>,
    shared_audio_mode: Arc<AtomicUsize>,
    shared_audio_kbps: Arc<AtomicUsize>,
    shared_mic_muted: Arc<AtomicBool>,
    shared_separate_tracks: Arc<AtomicBool>,
    shared_mic_device: Arc<RwLock<Option<String>>>,
    a_shared_game: Arc<RwLock<Option<String>>>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("clotch-audio-aac".into())
        .spawn(move || {
            priority::set_audio_priority();

            let mut current_audio_mode: Option<AudioMode> = None;
            let mut current_audio_kbps: u32 = 0;
            let mut active_mic_selector: Option<String> = None;

            let mut system_audio: Option<SystemAudioCapture> = None;
            let mut comms_audio: Option<CommsAudioCapture> = None;
            let mut mic_audio: Option<MicAudioCapture> = None;
            let mut session_separator = AudioSessionSeparator::new();

            let mut aac_encoder: Option<fdk_aac_rs::encoder::Encoder> = None;
            let mut game_stem_enc: Option<fdk_aac_rs::encoder::Encoder> = None;
            let mut discord_stem_enc: Option<fdk_aac_rs::encoder::Encoder> = None;
            let mut mic_stem_enc: Option<fdk_aac_rs::encoder::Encoder> = None;
            let mut other_stem_enc: Option<fdk_aac_rs::encoder::Encoder> = None;
            let mut silent_aac_bytes = Bytes::new();

            let mut last_sys_retry = Instant::now() - Duration::from_secs(10);
            let mut last_comms_retry = Instant::now() - Duration::from_secs(10);
            let mut last_mic_retry = Instant::now() - Duration::from_secs(10);
            let mut last_default_dev_check = Instant::now();
            let dp_enumerator = win::list_input_devices();
            let _ = dp_enumerator;

            let mut sys_queue: VecDeque<(f32, f32)> = VecDeque::with_capacity(4096);
            let mut comms_queue: VecDeque<(f32, f32)> = VecDeque::with_capacity(4096);
            let mut mic_queue: VecDeque<(f32, f32)> = VecDeque::with_capacity(4096);
            let mut last_mic_sample: (f32, f32) = (0.0, 0.0);
            let mut total_encoded_frames: u64 = 0;
            let mut mode_start_ms: u64 = a_start_ms;
            let mut last_sys_data_instant = Instant::now();

            let mut chunk_pcm = vec![0i16; 2048];
            let mut game_pcm = vec![0i16; 2048];
            let mut discord_pcm = vec![0i16; 2048];
            let mut mic_pcm = vec![0i16; 2048];
            let mut other_pcm = vec![0i16; 2048];
            let mut out_buf = vec![0u8; 2048];

            while !a_cancel.is_cancelled() {
                let t_start = Instant::now();
                let cfg_audio = AudioMode::from_usize(shared_audio_mode.load(Ordering::Relaxed));
                let cfg_kbps = (shared_audio_kbps.load(Ordering::Relaxed) as u32).clamp(64, 192);
                let is_mic_muted = shared_mic_muted.load(Ordering::Relaxed);
                let separate_tracks = shared_separate_tracks.load(Ordering::Relaxed);
                let desired_mic_sel = shared_mic_device.tx_or_clone();

                if desired_mic_sel != active_mic_selector {
                    active_mic_selector = desired_mic_sel.clone();
                    mic_audio = None;
                    last_mic_retry = Instant::now() - Duration::from_secs(10);
                }

                if Some(cfg_audio) != current_audio_mode || cfg_kbps != current_audio_kbps {
                    current_audio_mode = Some(cfg_audio);
                    current_audio_kbps = cfg_kbps;
                    system_audio = None;
                    comms_audio = None;
                    mic_audio = None;
                    last_sys_retry = Instant::now() - Duration::from_secs(10);
                    last_comms_retry = Instant::now() - Duration::from_secs(10);
                    last_mic_retry = Instant::now() - Duration::from_secs(10);
                    if cfg_audio == AudioMode::None {
                        aac_encoder = None;
                        game_stem_enc = None;
                        discord_stem_enc = None;
                        mic_stem_enc = None;
                        other_stem_enc = None;
                    } else {
                        aac_encoder = build_aac_encoder(cfg_kbps);
                        game_stem_enc = build_aac_encoder(cfg_kbps);
                        discord_stem_enc = build_aac_encoder(cfg_kbps);
                        mic_stem_enc = build_aac_encoder(cfg_kbps);
                        other_stem_enc = build_aac_encoder(cfg_kbps);
                        silent_aac_bytes = build_silent_aac_packet(cfg_kbps);
                    }
                    let cur_qpc = capture::query_qpc();
                    let qpc_delta = (cur_qpc - epoch_qpc).max(0) as u128;
                    let rel_ms = ((qpc_delta * 1000) / (qpc_freq.max(1) as u128)) as u64;
                    mode_start_ms = a_start_ms.wrapping_add(rel_ms);
                    total_encoded_frames = 0;
                    last_sys_data_instant = Instant::now();
                    sys_queue.clear();
                    comms_queue.clear();
                    mic_queue.clear();
                }

                if cfg_audio != AudioMode::None {
                    let need_sys = matches!(cfg_audio, AudioMode::SystemOnly | AudioMode::Both);
                    let need_mic = matches!(cfg_audio, AudioMode::MicOnly | AudioMode::Both);

                    if need_sys
                        && system_audio.is_none()
                        && last_sys_retry.elapsed() >= Duration::from_secs(2)
                    {
                        last_sys_retry = Instant::now();
                        system_audio = SystemAudioCapture::new().ok();
                    }

                    if need_sys
                        && comms_audio.is_none()
                        && last_comms_retry.elapsed() >= Duration::from_secs(5)
                    {
                        last_comms_retry = Instant::now();
                        comms_audio = CommsAudioCapture::new_if_distinct();
                    }

                    if need_mic
                        && mic_audio.is_none()
                        && last_mic_retry.elapsed() >= Duration::from_secs(2)
                    {
                        last_mic_retry = Instant::now();
                        if let Ok(m) =
                            MicAudioCapture::new_with_device(active_mic_selector.as_deref())
                        {
                            if let Some(ref s) = a_sink {
                                s.emit(
                                    "audio.mic_ready",
                                    None,
                                    json!({
                                        "device_id": m.device_id(),
                                        "device_name": m.device_name(),
                                        "muted": is_mic_muted,
                                    }),
                                );
                            }
                            mic_audio = Some(m);
                        }
                    }

                    if let Some(ref mut a_enc) = aac_encoder {
                        if let Some(c) = system_audio.as_mut() {
                            match c.read_frames() {
                                Ok(frames) => {
                                    if !frames.is_empty() {
                                        last_sys_data_instant = Instant::now();
                                        sys_queue.extend(frames);
                                    }
                                }
                                Err(e) => {
                                    emit_error(&a_sink, "system_audio_read_failed", &e);
                                    system_audio = None;
                                }
                            }
                        }

                        if let Some(c) = comms_audio.as_mut() {
                            match c.read_frames() {
                                Ok(frames) => comms_queue.extend(frames),
                                Err(_) => {
                                    comms_audio = None;
                                }
                            }
                        }

                        if let Some(c) = mic_audio.as_mut() {
                            match c.read_frames() {
                                Ok(frames) => {
                                    if is_mic_muted {
                                        mic_queue.extend(
                                            std::iter::repeat((0.0f32, 0.0f32)).take(frames.len()),
                                        );
                                    } else {
                                        mic_queue.extend(frames);
                                    }
                                }
                                Err(e) => {
                                    emit_error(&a_sink, "mic_audio_read_failed", &e);
                                    mic_audio = None;
                                }
                            }
                        }

                        if need_sys {
                            let cur_qpc = capture::query_qpc();
                            let qpc_ms = a_start_ms.wrapping_add(
                                (((cur_qpc - epoch_qpc).max(0) as u128) * 1000
                                    / (qpc_freq.max(1) as u128))
                                    as u64,
                            );
                            let nominal_pts_ms = mode_start_ms
                                .wrapping_add((total_encoded_frames * 1000) / 48_000);
                            let buffered_ms = (sys_queue.len() as u64 * 1000) / 48_000;
                            let front_edge_ms = nominal_pts_ms.saturating_add(buffered_ms);
                            if qpc_ms > front_edge_ms.saturating_add(32)
                                && last_sys_data_instant.elapsed() >= Duration::from_millis(25)
                            {
                                let deficit_ms = (qpc_ms - front_edge_ms).min(250);
                                let fill_samples = ((deficit_ms * 48) as usize).clamp(512, 4096);
                                sys_queue.extend(
                                    std::iter::repeat((0.0f32, 0.0f32)).take(fill_samples),
                                );
                                last_sys_data_instant = Instant::now();
                            }
                        }

                        if sys_queue.len() > 14_400 {
                            let excess = sys_queue.len() - 4_800;
                            sys_queue.drain(..excess);
                        }
                        if need_sys {
                            let max_aux_len = sys_queue.len().saturating_add(2048);
                            if comms_queue.len() > max_aux_len {
                                let excess = comms_queue.len() - sys_queue.len().max(1024);
                                comms_queue.drain(..excess);
                            }
                            if mic_queue.len() > max_aux_len {
                                let excess = mic_queue.len() - sys_queue.len().max(1024);
                                mic_queue.drain(..excess);
                            }
                        } else if mic_queue.len() > 14_400 {
                            let excess = mic_queue.len() - 4_800;
                            mic_queue.drain(..excess);
                        }

                        let _ = &mut last_default_dev_check;

                        while (need_sys && sys_queue.len() >= 1024)
                            || (!need_sys && mic_queue.len() >= 1024)
                        {
                            let cur_qpc = capture::query_qpc();
                            let qpc_ms = a_start_ms.wrapping_add(
                                (((cur_qpc - epoch_qpc).max(0) as u128) * 1000
                                    / (qpc_freq.max(1) as u128))
                                    as u64,
                            );
                            let remaining_frames = if need_sys {
                                sys_queue.len() as u64
                            } else {
                                mic_queue.len() as u64
                            };
                            let buffered_ms = (remaining_frames * 1000) / 48_000;
                            let measured_pts_ms = qpc_ms.saturating_sub(buffered_ms);
                            let nominal_pts_ms = mode_start_ms
                                .wrapping_add((total_encoded_frames * 1000) / 48_000);
                            let drift_ms = measured_pts_ms as i64 - nominal_pts_ms as i64;
                            if drift_ms.abs() > 90 {
                                mode_start_ms = measured_pts_ms
                                    .saturating_sub((total_encoded_frames * 1000) / 48_000);
                            } else if drift_ms > 12 && (total_encoded_frames / 1024) % 4 == 0 {
                                mode_start_ms = mode_start_ms.wrapping_add(1);
                            } else if drift_ms < -12 && (total_encoded_frames / 1024) % 4 == 0 {
                                mode_start_ms = mode_start_ms.saturating_sub(1);
                            }
                            let audio_pts_ms = mode_start_ms
                                .wrapping_add((total_encoded_frames * 1000) / 48_000);

                            let pid_val = a_shared_pid.load(Ordering::Relaxed) as u32;
                            let active_pid_opt = if pid_val != 0 { Some(pid_val) } else { None };
                            let active_game_opt = a_shared_game.tx_or_clone();

                            let (w_game, w_discord, w_other) = if separate_tracks && need_sys {
                                session_separator.sample_stem_weights(
                                    active_pid_opt,
                                    active_game_opt.as_deref(),
                                )
                            } else {
                                (1.0f32, 0.0f32, 0.0f32)
                            };

                            for frame_idx in 0..1024 {
                                let (sl, sr) = if need_sys {
                                    sys_queue.pop_front().unwrap_or((0.0, 0.0))
                                } else {
                                    (0.0, 0.0)
                                };
                                let (cl, cr) = if comms_audio.is_some() {
                                    comms_queue.pop_front().unwrap_or((0.0, 0.0))
                                } else {
                                    (0.0, 0.0)
                                };
                                let (raw_ml, raw_mr) = if need_mic {
                                    if need_sys && mic_queue.len() > sys_queue.len() + 1024 && frame_idx % 64 == 0 {
                                        let a = mic_queue.pop_front().unwrap_or(last_mic_sample);
                                        let b = mic_queue.pop_front().unwrap_or(a);
                                        last_mic_sample = ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
                                        last_mic_sample
                                    } else if let Some(s) = mic_queue.pop_front() {
                                        last_mic_sample = s;
                                        s
                                    } else {
                                        last_mic_sample = (last_mic_sample.0 * 0.96, last_mic_sample.1 * 0.96);
                                        last_mic_sample
                                    }
                                } else {
                                    last_mic_sample = (0.0, 0.0);
                                    (0.0, 0.0)
                                };
                                let (ml, mr) = if is_mic_muted {
                                    (0.0, 0.0)
                                } else {
                                    (raw_ml, raw_mr)
                                };

                                let mix_l = (sl + cl + ml).clamp(-1.0, 1.0);
                                let mix_r = (sr + cr + mr).clamp(-1.0, 1.0);
                                chunk_pcm[frame_idx * 2] = (mix_l * 32767.0) as i16;
                                chunk_pcm[frame_idx * 2 + 1] = (mix_r * 32767.0) as i16;

                                if separate_tracks {
                                    let gl = (sl * w_game).clamp(-1.0, 1.0);
                                    let gr = (sr * w_game).clamp(-1.0, 1.0);
                                    let dl = (sl * w_discord + cl).clamp(-1.0, 1.0);
                                    let dr = (sr * w_discord + cr).clamp(-1.0, 1.0);
                                    let ol = (sl * w_other).clamp(-1.0, 1.0);
                                    let or_ = (sr * w_other).clamp(-1.0, 1.0);

                                    game_pcm[frame_idx * 2] = (gl * 32767.0) as i16;
                                    game_pcm[frame_idx * 2 + 1] = (gr * 32767.0) as i16;
                                    discord_pcm[frame_idx * 2] = (dl * 32767.0) as i16;
                                    discord_pcm[frame_idx * 2 + 1] = (dr * 32767.0) as i16;
                                    mic_pcm[frame_idx * 2] = (ml * 32767.0) as i16;
                                    mic_pcm[frame_idx * 2 + 1] = (mr * 32767.0) as i16;
                                    other_pcm[frame_idx * 2] = (ol * 32767.0) as i16;
                                    other_pcm[frame_idx * 2 + 1] = (or_ * 32767.0) as i16;
                                }
                            }

                            if let Ok(out) = a_enc.encode(&chunk_pcm, &mut out_buf) {
                                let out_len = out.out_bytes();
                                if out_len > 0 {
                                    let master_bytes = Bytes::copy_from_slice(&out_buf[..out_len]);
                                    let stems_arc = if separate_tracks {
                                        let stem_t0 = Instant::now();
                                        let game_bytes = game_stem_enc
                                            .as_mut()
                                            .and_then(|e| e.encode(&game_pcm, &mut out_buf).ok())
                                            .filter(|r| r.out_bytes() > 0)
                                            .map(|r| Bytes::copy_from_slice(&out_buf[..r.out_bytes()]))
                                            .unwrap_or_else(|| silent_aac_bytes.clone());

                                        let discord_bytes = discord_stem_enc
                                            .as_mut()
                                            .and_then(|e| e.encode(&discord_pcm, &mut out_buf).ok())
                                            .filter(|r| r.out_bytes() > 0)
                                            .map(|r| Bytes::copy_from_slice(&out_buf[..r.out_bytes()]))
                                            .unwrap_or_else(|| silent_aac_bytes.clone());

                                        let mic_bytes = mic_stem_enc
                                            .as_mut()
                                            .and_then(|e| e.encode(&mic_pcm, &mut out_buf).ok())
                                            .filter(|r| r.out_bytes() > 0)
                                            .map(|r| Bytes::copy_from_slice(&out_buf[..r.out_bytes()]))
                                            .unwrap_or_else(|| silent_aac_bytes.clone());

                                        let other_bytes = other_stem_enc
                                            .as_mut()
                                            .and_then(|e| e.encode(&other_pcm, &mut out_buf).ok())
                                            .filter(|r| r.out_bytes() > 0)
                                            .map(|r| Bytes::copy_from_slice(&out_buf[..r.out_bytes()]))
                                            .unwrap_or_else(|| silent_aac_bytes.clone());

                                        let stem_encode_us = stem_t0.elapsed().as_micros() as u64;
                                        if total_encoded_frames % (48_000 * 30) < 1024 {
                                            diag::log_line(
                                                "AUDIO_STEMS",
                                                &format!("stem_encode_us={stem_encode_us} frames={total_encoded_frames}"),
                                            );
                                        }
                                        Some(Arc::new(buffer::AudioStemPackets {
                                            game: game_bytes,
                                            discord: discord_bytes,
                                            mic: mic_bytes,
                                            other: other_bytes,
                                        }))
                                    } else {
                                        None
                                    };

                                    let compressed = buffer::CompressedPacket {
                                        kind: buffer::PacketKind::Audio,
                                        pts_ms: audio_pts_ms,
                                        pts: total_encoded_frames as i64,
                                        dts: total_encoded_frames as i64,
                                        duration: 1024,
                                        is_keyframe: true,
                                        data: master_bytes,
                                        stems: stems_arc,
                                    };
                                    a_buffer.push_audio(audio_pts_ms, Arc::new(compressed));
                                }
                            }
                            total_encoded_frames += 1024;
                        }
                    }
                }

                let elapsed = t_start.elapsed();
                let target_interval = if cfg_audio == AudioMode::None {
                    Duration::from_millis(50)
                } else if sys_queue.len() >= 2048 || mic_queue.len() >= 2048 {
                    Duration::from_millis(2)
                } else {
                    Duration::from_millis(6)
                };
                if elapsed < target_interval {
                    std::thread::sleep(target_interval - elapsed);
                }
            }
        })
        .ok()
}

#[cfg(not(windows))]
pub fn spawn_audio_worker(
    _a_cancel: CancellationToken,
    _a_buffer: buffer::RingBuffer<buffer::CompressedPacket>,
    _a_sink: Option<EventSink>,
    _a_start_ms: u64,
    _epoch_qpc: i64,
    _qpc_freq: i64,
    _a_shared_pid: Arc<AtomicUsize>,
    _shared_audio_mode: Arc<AtomicUsize>,
    _shared_audio_kbps: Arc<AtomicUsize>,
    _shared_mic_muted: Arc<AtomicBool>,
    _shared_separate_tracks: Arc<AtomicBool>,
    _shared_mic_device: Arc<RwLock<Option<String>>>,
    _a_shared_game: Arc<RwLock<Option<String>>>,
) -> Option<std::thread::JoinHandle<()>> {
    None
}