// Spotify Web API: like (library), play queue, search, play, add to queue.
// Sign-in with PKCE (no client secret). Browser -> http://127.0.0.1:8989/login
use crate::util::{data_dir, emit, shell_open};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PORT: u16 = 8989;
const REDIRECT: &str = "http://127.0.0.1:8989/login";
const DEFAULT_CLIENT_ID: &str = "";
const SCOPES: &str = "user-read-currently-playing user-read-playback-state user-modify-playback-state user-library-read user-library-modify";
const API: &str = "https://api.spotify.com/v1";

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct Cfg {
    #[serde(default)] client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] refresh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] access: Option<String>,
    #[serde(default)] expires_at: u64,
}

#[derive(Serialize, Default, Clone)]
struct SpState {
    connected: bool, uri: String, title: String, artist: String, liked: bool, queue: Vec<Value>, error: String,
}

static CFG: Mutex<Option<Cfg>> = Mutex::new(None);
static STATE: Mutex<Option<SpState>> = Mutex::new(None);
static BACKOFF_UNTIL: AtomicU64 = AtomicU64::new(0);
static POLLING: AtomicBool = AtomicBool::new(false);
static WAKE: AtomicU64 = AtomicU64::new(0);   // to trigger polling immediately
static SKIPPING: AtomicBool = AtomicBool::new(false);

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }
fn cfg() -> Cfg { CFG.lock().unwrap().clone().unwrap_or_default() }
fn set_cfg(c: Cfg) {
    let _ = std::fs::write(data_dir().join("spotify.json"), serde_json::to_string(&c).unwrap_or_default());
    *CFG.lock().unwrap() = Some(c);
}
fn push(f: impl FnOnce(&mut SpState)) {
    let st = { let mut g = STATE.lock().unwrap(); let s = g.get_or_insert_with(SpState::default); f(s); s.clone() };
    emit("sp-state", st);
}
pub fn is_connected() -> bool { cfg().refresh.is_some() }
pub fn client_id() -> String { cfg().client_id }
/// New Client ID from Settings: if it differs, the old session becomes invalid
pub fn set_client_id(id: &str) {
    if cfg().client_id == id { return; }
    let c = Cfg { client_id: id.to_string(), ..Default::default() };
    set_cfg(c);
    push(|s| { *s = SpState::default(); });
}

pub fn init() {
    let path = data_dir().join("spotify.json");
    let mut c: Cfg = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok())
        .or_else(|| {
            // first run: take over the login from the old Electron version (no need to sign in again)
            if crate::util::is_test() || !cfg!(windows) { return None; }
            let old = std::env::var("APPDATA").ok()? + "\\pc-dynamic-island\\spotify.json";
            std::fs::read_to_string(old).ok().and_then(|s| serde_json::from_str(&s).ok())
        })
        .unwrap_or_default();
    if c.client_id.is_empty() { c.client_id = DEFAULT_CLIENT_ID.into(); }
    let connected = c.refresh.is_some();
    set_cfg(c);
    *STATE.lock().unwrap() = Some(SpState { connected, ..Default::default() });
    if connected { start_polling(); }
}

/* ---------- tokens + requests ---------- */
struct ApiErr { status: u16, msg: String }
impl ApiErr { fn new(status: u16, msg: impl Into<String>) -> Self { Self { status, msg: msg.into() } } }

fn friendly(e: &ApiErr) -> String {
    if e.msg == "not-connected" { return crate::settings::l("Spotify bağlı değil (Ayarlar → Spotify)", "Spotify is not connected (Settings → Spotify)").into(); }
    if e.status == 403 { return crate::settings::l("Spotify izin vermedi (uygulama sahibinde Premium gerekli olabilir)", "Spotify refused (the app owner may need Premium)").into(); }
    if e.status == 404 { return crate::settings::l("Açık bir Spotify cihazı bulunamadı", "No active Spotify device found").into(); }
    e.msg.clone()
}

async fn token_request(params: &[(&str, &str)]) -> Result<(), ApiErr> {
    let r = reqwest::Client::new().post("https://accounts.spotify.com/api/token").form(params).send().await
        .map_err(|e| ApiErr::new(0, e.to_string()))?;
    let status = r.status().as_u16();
    let j: Value = r.json().await.unwrap_or(Value::Null);
    if status >= 400 {
        let code = j["error"].as_str().unwrap_or("").to_string();
        let msg = j["error_description"].as_str().map(String::from).unwrap_or_else(|| code.clone());
        return Err(ApiErr::new(status, if code == "invalid_grant" { "invalid_grant".into() } else { msg }));
    }
    let mut c = cfg();
    c.access = j["access_token"].as_str().map(String::from);
    c.expires_at = now_ms() + j["expires_in"].as_u64().unwrap_or(3600) * 1000 - 60_000;
    if let Some(r) = j["refresh_token"].as_str() { c.refresh = Some(r.into()); }   // keep the old one if none is returned
    set_cfg(c);
    Ok(())
}

async fn ensure_token() -> Result<String, ApiErr> {
    let c = cfg();
    if let Some(a) = &c.access { if now_ms() < c.expires_at { return Ok(a.clone()); } }
    let Some(refresh) = c.refresh.clone() else { return Err(ApiErr::new(0, "not-connected")) };
    match token_request(&[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", &c.client_id)]).await {
        Ok(()) => Ok(cfg().access.unwrap_or_default()),
        Err(e) => {
            if e.msg == "invalid_grant" { disconnect(Some("Spotify oturumu sona erdi, yeniden bağlan".into())); }
            Err(e)
        }
    }
}

async fn api(method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Option<Value>, ApiErr> {
    let client = reqwest::Client::new();
    let mut tok = ensure_token().await?;
    for attempt in 0..2 {
        let mut rq = client.request(method.clone(), format!("{API}{path}")).bearer_auth(&tok);
        rq = match &body { Some(b) => rq.json(b), None => rq.header("Content-Length", "0") };
        let r = rq.send().await.map_err(|e| ApiErr::new(0, e.to_string()))?;
        let status = r.status().as_u16();
        if status == 401 && attempt == 0 {
            let mut c = cfg(); c.expires_at = 0; set_cfg(c);
            tok = ensure_token().await?;
            continue;
        }
        if status == 429 {
            let secs: u64 = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(30);
            BACKOFF_UNTIL.store(now_ms() + secs * 1000, Ordering::Relaxed);
            return Err(ApiErr::new(429, "Spotify istek sınırı, biraz bekleniyor"));
        }
        if status == 204 { return Ok(None); }
        let text = r.text().await.unwrap_or_default();
        if status >= 400 {
            let msg = serde_json::from_str::<Value>(&text).ok()
                .and_then(|j| j["error"]["message"].as_str().map(String::from))
                .unwrap_or_else(|| format!("HTTP {status}"));   // do not show the body if it is not JSON (e.g. an HTML error page)
            return Err(ApiErr::new(status, msg));
        }
        return Ok(serde_json::from_str(&text).ok());
    }
    Err(ApiErr::new(401, "yetki hatası"))
}

/* ---------- sign-in (PKCE) ---------- */
fn b64url(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

pub fn connect() {
    // No Client ID yet: Spotify would answer "client_id: Not present" -> ask for one instead of opening the browser
    if cfg().client_id.trim().is_empty() {
        push(|s| s.error = crate::settings::l("Önce Spotify Client ID girin (Ayarlar › Medya)", "Enter your Spotify Client ID first (Settings › Media)").into());
        crate::settings::show_window(crate::util::app());
        return;
    }
    std::thread::spawn(|| {
        let verifier = b64url(&(0..48).map(|_| rand::random::<u8>()).collect::<Vec<u8>>());
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        let state = b64url(&rand::random::<[u8; 16]>());
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => l,
            Err(e) => { push(|s| s.error = format!("Giriş sunucusu açılamadı ({e})")); return; }
        };
        let _ = listener.set_nonblocking(false);
        let client_id = cfg().client_id;
        let mut url = url::Url::parse("https://accounts.spotify.com/authorize").unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", &client_id).append_pair("response_type", "code")
            .append_pair("redirect_uri", REDIRECT).append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge).append_pair("scope", SCOPES).append_pair("state", &state);
        shell_open(url.as_str());

        let deadline = now_ms() + 5 * 60 * 1000;
        for stream in listener.incoming() {
            if now_ms() > deadline { break; }
            let Ok(mut s) = stream else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let page = |title: &str, msg: &str| format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><meta charset=utf-8><title>{title}</title><body style=\"font-family:Segoe UI,sans-serif;background:#000;color:#fff;display:grid;place-items:center;height:100vh;margin:0\"><div style=text-align:center><h2>{title}</h2><p style=color:#aaa>{msg}</p></div>");
            let Ok(u) = url::Url::parse(&format!("http://127.0.0.1:{PORT}{target}")) else { continue };
            if u.path() != "/login" { let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n"); continue; }
            let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
            if q.get("state") != Some(&state) { let _ = s.write_all(page("Hata", "Geçersiz istek.").as_bytes()); continue; }
            if q.contains_key("error") {
                let _ = s.write_all(page("Bağlanılmadı", "Spotify izni verilmedi.").as_bytes());
                push(|st| st.error = "Spotify izni verilmedi".into());
                break;
            }
            let code = q.get("code").cloned().unwrap_or_default();
            let res = tauri::async_runtime::block_on(token_request(&[
                ("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", REDIRECT),
                ("client_id", &client_id), ("code_verifier", &verifier)]));
            match res {
                Ok(()) => {
                    let _ = s.write_all(page("Bağlandı ✓", "Dynamic Island artık Spotify ile bağlı. Bu sekmeyi kapatabilirsin.").as_bytes());
                    push(|st| { st.connected = true; st.error.clear(); });
                    start_polling();
                    crate::settings::broadcast();
                }
                Err(e) => {
                    let _ = s.write_all(page("Bağlanılamadı", &e.msg).as_bytes());
                    push(|st| st.error = format!("Spotify girişi başarısız: {}", e.msg));
                }
            }
            break;
        }
    });
}

pub fn disconnect(msg: Option<String>) {
    let c = Cfg { client_id: cfg().client_id, ..Default::default() };
    set_cfg(c);
    push(|s| { *s = SpState { error: msg.unwrap_or_default(), ..Default::default() }; });
    crate::settings::broadcast();
}

/* ---------- now playing + like + queue ---------- */
fn pick_image(imgs: &Value) -> String {
    let Some(arr) = imgs.as_array() else { return String::new() };
    let mut v: Vec<&Value> = arr.iter().collect();
    v.sort_by_key(|i| i["width"].as_u64().unwrap_or(0));
    v.iter().find(|i| i["width"].as_u64().unwrap_or(0) >= 60).or(v.last())
        .and_then(|i| i["url"].as_str()).unwrap_or("").to_string()
}
fn slim(t: &Value) -> Value {
    let artist = t["artists"].as_array().map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
        .filter(|s| !s.is_empty()).or_else(|| t["show"]["name"].as_str().map(String::from)).unwrap_or_default();
    let image = if t["album"]["images"].is_array() { pick_image(&t["album"]["images"]) } else { pick_image(&t["images"]) };
    json!({ "uri": t["uri"], "name": t["name"], "artist": artist, "image": image })
}

async fn refresh_queue() {
    match api(reqwest::Method::GET, "/me/player/queue", None).await {
        Ok(q) => {
            let list: Vec<Value> = q.as_ref().and_then(|q| q["queue"].as_array()).map(|a| a.iter().take(12).map(slim).collect()).unwrap_or_default();
            push(|s| s.queue = list);
        }
        Err(e) => push(|s| s.error = friendly(&e)),
    }
}

async fn poll_once() -> u64 {
    if now_ms() < BACKOFF_UNTIL.load(Ordering::Relaxed) { return BACKOFF_UNTIL.load(Ordering::Relaxed) - now_ms(); }
    match api(reqwest::Method::GET, "/me/player/currently-playing?additional_types=track,episode", None).await {
        Ok(cur) => {
            let item = cur.as_ref().map(|c| c["item"].clone()).filter(|i| i.is_object());
            let Some(item) = item else {
                push(|s| { s.uri.clear(); s.liked = false; });
                return 15_000;
            };
            let uri = item["uri"].as_str().unwrap_or("").to_string();
            let same = STATE.lock().unwrap().as_ref().map(|s| s.uri == uri).unwrap_or(false);
            if !same {
                let t = slim(&item);
                let liked = api(reqwest::Method::GET, &format!("/me/library/contains?uris={}", url_enc(&uri)), None).await.ok()
                    .flatten().and_then(|v| v.as_array().and_then(|a| a.first()).and_then(|b| b.as_bool())).unwrap_or(false);
                push(|s| {
                    s.uri = uri; s.title = t["name"].as_str().unwrap_or("").into();
                    s.artist = t["artist"].as_str().unwrap_or("").into(); s.liked = liked; s.error.clear();
                });
                refresh_queue().await;
            }
            5_000
        }
        Err(e) => { if e.msg != "not-connected" { push(|s| s.error = friendly(&e)); } 20_000 }
    }
}

fn start_polling() {
    if POLLING.swap(true, Ordering::Relaxed) { WAKE.fetch_add(1, Ordering::Relaxed); return; }
    tauri::async_runtime::spawn(async {
        loop {
            if cfg().refresh.is_none() { POLLING.store(false, Ordering::Relaxed); return; }
            let wait = poll_once().await;
            let w0 = WAKE.load(Ordering::Relaxed);
            let until = now_ms() + wait;
            while now_ms() < until && WAKE.load(Ordering::Relaxed) == w0 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
}

fn url_enc(s: &str) -> String { url::form_urlencoded::byte_serialize(s.as_bytes()).collect() }
fn is_track_uri(u: &str) -> bool {
    let mut p = u.split(':');
    p.next() == Some("spotify") && matches!(p.next(), Some("track") | Some("episode"))
        && p.next().map(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or(false)
        && p.next().is_none()
}

/* ---------- commands ---------- */
#[tauri::command] pub fn sp_get() { let c = is_connected(); push(|s| s.connected = c); }
#[tauri::command] pub fn sp_connect() { connect(); }
#[tauri::command] pub fn sp_disconnect() { disconnect(None); }
#[tauri::command] pub fn sp_queue() { if is_connected() { tauri::async_runtime::spawn(refresh_queue()); } }

#[tauri::command]
pub fn sp_like() {
    let (uri, liked) = { let g = STATE.lock().unwrap(); g.as_ref().map(|s| (s.uri.clone(), s.liked)).unwrap_or_default() };
    if uri.is_empty() { return; }
    let want = !liked;
    push(|s| s.liked = want);   // instant feedback
    tauri::async_runtime::spawn(async move {
        let m = if want { reqwest::Method::PUT } else { reqwest::Method::DELETE };
        if let Err(e) = api(m, &format!("/me/library?uris={}", url_enc(&uri)), None).await {
            push(|s| { s.liked = !want; s.error = friendly(&e); });
        }
    });
}

async fn play_uris(uris: Vec<String>) -> Result<(), ApiErr> {
    let body = json!({ "uris": uris });
    match api(reqwest::Method::PUT, "/me/player/play", Some(body.clone())).await {
        Ok(_) => Ok(()),
        Err(e) if e.status == 404 => {
            // If no device is active, pick the first available one (preferably the computer) and retry
            if let Ok(Some(r)) = api(reqwest::Method::GET, "/me/player/devices", None).await {
                if let Some(arr) = r["devices"].as_array() {
                    let dev = arr.iter().find(|d| d["is_active"].as_bool() == Some(true))
                        .or_else(|| arr.iter().find(|d| d["type"].as_str().map(|t| t.eq_ignore_ascii_case("computer")).unwrap_or(false)))
                        .or_else(|| arr.first());
                    if let Some(id) = dev.and_then(|d| d["id"].as_str()) {
                        api(reqwest::Method::PUT, &format!("/me/player/play?device_id={}", url_enc(id)), Some(body)).await?;
                        return Ok(());
                    }
                }
            }
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// Jump straight to the n-th track in the queue (the tracks in between are not played)
#[tauri::command]
pub fn sp_skipto(i: i64) {
    if !(0..=20).contains(&i) || SKIPPING.swap(true, Ordering::Relaxed) { return; }
    let uris: Vec<String> = {
        let g = STATE.lock().unwrap();
        g.as_ref().map(|s| {
            s.queue.iter().skip(i as usize)
                .filter_map(|t| t["uri"].as_str().map(String::from))
                .filter(|u| is_track_uri(u))
                .collect()
        }).unwrap_or_default()
    };
    tauri::async_runtime::spawn(async move {
        if !uris.is_empty() && is_connected() {
            match play_uris(uris).await {
                Ok(()) => {}
                Err(e) => {
                    push(|s| s.error = if e.status == 403 {
                        crate::settings::l("Doğrudan geçiş için Ayarlar → Spotify'dan yeniden bağlan", "Reconnect Spotify in Settings for instant skip").into()
                    } else { friendly(&e) });
                }
            }
        } else {
            for _ in 0..=i {
                crate::media::send("next");
                tokio::time::sleep(Duration::from_millis(350)).await;
            }
        }
        SKIPPING.store(false, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(250)).await;
        WAKE.fetch_add(1, Ordering::Relaxed);
    });
}

#[tauri::command]
pub fn sp_search(q: String) {
    let q: String = q.trim().chars().take(100).collect();
    tauri::async_runtime::spawn(async move {
        if q.is_empty() { emit("sp-search-results", json!({ "q": q, "items": [] })); return; }
        match api(reqwest::Method::GET, &format!("/search?type=track&limit=10&q={}", url_enc(&q)), None).await {
            Ok(r) => {
                let items: Vec<Value> = r.as_ref().and_then(|r| r["tracks"]["items"].as_array()).map(|a| a.iter().map(slim).collect()).unwrap_or_default();
                emit("sp-search-results", json!({ "q": q, "items": items }));
            }
            Err(e) => emit("sp-search-results", json!({ "q": q, "items": [], "error": friendly(&e) })),
        }
    });
}

async fn similar_tracks(uri: &str) -> Vec<String> {
    let Some(id) = uri.strip_prefix("spotify:track:") else { return vec![] };
    // 1. Try /recommendations first (may return 403/404 for some dev-mode apps)
    if let Ok(Some(r)) = api(reqwest::Method::GET, &format!("/recommendations?seed_tracks={}&limit=10", url_enc(id)), None).await {
        if let Some(arr) = r["tracks"].as_array() {
            let uris: Vec<String> = arr.iter().filter_map(|t| t["uri"].as_str().map(String::from)).filter(|u| u != uri).collect();
            if !uris.is_empty() { return uris; }
        }
    }
    // 2. Fallback: the track artist's top tracks
    let Ok(Some(tr)) = api(reqwest::Method::GET, &format!("/tracks/{}", url_enc(id)), None).await else { return vec![] };
    let Some(aid) = tr["artists"].as_array().and_then(|a| a.first()).and_then(|a| a["id"].as_str()) else { return vec![] };
    let Ok(Some(top)) = api(reqwest::Method::GET, &format!("/artists/{}/top-tracks", url_enc(aid)), None).await else { return vec![] };
    top["tracks"].as_array().map(|arr| {
        arr.iter().filter_map(|t| t["uri"].as_str().map(String::from)).filter(|u| u != uri).take(10).collect()
    }).unwrap_or_default()
}

#[tauri::command]
pub fn sp_play(uri: String) {
    if !is_track_uri(&uri) { return; }
    let autoplay = crate::settings::get().spotify_autoplay;
    tauri::async_runtime::spawn(async move {
        let mut uris = vec![uri.clone()];
        if autoplay {
            uris.extend(similar_tracks(&uri).await);
        }
        match play_uris(uris).await {
            Ok(_) => { tokio::time::sleep(Duration::from_millis(250)).await; WAKE.fetch_add(1, Ordering::Relaxed); }
            Err(e) => push(|s| s.error = friendly(&e)),
        }
    });
}

#[tauri::command]
pub fn sp_enqueue(uri: String) {
    if !is_track_uri(&uri) { return; }
    tauri::async_runtime::spawn(async move {
        match api(reqwest::Method::POST, &format!("/me/player/queue?uri={}", url_enc(&uri)), None).await {
            Ok(_) => { emit("shelf-msg", crate::settings::l("Sıraya eklendi", "Added to queue").to_string()); tokio::time::sleep(Duration::from_millis(600)).await; refresh_queue().await; }
            Err(e) => push(|s| s.error = friendly(&e)),
        }
    });
}

#[tauri::command]
pub fn sp_devices() {
    if !is_connected() { emit("sp-devices", json!({ "items": [] })); return; }
    tauri::async_runtime::spawn(async {
        match api(reqwest::Method::GET, "/me/player/devices", None).await {
            Ok(r) => {
                let items: Vec<Value> = r.as_ref().and_then(|v| v["devices"].as_array()).map(|arr| {
                    arr.iter().filter(|d| !d["is_restricted"].as_bool().unwrap_or(false)).map(|d| json!({
                        "id": d["id"], "name": d["name"], "type": d["type"],
                        "active": d["is_active"].as_bool().unwrap_or(false),
                        "vol": d["volume_percent"],
                    })).collect()
                }).unwrap_or_default();
                emit("sp-devices", json!({ "items": items }));
            }
            Err(e) => emit("sp-devices", json!({ "items": [], "error": friendly(&e) })),
        }
    });
}

#[tauri::command]
pub fn sp_transfer(device_id: String) {
    let id = device_id.trim().to_string();
    if id.is_empty() || id.len() > 120 { return; }
    tauri::async_runtime::spawn(async move {
        match api(reqwest::Method::PUT, "/me/player", Some(json!({ "device_ids": [id], "play": true }))).await {
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(600)).await;
                WAKE.fetch_add(1, Ordering::Relaxed);
                sp_devices();
            }
            Err(e) => push(|s| s.error = friendly(&e)),
        }
    });
}
