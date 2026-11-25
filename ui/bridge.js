// Tauri counterpart of Electron's window.island API: the UI code runs unchanged.
(() => {
  const T = window.__TAURI__;
  const invoke = (cmd, args) => T.core.invoke(cmd, args).catch((e) => console.warn(cmd, e));
  const on = (ev, cb) => T.event.listen(ev, (e) => cb(e.payload));

  const IS_MAC = /Mac/i.test(navigator.platform || navigator.userAgent || '');
  document.documentElement.classList.toggle('mac', IS_MAC);

  window.island = {
    platform: IS_MAC ? 'macos' : 'windows',
    onNotch: (cb) => on('notch', cb),
    // media / volume / window
    onMedia: (cb) => on('media', cb),
    cmd: (c) => invoke('media_cmd', { cmd: c }),
    audio: (c) => invoke('audio_cmd', { cmd: c }),
    menu: () => invoke('show_menu'),
    rect: (r) => invoke('set_rect', { r }),
    kbdFocus: (v) => invoke('kbd_focus', { on: !!v }),
    getSettings: () => T.core.invoke('get_settings').catch(() => ({})),
    setSetting: (key, value) => invoke('set_setting', { key, value }),
    onSettings: (cb) => on('settings', cb),

    // shelf
    pathFor: () => '',
    onShelf: (cb) => on('shelf', cb),
    onShelfMsg: (cb) => on('shelf-msg', cb),
    shelfGet: () => invoke('shelf_get'),
    shelfAdd: (paths) => invoke('shelf_add', { paths }),
    shelfRemove: (id) => invoke('shelf_remove', { id }),
    shelfClear: () => invoke('shelf_clear'),
    shelfOpen: (id) => invoke('shelf_open', { id }),
    shelfMenu: (id) => invoke('shelf_menu', { id }),
    shelfDrag: (ids) => invoke('shelf_drag', { ids }),
    shelfCopy: (ids) => invoke('shelf_copy', { ids }),
    shelfPaste: () => invoke('shelf_paste'),
    // In Tauri, file drop events come from the window: { type: enter|over|drop|leave, paths }
    onFileDrag: (cb) => T.webviewWindow.getCurrentWebviewWindow().onDragDropEvent((e) => cb(e.payload)),

    // spotify
    onSpotify: (cb) => on('sp-state', cb),
    spGet: () => invoke('sp_get'),
    spLike: () => invoke('sp_like'),
    spQueue: () => invoke('sp_queue'),
    spSkipTo: (i) => invoke('sp_skipto', { i }),
    spConnect: () => invoke('sp_connect'),
    spSearch: (q) => invoke('sp_search', { q }),
    onSearchResults: (cb) => on('sp-search-results', cb),
    spPlay: (uri) => invoke('sp_play', { uri }),
    spEnqueue: (uri) => invoke('sp_enqueue', { uri }),
    spDevices: () => invoke('sp_devices'),
    onSpDevices: (cb) => on('sp-devices', cb),
    spTransfer: (deviceId) => invoke('sp_transfer', { deviceId }),

    // Apple Music (macOS): heart, AirPlay devices, library search
    amFavorite: (on) => invoke('am_favorite', { on: !!on }),
    amDevices: () => invoke('am_devices'),
    amAirplay: (name, add) => invoke('am_airplay', { name, add: !!add }),
    amSearch: (q) => invoke('am_search', { q }),
    amPlay: (uri) => invoke('am_play', { uri }),
    amQueue: () => invoke('am_queue'),
    amSkipTo: (index) => invoke('am_skipto', { index }),
    amMenu: () => invoke('am_menu'),
    onAmQueue: (cb) => on('am-queue', cb),
    onAmMsg: (cb) => on('am-msg', cb),

    // lyrics
    lyrics: (q) => T.core.invoke('lyrics_get', { q }).catch(() => ({ found: false })),

    // system / audio output / headphones / fullscreen / shortcut
    switchAudioOutput: () => T.core.invoke('switch_audio_output').catch(() => ({ ok: false })),
    getSysmon: () => T.core.invoke('get_sysmon').catch(() => ({})),
    onSysmon: (cb) => on('sysmon', cb),
    onHeadsetConnected: (cb) => on('headset-connected', cb),
    getWeather: () => T.core.invoke('get_weather').catch(() => ({})),
    onWeather: (cb) => on('weather', cb),
    onDownloads: (cb) => on('downloads', cb),
    onDownloadDone: (cb) => on('download-done', cb),
    downloadOpen: (path) => invoke('download_open', { path }),
    focusApp: (appName) => invoke('focus_notif_app', { appName }),
    onFullscreen: (cb) => on('fullscreen', cb),
    onShortcut: (cb) => on('shortcut', cb),
    getStats: () => T.core.invoke('get_stats').catch(() => ({})),
    onStats: (cb) => on('stats', cb),
  };
})();
