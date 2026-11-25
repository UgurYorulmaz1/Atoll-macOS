// macOS volume: level / mute of the default output device through CoreAudio.
// With Accessibility access, a CGEventTap takes over the volume keys: the system HUD is suppressed and the island
// shows the level instead (same 1/16 steps as macOS; ⌥⇧ + volume key = fine 1/64 steps). Without access the
// keys keep their normal behaviour and the island still follows every volume change.
use crate::macos::ffi::*;
use crate::util::emit;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static TX: OnceLock<Mutex<Sender<String>>> = OnceLock::new();
static KEYS: AtomicU32 = AtomicU32::new(0);

pub fn send(cmd: &str) {
    if let Some(tx) = TX.get() { let _ = tx.lock().unwrap().send(cmd.to_string()); }
}

fn default_output() -> Option<AudioObjectID> {
    unsafe { ca_get::<AudioObjectID>(kAudioObjectSystemObject, addr(kDefaultOutputDevice, kScopeGlobal)).filter(|d| *d != 0) }
}

/// Volume 0..1 of a device: virtual main volume, otherwise the average of the channel volumes
unsafe fn get_volume(dev: AudioObjectID) -> Option<f32> {
    if let Some(v) = ca_get::<f32>(dev, addr(kVirtualMainVolume, kScopeOutput)) { return Some(v); }
    if let Some(v) = ca_get::<f32>(dev, addr(kVolumeScalar, kScopeOutput)) { return Some(v); }
    let ch: Vec<f32> = (1..=2).filter_map(|c| ca_get::<f32>(dev, AudioObjectPropertyAddress { selector: kVolumeScalar, scope: kScopeOutput, element: c })).collect();
    if ch.is_empty() { None } else { Some(ch.iter().sum::<f32>() / ch.len() as f32) }
}

unsafe fn set_volume(dev: AudioObjectID, v: f32) {
    let v = v.clamp(0.0, 1.0);
    if ca_set(dev, addr(kVirtualMainVolume, kScopeOutput), v) { return; }
    if ca_set(dev, addr(kVolumeScalar, kScopeOutput), v) { return; }
    for c in 1..=2 { ca_set(dev, AudioObjectPropertyAddress { selector: kVolumeScalar, scope: kScopeOutput, element: c }, v); }
}

unsafe fn get_mute(dev: AudioObjectID) -> bool { ca_get::<u32>(dev, addr(kMute, kScopeOutput)).unwrap_or(0) != 0 }
unsafe fn set_mute(dev: AudioObjectID, m: bool) { ca_set::<u32>(dev, addr(kMute, kScopeOutput), m as u32); }

fn apply(cmd: &str) {
    let Some(dev) = default_output() else { return };
    unsafe {
        let v = get_volume(dev).unwrap_or(0.0);
        let mut parts = cmd.split(' ');
        match parts.next().unwrap_or("") {
            c @ ("up" | "down" | "up-fine" | "down-fine") => {
                let step = if c.ends_with("fine") { 1.0 / 64.0 } else { 1.0 / 16.0 };
                let steps = 1.0 / step;
                let d = if c.starts_with("up") { 1.0 } else { -1.0 };
                let n = (((v * steps).round() + d) / steps).clamp(0.0, 1.0);
                set_volume(dev, n);
                if c.starts_with("up") { set_mute(dev, false); }
            }
            "mute" => set_mute(dev, !get_mute(dev)),
            "set" => {
                let x: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(v).clamp(0.0, 1.0);
                set_volume(dev, x);
                if x > 0.0 { set_mute(dev, false); }
            }
            _ => {}
        }
    }
}

fn state() -> Option<(u32, bool)> {
    let dev = default_output()?;
    unsafe { Some(((get_volume(dev)? * 100.0).round() as u32, get_mute(dev))) }
}

pub fn start(hook: bool) {
    let (tx, rx) = channel::<String>();
    let _ = TX.set(Mutex::new(tx));
    std::thread::spawn(move || {
        let mut last = (u32::MAX, false, u32::MAX);
        let mut tick: u32 = 0;
        loop {
            match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(cmd) => apply(&cmd),
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            tick = tick.wrapping_add(1);
            if let Some((level, muted)) = state() {
                let cur = (level, muted, KEYS.load(Ordering::Relaxed));
                if cur != last || tick % 60 == 0 {
                    last = cur;
                    emit("media", json!({ "type": "volume", "level": level, "muted": muted, "keys": cur.2 }));
                }
            }
        }
    });
    if hook { std::thread::spawn(run_key_tap); }
}

/* ---------- volume keys (CGEventTap on NX_SYSDEFINED media key events) ---------- */
fn run_key_tap() {
    // wait until the user grants Accessibility access (System Settings > Privacy & Security > Accessibility)
    loop {
        if crate::macos::accessibility_trusted() { break; }
        std::thread::sleep(Duration::from_secs(5));
    }
    unsafe {
        let mask: u64 = 1 << NX_SYSDEFINED;
        let tap = CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap, kCGEventTapOptionDefault, mask, tap_cb, std::ptr::null_mut());
        if tap.is_null() { eprintln!("volume keys: event tap not available"); return; }
        TAP.store(tap as usize, Ordering::Relaxed);
        let src = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), src, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        CFRunLoopRun();
    }
}

static TAP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const NX_KEYTYPE_SOUND_UP: isize = 0;
const NX_KEYTYPE_SOUND_DOWN: isize = 1;
const NX_KEYTYPE_MUTE: isize = 7;

unsafe extern "C" fn tap_cb(_proxy: CGEventTapProxy, ty: u32, ev: CGEventRef, _user: *mut std::ffi::c_void) -> CGEventRef {
    if ty == kCGEventTapDisabledByTimeout || ty == kCGEventTapDisabledByUserInput {
        let t = TAP.load(Ordering::Relaxed);
        if t != 0 { CGEventTapEnable(t as *mut _, true); }
        return ev;
    }
    if ty != NX_SYSDEFINED { return ev; }
    let Some(nse): Option<objc2::rc::Retained<objc2_app_kit::NSEvent>> =
        objc2::msg_send![objc2::class!(NSEvent), eventWithCGEvent: ev] else { return ev };
    let subtype: i16 = objc2::msg_send![&*nse, subtype];
    if subtype != 8 { return ev; }   // NX_SUBTYPE_AUX_CONTROL_BUTTONS
    let data1: isize = objc2::msg_send![&*nse, data1];
    let key = (data1 & 0xFFFF_0000) >> 16;
    let state = (data1 & 0xFF00) >> 8;   // 0xA = down, 0xB = up
    if !matches!(key, NX_KEYTYPE_SOUND_UP | NX_KEYTYPE_SOUND_DOWN | NX_KEYTYPE_MUTE) { return ev; }
    if state == 0xA {
        let flags = CGEventGetFlags(ev);
        let fine = flags & kCGEventFlagMaskShift != 0 && flags & kCGEventFlagMaskAlternate != 0;
        KEYS.fetch_add(1, Ordering::Relaxed);
        send(match (key, fine) {
            (NX_KEYTYPE_SOUND_UP, false) => "up",
            (NX_KEYTYPE_SOUND_UP, true) => "up-fine",
            (NX_KEYTYPE_SOUND_DOWN, false) => "down",
            (NX_KEYTYPE_SOUND_DOWN, true) => "down-fine",
            _ => "mute",
        });
    }
    std::ptr::null_mut()   // swallow -> no system volume HUD, the island shows it
}

/* ---------- output devices (quick switcher: speakers <-> AirPods ...) ---------- */
fn has_output_streams(dev: AudioObjectID) -> bool {
    unsafe {
        let a = addr(kStreams, kScopeOutput);
        let mut size = 0u32;
        AudioObjectGetPropertyDataSize(dev, &a, 0, std::ptr::null(), &mut size) == 0 && size > 0
    }
}

pub fn device_name(dev: AudioObjectID) -> String {
    unsafe { ca_get_string(dev, addr(kName, kScopeGlobal)).unwrap_or_else(|| format!("Audio {dev}")) }
}

#[allow(dead_code)]
pub fn is_bluetooth(dev: AudioObjectID) -> bool {
    unsafe { matches!(ca_get::<u32>(dev, addr(kTransportType, kScopeGlobal)), Some(t) if t == kTransportBluetooth || t == kTransportBluetoothLE) }
}

/// Active audio outputs: (UID, Name, IsDefault)
pub fn list_outputs() -> Vec<(String, String, bool)> {
    let def = default_output().unwrap_or(0);
    let devs: Vec<AudioObjectID> = unsafe { ca_get_vec(kAudioObjectSystemObject, addr(kDevices, kScopeGlobal)) };
    devs.into_iter().filter(|d| has_output_streams(*d)).filter_map(|d| {
        let uid = unsafe { ca_get_string(d, addr(kDeviceUID, kScopeGlobal)) }?;
        Some((uid, device_name(d), d == def))
    }).collect()
}

fn device_by_uid(uid: &str) -> Option<AudioObjectID> {
    let devs: Vec<AudioObjectID> = unsafe { ca_get_vec(kAudioObjectSystemObject, addr(kDevices, kScopeGlobal)) };
    devs.into_iter().find(|d| unsafe { ca_get_string(*d, addr(kDeviceUID, kScopeGlobal)) }.as_deref() == Some(uid))
}

/// One-click switch between outputs; also moves system sounds to the same device
#[tauri::command]
pub fn switch_audio_output() -> serde_json::Value {
    if !crate::settings::get().audio_switch { return json!({ "ok": false, "disabled": true }); }
    let list = list_outputs();
    if list.is_empty() { return json!({ "ok": false, "devices": [] }); }
    if list.len() == 1 { return json!({ "ok": true, "switched": false, "name": list[0].1, "devices": list }); }
    let cur = list.iter().position(|x| x.2).unwrap_or(0);
    let (next_uid, next_name, _) = &list[(cur + 1) % list.len()];
    let ok = device_by_uid(next_uid).map(|d| unsafe {
        let a = ca_set(kAudioObjectSystemObject, addr(kDefaultOutputDevice, kScopeGlobal), d);
        ca_set(kAudioObjectSystemObject, addr(kDefaultSystemOutputDevice, kScopeGlobal), d);
        a
    }).unwrap_or(false);
    json!({ "ok": ok, "switched": ok, "name": next_name, "devices": list_outputs() })
}

/// Is any input device (microphone) in use by some process right now? (public CoreAudio, no permission needed)
pub fn mic_in_use() -> bool {
    let devs: Vec<AudioObjectID> = unsafe { ca_get_vec(kAudioObjectSystemObject, addr(kDevices, kScopeGlobal)) };
    devs.into_iter().any(|d| unsafe {
        let a = addr(kStreams, kScopeInput);
        let mut size = 0u32;
        let is_input = AudioObjectGetPropertyDataSize(d, &a, 0, std::ptr::null(), &mut size) == 0 && size > 0;
        is_input && ca_get::<u32>(d, addr(kIsRunningSomewhere, kScopeGlobal)).unwrap_or(0) != 0
    })
}
