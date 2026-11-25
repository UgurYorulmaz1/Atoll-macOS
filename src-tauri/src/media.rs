// Windows media session (SMTC): now playing, artwork, controls. Direct WinRT instead of PowerShell.
use crate::util::{b64, emit, filetime_to_unix_ms};
use serde_json::json;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::core::{Interface, Result};
use windows_future::IAsyncOperation;
use windows::Media::Control::*;
use windows::Media::MediaPlaybackAutoRepeatMode;
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

static TX: OnceLock<Mutex<Sender<String>>> = OnceLock::new();

pub fn send(cmd: &str) {
    if let Some(tx) = TX.get() { let _ = tx.lock().unwrap().send(cmd.to_string()); }
}

pub fn start() {
    let (tx, rx) = channel::<String>();
    let _ = TX.set(Mutex::new(tx));
    std::thread::spawn(move || {
        unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
        let mgr = loop {
            match GlobalSystemMediaTransportControlsSessionManager::RequestAsync().and_then(|op| op.join()) {
                Ok(m) => break m,
                Err(_) => std::thread::sleep(Duration::from_secs(3)),
            }
        };
        let mut last_json = String::new();
        let mut last_key = String::new();
        let mut thumb_tries: i32 = 0;
        let mut last_thumb: Option<serde_json::Value> = None;
        let mut tick: u32 = 0;
        let (mut sec_json, mut sec_key, mut sec_thumb) = (String::new(), String::new(), None::<serde_json::Value>);
        let mut prev_playing: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            // handle commands immediately; otherwise poll the state every 400 ms
            match rx.recv_timeout(Duration::from_millis(800)) {
                Ok(cmd) => { let _ = handle(&mgr, &cmd); last_json.clear(); continue; }
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            tick = tick.wrapping_add(1);
            enforce_focus(&mgr, &mut prev_playing, tick == 1);
            // resend occasionally so the UI does not miss the state if it loads late (or reloads)
            let resend = tick % 8 == 0;
            match snapshot(&mgr) {
                Ok(Some((v, key, props))) => {
                    if v["playing"].as_bool().unwrap_or(false) {
                        let t_str = v["title"].as_str().unwrap_or("");
                        let a_str = v["artist"].as_str().unwrap_or("");
                        crate::stats::record_tick(t_str, a_str, 400);
                    }
                    let s = v.to_string();
                    if s != last_json || resend { emit("media", v); last_json = s; }
                    if key != last_key { last_key = key.clone(); thumb_tries = 0; last_thumb = None; }
                    if (0..8).contains(&thumb_tries) {
                        match props.Thumbnail().ok().and_then(|t| read_ref(&t)) {
                            Some(data) => {
                                let t = json!({ "type": "thumb", "key": key, "data": data });
                                emit("media", t.clone()); last_thumb = Some(t); thumb_tries = -1;
                            }
                            None => thumb_tries += 1,
                        }
                    } else if resend && tick % 24 == 0 {
                        if let Some(t) = &last_thumb { emit("media", t.clone()); }
                    }
                }
                Ok(None) | Err(_) => {
                    let s = r#"{"type":"media","active":false}"#;
                    if last_json != s { emit("media", json!({ "type": "media", "active": false })); last_json = s.into(); }
                    last_key.clear();
                }
            }
            // A SECOND source playing at the same time (shown as a bubble)
            match secondary(&mgr, &last_key) {
                Some((v, key, props)) => {
                    let s = v.to_string();
                    if s != sec_json || resend { emit("media", v); sec_json = s; }
                    if key != sec_key {
                        sec_key = key.clone();
                        if let Some(data) = props.Thumbnail().ok().and_then(|t| read_ref(&t)) {
                            sec_thumb = Some(json!({ "type": "thumb2", "key": key, "data": data }));
                        } else { sec_thumb = None; }
                        if let Some(t) = &sec_thumb { emit("media", t.clone()); }
                    } else if resend && tick % 24 == 0 {
                        if let Some(t) = &sec_thumb { emit("media", t.clone()); }
                    }
                }
                None => {
                    let s = r#"{"type":"media2","active":false}"#;
                    if sec_json != s || resend { emit("media", json!({ "type": "media2", "active": false })); sec_json = s.into(); }
                    sec_key.clear();
                }
            }
        }
    });
}

fn split_artist(artist: String, album: String) -> (String, String) {
    // Apple Music puts the album into the artist field: "Artist — Album - Single" -> split
    match artist.split_once(" \u{2014} ") {
        Some((a, rest)) => (a.trim().to_string(), if album.is_empty() { rest.trim().to_string() } else { album }),
        None => (artist, album),
    }
}

/// The first session, other than the main source, that plays something DIFFERENT.
/// (Twin sessions reporting the same track - e.g. spotifast + official Spotify - do not count as a second source.)
fn secondary(mgr: &GlobalSystemMediaTransportControlsSessionManager, primary_key: &str)
    -> Option<(serde_json::Value, String, GlobalSystemMediaTransportControlsSessionMediaProperties)> {
    let primary = LAST_APP.lock().unwrap().clone();
    for s in mgr.GetSessions().ok()? {
        let id = app_id(&s);
        if id == primary || !is_playing(&s) { continue; }
        let Some(p) = s.TryGetMediaPropertiesAsync().ok().and_then(|op| op.join().ok()) else { continue };
        let title = p.Title().map(|h| h.to_string()).unwrap_or_default();
        if title.is_empty() { continue; }
        let (artist, _) = split_artist(p.Artist().map(|h| h.to_string()).unwrap_or_default(), String::new());
        let key = format!("{}|{}", title, artist);
        if key.eq_ignore_ascii_case(primary_key) || title.trim().eq_ignore_ascii_case(primary_key.split('|').next().unwrap_or("").trim()) { continue; }
        let v = json!({ "type": "media2", "active": true, "app": id, "title": title, "artist": artist, "key": key });
        return Some((v, key, p));
    }
    None
}

fn snapshot(mgr: &GlobalSystemMediaTransportControlsSessionManager)
    -> Result<Option<(serde_json::Value, String, GlobalSystemMediaTransportControlsSessionMediaProperties)>> {
    let Some(s) = pick(mgr) else { return Ok(None) };
    let p = s.TryGetMediaPropertiesAsync()?.join()?;
    let pb = s.GetPlaybackInfo()?;
    let tl = s.GetTimelineProperties()?;
    let ctl = pb.Controls()?;
    let title = p.Title().map(|h| h.to_string()).unwrap_or_default();
    let (artist, album) = split_artist(
        p.Artist().map(|h| h.to_string()).unwrap_or_default(),
        p.AlbumTitle().map(|h| h.to_string()).unwrap_or_default());
    let key = format!("{}|{}", title, artist);
    let app = app_id(&s);
    let start = tl.StartTime()?.Duration;
    let end = tl.EndTime()?.Duration;
    let pos = tl.Position()?.Duration;
    let shuffle = pb.IsShuffleActive().ok().and_then(|r| r.Value().ok()).unwrap_or(false);
    let repeat = pb.AutoRepeatMode().ok().and_then(|r| r.Value().ok()).map(|m| m.0).unwrap_or(0);
    let r2 = |x: f64| (x * 100.0).round() / 100.0;
    let v = json!({
        "type": "media",
        "active": !title.is_empty(),
        "key": key,
        "app": app,
        "title": title, "artist": artist,
        "album": album,
        "playing": pb.PlaybackStatus()? == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing,
        "position": r2((pos - start) as f64 / 1e7),
        "duration": r2((end - start) as f64 / 1e7),
        "updated": filetime_to_unix_ms(tl.LastUpdatedTime()?.UniversalTime),
        // Apple Music: seek via UI Automation, not through Windows
        "canSeek": ctl.IsPlaybackPositionEnabled().unwrap_or(false) || is_apple_music(&app),
        "canShuffle": ctl.IsShuffleEnabled().unwrap_or(false),
        "canRepeat": ctl.IsRepeatEnabled().unwrap_or(false),
        "canNext": ctl.IsNextEnabled().unwrap_or(false),
        "canPrev": ctl.IsPreviousEnabled().unwrap_or(false),
        "shuffle": shuffle, "repeat": repeat,
    });
    Ok(Some((v, key, p)))
}

/* ---------- session selection ----------
   Windows' "current session" is not always the app that is playing (e.g. Spotify paused, Apple Music playing).
   We pick the session ourselves and always route display and commands to the SAME one: stick with the selected session while it plays,
   otherwise the first playing session, otherwise the last selected one, and finally the one Windows picked. */
static LAST_APP: Mutex<String> = Mutex::new(String::new());

fn app_id(s: &GlobalSystemMediaTransportControlsSession) -> String {
    s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default()
}
fn is_playing(s: &GlobalSystemMediaTransportControlsSession) -> bool {
    s.GetPlaybackInfo().and_then(|p| p.PlaybackStatus()).map(|st| st == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing).unwrap_or(false)
}
fn pick(mgr: &GlobalSystemMediaTransportControlsSessionManager) -> Option<GlobalSystemMediaTransportControlsSession> {
    let last_id = LAST_APP.lock().unwrap().clone();
    let mut playing = None;
    let mut last = None;
    if let Ok(list) = mgr.GetSessions() {
        for s in list {
            if playing.is_none() && is_playing(&s) { playing = Some(s.clone()); }
            if !last_id.is_empty() && app_id(&s) == last_id { last = Some(s.clone()); }
        }
    }
    let chosen = match &last {
        Some(l) if is_playing(l) => last.clone(),
        _ => playing.or(last).or_else(|| mgr.GetCurrentSession().ok()),
    }?;
    *LAST_APP.lock().unwrap() = app_id(&chosen);
    Some(chosen)
}
fn is_apple_music(id: &str) -> bool { id.contains("AppleMusic") }

/* ---------- audio focus: one source at a time ----------
   When an app STARTS playing while another one is playing too, the new one becomes the main source and the others are paused.
   (Settings > "Single audio source", off by default.) */
fn pause_others(mgr: &GlobalSystemMediaTransportControlsSessionManager, keep: &str) {
    if let Ok(list) = mgr.GetSessions() {
        for s in list {
            if app_id(&s) != keep && is_playing(&s) {
                if let Ok(op) = s.TryPauseAsync() { let _ = op.join(); }
            }
        }
    }
}

fn enforce_focus(mgr: &GlobalSystemMediaTransportControlsSessionManager,
                 prev: &mut std::collections::HashSet<String>, first: bool) {
    let mut now = std::collections::HashSet::new();
    let mut order: Vec<String> = Vec::new();
    if let Ok(list) = mgr.GetSessions() {
        for s in list { if is_playing(&s) { let id = app_id(&s); order.push(id.clone()); now.insert(id); } }
    }
    let on = crate::settings::get().single_source;
    if on && !first && now.len() > 1 {
        // the newly started source (not playing in the previous tick) wins
        if let Some(newest) = order.iter().find(|id| !prev.contains(*id)).cloned() {
            *LAST_APP.lock().unwrap() = newest.clone();
            pause_others(mgr, &newest);
            now.retain(|id| *id == newest);
        }
    }
    *prev = now;
}

fn handle(mgr: &GlobalSystemMediaTransportControlsSessionManager, cmd: &str) -> Result<()> {
    // "focus <app>": make that source the main one (when its bubble is clicked)
    if let Some(id) = cmd.strip_prefix("focus ") {
        if mgr.GetSessions()?.into_iter().any(|s| app_id(&s) == id) {
            *LAST_APP.lock().unwrap() = id.to_string();
            if crate::settings::get().single_source { pause_others(mgr, id); }
        }
        return Ok(());
    }
    let Some(s) = pick(mgr) else { return Ok(()) };
    let mut parts = cmd.split(' ');
    match parts.next().unwrap_or("") {
        "toggle" => {
            // some apps expect separate play/pause instead of "toggle"
            let ctl = s.GetPlaybackInfo()?.Controls()?;
            if is_playing(&s) && ctl.IsPauseEnabled().unwrap_or(false) { s.TryPauseAsync()?.join()?; }
            else if !is_playing(&s) && ctl.IsPlayEnabled().unwrap_or(false) { s.TryPlayAsync()?.join()?; }
            else { s.TryTogglePlayPauseAsync()?.join()?; }
        }
        "next" => { s.TrySkipNextAsync()?.join()?; }
        "prev" => { s.TrySkipPreviousAsync()?.join()?; }
        "seek" => {
            let sec: f64 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
            let can = s.GetPlaybackInfo()?.Controls()?.IsPlaybackPositionEnabled().unwrap_or(false);
            if can {
                let start = s.GetTimelineProperties()?.StartTime()?.Duration;
                s.TryChangePlaybackPositionAsync(start + (sec * 1e7) as i64)?.join()?;
            } else if is_apple_music(&app_id(&s)) {
                // Apple Music does not allow seeking through Windows: set its own progress bar via UI Automation
                crate::uia::apple_music_seek(sec)?;
            }
        }
        "shuffle" => {
            let cur = s.GetPlaybackInfo()?.IsShuffleActive().ok().and_then(|r| r.Value().ok()).unwrap_or(false);
            s.TryChangeShuffleActiveAsync(!cur)?.join()?;
        }
        "repeat" => {
            // None(0) -> List(2) -> Track(1) -> None
            let cur = s.GetPlaybackInfo()?.AutoRepeatMode().ok().and_then(|r| r.Value().ok()).map(|m| m.0).unwrap_or(0);
            let next = match cur { 0 => 2, 2 => 1, _ => 0 };
            s.TryChangeAutoRepeatModeAsync(MediaPlaybackAutoRepeatMode(next))?.join()?;
        }
        _ => {}
    }
    Ok(())
}

/// Convert a stream reference (artwork, app logo) into a data URL
pub fn read_ref(r: &IRandomAccessStreamReference) -> Option<String> {
    let s = r.OpenReadAsync().ok()?.join().ok()?;
    let size = s.Size().ok()?;
    if !(100..8_000_000).contains(&size) { return None; }
    // Windows may return the type as a list: "image/jpeg,image/jpe,image/jpg" -> take the first
    let ct = s.ContentType().map(|h| h.to_string()).unwrap_or_default()
        .split(',').next().unwrap_or("").trim().to_string();
    let input = s.GetInputStreamAt(0).ok()?;
    let reader = DataReader::CreateDataReader(&input).ok()?;
    let op: IAsyncOperation<u32> = reader.LoadAsync(size as u32).ok()?.cast().ok()?;
    let n = op.join().ok()?;
    let mut buf = vec![0u8; n as usize];
    reader.ReadBytes(&mut buf).ok()?;
    Some(format!("data:{};base64,{}", if ct.is_empty() { "image/png" } else { &ct }, b64(&buf)))
}
