#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use slint::{Model, VecModel, SharedString, Timer, TimerMode};
use rfd::FileDialog;
use serde::{Serialize, Deserialize};
use notify::{Watcher, Event};
use slint::winit_030::{CustomApplicationHandler, EventResult};

type Hresult = i32;

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const CLSID_SHELL_LINK: Guid = Guid {
    data1: 0x00021401,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

const IID_ISHELL_LINK_W: Guid = Guid {
    data1: 0x000214F9,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

const IID_IPERSIST_FILE: Guid = Guid {
    data1: 0x0000010B,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(pvReserved: *mut std::ffi::c_void, dwCoInit: u32) -> Hresult;
    fn CoCreateInstance(
        rclsid: *const Guid,
        pUnkOuter: *mut std::ffi::c_void,
        dwClsContext: u32,
        riid: *const Guid,
        ppv: *mut *mut std::ffi::c_void,
    ) -> Hresult;
    fn CoUninitialize();
}

#[repr(C)]
struct IShellLinkWVtbl {
    query_interface: unsafe extern "system" fn(*mut std::ffi::c_void, *const Guid, *mut *mut std::ffi::c_void) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    get_path: unsafe extern "system" fn(*mut std::ffi::c_void, *mut u16, i32, *mut std::ffi::c_void, u32) -> Hresult,
}

#[repr(C)]
struct IPersistFileVtbl {
    query_interface: unsafe extern "system" fn(*mut std::ffi::c_void, *const Guid, *mut *mut std::ffi::c_void) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    get_class_id: unsafe extern "system" fn(*mut std::ffi::c_void, *mut Guid) -> Hresult,
    is_dirty: unsafe extern "system" fn(*mut std::ffi::c_void) -> Hresult,
    load: unsafe extern "system" fn(*mut std::ffi::c_void, *const u16, u32) -> Hresult,
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn resolve_lnk(path: &str) -> Option<(String, String)> {
    unsafe {
        let co_result = CoInitializeEx(ptr::null_mut(), 2);
        if co_result < 0 && co_result != -2147417835 {
            return None;
        }

        let mut shell_link: *mut std::ffi::c_void = ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_SHELL_LINK,
            ptr::null_mut(),
            1,
            &IID_ISHELL_LINK_W,
            &mut shell_link,
        );
        if hr < 0 || shell_link.is_null() {
            if co_result >= 0 { CoUninitialize(); }
            return None;
        }

        let sl_vtbl = *(shell_link as *mut *const IShellLinkWVtbl);

        let mut persist_file: *mut std::ffi::c_void = ptr::null_mut();
        let hr = ((*sl_vtbl).query_interface)(shell_link, &IID_IPERSIST_FILE, &mut persist_file);
        if hr < 0 || persist_file.is_null() {
            ((*sl_vtbl).release)(shell_link);
            if co_result >= 0 { CoUninitialize(); }
            return None;
        }

        let pf_vtbl = *(persist_file as *mut *const IPersistFileVtbl);
        let wide_path = to_wide(path);
        let hr = ((*pf_vtbl).load)(persist_file, wide_path.as_ptr(), 0);
        if hr < 0 {
            ((*pf_vtbl).release)(persist_file);
            ((*sl_vtbl).release)(shell_link);
            if co_result >= 0 { CoUninitialize(); }
            return None;
        }

        let mut buf = [0u16; 1024];
        let hr = ((*sl_vtbl).get_path)(shell_link, buf.as_mut_ptr(), 1024, ptr::null_mut(), 0);
        if hr >= 0 {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            let target_path = String::from_utf16_lossy(&buf[..len]);
            if !target_path.is_empty() {
                let path_obj = std::path::Path::new(&target_path);
                let name = path_obj
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown")
                    .to_string();
                ((*pf_vtbl).release)(persist_file);
                ((*sl_vtbl).release)(shell_link);
                if co_result >= 0 { CoUninitialize(); }
                return Some((name, target_path));
            }
        }

        ((*pf_vtbl).release)(persist_file);
        ((*sl_vtbl).release)(shell_link);
        if co_result >= 0 { CoUninitialize(); }
        None
    }
}

// ビルドスクリプトによって出力されたRustコードを取り込む
slint::include_modules!();

// 1. 保存用の構造体（差分比較のため PartialEq を実装）
#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct SavedApp {
    name: String,
    path: String,
}

// 2. メイン（UI）スレッドからのみアクセスされる、モデルへのスレッドローカル参照
thread_local! {
    static APPS_MODEL: RefCell<Option<Rc<VecModel<AppItem>>>> = const { RefCell::new(None) };
}

// ファイルからデータを読み込む関数
fn load_apps() -> Vec<SavedApp> {
    std::fs::read_to_string("apps.json")
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

// jsonファイルに保存する関数
fn save_apps(apps: &[SavedApp]) {
    if let Ok(json) = serde_json::to_string_pretty(apps) {
        let _ = std::fs::write("apps.json", json);
    }
}

// 3. 設定用の構造体
#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    confirm_on_delete: bool,
    show_edit_buttons: bool,
    app_name_font_size: i32,
    app_path_font_size: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            confirm_on_delete: true,
            show_edit_buttons: true,
            app_name_font_size: 14,
            app_path_font_size: 11,
        }
    }
}

// settings.json から読み込む（なければデフォルトで作成）
fn load_settings() -> Settings {
    match std::fs::read_to_string("settings.json") {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => {
            let defaults = Settings::default();
            if let Ok(json) = serde_json::to_string_pretty(&defaults) {
                let _ = std::fs::write("settings.json", json);
            }
            defaults
        }
    }
}

// 4. DnDハンドラ（OSからのファイルドロップをキャプチャ）
struct DndHandler {
    pending_files: Arc<Mutex<Vec<PathBuf>>>,
    theme_set: bool,
}

#[cfg(target_os = "windows")]
#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: *mut std::ffi::c_void,
        dwAttribute: u32,
        pvAttribute: *const u32,
        cbAttribute: u32,
    ) -> i32;
}

#[cfg(target_os = "windows")]
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;

impl CustomApplicationHandler for DndHandler {
    fn window_event(
        &mut self,
        _event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        winit_window: Option<&winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &winit::event::WindowEvent,
    ) -> EventResult {
        #[cfg(target_os = "windows")]
        if !self.theme_set {
            if let Some(w) = winit_window {
                use raw_window_handle::HasWindowHandle;
                if let Ok(handle) = w.window_handle() {
                    let raw = handle.as_raw();
                    if let raw_window_handle::RawWindowHandle::Win32(win32) = raw {
                        let hwnd = win32.hwnd.get() as *mut std::ffi::c_void;
                        let value: u32 = 0;
                        unsafe {
                            DwmSetWindowAttribute(
                                hwnd,
                                DWMWA_USE_IMMERSIVE_DARK_MODE,
                                &value as *const u32,
                                4,
                            );
                        }
                    }
                }
                self.theme_set = true;
            }
        }
        if let winit::event::WindowEvent::DroppedFile(path) = event {
            self.pending_files.lock().unwrap().push(path.clone());
        }
        EventResult::Propagate
    }
}


fn main() -> Result<(), Box<dyn std::error::Error>> {
    // DnDバックエンドの設定（MainWindow生成前に必要）
    let pending_files: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let _backend = slint::BackendSelector::new()
        .with_winit_custom_application_handler(DndHandler {
            pending_files: pending_files.clone(),
            theme_set: false,
        })
        .select();

    let ui = MainWindow::new()?;

    // データの読み込みと、スレッド間で共有する「現在の最新データ状態」
    let loaded_apps = load_apps();
    let shared_apps = Arc::new(Mutex::new(loaded_apps.clone()));

    // SlintのUIと同期するためのモデルを構築
    let apps_model = Rc::new(VecModel::<AppItem>::default());
    for app in &loaded_apps {
        apps_model.push(AppItem {
            name: SharedString::from(app.name.clone()),
            path: SharedString::from(app.path.clone()),
        });
    }

    // 後からファイル監視スレッド経由で安全にアクセスできるよう、スレッドローカルに保管
    APPS_MODEL.with(|m| {
        *m.borrow_mut() = Some(apps_model.clone());
    });

    // UIにモデルを設定
    ui.set_apps(apps_model.clone().into());

    // 設定の読み込みと適用
    let settings = load_settings();
    ui.set_app_name_font_size(settings.app_name_font_size);
    ui.set_app_path_font_size(settings.app_path_font_size);
    ui.set_show_edit_buttons(settings.show_edit_buttons);
    let confirm_on_delete = Arc::new(AtomicBool::new(settings.confirm_on_delete));

    // --- コールバック処理の実装 ---

    // 0. トースト通知の設定
    let ui_handle_for_toast = ui.as_weak();
    let toast_timer = Rc::new(Timer::default());
    let toast_timer_clone = toast_timer.clone();
    ui.on_toast_close(move || {
        if let Some(ui) = ui_handle_for_toast.upgrade() {
            ui.set_toast_text(SharedString::default());
        }
        toast_timer_clone.stop();
    });

    // 1. 起動処理
    let ui_handle_launch = ui.as_weak();
    let toast_timer_clone = toast_timer.clone();
    ui.on_launch_app(move |path| {
        let path_str = path.to_string();
        if let Err(err) = std::process::Command::new(&path_str).spawn() {
            let msg = format!("{}を起動できませんでした: {}", path_str, err);
            eprintln!("{}", msg);
            if let Some(ui) = ui_handle_launch.upgrade() {
                ui.set_toast_text(SharedString::from(&msg));
                let ui_timer = ui.as_weak();
                toast_timer_clone.start(
                    TimerMode::SingleShot,
                    std::time::Duration::from_secs(5),
                    move || {
                        if let Some(ui) = ui_timer.upgrade() {
                            ui.set_toast_text(SharedString::default());
                        }
                    },
                );
            }
        }
    });

    // 2. アプリの追加処理（複数ファイル対応、.lnk は解決）
    let apps_model_clone = apps_model.clone();
    let shared_apps_clone = shared_apps.clone();
    ui.on_add_app_clicked(move || {
        if let Some(file_paths) = FileDialog::new()
            .add_filter("実行可能ファイル", &["exe", "lnk"])
            .pick_files()
        {
            let mut new_apps = Vec::new();

            for file_path in &file_paths {
                let path_str = file_path.to_string_lossy().to_string();
                let (name, path) = if path_str.to_lowercase().ends_with(".lnk") {
                    resolve_lnk(&path_str).unwrap_or_else(|| {
                        let n = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown").to_string();
                        (n, path_str.clone())
                    })
                } else {
                    let n = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown").to_string();
                    (n, path_str)
                };

                new_apps.push(SavedApp { name: name.clone(), path: path.clone() });

                apps_model_clone.push(AppItem {
                    name: SharedString::from(name),
                    path: SharedString::from(path),
                });
            }

            if !new_apps.is_empty() {
                let mut apps = shared_apps_clone.lock().unwrap();
                apps.extend(new_apps);
                save_apps(&apps);
            }
        }
    });

    // 3. アプリの削除処理（確認ダイアログ付き）
    let apps_model_clone = apps_model.clone();
    let shared_apps_clone = shared_apps.clone();
    let confirm_on_delete_clone = confirm_on_delete.clone();
    ui.on_delete_app(move |idx| {
        if idx >= 0 && (idx as usize) < apps_model_clone.row_count() {
            // 確認ダイアログの表示が設定されている場合のみ表示
            if confirm_on_delete_clone.load(Ordering::Relaxed) {
                let app_name = apps_model_clone
                    .row_data(idx as usize)
                    .map(|app| app.name.to_string())
                    .unwrap_or_else(|| "アプリケーション".to_string());

                let confirm = rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title("削除の確認")
                    .set_description(format!("{}を一覧から削除してもよろしいですか？", app_name))
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show();

                if confirm != rfd::MessageDialogResult::Yes {
                    return;
                }
            }

            // 共有状態を更新してファイルへ保存
            {
                let mut apps = shared_apps_clone.lock().unwrap();
                if (idx as usize) < apps.len() {
                    apps.remove(idx as usize);
                    save_apps(&apps);
                }
            }

            // UIモデルから削除
            apps_model_clone.remove(idx as usize);
        }
    });

    // --- ファイル変更監視（ホットリロード）の設定 ---
    let shared_apps_watcher = shared_apps.clone();
    let ui_handle = ui.as_weak();

    // 監視スレッドからのファイルシステムイベントを処理するハンドラ
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            // イベントの中に "apps.json" が含まれているか確認
            if event.paths.iter().any(|p| p.ends_with("apps.json")) {
                let file_apps = load_apps();
                let mut current_apps = shared_apps_watcher.lock().unwrap();

                // 外部エディタなどで中身が「実際に変更された」場合のみUIを更新する
                // （これがないと、アプリ本体が保存した際にも再ロードが発生してしまいます）
                if file_apps != *current_apps {
                    *current_apps = file_apps.clone();

                    // メインスレッド（UIスレッド）のイベントループ上で安全にモデルを差し替え
                    let _ = ui_handle.upgrade_in_event_loop(move |_ui| {
                        APPS_MODEL.with(|m| {
                            if let Some(ref model) = *m.borrow() {
                                let new_items: Vec<AppItem> = file_apps
                                    .into_iter()
                                    .map(|app| AppItem {
                                        name: SharedString::from(app.name),
                                        path: SharedString::from(app.path),
                                    })
                                    .collect();
                                
                                // Slint 1.16 で提供された set_vec を用いてUIモデル全体を置換
                                model.set_vec(new_items);
                            }
                        });
                    });
                }
            }
        }
    })?;

    // カレントディレクトリ（"."）を監視
    // （ファイル直接監視は、エディタによっては一時ファイル保存時の挙動により監視が途切れるため、フォルダ監視が推奨されます）
    watcher.watch(std::path::Path::new("."), notify::RecursiveMode::NonRecursive)?;

    // 監視インスタンス（watcher）がmain関数を抜けて自動消滅（ドロップ）しないよう、変数として保持
    let _watcher = watcher;

    // DnD でドロップされたファイルを定期的に処理
    let pending_files_timer = pending_files.clone();
    let apps_model_dnd = apps_model.clone();
    let shared_apps_dnd = shared_apps.clone();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(200), move || {
        let paths: Vec<PathBuf> = {
            let mut pending = pending_files_timer.lock().unwrap();
            pending.drain(..).collect()
        };
        if paths.is_empty() {
            return;
        }

        let allowed_extensions = ["exe", "lnk"];
        let mut new_apps = Vec::new();
        for file_path in &paths {
            let ext = file_path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if !allowed_extensions.contains(&ext.as_str()) {
                continue;
            }
            let path_str = file_path.to_string_lossy().to_string();
            let (name, path) = if path_str.to_lowercase().ends_with(".lnk") {
                resolve_lnk(&path_str).unwrap_or_else(|| {
                    let n = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown").to_string();
                    (n, path_str.clone())
                })
            } else {
                let n = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown").to_string();
                (n, path_str)
            };
            new_apps.push(SavedApp { name: name.clone(), path: path.clone() });
            apps_model_dnd.push(AppItem {
                name: SharedString::from(name),
                path: SharedString::from(path),
            });
        }

        let mut apps = shared_apps_dnd.lock().unwrap();
        apps.extend(new_apps);
        save_apps(&apps);
    });
    let _timer = timer;

    ui.run()?;
    Ok(())
}