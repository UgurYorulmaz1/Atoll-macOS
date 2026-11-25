// Live system & gaming monitor (CPU / GPU / RAM / download speed),
// microphone & camera privacy dot (ConsentStore),
// Bluetooth & headphones-connected animation + battery percentage (SetupAPI DEVPKEY_Bluetooth_Battery).
use crate::util::emit;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::sync::Mutex;
use std::time::Duration;
use windows::core::{w, GUID};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDevicePropertyW, DIGCF_ALLCLASSES, DIGCF_PRESENT, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Properties::DEVPROPTYPE;
use windows::Win32::Foundation::{DEVPROPKEY, FILETIME};
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

static LAST_SNAP: Mutex<Option<Value>> = Mutex::new(None);

const CREATE_NO_WINDOW: u32 = 0x08000000;

// DEVPKEY_Device_FriendlyName: {a45c254e-df1c-4efd-8020-67d146a850e0}, 14
const DEVPKEY_NAME: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};
// DEVPKEY_Bluetooth_Battery: {104ea319-6ee2-4701-bd47-8ddbf425bbe5}, 2
const DEVPKEY_BT_BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5),
    pid: 2,
};
// DEVPKEY_Bluetooth_DeviceAddress: {2bd67d8b-8beb-48d5-87e0-6cda3428040a}, 1
const DEVPKEY_BT_ADDR: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x2bd67d8b_8beb_48d5_87e0_6cda3428040a),
    pid: 1,
};

fn ft_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

/// (idle, kernel, user)
fn cpu_times() -> (u64, u64, u64) {
    unsafe {
        let (mut idle, mut kern, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        let _ = GetSystemTimes(Some(&mut idle), Some(&mut kern), Some(&mut user));
        (ft_to_u64(idle), ft_to_u64(kern), ft_to_u64(user))
    }
}

/// (used_ram_percent, used_gb, total_gb)
fn ram_usage() -> (u32, f64, f64) {
    unsafe {
        let mut m = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut m).is_ok() {
            let tot = m.ullTotalPhys as f64 / 1073741824.0;
            let used = (m.ullTotalPhys.saturating_sub(m.ullAvailPhys)) as f64 / 1073741824.0;
            (m.dwMemoryLoad, (used * 10.0).round() / 10.0, (tot * 10.0).round() / 10.0)
        } else {
            (0, 0.0, 0.0)
        }
    }
}

/// Total received/sent network bytes (active interfaces, excluding loopback)
fn net_bytes() -> (u64, u64) {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).is_err() || table.is_null() {
            return (0, 0);
        }
        let t = &*table;
        let rows = std::slice::from_raw_parts(t.Table.as_ptr(), t.NumEntries as usize);
        let (mut rx, mut tx) = (0u64, 0u64);
        for r in rows {
            // IF_TYPE_SOFTWARE_LOOPBACK = 24
            if r.Type != 24 && r.OperStatus == IfOperStatusUp {
                rx = rx.wrapping_add(r.InOctets);
                tx = tx.wrapping_add(r.OutOctets);
            }
        }
        FreeMibTable(table as *const _);
        (rx, tx)
    }
}

/// NVIDIA GPU temperature (C) and load (%)
fn query_gpu() -> Option<(u32, u32)> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=temperature.gpu,utilization.gpu", "--format=csv,noheader,nounits"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?.trim();
    let mut parts = line.split(',');
    let temp: f64 = parts.next()?.trim().parse().ok()?;
    let util: f64 = parts.next()?.trim().parse().ok()?;
    Some((temp.round() as u32, util.round() as u32))
}

/// Finds the apps currently using the microphone or camera (Windows ConsentStore)
fn check_privacy_store(kind: &str) -> Vec<String> {
    let mut active = Vec::new();
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let base = format!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{kind}");
    let Ok(root) = hkcu.open_subkey(&base) else { return active };

    let mut check_sub = |sub: &winreg::RegKey, raw_name: &str| {
        let start: u64 = sub.get_value("LastUsedTimeStart").unwrap_or(0);
        let stop: u64 = sub.get_value("LastUsedTimeStop").unwrap_or(1);
        if start > 0 && stop == 0 {
            let clean = if raw_name.contains('#') {
                raw_name.rsplit('#').next().unwrap_or(raw_name).replace(".exe", "")
            } else {
                raw_name.split('_').next().unwrap_or(raw_name).rsplit('.').next().unwrap_or(raw_name).to_string()
            };
            if !clean.is_empty() && !active.contains(&clean) {
                active.push(clean);
            }
        }
    };

    for name in root.enum_keys().flatten() {
        let Ok(sub) = root.open_subkey(&name) else { continue };
        if name == "NonPackaged" {
            for np_name in sub.enum_keys().flatten() {
                if let Ok(np_sub) = sub.open_subkey(&np_name) {
                    check_sub(&np_sub, &np_name);
                }
            }
        } else {
            check_sub(&sub, &name);
        }
    }
    active
}

/// Is OBS, Xbox Game Bar or another screen recorder recording video right now?
/// Returns (AppName, Secs) if an .mp4/.mkv/.flv/.mov/.ts/.avi file changed size/write time in the last 4 seconds or a recording state is active.
fn detect_active_recording() -> Option<(String, u64)> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(up) = std::env::var("USERPROFILE") {
        let v = std::path::PathBuf::from(&up).join("Videos");
        dirs.push(v.join("Captures"));
        dirs.push(v.clone());
    }
    // OBS global.ini -> active profile -> FilePath / RecFilePath in basic.ini
    if let Ok(ap) = std::env::var("APPDATA") {
        let obs_base = std::path::PathBuf::from(&ap).join("obs-studio").join("basic").join("profiles");
        if let Ok(rd) = std::fs::read_dir(&obs_base) {
            for e in rd.flatten() {
                let ini = e.path().join("basic.ini");
                if let Ok(txt) = std::fs::read_to_string(&ini) {
                    for line in txt.lines() {
                        let t = line.trim();
                        if let Some(p) = t.strip_prefix("FilePath=").or_else(|| t.strip_prefix("RecFilePath=")) {
                            let pb = std::path::PathBuf::from(p.trim());
                            if pb.is_dir() && !dirs.contains(&pb) {
                                dirs.push(pb);
                            }
                        }
                    }
                }
            }
        }
    }

    let now = std::time::SystemTime::now();
    let exts = ["mkv", "mp4", "flv", "mov", "ts", "avi", "webm"];
    for d in &dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() { continue; }
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if !exts.contains(&ext.as_str()) { continue; }
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            if meta.len() < 1024 { continue; }
            let Ok(mtime) = meta.modified() else { continue };
            let Ok(age) = now.duration_since(mtime) else { continue };
            // written to disk within the last 4.5 s means a recording is live
            if age.as_secs_f64() <= 4.5 {
                let dur = meta.created().ok()
                    .and_then(|c| now.duration_since(c).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(1);
                let is_captures = d.to_string_lossy().to_lowercase().contains("captures");
                let app = if is_captures { "Xbox Game Bar" } else { "OBS / Ekran Kaydı" };
                return Some((app.to_string(), dur));
            }
        }
    }

    // additionally the Xbox Game Bar Broadcast/DVR recording key
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    if let Ok(k) = hkcu.open_subkey(r"System\GameConfigStore") {
        let dvr: u32 = k.get_value("GameDVR_DVR LetUserRecord").unwrap_or(0);
        let _ = dvr;
    }
    None
}

/// Reads battery level and connection state of Bluetooth devices and headphones directly via SetupAPI
pub fn query_bluetooth_and_audio() -> (Vec<Value>, Option<(String, Option<u8>)>) {
    // 1. Check the active audio outputs (any headphones / Buds / headset?)
    let outputs = crate::audio::list_outputs();
    let mut bt_batteries: HashMap<String, (u8, bool)> = HashMap::new(); // name -> (pct, is_ble)

    unsafe {
        // Scan both Classic Bluetooth (BTHENUM) and BLE GATT Battery Service 0x180F (BTHLEDEVICE / BTHLE) devices
        for (enumerator, is_ble) in [(w!("BTHENUM"), false), (w!("BTHLEDEVICE"), true), (w!("BTHLE"), true)] {
            if let Ok(hdev) = SetupDiGetClassDevsW(None, enumerator, None, DIGCF_PRESENT | DIGCF_ALLCLASSES) {
                let mut idx = 0u32;
                loop {
                    let mut data = SP_DEVINFO_DATA {
                        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                        ..Default::default()
                    };
                    if SetupDiEnumDeviceInfo(hdev, idx, &mut data).is_err() {
                        break;
                    }
                    idx += 1;

                    // Battery percentage (1 byte: DEVPROP_TYPE_BYTE)
                    let mut ptype = DEVPROPTYPE::default();
                    let mut bbuf = [0u8; 4];
                    let mut req = 0u32;
                    if SetupDiGetDevicePropertyW(hdev, &data, &DEVPKEY_BT_BATTERY, &mut ptype, Some(&mut bbuf), Some(&mut req), 0).is_ok() {
                        let batt = bbuf[0];
                        if (1..=100).contains(&batt) {
                            let mut nbuf = [0u8; 512];
                            if SetupDiGetDevicePropertyW(hdev, &data, &DEVPKEY_NAME, &mut ptype, Some(&mut nbuf), Some(&mut req), 0).is_ok() && req >= 2 {
                                let u16s = std::slice::from_raw_parts(nbuf.as_ptr() as *const u16, (req as usize / 2).saturating_sub(1));
                                let raw_name = String::from_utf16_lossy(u16s);
                                let clean_name = raw_name
                                    .replace(" Hands-Free AG", "")
                                    .replace(" Avrcp Transport", "")
                                    .replace(" Battery Service", "")
                                    .trim()
                                    .to_string();
                                if !clean_name.is_empty() {
                                    bt_batteries.entry(clean_name).or_insert((batt, is_ble));
                                }
                            }
                        }
                    }
                }
                let _ = SetupDiDestroyDeviceInfoList(hdev);
            }
        }
    }

    let mut list = Vec::new();
    for (name, (batt, is_ble)) in &bt_batteries {
        let is_active_audio = outputs.iter().any(|(_, out_name, _)| out_name.to_lowercase().contains(&name.to_lowercase()));
        list.push(json!({
            "name": name,
            "battery": batt,
            "activeAudio": is_active_audio,
            "ble": is_ble,
        }));
    }

    // Which headphones are the default or active ones right now?
    let mut active_headset: Option<(String, Option<u8>)> = None;
    for (_, out_name, is_def) in &outputs {
        let low = out_name.to_lowercase();
        let is_hp = low.contains("buds") || low.contains("kulak") || low.contains("headphone") || low.contains("headset") || low.contains("airpods");
        let matched_batt = bt_batteries.iter()
            .find(|(bt_name, _)| low.contains(&bt_name.to_lowercase()))
            .map(|(_, (b, _))| *b);
        if is_hp || matched_batt.is_some() {
            if *is_def || active_headset.is_none() {
                active_headset = Some((out_name.clone(), matched_batt));
            }
        }
    }

    let _ = &DEVPKEY_BT_ADDR;
    (list, active_headset)
}

#[tauri::command]
pub fn get_sysmon() -> Value {
    LAST_SNAP.lock().unwrap().clone().unwrap_or_else(|| json!({}))
}

pub fn start() {
    // Each feature runs only if enabled in Settings; with all of them off no sensor is read (zero load).
    std::thread::spawn(|| {
        let mut prev_cpu = cpu_times();
        let mut prev_net = net_bytes();
        let mut prev_net_t = std::time::Instant::now();
        let mut tick = 0u32;
        let mut gpu_cache: Option<(u32, u32)> = None;
        let mut hs_cache: Option<(Vec<Value>, Option<(String, Option<u8>)>, String)> = None;
        let mut known_outputs: Vec<String> = Vec::new();
        let mut known_bt: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut headset_was_on = false;

        loop {
            let s0 = crate::settings::get();
            if !s0.show_sys_monitor && !s0.headset_alerts && !s0.privacy_dots && !s0.recording_dot {
                gpu_cache = None;
                hs_cache = None;
                headset_was_on = false;
                crate::settings::wait_until(|s| s.show_sys_monitor || s.headset_alerts || s.privacy_dots || s.recording_dot);
            }
            std::thread::sleep(Duration::from_millis(2500));
            tick = tick.wrapping_add(1);
            let s = crate::settings::get();
            let (want_sys, want_hs, want_priv, want_rec) = (s.show_sys_monitor, s.headset_alerts, s.privacy_dots, s.recording_dot);
            if !want_hs { headset_was_on = false; hs_cache = None; }
            if !want_sys && !want_hs && !want_priv && !want_rec { continue; }

            let mut snap = serde_json::Map::new();

            // 1-4. CPU / RAM / network / GPU (only if "Live system monitor" is on)
            if want_sys {
                let cur_cpu = cpu_times();
                let d_idle = cur_cpu.0.saturating_sub(prev_cpu.0);
                let d_kern = cur_cpu.1.saturating_sub(prev_cpu.1);
                let d_user = cur_cpu.2.saturating_sub(prev_cpu.2);
                prev_cpu = cur_cpu;
                let total_sys = d_kern + d_user;
                let cpu_pct = if total_sys > 0 {
                    ((total_sys.saturating_sub(d_idle) as f64 / total_sys as f64) * 100.0).round().min(100.0) as u32
                } else { 0 };

                let (ram_pct, ram_used_gb, ram_tot_gb) = ram_usage();

                let cur_net = net_bytes();
                let now = std::time::Instant::now();
                let secs = now.duration_since(prev_net_t).as_secs_f64().max(0.5);
                let down_bps = (cur_net.0.saturating_sub(prev_net.0) as f64 / secs).round() as u64;
                let up_bps = (cur_net.1.saturating_sub(prev_net.1) as f64 / secs).round() as u64;
                prev_net = cur_net;
                prev_net_t = now;

                if tick % 5 == 1 || gpu_cache.is_none() { gpu_cache = query_gpu(); }

                snap.insert("cpuPct".into(), json!(cpu_pct));
                snap.insert("ramPct".into(), json!(ram_pct));
                snap.insert("ramUsedGb".into(), json!(ram_used_gb));
                snap.insert("ramTotalGb".into(), json!(ram_tot_gb));
                snap.insert("downBps".into(), json!(down_bps));
                snap.insert("upBps".into(), json!(up_bps));
                snap.insert("gpuTemp".into(), json!(gpu_cache.map(|x| x.0)));
                snap.insert("gpuPct".into(), json!(gpu_cache.map(|x| x.1)));
            } else {
                gpu_cache = None;
            }

            // 5. Microphone / camera privacy dots
            if want_priv {
                let mic_apps = check_privacy_store("microphone");
                let cam_apps = check_privacy_store("webcam");
                snap.insert("micActive".into(), json!(!mic_apps.is_empty()));
                snap.insert("micApps".into(), json!(mic_apps));
                snap.insert("camActive".into(), json!(!cam_apps.is_empty()));
                snap.insert("camApps".into(), json!(cam_apps));
            }

            // 5b. OBS / Xbox Game Bar recording indicator (red dot + duration)
            if want_rec {
                if let Some((app, secs)) = detect_active_recording() {
                    snap.insert("recActive".into(), json!(true));
                    snap.insert("recApp".into(), json!(app));
                    snap.insert("recSecs".into(), json!(secs));
                } else {
                    snap.insert("recActive".into(), json!(false));
                }
            }

            // 6. Headphones-connected animation + battery percentage (SetupAPI scan every 6 s)
            if want_hs {
                if !headset_was_on || hs_cache.is_none() || tick % 3 == 0 {
                    let outputs = crate::audio::list_outputs();
                    let (bt_list, headset) = query_bluetooth_and_audio();
                    let active_bt: Vec<(String, Option<u64>)> = bt_list.iter()
                        .filter(|b| b["activeAudio"].as_bool().unwrap_or(false))
                        .map(|b| (b["name"].as_str().unwrap_or("").to_string(), b["battery"].as_u64()))
                        .collect();
                    if !headset_was_on {
                        known_outputs = outputs.iter().map(|(id, _, _)| id.clone()).collect();
                        known_bt = active_bt.iter().map(|(n, _)| n.clone()).collect();
                        headset_was_on = true;
                    } else {
                        let mut announced = false;
                        for (name, batt) in &active_bt {
                            if !known_bt.contains(name) && !announced {
                                emit("headset-connected", json!({ "name": name, "battery": batt }));
                                announced = true;
                            }
                        }
                        for (id, name, _) in &outputs {
                            if !known_outputs.contains(id) && !announced {
                                let low = name.to_lowercase();
                                let batt = bt_list.iter()
                                    .find(|b| low.contains(&b["name"].as_str().unwrap_or("").to_lowercase()))
                                    .and_then(|b| b["battery"].as_u64());
                                emit("headset-connected", json!({ "name": name, "battery": batt }));
                                announced = true;
                            }
                        }
                        known_outputs = outputs.iter().map(|(id, _, _)| id.clone()).collect();
                        known_bt = active_bt.iter().map(|(n, _)| n.clone()).collect();
                    }
                    let def_audio = outputs.iter().find(|x| x.2).map(|x| x.1.clone()).unwrap_or_default();
                    hs_cache = Some((bt_list, headset, def_audio));
                }
                if let Some((bt_list, headset, def_audio)) = &hs_cache {
                    snap.insert("defaultAudio".into(), json!(def_audio));
                    snap.insert("bluetooth".into(), json!(bt_list));
                    snap.insert("headset".into(), json!(headset.as_ref().map(|(n, b)| json!({ "name": n, "battery": b }))));
                }
            }

            let snap = Value::Object(snap);
            let changed = {
                let mut g = LAST_SNAP.lock().unwrap();
                if g.as_ref() == Some(&snap) { false } else { *g = Some(snap.clone()); true }
            };
            if changed { emit("sysmon", snap); }
        }
    });
}
