// Volume: level/mute via Core Audio; a low-level keyboard hook captures the volume keys
// (the Windows volume flyout is suppressed and the island shows the level instead). Keys return to normal when the app exits.
use crate::util::emit;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::core::{Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{LPARAM, LRESULT, PROPERTYKEY, WPARAM};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eCommunications, eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::*;

static TX: OnceLock<Mutex<Sender<String>>> = OnceLock::new();
static KEYS: AtomicU32 = AtomicU32::new(0);

pub fn send(cmd: &str) {
    if let Some(tx) = TX.get() { let _ = tx.lock().unwrap().send(cmd.to_string()); }
}

fn endpoint() -> Result<IAudioEndpointVolume> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
        dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    }
}

fn apply(cmd: &str) -> Result<()> {
    let ep = endpoint()?;
    unsafe {
        let v = ep.GetMasterVolumeLevelScalar()?;
        let mut parts = cmd.split(' ');
        match parts.next().unwrap_or("") {
            "up" | "down" => {
                let d = if cmd == "up" { 0.02 } else { -0.02 };
                let n = (((v + d) * 50.0).round() / 50.0).clamp(0.0, 1.0);   // 2% steps
                ep.SetMasterVolumeLevelScalar(n, std::ptr::null())?;
                if cmd == "up" { ep.SetMute(false, std::ptr::null())?; }
            }
            "mute" => { let m = ep.GetMute()?.as_bool(); ep.SetMute(!m, std::ptr::null())?; }
            "set" => {
                let x: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(v).clamp(0.0, 1.0);
                ep.SetMasterVolumeLevelScalar(x, std::ptr::null())?;
                if x > 0.0 { ep.SetMute(false, std::ptr::null())?; }
            }
            _ => {}
        }
    }
    Ok(())
}

fn state() -> Option<(u32, bool)> {
    let ep = endpoint().ok()?;
    unsafe { Some(((ep.GetMasterVolumeLevelScalar().ok()? * 100.0).round() as u32, ep.GetMute().ok()?.as_bool())) }
}

pub fn start(hook: bool) {
    let (tx, rx) = channel::<String>();
    let _ = TX.set(Mutex::new(tx));
    std::thread::spawn(move || {
        unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
        let mut last = (u32::MAX, false, u32::MAX);
        let mut tick: u32 = 0;
        loop {
            match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(cmd) => { let _ = apply(&cmd); }
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            tick = tick.wrapping_add(1);
            if let Some((level, muted)) = state() {
                let cur = (level, muted, KEYS.load(Ordering::Relaxed));
                // send immediately on change; also resend every 3 s (in case the UI loads late; not counted as a change)
                if cur != last || tick % 60 == 0 {
                    last = cur;
                    emit("media", json!({ "type": "volume", "level": level, "muted": muted, "keys": cur.2 }));
                }
            }
        }
    });
    if hook {
        std::thread::spawn(|| unsafe {
            let hmod = GetModuleHandleW(None).ok();
            let _hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), hmod.map(|h| h.into()), 0);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        });
    }
}

unsafe extern "system" fn kb_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let vk = kb.vkCode;
        if vk == 0xAD || vk == 0xAE || vk == 0xAF {   // mute / down / up
            let m = wparam.0 as u32;
            if m == WM_KEYDOWN || m == WM_SYSKEYDOWN {
                KEYS.fetch_add(1, Ordering::Relaxed);
                send(match vk { 0xAF => "up", 0xAE => "down", _ => "mute" });
            }
            return LRESULT(1);   // swallow the key -> no Windows volume flyout
        }
        // Global shortcut: Ctrl + Shift + Space (0x20) -> open the island / Spotify search or shelf
        if vk == 0x20 && (wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN) {
            let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
            let shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
            if ctrl && shift && crate::settings::get().global_shortcut {
                emit("shortcut", json!({ "action": "toggle-expand" }));
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/* ---------- Quick audio output switcher (IPolicyConfig COM) ---------- */
#[repr(C)]
struct IPolicyConfigVtbl {
    base: windows::core::IUnknown_Vtbl,
    get_mix_format: usize,
    get_device_format: usize,
    reset_device_format: usize,
    set_device_format: usize,
    get_processing_period: usize,
    set_processing_period: usize,
    get_share_mode: usize,
    set_share_mode: usize,
    get_property_value: usize,
    set_property_value: usize,
    set_default_endpoint: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, u32) -> HRESULT,
    set_endpoint_visibility: usize,
}

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

unsafe fn propvariant_to_string(pv: &PROPVARIANT) -> Option<String> {
    // VT_LPWSTR = 31; pointer offset 8 inside PROPVARIANT union
    let raw = pv as *const PROPVARIANT as *const u8;
    let vt = *(raw as *const u16);
    if vt != 31 { return None; }
    let ptr = *(raw.add(8) as *const *const u16);
    if ptr.is_null() { return None; }
    let mut len = 0usize;
    while *ptr.add(len) != 0 { len += 1; }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len)))
}

unsafe fn set_default_audio_device(dev_id: &str) -> bool {
    let w = crate::util::wide(dev_id);
    let mut obj: *mut std::ffi::c_void = std::ptr::null_mut();
    if windows::Win32::System::Com::CoCreateInstance(
        &CLSID_POLICY_CONFIG,
        None,
        CLSCTX_ALL,
    ).ok().and_then(|unk: windows::core::IUnknown| {
        let mut p = std::ptr::null_mut();
        (unk.vtable().QueryInterface)(unk.as_raw(), &IID_IPOLICY_CONFIG, &mut p).ok().ok()?;
        obj = p;
        Some(())
    }).is_none() || obj.is_null() {
        return false;
    }
    let vtbl = *(obj as *const *const IPolicyConfigVtbl);
    for role in [eConsole.0 as u32, eMultimedia.0 as u32, eCommunications.0 as u32] {
        let _ = ((*vtbl).set_default_endpoint)(obj, PCWSTR(w.as_ptr()), role);
    }
    let unk = windows::core::IUnknown::from_raw(obj);
    drop(unk);
    true
}

/// Lists active audio output devices (Id, Name, IsSelected)
pub fn list_outputs() -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return out };
        let def_id = en.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()
            .and_then(|d| d.GetId().ok())
            .and_then(|p| p.to_string().ok())
            .unwrap_or_default();
        let Ok(col) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return out };
        let count = col.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(dev) = col.Item(i) else { continue };
            let Ok(id_pw) = dev.GetId() else { continue };
            let Ok(id) = id_pw.to_string() else { continue };
            let name = dev.OpenPropertyStore(STGM_READ).ok()
                .and_then(|st| st.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok())
                .and_then(|pv| propvariant_to_string(&pv))
                .unwrap_or_else(|| id.clone());
            let is_def = id == def_id;
            out.push((id, name, is_def));
        }
    }
    out
}

/// One-click switch between speakers <-> headphones; returns the new device's name
#[tauri::command]
pub fn switch_audio_output() -> serde_json::Value {
    if !crate::settings::get().audio_switch {
        return json!({ "ok": false, "disabled": true });
    }
    let list = list_outputs();
    if list.is_empty() {
        return json!({ "ok": false, "devices": [] });
    }
    if list.len() == 1 {
        return json!({ "ok": true, "switched": false, "name": list[0].1, "devices": list });
    }
    let cur_idx = list.iter().position(|x| x.2).unwrap_or(0);
    let next_idx = (cur_idx + 1) % list.len();
    let (next_id, next_name, _) = &list[next_idx];
    let ok = unsafe { set_default_audio_device(next_id) };
    json!({
        "ok": ok,
        "switched": ok,
        "name": next_name,
        "devices": list_outputs(),
    })
}
