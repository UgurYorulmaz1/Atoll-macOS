// macOS live system monitor (CPU / GPU / RAM / network), microphone & camera in-use dots,
// screen-recording dot, headphones-connected animation with AirPods / Magic accessories battery levels.
// Everything uses public APIs that need no permission: mach host statistics, getifaddrs, IOKit registry,
// CoreAudio / CoreMediaIO "is running somewhere", system_profiler for Bluetooth batteries.
use crate::macos::ffi::*;
use crate::util::emit;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static LAST_SNAP: Mutex<Option<Value>> = Mutex::new(None);

/* ---------- CPU / RAM (mach) ---------- */
extern "C" {
    fn mach_host_self() -> u32;
    fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}
const HOST_CPU_LOAD_INFO: i32 = 3;
const HOST_VM_INFO64: i32 = 4;

#[repr(C)]
#[derive(Default)]
struct VmStat64 {
    free_count: u32, active_count: u32, inactive_count: u32, wire_count: u32,
    zero_fill_count: u64, reactivations: u64, pageins: u64, pageouts: u64, faults: u64,
    cow_faults: u64, lookups: u64, hits: u64, purges: u64,
    purgeable_count: u32, speculative_count: u32,
    decompressions: u64, compressions: u64, swapins: u64, swapouts: u64,
    compressor_page_count: u32, throttled_count: u32, external_page_count: u32, internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

/// (busy ticks, total ticks)
fn cpu_ticks() -> (u64, u64) {
    let mut t = [0u32; 4];   // user, system, idle, nice
    let mut count = 4u32;
    let r = unsafe { host_statistics(mach_host_self(), HOST_CPU_LOAD_INFO, t.as_mut_ptr() as *mut i32, &mut count) };
    if r != 0 { return (0, 0); }
    let busy = t[0] as u64 + t[1] as u64 + t[3] as u64;
    (busy, busy + t[2] as u64)
}

/// (used %, used GB, total GB) — "used" like Activity Monitor: app memory + wired + compressed
fn ram_usage() -> (u32, f64, f64) {
    let total = crate::shellx::total_ram() as f64;
    let mut vm = VmStat64::default();
    let mut count = (std::mem::size_of::<VmStat64>() / 4) as u32;
    let r = unsafe { host_statistics64(mach_host_self(), HOST_VM_INFO64, &mut vm as *mut VmStat64 as *mut i32, &mut count) };
    if r != 0 || total <= 0.0 { return (0, 0.0, 0.0); }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as f64;
    let app = (vm.internal_page_count as f64 - vm.purgeable_count as f64).max(0.0);
    let used = ((app + vm.wire_count as f64 + vm.compressor_page_count as f64) * page).min(total);
    let gb = 1073741824.0;
    (((used / total) * 100.0).round() as u32, (used / gb * 10.0).round() / 10.0, (total / gb * 10.0).round() / 10.0)
}

/* ---------- network ---------- */
#[repr(C)]
struct IfData {   // struct if_data (net/if_var.h)
    ifi_type: u8, ifi_typelen: u8, ifi_physical: u8, ifi_addrlen: u8, ifi_hdrlen: u8, ifi_recvquota: u8, ifi_xmitquota: u8, ifi_unused1: u8,
    ifi_mtu: u32, ifi_metric: u32, ifi_baudrate: u32,
    ifi_ipackets: u32, ifi_ierrors: u32, ifi_opackets: u32, ifi_oerrors: u32, ifi_collisions: u32,
    ifi_ibytes: u32, ifi_obytes: u32,
}

/// Per-interface byte counters (32-bit, wrap-safe deltas are computed by the caller)
fn net_counters() -> HashMap<String, (u32, u32)> {
    let mut out = HashMap::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 { return out; }
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_data.is_null() { continue; }
            if (*ifa.ifa_addr).sa_family as i32 != libc::AF_LINK { continue; }
            let flags = ifa.ifa_flags as i32;
            if flags & libc::IFF_UP == 0 || flags & libc::IFF_RUNNING == 0 || flags & libc::IFF_LOOPBACK != 0 { continue; }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().to_string();
            // skip tunnels / AirDrop / virtual bridges so traffic is not counted twice
            if name.starts_with("utun") || name.starts_with("awdl") || name.starts_with("llw") || name.starts_with("bridge") || name.starts_with("gif") || name.starts_with("stf") { continue; }
            let d = &*(ifa.ifa_data as *const IfData);
            out.insert(name, (d.ifi_ibytes, d.ifi_obytes));
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/* ---------- GPU (IOAccelerator performance statistics: Apple, AMD and Intel GPUs) ---------- */
unsafe fn io_props(service: u32) -> Option<Retained<NSDictionary<NSString, AnyObject>>> {
    let mut props: *mut std::ffi::c_void = std::ptr::null_mut();
    if IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0) != 0 || props.is_null() { return None; }
    Retained::from_raw(props as *mut NSDictionary<NSString, AnyObject>)
}

unsafe fn io_services(class: &std::ffi::CStr) -> Vec<u32> {
    let m = IOServiceMatching(class.as_ptr());
    if m.is_null() { return vec![]; }
    let mut it = 0u32;
    if IOServiceGetMatchingServices(0, m, &mut it) != 0 { return vec![]; }
    let mut v = Vec::new();
    loop { let s = IOIteratorNext(it); if s == 0 { break; } v.push(s); }
    IOObjectRelease(it);
    v
}

fn num(d: &NSDictionary<NSString, AnyObject>, k: &str) -> Option<f64> {
    d.objectForKey(&NSString::from_str(k)).and_then(|o| o.downcast::<NSNumber>().ok()).map(|n| n.doubleValue())
}

/// (temperature °C if the driver reports it, utilization %)
fn query_gpu() -> Option<(Option<u32>, u32)> {
    unsafe {
        let mut best: Option<(Option<u32>, u32)> = None;
        for s in io_services(c"IOAccelerator") {
            if let Some(p) = io_props(s) {
                if let Some(stats) = p.objectForKey(&NSString::from_str("PerformanceStatistics"))
                    .and_then(|o| o.downcast::<NSDictionary>().ok()) {
                    let stats: &NSDictionary<NSString, AnyObject> = &*(Retained::as_ptr(&stats) as *const NSDictionary<NSString, AnyObject>);
                    let util = num(stats, "Device Utilization %").or_else(|| num(stats, "GPU Activity(%)"));
                    let temp = num(stats, "Temperature(C)").filter(|t| *t > 0.0 && *t < 130.0).map(|t| t.round() as u32);
                    if let Some(u) = util {
                        let cur = (temp, u.round().clamp(0.0, 100.0) as u32);
                        // with several GPUs (iGPU + dGPU) show the busier one
                        if best.map(|b| cur.1 >= b.1).unwrap_or(true) { best = Some(cur); }
                    }
                }
            }
            IOObjectRelease(s);
        }
        best
    }
}

/* ---------- camera in use (CoreMediaIO) ---------- */
fn camera_in_use() -> bool {
    unsafe {
        let a = addr(fourcc(b"dev#"), kScopeGlobal);
        let mut size = 0u32;
        if CMIOObjectGetPropertyDataSize(1, &a, 0, std::ptr::null(), &mut size) != 0 || size == 0 { return false; }
        let n = size as usize / 4;
        let mut devs = vec![0u32; n];
        let mut used = 0u32;
        if CMIOObjectGetPropertyData(1, &a, 0, std::ptr::null(), size, &mut used, devs.as_mut_ptr() as *mut _) != 0 { return false; }
        devs.iter().any(|d| {
            let a = addr(kIsRunningSomewhere, kScopeGlobal);
            let mut v = 0u32;
            let mut used = 0u32;
            CMIOObjectGetPropertyData(*d, &a, 0, std::ptr::null(), 4, &mut used, &mut v as *mut u32 as *mut _) == 0 && v != 0
        })
    }
}

/* ---------- screen recording ---------- */
fn screencapture_dir() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("/usr/bin/defaults").args(["read", "com.apple.screencapture", "location"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let p = if let Some(rest) = s.strip_prefix("~/") { crate::macos::home().join(rest) } else { std::path::PathBuf::from(s) };
    p.is_dir().then_some(p)
}

/// OBS, QuickTime or another recorder writing a video file right now? -> (app, seconds)
fn detect_active_recording(dirs: &[std::path::PathBuf]) -> Option<(String, u64)> {
    let now = std::time::SystemTime::now();
    let exts = ["mkv", "mp4", "flv", "mov", "ts", "avi", "webm", "m4v"];
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if !exts.contains(&ext.as_str()) { continue; }
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            if !meta.is_file() || meta.len() < 1024 { continue; }
            let Ok(age) = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).ok_or(()) else { continue };
            if age.as_secs_f64() <= 4.5 {
                let dur = meta.created().ok().and_then(|c| now.duration_since(c).ok()).map(|d| d.as_secs()).unwrap_or(1);
                let app = if crate::macos::is_running("com.obsproject.obs-studio") { "OBS" } else { "QuickTime / Ekran Kaydı" };
                return Some((app.to_string(), dur));
            }
        }
    }
    None
}

fn recording_dirs() -> Vec<std::path::PathBuf> {
    let home = crate::macos::home();
    let mut dirs = vec![home.join("Movies"), home.join("Desktop")];
    if let Some(d) = screencapture_dir() { if !dirs.contains(&d) { dirs.push(d); } }
    let obs = home.join("Library/Application Support/obs-studio/basic/profiles");
    if let Ok(rd) = std::fs::read_dir(obs) {
        for e in rd.flatten() {
            if let Ok(txt) = std::fs::read_to_string(e.path().join("basic.ini")) {
                for line in txt.lines() {
                    let t = line.trim();
                    if let Some(p) = t.strip_prefix("FilePath=").or_else(|| t.strip_prefix("RecFilePath=")) {
                        let pb = std::path::PathBuf::from(p.trim());
                        if pb.is_dir() && !dirs.contains(&pb) { dirs.push(pb); }
                    }
                }
            }
        }
    }
    dirs
}

/* ---------- Bluetooth batteries ---------- */
fn pct(v: Option<&Value>) -> Option<u8> {
    let s = v?.as_str()?;
    s.trim_end_matches('%').trim().parse::<u8>().ok().filter(|p| (1..=100).contains(p))
}

/// AirPods / Beats / Magic Keyboard, Mouse, Trackpad / game controllers: name -> battery %
fn bluetooth_batteries() -> HashMap<String, u8> {
    let mut map = HashMap::new();
    // Magic accessories and other HID devices publish BatteryPercent in the IOKit registry (instant)
    unsafe {
        for s in io_services(c"AppleDeviceManagementHIDEventService") {
            if let Some(p) = io_props(s) {
                let name = p.objectForKey(&NSString::from_str("Product")).and_then(|o| o.downcast::<NSString>().ok()).map(|x| x.to_string());
                if let (Some(n), Some(b)) = (name, num(&p, "BatteryPercent")) {
                    if (1.0..=100.0).contains(&b) { map.insert(n, b as u8); }
                }
            }
            IOObjectRelease(s);
        }
    }
    // AirPods & headphones: system_profiler (left / right / case / main)
    if let Ok(out) = std::process::Command::new("/usr/sbin/system_profiler").args(["SPBluetoothDataType", "-json", "-detailLevel", "basic"]).output() {
        if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
            for ctl in v["SPBluetoothDataType"].as_array().into_iter().flatten() {
                for dev in ctl["device_connected"].as_array().into_iter().flatten() {
                    let Some(obj) = dev.as_object() else { continue };
                    for (name, info) in obj {
                        let levels: Vec<u8> = ["device_batteryLevelMain", "device_batteryLevelLeft", "device_batteryLevelRight"]
                            .iter().filter_map(|k| pct(info.get(*k))).collect();
                        if let Some(min) = levels.iter().min() { map.entry(name.clone()).or_insert(*min); }
                    }
                }
            }
        }
    }
    map
}

pub fn query_bluetooth_and_audio(batts: &HashMap<String, u8>) -> (Vec<Value>, Option<(String, Option<u8>)>) {
    let outputs = crate::audio::list_outputs();
    let list: Vec<Value> = batts.iter().map(|(name, b)| {
        let active = outputs.iter().any(|(_, o, _)| o.to_lowercase().contains(&name.to_lowercase()) || name.to_lowercase().contains(&o.to_lowercase()));
        json!({ "name": name, "battery": b, "activeAudio": active, "ble": false })
    }).collect();
    let mut headset: Option<(String, Option<u8>)> = None;
    for (_, out_name, is_def) in &outputs {
        let low = out_name.to_lowercase();
        let is_hp = ["airpods", "beats", "buds", "headphone", "headset", "kulak", "earphone", "wh-", "wf-", "bose", "jabra"].iter().any(|k| low.contains(k));
        let batt = batts.iter().find(|(n, _)| low.contains(&n.to_lowercase()) || n.to_lowercase().contains(&low)).map(|(_, b)| *b);
        if (is_hp || batt.is_some()) && (*is_def || headset.is_none()) { headset = Some((out_name.clone(), batt)); }
    }
    (list, headset)
}

#[tauri::command]
pub fn get_sysmon() -> Value { LAST_SNAP.lock().unwrap().clone().unwrap_or_else(|| json!({})) }

pub fn start() {
    // Each feature runs only if enabled in Settings; with all of them off nothing is read (zero load).
    std::thread::spawn(|| {
        let mut prev_cpu = cpu_ticks();
        let mut prev_net = net_counters();
        let mut prev_net_t = Instant::now();
        let mut tick = 0u32;
        let mut gpu_cache: Option<(Option<u32>, u32)> = None;
        let mut batt_cache: (HashMap<String, u8>, Option<Instant>) = (HashMap::new(), None);
        let mut rec_dirs: (Vec<std::path::PathBuf>, Option<Instant>) = (vec![], None);
        let mut hs_cache: Option<(Vec<Value>, Option<(String, Option<u8>)>, String)> = None;
        let mut known_outputs: Vec<String> = Vec::new();
        let mut known_bt: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut headset_was_on = false;

        loop {
            let s0 = crate::settings::get();
            if !s0.show_sys_monitor && !s0.headset_alerts && !s0.privacy_dots && !s0.recording_dot {
                gpu_cache = None; hs_cache = None; headset_was_on = false;
                crate::settings::wait_until(|s| s.show_sys_monitor || s.headset_alerts || s.privacy_dots || s.recording_dot);
                prev_cpu = cpu_ticks(); prev_net = net_counters(); prev_net_t = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(2500));
            tick = tick.wrapping_add(1);
            let s = crate::settings::get();
            let (want_sys, want_hs, want_priv, want_rec) = (s.show_sys_monitor, s.headset_alerts, s.privacy_dots, s.recording_dot);
            if !want_hs { headset_was_on = false; hs_cache = None; }
            if !want_sys && !want_hs && !want_priv && !want_rec { continue; }
            let mut snap = serde_json::Map::new();

            if want_sys {
                let cur = cpu_ticks();
                let (db, dt) = (cur.0.saturating_sub(prev_cpu.0), cur.1.saturating_sub(prev_cpu.1));
                prev_cpu = cur;
                let cpu_pct = if dt > 0 { ((db as f64 / dt as f64) * 100.0).round().min(100.0) as u32 } else { 0 };
                let (ram_pct, ram_used, ram_tot) = ram_usage();
                let cur_net = net_counters();
                let now = Instant::now();
                let secs = now.duration_since(prev_net_t).as_secs_f64().max(0.5);
                let (mut rx, mut tx) = (0u64, 0u64);
                for (k, (i, o)) in &cur_net {
                    if let Some((pi, po)) = prev_net.get(k) { rx += i.wrapping_sub(*pi) as u64; tx += o.wrapping_sub(*po) as u64; }
                }
                prev_net = cur_net; prev_net_t = now;
                if tick % 2 == 1 || gpu_cache.is_none() { gpu_cache = query_gpu(); }
                snap.insert("cpuPct".into(), json!(cpu_pct));
                snap.insert("ramPct".into(), json!(ram_pct));
                snap.insert("ramUsedGb".into(), json!(ram_used));
                snap.insert("ramTotalGb".into(), json!(ram_tot));
                snap.insert("downBps".into(), json!((rx as f64 / secs).round() as u64));
                snap.insert("upBps".into(), json!((tx as f64 / secs).round() as u64));
                snap.insert("gpuTemp".into(), json!(gpu_cache.and_then(|g| g.0)));
                snap.insert("gpuPct".into(), json!(gpu_cache.map(|g| g.1)));
            } else { gpu_cache = None; }

            if want_priv {
                let mic = crate::audio::mic_in_use();
                let cam = camera_in_use();
                snap.insert("micActive".into(), json!(mic));
                snap.insert("micApps".into(), json!(Vec::<String>::new()));
                snap.insert("camActive".into(), json!(cam));
                snap.insert("camApps".into(), json!(Vec::<String>::new()));
            }

            if want_rec {
                if rec_dirs.1.map(|t| t.elapsed() > Duration::from_secs(60)).unwrap_or(true) { rec_dirs = (recording_dirs(), Some(Instant::now())); }
                match detect_active_recording(&rec_dirs.0) {
                    Some((app, secs)) => { snap.insert("recActive".into(), json!(true)); snap.insert("recApp".into(), json!(app)); snap.insert("recSecs".into(), json!(secs)); }
                    None => { snap.insert("recActive".into(), json!(false)); }
                }
            }

            if want_hs {
                if !headset_was_on || hs_cache.is_none() || tick % 3 == 0 {
                    let outputs = crate::audio::list_outputs();
                    // system_profiler is slow (~1 s): refresh batteries at most every 60 s, or right after an output change
                    let out_ids: Vec<String> = outputs.iter().map(|(id, _, _)| id.clone()).collect();
                    let outputs_changed = out_ids != known_outputs;
                    if outputs_changed || batt_cache.1.map(|t| t.elapsed() > Duration::from_secs(60)).unwrap_or(true) {
                        batt_cache = (bluetooth_batteries(), Some(Instant::now()));
                    }
                    let (bt_list, headset) = query_bluetooth_and_audio(&batt_cache.0);
                    let active_bt: Vec<(String, Option<u64>)> = bt_list.iter().filter(|b| b["activeAudio"].as_bool().unwrap_or(false))
                        .map(|b| (b["name"].as_str().unwrap_or("").to_string(), b["battery"].as_u64())).collect();
                    if !headset_was_on {
                        known_outputs = out_ids;
                        known_bt = active_bt.iter().map(|(n, _)| n.clone()).collect();
                        headset_was_on = true;
                    } else {
                        let mut announced = false;
                        for (name, batt) in &active_bt {
                            if !known_bt.contains(name) && !announced { emit("headset-connected", json!({ "name": name, "battery": batt })); announced = true; }
                        }
                        for (id, name, _) in &outputs {
                            if !known_outputs.contains(id) && !announced {
                                let low = name.to_lowercase();
                                let batt = bt_list.iter().find(|b| low.contains(&b["name"].as_str().unwrap_or("").to_lowercase())).and_then(|b| b["battery"].as_u64());
                                emit("headset-connected", json!({ "name": name, "battery": batt }));
                                announced = true;
                            }
                        }
                        known_outputs = out_ids;
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
