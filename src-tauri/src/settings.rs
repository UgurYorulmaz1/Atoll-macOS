// Settings: saved to disk; every change sends a "settings" event to all windows.
use crate::util::{data_dir, emit};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Condvar, Mutex, OnceLock};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub lang: String,               // one of the 12 language codes ("tr", "en", "de", ...)
    pub browser_lyrics: bool,       // lyrics in browsers (always on for YouTube Music)
    pub clipboard_history: bool,
    pub storage: String,            // "disk" | "ram"
    pub storage_dir: String,        // empty = default folder
    pub limit_gb: f64,              // shelf size limit
    pub save_ram_on_exit: bool,
    // New features: all OFF BY DEFAULT, enabled only from Settings
    pub show_sys_monitor: bool,     // live system & gaming monitor (CPU/GPU/RAM/network)
    pub headset_alerts: bool,       // headphones-connected animation + battery percentage
    pub privacy_dots: bool,         // microphone / camera privacy dots
    pub audio_switch: bool,         // right-click the speaker icon: switch audio output
    pub auto_hide_fullscreen: bool, // auto-hide during fullscreen games/videos
    pub global_shortcut: bool,      // open the island with Ctrl+Shift+Space
    pub single_source: bool,        // one audio source at a time: the new one wins, the other pauses (default OFF)
    pub island_size: String,        // "sm" | "md" | "lg" (default "md" = standard size)
    pub monitor_index: usize,       // 0 = primary display, 1.. = other displays
    pub recording_dot: bool,        // OBS / Xbox Game Bar recording indicator (red dot + duration)
    pub listening_stats: bool,      // local play history and weekly/daily summary card
    pub call_alerts: bool,          // WhatsApp/Telegram/Teams incoming call card
    pub weather: bool,              // weather + rain alert (IP location)
    pub downloads: bool,            // Steam / browser download progress
    pub spotify_autoplay: bool,     // after playing from Spotify search, queue similar tracks
    pub easy_mode: bool,            // Simple mode (on by default: minimal settings)
    pub scale_mode: String,         // "auto" (follows Windows scaling) | "custom" (independent)
    pub custom_scale: u32,          // 75..=115 (%) custom scale
    pub auto_dim: bool,             // slight transparency while idle (off by default)
    pub top_offset: u32,            // 0..=48 px top offset
    pub idle_opacity: u32,          // 35..=100 (%) idle opacity
    pub bg_style: String,           // "pure" | "glass" | "tinted"
    pub accent_color: String,       // "auto" or "#rrggbb"
    pub anim_speed: String,         // "snappy" | "normal" | "relaxed"
    pub clock_24h: bool,            // 24-hour / 12-hour clock
    pub gpu_accel: bool,            // WebView2 GPU rasterization & hardware acceleration
}

impl Default for Settings {
    fn default() -> Self {
        Self { lang: default_lang(), browser_lyrics: false, clipboard_history: false, storage: "disk".into(),
            storage_dir: String::new(), limit_gb: 16.0, save_ram_on_exit: true,
            show_sys_monitor: false, headset_alerts: false, privacy_dots: false,
            audio_switch: false, auto_hide_fullscreen: false, global_shortcut: false, single_source: false,
            island_size: "md".into(), monitor_index: 0, recording_dot: false, listening_stats: false,
            call_alerts: false, weather: false, downloads: false, spotify_autoplay: false,
            easy_mode: true, scale_mode: "auto".into(), custom_scale: 100, auto_dim: false,
            top_offset: DEFAULT_TOP, idle_opacity: 100, bg_style: "pure".into(), accent_color: "auto".into(),
            anim_speed: "normal".into(), clock_24h: true, gpu_accel: true }
    }
}

/// Reads the gpuAccel setting from disk before WebView2 starts (while the AppHandle is not ready yet)
#[cfg_attr(not(windows), allow(dead_code))]
pub fn gpu_accel_early() -> bool {
    let Ok(appdata) = std::env::var("APPDATA") else { return true };
    let folder = if crate::util::is_test() { "com.island.dynamic-test" } else { "com.island.dynamic" };
    let p = std::path::PathBuf::from(appdata).join(folder).join("settings.json");
    std::fs::read_to_string(p).ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("gpuAccel").and_then(|b| b.as_bool()))
        .unwrap_or(true)
}

/// macOS: the island hangs from the top edge (inside the notch / over the menu bar); Windows: 8 px below the edge
const DEFAULT_TOP: u32 = if cfg!(target_os = "macos") { 0 } else { 8 };

static S: Mutex<Option<Settings>> = Mutex::new(None);
static WAKE_SYNC: Condvar = Condvar::new();
static WAKE_ASYNC: OnceLock<tokio::sync::Notify> = OnceLock::new();

fn async_notify() -> &'static tokio::sync::Notify { WAKE_ASYNC.get_or_init(tokio::sync::Notify::new) }

/// Parks the thread at 0 CPU while the feature is off; wakes it immediately when the setting changes
pub fn wait_until<F: Fn(&Settings) -> bool>(pred: F) {
    let mut g = S.lock().unwrap();
    while !g.as_ref().map(&pred).unwrap_or(false) {
        g = WAKE_SYNC.wait(g).unwrap();
    }
}

/// 0-CPU wait for async tasks
pub async fn wait_until_async<F: Fn(&Settings) -> bool>(pred: F) {
    loop {
        if pred(&get()) { return; }
        async_notify().notified().await;
    }
}

/// Default language code based on the macOS preferred languages (System Settings > Language & Region)
#[cfg(target_os = "macos")]
fn default_lang() -> String {
    let code = crate::macos::preferred_language();
    let base = code.split(['-', '_']).next().unwrap_or("en").to_lowercase();
    if matches!(base.as_str(), "tr" | "de" | "fr" | "es" | "it" | "pt" | "ru" | "ja" | "ko" | "zh" | "ar") { base } else { "en".into() }
}

/// Default language code based on the Windows display language
#[cfg(windows)]
fn default_lang() -> String {
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    match id & 0x3FF {
        0x1F => "tr",
        0x07 => "de",
        0x0C => "fr",
        0x0A => "es",
        0x10 => "it",
        0x16 => "pt",
        0x19 => "ru",
        0x11 => "ja",
        0x12 => "ko",
        0x04 => "zh",
        0x01 => "ar",
        _ => "en",
    }.into()
}

fn file() -> std::path::PathBuf { data_dir().join("settings.json") }

pub fn init() {
    let s: Settings = std::fs::read_to_string(file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    *S.lock().unwrap() = Some(s);
}

pub fn get() -> Settings { S.lock().unwrap().clone().unwrap_or_default() }
/// Bilingual text
pub fn l<'a>(tr: &'a str, en: &'a str) -> &'a str { if get().lang == "tr" { tr } else { en } }

/// Menu and window strings for all 12 supported languages
pub fn tr(key: &str) -> &'static str {
    let lang = get().lang;
    // macOS wording ("Open at Login" instead of "Start with Windows")
    let key = if cfg!(target_os = "macos") && key == "autostart" { "autostart_mac" } else { key };
    match (lang.as_str(), key) {
        ("tr", "autostart_mac") => "Girişte aç",
        ("de", "autostart_mac") => "Bei der Anmeldung öffnen",
        ("fr", "autostart_mac") => "Ouvrir à l'ouverture de session",
        ("es", "autostart_mac") => "Abrir al iniciar sesión",
        ("it", "autostart_mac") => "Apri al login",
        ("pt", "autostart_mac") => "Abrir ao iniciar sessão",
        ("ru", "autostart_mac") => "Открывать при входе",
        ("ar", "autostart_mac") => "الفتح عند تسجيل الدخول",
        ("ja", "autostart_mac") => "ログイン時に開く",
        ("ko", "autostart_mac") => "로그인 시 열기",
        ("zh", "autostart_mac") => "登录时打开",
        (_, "autostart_mac") => "Open at Login",

        ("tr", "settings") => "Ayarlar…",
        ("de", "settings") => "Einstellungen…",
        ("fr", "settings") => "Paramètres…",
        ("es", "settings") => "Ajustes…",
        ("it", "settings") => "Impostazioni…",
        ("pt", "settings") => "Configurações…",
        ("ru", "settings") => "Настройки…",
        ("ar", "settings") => "الإعدادات…",
        ("ja", "settings") => "設定…",
        ("ko", "settings") => "설정…",
        ("zh", "settings") => "设置…",
        (_, "settings") => "Settings…",

        ("tr", "autostart") => "Windows ile başlat",
        ("de", "autostart") => "Mit Windows starten",
        ("fr", "autostart") => "Lancer avec Windows",
        ("es", "autostart") => "Iniciar con Windows",
        ("it", "autostart") => "Avvia con Windows",
        ("pt", "autostart") => "Iniciar com o Windows",
        ("ru", "autostart") => "Запускать с Windows",
        ("ar", "autostart") => "التشغيل مع Windows",
        ("ja", "autostart") => "Windows 起動時に開始",
        ("ko", "autostart") => "Windows 시작 시 실행",
        ("zh", "autostart") => "开机自启动",
        (_, "autostart") => "Start with Windows",

        ("tr", "reload") => "Yeniden yükle",
        ("de", "reload") => "Neu laden",
        ("fr", "reload") => "Recharger",
        ("es", "reload") => "Recargar",
        ("it", "reload") => "Ricarica",
        ("pt", "reload") => "Recarregar",
        ("ru", "reload") => "Перезагрузить",
        ("ar", "reload") => "إعادة تحميل",
        ("ja", "reload") => "再読み込み",
        ("ko", "reload") => "새로 고침",
        ("zh", "reload") => "重新加载",
        (_, "reload") => "Reload",

        ("tr", "quit") => "Çıkış",
        ("de", "quit") => "Beenden",
        ("fr", "quit") => "Quitter",
        ("es", "quit") => "Salir",
        ("it", "quit") => "Esci",
        ("pt", "quit") => "Sair",
        ("ru", "quit") => "Выход",
        ("ar", "quit") => "خروج",
        ("ja", "quit") => "終了",
        ("ko", "quit") => "종료",
        ("zh", "quit") => "退出",
        (_, "quit") => "Quit",

        ("tr", "settings_title") => "Dynamic Island · Ayarlar",
        ("de", "settings_title") => "Dynamic Island · Einstellungen",
        ("fr", "settings_title") => "Dynamic Island · Paramètres",
        ("es", "settings_title") => "Dynamic Island · Ajustes",
        ("it", "settings_title") => "Dynamic Island · Impostazioni",
        ("pt", "settings_title") => "Dynamic Island · Configurações",
        ("ru", "settings_title") => "Dynamic Island · Настройки",
        ("ar", "settings_title") => "Dynamic Island · الإعدادات",
        ("ja", "settings_title") => "Dynamic Island · 設定",
        ("ko", "settings_title") => "Dynamic Island · 설정",
        ("zh", "settings_title") => "Dynamic Island · 设置",
        (_, "settings_title") => "Dynamic Island · Settings",

        ("tr", "sh_open") => "Aç",
        ("de", "sh_open") => "Öffnen",
        ("fr", "sh_open") => "Ouvrir",
        ("es", "sh_open") => "Abrir",
        ("it", "sh_open") => "Apri",
        ("pt", "sh_open") => "Abrir",
        ("ru", "sh_open") => "Открыть",
        ("ar", "sh_open") => "فتح",
        ("ja", "sh_open") => "開く",
        ("ko", "sh_open") => "열기",
        ("zh", "sh_open") => "打开",
        (_, "sh_open") => "Open",

        ("tr", "sh_copy") => "Panoya kopyala",
        ("de", "sh_copy") => "In Zwischenablage kopieren",
        ("fr", "sh_copy") => "Copier dans le presse-papiers",
        ("es", "sh_copy") => "Copiar al portapapeles",
        ("it", "sh_copy") => "Copia negli appunti",
        ("pt", "sh_copy") => "Copiar para área de transferência",
        ("ru", "sh_copy") => "Копировать в буфер",
        ("ar", "sh_copy") => "نسخ إلى الحافظة",
        ("ja", "sh_copy") => "クリップボードにコピー",
        ("ko", "sh_copy") => "클립보드에 복사",
        ("zh", "sh_copy") => "复制到剪贴板",
        (_, "sh_copy") => "Copy to clipboard",

        ("tr", "sh_orig") => "Orijinal konumunu göster",
        ("de", "sh_orig") => "Ursprungsort anzeigen",
        ("fr", "sh_orig") => "Afficher l'emplacement d'origine",
        ("es", "sh_orig") => "Mostrar ubicación original",
        ("it", "sh_orig") => "Mostra posizione originale",
        ("pt", "sh_orig") => "Mostrar local original",
        ("ru", "sh_orig") => "Показать исходное расположение",
        ("ar", "sh_orig") => "إظهار الموقع الأصلي",
        ("ja", "sh_orig") => "元の場所を表示",
        ("ko", "sh_orig") => "원래 위치 표시",
        ("zh", "sh_orig") => "显示原始位置",
        (_, "sh_orig") => "Show original location",

        ("tr", "sh_remove") => "Raftan kaldır",
        ("de", "sh_remove") => "Aus Ablage entfernen",
        ("fr", "sh_remove") => "Retirer de l'étagère",
        ("es", "sh_remove") => "Quitar del estante",
        ("it", "sh_remove") => "Rimuovi dallo scaffale",
        ("pt", "sh_remove") => "Remover da prateleira",
        ("ru", "sh_remove") => "Убрать с полки",
        ("ar", "sh_remove") => "إزالة من الرف",
        ("ja", "sh_remove") => "シェルフから削除",
        ("ko", "sh_remove") => "선반에서 제거",
        ("zh", "sh_remove") => "从暂存架移除",
        (_, "sh_remove") => "Remove from shelf",

        _ => "",
    }
}

fn save(s: &Settings) {
    let _ = std::fs::write(file(), serde_json::to_string_pretty(s).unwrap_or_default());
    *S.lock().unwrap() = Some(s.clone());
    WAKE_SYNC.notify_all();
    async_notify().notify_waiters();
}

/// Settings plus other state shown by the Settings page
pub fn snapshot() -> Value {
    let mut v = serde_json::to_value(get()).unwrap_or(json!({}));
    v["autostart"] = json!(crate::winutil::autostart_enabled());
    v["clientId"] = json!(crate::spotify::client_id());
    v["spotifyConnected"] = json!(crate::spotify::is_connected());
    v["dataDir"] = json!(data_dir().display().to_string());
    v["defaultStorageDir"] = json!(data_dir().join("shelf-cache").display().to_string());
    v["ramTotalGb"] = json!((crate::shellx::total_ram() as f64 / 1073741824.0 * 10.0).round() / 10.0);
    v["ramLimitGb"] = json!((crate::shelf::ram_limit_bytes() as f64 / 1073741824.0 * 10.0).round() / 10.0);
    v["usedBytes"] = json!(crate::shelf::used_bytes());
    v["monitors"] = json!(crate::winutil::list_monitors());
    v["platform"] = json!(std::env::consts::OS);
    #[cfg(target_os = "macos")]
    { v["mac"] = crate::macos::permissions_snapshot(); }
    v
}

/// Update the Settings page when shelf usage changes (without rebuilding the tray)
pub fn broadcast_usage() { emit("settings", snapshot()); }

pub fn broadcast() {
    emit("settings", snapshot());
    crate::rebuild_tray();
    if let Some(w) = crate::util::app().get_webview_window("settings") {
        let _ = w.set_title(tr("settings_title"));
    }
}

#[tauri::command]
pub fn get_settings() -> Value { snapshot() }

#[tauri::command]
pub fn set_setting(key: String, value: Value) -> Result<Value, String> {
    let mut s = get();
    match key.as_str() {
        "lang" => {
            let v = value.as_str().unwrap_or("");
            if !matches!(v, "tr" | "en" | "de" | "fr" | "es" | "it" | "pt" | "ru" | "ja" | "ko" | "zh" | "ar") {
                return Err("bad lang".into());
            }
            s.lang = v.into();
        }
        "browserLyrics" => s.browser_lyrics = value.as_bool().ok_or("bool")?,
        "clipboardHistory" => s.clipboard_history = value.as_bool().ok_or("bool")?,
        "saveRamOnExit" => s.save_ram_on_exit = value.as_bool().ok_or("bool")?,
        "showSysMonitor" => s.show_sys_monitor = value.as_bool().ok_or("bool")?,
        "headsetAlerts" => s.headset_alerts = value.as_bool().ok_or("bool")?,
        "privacyDots" => s.privacy_dots = value.as_bool().ok_or("bool")?,
        "audioSwitch" => s.audio_switch = value.as_bool().ok_or("bool")?,
        "autoHideFullscreen" => s.auto_hide_fullscreen = value.as_bool().ok_or("bool")?,
        "globalShortcut" => {
            s.global_shortcut = value.as_bool().ok_or("bool")?;
            #[cfg(target_os = "macos")]
            crate::macos::hotkey::apply(s.global_shortcut);
        }
        "singleSource" => s.single_source = value.as_bool().ok_or("bool")?,
        "recordingDot" => s.recording_dot = value.as_bool().ok_or("bool")?,
        "listeningStats" => s.listening_stats = value.as_bool().ok_or("bool")?,
        "callAlerts" => s.call_alerts = value.as_bool().ok_or("bool")?,
        "weather" => s.weather = value.as_bool().ok_or("bool")?,
        "downloads" => s.downloads = value.as_bool().ok_or("bool")?,
        "spotifyAutoplay" => s.spotify_autoplay = value.as_bool().ok_or("bool")?,
        "easyMode" => s.easy_mode = value.as_bool().ok_or("bool")?,
        "scaleMode" => {
            let v = value.as_str().unwrap_or("");
            if !matches!(v, "auto" | "custom") { return Err("bad scaleMode".into()); }
            s.scale_mode = v.into();
        }
        "customScale" => {
            let v = value.as_u64().ok_or("number")? as u32;
            if !(75..=115).contains(&v) { return Err("bad scale".into()); }
            s.custom_scale = v;
        }
        "autoDim" => s.auto_dim = value.as_bool().ok_or("bool")?,
        "topOffset" => {
            let v = value.as_u64().ok_or("number")? as u32;
            if v > 48 { return Err("bad offset".into()); }
            s.top_offset = v;
        }
        "idleOpacity" => {
            let v = value.as_u64().ok_or("number")? as u32;
            if !(35..=100).contains(&v) { return Err("bad opacity".into()); }
            s.idle_opacity = v;
        }
        "bgStyle" => {
            let v = value.as_str().unwrap_or("");
            if !matches!(v, "pure" | "glass" | "tinted") { return Err("bad bgStyle".into()); }
            s.bg_style = v.into();
        }
        "accentColor" => {
            let v = value.as_str().unwrap_or("");
            let valid_hex = v.len() == 7 && v.starts_with('#') && v[1..].chars().all(|c| c.is_ascii_hexdigit());
            if v != "auto" && !valid_hex { return Err("bad color".into()); }
            s.accent_color = v.into();
        }
        "animSpeed" => {
            let v = value.as_str().unwrap_or("");
            if !matches!(v, "snappy" | "normal" | "relaxed") { return Err("bad speed".into()); }
            s.anim_speed = v.into();
        }
        "clock24h" => s.clock_24h = value.as_bool().ok_or("bool")?,
        "gpuAccel" => s.gpu_accel = value.as_bool().ok_or("bool")?,
        // macOS: open the matching pane of System Settings (Privacy & Security)
        "openPrivacyPane" => {
            #[cfg(target_os = "macos")]
            crate::macos::open_privacy_pane(value.as_str().unwrap_or(""));
            return Ok(snapshot());
        }
        "resetCustom" => {
            s.island_size = "md".into();
            s.scale_mode = "auto".into();
            s.custom_scale = 100;
            s.auto_dim = false;
            s.top_offset = DEFAULT_TOP;
            s.idle_opacity = 100;
            s.bg_style = "pure".into();
            s.accent_color = "auto".into();
            s.anim_speed = "normal".into();
            s.clock_24h = true;
        }
        "islandSize" => {
            let v = value.as_str().unwrap_or("");
            if !matches!(v, "sm" | "md" | "lg") { return Err("bad size".into()); }
            s.island_size = v.into();
        }
        "monitorIndex" => {
            let v = value.as_u64().ok_or("number")? as usize;
            s.monitor_index = v;
            save(&s);
            crate::winutil::place_now();
            broadcast();
            return Ok(snapshot());
        }
        "storage" => {
            let v = value.as_str().unwrap_or("");
            if v != "disk" && v != "ram" { return Err("bad storage".into()); }
            s.storage = v.into();
        }
        "limitGb" => {
            let v = value.as_f64().ok_or("number")?;
            if !(0.5..=1024.0).contains(&v) { return Err("bad limit".into()); }
            s.limit_gb = v;
        }
        "storageDir" => {
            let v = value.as_str().unwrap_or("").trim().to_string();
            if !v.is_empty() && !std::path::Path::new(&v).is_dir() { return Err("folder not found".into()); }
            s.storage_dir = v;
        }
        "autostart" => { crate::winutil::set_autostart(value.as_bool().ok_or("bool")?); }
        "clientId" => {
            let v = value.as_str().unwrap_or("").trim().to_string();
            if v.len() != 32 || !v.chars().all(|c| c.is_ascii_hexdigit()) { return Err("Client ID 32 karakterlik bir kod olmalı".into()); }
            crate::spotify::set_client_id(&v);
        }
        _ => return Err("unknown key".into()),
    }
    save(&s);
    broadcast();
    Ok(snapshot())
}

#[tauri::command]
pub async fn open_settings(app: AppHandle) { show_window(&app); }

/// On Windows, creating a window inside a synchronous command/event blocks the main loop: create it in a background task
pub fn show_window(app: &AppHandle) {
    // macOS: an accessory (menu bar) app must activate itself so the Settings window comes to the front
    #[cfg(target_os = "macos")]
    crate::macos::activate_self();
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.unminimize(); let _ = w.show(); let _ = w.set_focus();
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("settings.html".into()))
            .title(tr("settings_title"))
            .inner_size(560.0, 790.0)
            .resizable(false)
            .maximizable(false)
            .center()
            .build();
    });
}
