#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Platform layer: the same module names (and public API) on both systems.
// Windows: WinRT / Win32 implementations next to this file. macOS: native AppKit / CoreAudio / MediaRemote in ./macos.
mod applemusic;
#[cfg_attr(target_os = "macos", path = "macos/audio.rs")]
mod audio;
mod downloads;
mod weather;
mod lyrics;
#[cfg_attr(target_os = "macos", path = "macos/media.rs")]
mod media;
#[cfg_attr(target_os = "macos", path = "macos/notif.rs")]
mod notif;
mod settings;
#[cfg_attr(target_os = "macos", path = "macos/shellx.rs")]
mod shellx;
mod shelf;
mod spotify;
mod stats;
#[cfg_attr(target_os = "macos", path = "macos/sysmon.rs")]
mod sysmon;
#[cfg(windows)]
mod uia;
mod util;
#[cfg_attr(target_os = "macos", path = "macos/winutil.rs")]
mod winutil;
#[cfg(target_os = "macos")]
mod macos;

use serde::Deserialize;
use settings::tr;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
#[cfg(not(target_os = "macos"))]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct Rect { pub x: f64, pub y: f64, pub w: f64, pub h: f64 }

/* ---------- commands ---------- */
#[tauri::command]
fn media_cmd(cmd: String) {
    let ok = matches!(cmd.as_str(), "toggle" | "next" | "prev" | "shuffle" | "repeat")
        || (cmd.starts_with("seek ") && cmd[5..].parse::<f64>().is_ok())
        || (cmd.starts_with("focus ") && cmd.len() < 200
            && cmd[6..].chars().all(|c| c.is_ascii_alphanumeric() || "._!-{}\\".contains(c)));
    if ok { media::send(&cmd); }
}

#[tauri::command]
fn audio_cmd(cmd: String) {
    let ok = matches!(cmd.as_str(), "up" | "down" | "mute")
        || (cmd.starts_with("set ") && cmd[4..].parse::<f32>().map(|v| (0.0..=1.0).contains(&v)).unwrap_or(false));
    if ok { audio::send(&cmd); }
}

#[tauri::command]
fn set_rect(r: Rect) { winutil::set_rect(r); }

/// Debug builds only: writes the automated test report
#[tauri::command]
fn test_report(_s: String) {
    #[cfg(debug_assertions)]
    if util::is_test() { if let Ok(p) = std::env::var("ISLAND_REPORT") { let _ = std::fs::write(p, _s); } }
}

#[tauri::command]
fn kbd_focus(window: WebviewWindow, on: bool) { winutil::kbd_focus(&window, on); }

/// Right-click menu on the island
#[tauri::command]
fn show_menu(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    let e = |x: tauri::Error| x.to_string();
    let menu = Menu::with_items(&app, &[
        &MenuItem::with_id(&app, "settings", tr("settings"), true, None::<&str>).map_err(e)?,
        &CheckMenuItem::with_id(&app, "autostart", tr("autostart"), true, winutil::autostart_enabled(), None::<&str>).map_err(e)?,
        &MenuItem::with_id(&app, "reload", tr("reload"), true, None::<&str>).map_err(e)?,
        &PredefinedMenuItem::separator(&app).map_err(e)?,
        &MenuItem::with_id(&app, "quit", tr("quit"), true, None::<&str>).map_err(e)?,
    ]).map_err(e)?;
    window.popup_menu(&menu).map_err(e)
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        "settings" => settings::show_window(app),
        "autostart" => { winutil::set_autostart(!winutil::autostart_enabled()); settings::broadcast(); }
        "reload" => if let Some(w) = app.get_webview_window("main") { let _ = w.eval("location.reload()"); },
        "quit" => app.exit(0),
        other if other.starts_with("am-") => applemusic::on_menu(other),
        other => shelf::on_menu(other),
    }
}

/* ---------- system tray ---------- */
fn tray_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    // macOS: the status item is the app's only menu, so it also carries "Open at Login" (with ⌘, / ⌘Q shortcuts)
    #[cfg(target_os = "macos")]
    return Menu::with_items(app, &[
        &MenuItem::with_id(app, "settings", tr("settings"), true, Some("CmdOrCtrl+,"))?,
        &CheckMenuItem::with_id(app, "autostart", tr("autostart"), true, winutil::autostart_enabled(), None::<&str>)?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "quit", tr("quit"), true, Some("CmdOrCtrl+Q"))?,
    ]);
    #[cfg(not(target_os = "macos"))]
    Menu::with_items(app, &[
        &MenuItem::with_id(app, "settings", tr("settings"), true, None::<&str>)?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "quit", tr("quit"), true, None::<&str>)?,
    ])
}

/// Rebuild the tray menu when the language changes
pub fn rebuild_tray() {
    let app = util::app();
    if let (Some(tray), Ok(menu)) = (app.tray_by_id("island"), tray_menu(app)) { let _ = tray.set_menu(Some(menu)); }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    #[cfg(not(target_os = "macos"))]
    let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    // macOS: monochrome template image, tinted automatically for light/dark menu bars
    #[cfg(target_os = "macos")]
    let icon = Image::from_bytes(include_bytes!("../icons/tray-template.png"))?;
    TrayIconBuilder::with_id("island")
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Dynamic Island")
        .menu(&tray_menu(app)?)
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_tray_icon_event(|tray, ev| {
            // left click: Settings; right click: menu (macOS: click opens the menu, like every status item)
            #[cfg(target_os = "macos")]
            let _ = (&tray, &ev);
            #[cfg(not(target_os = "macos"))]
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                settings::show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    // single instance (except in test mode)
    if !util::is_test() && !winutil::single_instance() { return; }

    // WebView2 GPU acceleration (on, or software fallback, depending on the setting)
    #[cfg(windows)]
    if std::env::var_os("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_none() {
        let args = if settings::gpu_accel_early() {
            "--enable-gpu-rasterization --enable-zero-copy --ignore-gpu-blocklist --enable-accelerated-2d-canvas"
        } else {
            "--disable-gpu --disable-gpu-compositing"
        };
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", args);
    }

    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_plugin_global_shortcut::Builder::new().build());
    builder
        .invoke_handler(tauri::generate_handler![
            media_cmd, audio_cmd, set_rect, kbd_focus, show_menu, test_report,
            audio::switch_audio_output, sysmon::get_sysmon, stats::get_stats,
            weather::get_weather, downloads::download_open, notif::focus_notif_app,
            settings::get_settings, settings::set_setting, settings::open_settings, shelf::pick_storage_dir,
            shelf::shelf_get, shelf::shelf_add, shelf::shelf_remove, shelf::shelf_clear, shelf::shelf_open,
            shelf::shelf_menu, shelf::shelf_drag, shelf::shelf_copy, shelf::shelf_paste,
            spotify::sp_get, spotify::sp_like, spotify::sp_queue, spotify::sp_skipto, spotify::sp_connect,
            spotify::sp_disconnect, spotify::sp_search, spotify::sp_play, spotify::sp_enqueue,
            spotify::sp_devices, spotify::sp_transfer,
            lyrics::lyrics_get,
            applemusic::am_favorite, applemusic::am_devices, applemusic::am_airplay, applemusic::am_search, applemusic::am_play,
            applemusic::am_queue, applemusic::am_skipto, applemusic::am_menu,
        ])
        .on_menu_event(|app, ev| on_menu(app, ev.id().as_ref()))
        .setup(|app| {
            // macOS: menu bar agent (no Dock icon / app menu), like the system's own status items
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            util::set_app(app.handle().clone());
            settings::init();
            let win = app.get_webview_window("main").expect("main window");
            winutil::setup(&win);
            shelf::init();
            spotify::init();
            stats::init();
            media::start();
            notif::start();
            audio::start(!util::is_test());
            sysmon::start();
            weather::start();
            downloads::start();
            #[cfg(target_os = "macos")]
            macos::hotkey::apply(settings::get().global_shortcut);
            if let Err(e) = setup_tray(app.handle()) { eprintln!("could not create tray icon: {e}"); }
            let _ = win.show();
            // Test mode (debug builds only)
            #[cfg(debug_assertions)]
            if util::is_test() {
                if let Ok(path) = std::env::var("ISLAND_EVAL") {
                    let w = win.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(6));
                        if let Ok(js) = std::fs::read_to_string(path) { let _ = w.eval(&js); }
                    });
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start Atoll Island")
        .run(|_app, ev| {
            // on exit, save shelf items kept in RAM to disk (if enabled)
            if let tauri::RunEvent::Exit = ev { shelf::on_exit(); }
        });
}
