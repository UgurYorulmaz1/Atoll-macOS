// Download progress (only while Settings > "Download progress" is on):
// - Steam: appmanifest_*.acf in each library -> BytesDownloaded / BytesToDownload (real percentage)
// - Browsers (Chrome/Edge/Firefox/Opera/Brave/Arc/Vivaldi): partial files in the Downloads folder
//   (.crdownload / .part / .partial); real % from the Chromium History database.
// - Safari (macOS): the "name.download" package carries its own Info.plist with bytes so far / total -> real %.
//   When the partial file disappears and the final file appears: "Download complete" (click to open / add to shelf).
use crate::util::emit;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

static DONE: Mutex<Vec<String>> = Mutex::new(Vec::new());   // finished files that can be opened

fn home() -> Option<PathBuf> {
    std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok().map(PathBuf::from)
}

fn downloads_dir() -> Option<PathBuf> {
    home().map(|u| u.join("Downloads")).filter(|p| p.is_dir())
}

/* ---------- Steam ---------- */
#[cfg(windows)]
fn steam_root() -> Option<PathBuf> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let steam = hkcu.open_subkey(r"Software\Valve\Steam").and_then(|k| k.get_value::<String, _>("SteamPath")).ok()?;
    Some(PathBuf::from(steam.replace('/', "\\")))
}
#[cfg(not(windows))]
fn steam_root() -> Option<PathBuf> {
    home().map(|h| h.join("Library/Application Support/Steam")).filter(|p| p.is_dir())
}

fn steam_libraries() -> Vec<PathBuf> {
    let Some(root) = steam_root() else { return vec![] };
    let mut libs = vec![root.join("steamapps")];
    if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
        for line in vdf.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("\"path\"") {
                let p = rest.trim().trim_matches('"');
                let p = if cfg!(windows) { p.replace("\\\\", "\\") } else { p.to_string() };
                let lib = PathBuf::from(p).join("steamapps");
                // avoid listing the same library twice due to letter case or / vs \
                let key = |x: &PathBuf| x.to_string_lossy().to_lowercase().replace('/', "\\");
                if lib.is_dir() && !libs.iter().any(|l| key(l) == key(&lib)) { libs.push(lib); }
            }
        }
    }
    libs
}

/// "[2026-11-22 11:17:26]" -> day*86400+sec (only used for differences)
fn log_time(line: &str) -> Option<i64> {
    let t = line.get(1..20)?;
    let n = |a: usize, b: usize| t.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400); let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy
}
fn now_local() -> i64 {
    let (y, mo, d, h, mi, s) = crate::util::local_time();
    days_from_civil(y as i64, mo as i64, d as i64) * 86400 + h as i64 * 3600 + mi as i64 * 60 + s as i64
}

/// (downloaded, total, paused) - estimated progress from Steam's content_log.txt
fn steam_log_progress(appid: &str) -> Option<(u64, u64, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(steam_root()?.join("logs").join("content_log.txt")).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(400_000))).ok()?;
    let mut raw = Vec::new(); f.read_to_end(&mut raw).ok()?;
    let txt = String::from_utf8_lossy(&raw);
    let lines: Vec<&str> = txt.lines().collect();
    let start_tag = format!("AppID {appid} update started : download ");
    let si = lines.iter().rposition(|l| l.contains(&start_tag))?;
    let sl = lines[si];
    let after = &sl[sl.find(&start_tag)? + start_tag.len()..];
    let (a, rest) = after.split_once('/')?;
    let start_done: u64 = a.trim().parse().ok()?;
    let total: u64 = rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    let t0 = log_time(sl)?;
    let mut prev = t0;
    let mut last_rate = 0.0f64;      // bytes/sec
    let mut got: f64 = 0.0;
    let mut paused = false;
    let pause_tag = format!("AppID {appid} state changed");
    for l in &lines[si + 1..] {
        if let Some(pos) = l.find("Current download rate: ") {
            let mbps: f64 = l[pos + 23..].split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            if let Some(t) = log_time(l) {
                last_rate = mbps * 1_000_000.0 / 8.0;
                got += last_rate * (t - prev).max(0) as f64;
                prev = t;
            }
        } else if l.contains(&pause_tag) {
            paused = l.contains("Paused") || l.contains("(Canceled)");
            if !paused { if let Some(t) = log_time(l) { prev = prev.max(t); } }
        }
    }
    if !paused { got += last_rate * (now_local() - prev).clamp(0, 120) as f64; }   // since the last entry (at most 2 min)
    let done = (start_done as f64 + got).min(total as f64 * 0.99) as u64;
    Some((done, total, paused))
}

fn acf_value(txt: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    txt.lines().find_map(|l| {
        let t = l.trim();
        t.strip_prefix(&needle).map(|r| r.trim().trim_matches('"').to_string())
    })
}

fn steam_downloads(libs: &[PathBuf]) -> Vec<Value> {
    let mut out = Vec::new();
    for lib in libs {
        let Ok(rd) = std::fs::read_dir(lib) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) { continue; }
            let Ok(txt) = std::fs::read_to_string(&p) else { continue };
            let num = |k: &str| -> u64 { acf_value(&txt, k).and_then(|v| v.parse().ok()).unwrap_or(0) };
            let flags = num("StateFlags");
            // Steam does not update BytesDownloaded while downloading and preallocates files at full size
            // (so folder size is useless). Progress: content_log.txt -> "update started : download A/B" +
            // the time-weighted sum of the "Current download rate" entries written once a minute.
            let appid = name.trim_start_matches("appmanifest_").trim_end_matches(".acf");
            let mut paused = false;
            let (total, done) = match (flags & 1024 != 0).then(|| steam_log_progress(appid)).flatten() {
                Some((d, t, p)) => { paused = p; (t, d) }
                None => (num("BytesToDownload"), num("BytesDownloaded")),
            };
            // StateFlags: 2 = update required, 512 = paused, 1024 = download started (1026 = downloading)
            if total > 0 && done < total && flags & 1024 != 0 && flags & 512 == 0 {
                let title = acf_value(&txt, "name").unwrap_or_else(|| "Steam".into());
                out.push(json!({ "kind": "steam", "name": title, "done": done, "total": total, "paused": paused,
                    "manifest": p.to_string_lossy(),
                    "pct": ((done as f64 / total as f64) * 100.0).floor() as u32 }));
            }
        }
    }
    out
}

/* ---------- browsers ---------- */
/* ---------- browsers (real % for Chrome/Edge/Brave/Opera + Firefox) ---------- */
const PARTIAL: [&str; 4] = [".crdownload", ".part", ".partial", ".download"];

fn is_partial(name: &str) -> bool { let l = name.to_lowercase(); PARTIAL.iter().any(|e| l.ends_with(e)) }

fn final_name(partial: &str) -> String {
    let l = partial.to_lowercase();
    for e in PARTIAL { if l.ends_with(e) { return partial[..partial.len() - e.len()].to_string(); } }
    partial.to_string()
}

/// Reads active downloads (current_path/file -> (target_name, received, total)) from the History SQLite database of Chromium-based browsers
fn browser_sqlite_progress() -> HashMap<String, (String, u64, u64)> {
    let mut map = HashMap::new();
    #[cfg(windows)]
    let candidates = {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let roaming = std::env::var("APPDATA").unwrap_or_default();
        vec![
            PathBuf::from(&local).join(r"Google\Chrome\User Data\Default\History"),
            PathBuf::from(&local).join(r"Microsoft\Edge\User Data\Default\History"),
            PathBuf::from(&local).join(r"BraveSoftware\Brave-Browser\User Data\Default\History"),
            PathBuf::from(&roaming).join(r"Opera Software\Opera Stable\History"),
        ]
    };
    #[cfg(not(windows))]
    let candidates: Vec<PathBuf> = {
        let sup = home().unwrap_or_default().join("Library/Application Support");
        ["Google/Chrome/Default", "Microsoft Edge/Default", "BraveSoftware/Brave-Browser/Default",
         "Arc/User Data/Default", "Vivaldi/Default", "com.operasoftware.Opera/Default", "com.operasoftware.Opera",
         "Chromium/Default"]
            .iter().map(|p| sup.join(p).join("History")).collect()
    };
    let tmp = std::env::temp_dir().join("atoll_hist_probe.db");
    for hist in candidates {
        if !hist.is_file() { continue; }
        if std::fs::copy(&hist, &tmp).is_err() { continue; }
        if let Ok(conn) = rusqlite::Connection::open_with_flags(&tmp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            if let Ok(mut stmt) = conn.prepare("SELECT current_path, target_path, received_bytes, total_bytes FROM downloads WHERE state = 0 AND total_bytes > 0") {
                if let Ok(rows) = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?))
                }) {
                    for r in rows.flatten() {
                        let (cur, tgt, rec, tot) = r;
                        if tot <= 0 { continue; }
                        let tname = Path::new(&tgt).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        let cname = Path::new(&cur).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                        if !cname.is_empty() {
                            map.insert(cname, (tname, rec.max(0) as u64, tot as u64));
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&tmp);
    }
    map
}

/* ---------- Epic Games Launcher ---------- */
fn epic_downloads() -> Vec<Value> {
    use std::io::{Read, Seek, SeekFrom};
    #[cfg(windows)]
    let log_path = { let Ok(local) = std::env::var("LOCALAPPDATA") else { return vec![] };
        PathBuf::from(local).join(r"EpicGamesLauncher\Saved\Logs\EpicGamesLauncher.log") };
    #[cfg(not(windows))]
    let log_path = {
        let h = home().unwrap_or_default();
        let c = [h.join("Library/Logs/Unreal Engine/EpicGamesLauncher/EpicGamesLauncher.log"),
                 h.join("Library/Logs/EpicGamesLauncher/EpicGamesLauncher.log")];
        let Some(p) = c.into_iter().find(|p| p.is_file()) else { return vec![] };
        p
    };
    let Ok(meta) = std::fs::metadata(&log_path) else { return vec![] };
    // If nothing changed in the last 90 s there is no active Epic download (zero work)
    let recent = meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).map(|d| d < Duration::from_secs(90)).unwrap_or(false);
    if !recent { return vec![]; }
    let Ok(mut f) = std::fs::File::open(&log_path) else { return vec![] };
    let len = meta.len();
    if f.seek(SeekFrom::Start(len.saturating_sub(120_000))).is_err() { return vec![]; }
    let mut raw = Vec::new();
    if f.read_to_end(&mut raw).is_err() { return vec![]; }
    let txt = String::from_utf8_lossy(&raw);
    let mut app_name = String::from("Epic Games");
    let mut pct: Option<u32> = None;
    let mut done_b: u64 = 0;
    let mut total_b: u64 = 0;
    let mut finished = false;
    for line in txt.lines() {
        if let Some(p) = line.find("AppName:") {
            let v = line[p + 8..].trim().split_whitespace().next().unwrap_or("");
            if !v.is_empty() { app_name = v.trim_matches(|c| c == '"' || c == ',' || c == '\'').to_string(); }
        }
        if line.contains("BuildPatchServices: Progress:") || line.contains("FBuildPatchInstaller") && line.contains("Progress:") {
            if let Some(p) = line.find("Progress:") {
                let rest = line[p + 9..].trim();
                if let Some(num) = rest.split(|c: char| !c.is_ascii_digit() && c != '.').next().and_then(|s| s.parse::<f64>().ok()) {
                    let v = if num <= 1.0 && rest.contains('.') && !rest.contains('%') { (num * 100.0).floor() as u32 } else { num.floor() as u32 };
                    pct = Some(v.min(99));
                    finished = false;
                }
            }
        }
        if line.contains("Completed installation") || line.contains("Installation complete") || line.contains("Paused installation") {
            finished = true;
        }
        if let Some(p) = line.find("Downloaded:") {
            if let Some(n) = line[p + 11..].trim().split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) { done_b = n; }
        }
        if let Some(p) = line.find("Total:") {
            if let Some(n) = line[p + 6..].trim().split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) { total_b = n; }
        }
    }
    if let (Some(p), false) = (pct, finished) {
        return vec![json!({ "kind": "epic", "name": app_name, "done": done_b, "total": total_b, "pct": p })];
    }
    vec![]
}

#[tauri::command]
pub fn download_open(path: String) {
    if DONE.lock().unwrap().contains(&path) { crate::util::shell_open(&path); }
}

pub fn start() {
    std::thread::spawn(|| {
        let mut libs: Vec<PathBuf> = Vec::new();
        let mut libs_at = Instant::now() - Duration::from_secs(3600);
        // partial file -> (last size, last time, speed B/s)
        let mut seen: HashMap<PathBuf, (u64, Instant, f64)> = HashMap::new();
        let mut was_active = false;
        let mut steam_seen: HashMap<String, String> = HashMap::new();   // game name -> appmanifest path
        loop {
            if !crate::settings::get().downloads {
                if was_active { emit("downloads", json!({ "items": [] })); was_active = false; }
                seen.clear(); steam_seen.clear();
                crate::settings::wait_until(|s| s.downloads);
            }
            std::thread::sleep(Duration::from_millis(if was_active { 1500 } else { 4000 }));
            if !crate::settings::get().downloads { continue; }
            if libs_at.elapsed() > Duration::from_secs(300) { libs = steam_libraries(); libs_at = Instant::now(); }
            let mut items = steam_downloads(&libs);
            items.extend(epic_downloads());

            // Steam: a game that leaves the list counts as finished only if its manifest says "fully installed".
            // (A paused download or an .acf that cannot be read right now must not produce a false "finished".)
            let now_steam: HashMap<String, String> = items.iter().filter(|v| v["kind"].as_str() == Some("steam"))
                .filter_map(|v| Some((v["name"].as_str()?.to_string(), v["manifest"].as_str().unwrap_or("").to_string()))).collect();
            let mut keep: HashMap<String, String> = HashMap::new();
            for (g, manifest) in steam_seen.iter() {
                if now_steam.contains_key(g) { continue; }
                match std::fs::read_to_string(manifest).ok().and_then(|t| acf_value(&t, "StateFlags")).and_then(|v| v.parse::<u64>().ok()) {
                    Some(f) if f & 4 != 0 && f & (1024 | 512) == 0 => {
                        emit("download-done", json!({ "kind": "steam", "name": g, "path": null }));
                    }
                    Some(f) if f & 512 != 0 => {}                                 // paused: no notification
                    _ => { keep.insert(g.clone(), manifest.clone()); }             // unreadable / still updating: keep tracking
                }
            }
            steam_seen = now_steam;
            steam_seen.extend(keep);

            // Browsers
            if let Some(dir) = downloads_dir() {
                let mut current: HashSet<PathBuf> = HashSet::new();
                let mut partials: Vec<(PathBuf, String, u64)> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        let p = e.path();
                        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        if !is_partial(&name) { continue; }
                        let size = if p.is_file() { std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) }
                            else if p.is_dir() && name.to_lowercase().ends_with(".download") { dir_size(&p) }   // Safari package
                            else { continue };
                        current.insert(p.clone());
                        partials.push((p, name, size));
                    }
                }
                let db_prog = if !partials.is_empty() { browser_sqlite_progress() } else { HashMap::new() };
                for (p, name, size) in partials {
                    let now = Instant::now();
                    let entry = seen.entry(p.clone()).or_insert((size, now, 0.0));
                    let dt = now.duration_since(entry.1).as_secs_f64();
                    if dt >= 0.9 {
                        let inst = size.saturating_sub(entry.0) as f64 / dt;
                        entry.2 = if entry.2 == 0.0 { inst } else { entry.2 * 0.6 + inst * 0.4 };   // smoothed speed
                        entry.0 = size; entry.1 = now;
                    }
                    let mut shown = final_name(&name);
                    let mut done_v = size;
                    let mut total_v: Option<u64> = None;
                    let mut pct_v: Option<u32> = None;
                    if let Some((rec, tot)) = safari_progress(&p) {
                        done_v = rec;
                        total_v = Some(tot);
                        pct_v = Some(((rec as f64 / tot as f64) * 100.0).floor().min(99.0) as u32);
                    } else if let Some((tname, rec, tot)) = db_prog.get(&name.to_lowercase()) {
                        if !tname.is_empty() { shown = tname.clone(); }
                        done_v = (*rec).max(size);
                        total_v = Some(*tot);
                        pct_v = Some(((done_v as f64 / *tot as f64) * 100.0).floor().min(99.0) as u32);
                    }
                    items.push(json!({ "kind": "browser", "name": shown, "done": done_v, "total": total_v, "pct": pct_v, "speed": entry.2.round() as u64 }));
                }
                // finished partial files
                let finished: Vec<PathBuf> = seen.keys().filter(|k| !current.contains(*k)).cloned().collect();
                for f in finished {
                    seen.remove(&f);
                    let fname = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let mut target = dir.join(final_name(&fname));
                    if !target.is_file() || std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0) == 0 {
                        // Chrome "Unconfirmed 123.crdownload" -> newest file changed in the last 10 s
                        target = newest_recent(&dir).unwrap_or(target);
                    }
                    if target.is_file() && std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0) > 0 {
                        let path = target.to_string_lossy().to_string();
                        {
                            let mut d = DONE.lock().unwrap();
                            d.push(path.clone());
                            let len = d.len(); if len > 50 { d.drain(..len - 50); }
                        }
                        let name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        emit("download-done", json!({ "kind": "browser", "name": name, "path": path }));
                    }
                }
            }

            let active = !items.is_empty();
            if active || was_active { emit("downloads", json!({ "items": items })); }
            was_active = active;
        }
    });
}

fn dir_size(p: &Path) -> u64 {
    if p.is_file() { return std::fs::metadata(p).map(|m| m.len()).unwrap_or(0); }
    std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| dir_size(&e.path())).sum()).unwrap_or(0)
}

/// Safari: "file.zip.download/Info.plist" -> (bytes so far, total)
#[cfg(target_os = "macos")]
fn safari_progress(pkg: &Path) -> Option<(u64, u64)> {
    if !pkg.is_dir() { return None; }
    let v: plist::Value = plist::from_file(pkg.join("Info.plist")).ok()?;
    let d = v.as_dictionary()?;
    let n = |k: &str| d.get(k).and_then(|x| x.as_unsigned_integer().or_else(|| x.as_signed_integer().map(|i| i.max(0) as u64)));
    let total = n("DownloadEntryProgressTotalToLoad")?;
    let done = n("DownloadEntryProgressBytesSoFar").unwrap_or(0);
    (total > 0).then_some((done.min(total), total))
}
#[cfg(not(target_os = "macos"))]
fn safari_progress(_pkg: &Path) -> Option<(u64, u64)> { None }

fn newest_recent(dir: &Path) -> Option<PathBuf> {
    let now = SystemTime::now();
    std::fs::read_dir(dir).ok()?.flatten()
        .filter(|e| e.path().is_file() && !is_partial(&e.file_name().to_string_lossy()))
        .filter_map(|e| { let m = e.metadata().ok()?.modified().ok()?; Some((now.duration_since(m).ok()?, e.path())) })
        .filter(|(age, _)| *age < Duration::from_secs(10))
        .min_by_key(|(age, _)| *age)
        .map(|(_, p)| p)
}
