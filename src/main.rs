#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use notify::{Event, Watcher};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use slint::winit_030::{CustomApplicationHandler, EventResult};
use slint::{Model, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
    query_interface: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        *const Guid,
        *mut *mut std::ffi::c_void,
    ) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    get_path: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        *mut u16,
        i32,
        *mut std::ffi::c_void,
        u32,
    ) -> Hresult,
}

#[repr(C)]
struct IPersistFileVtbl {
    query_interface: unsafe extern "system" fn(
        *mut std::ffi::c_void,
        *const Guid,
        *mut *mut std::ffi::c_void,
    ) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32,
    get_class_id: unsafe extern "system" fn(*mut std::ffi::c_void, *mut Guid) -> Hresult,
    is_dirty: unsafe extern "system" fn(*mut std::ffi::c_void) -> Hresult,
    load: unsafe extern "system" fn(*mut std::ffi::c_void, *const u16, u32) -> Hresult,
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
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
            if co_result >= 0 {
                CoUninitialize();
            }
            return None;
        }

        let sl_vtbl = *(shell_link as *mut *const IShellLinkWVtbl);

        let mut persist_file: *mut std::ffi::c_void = ptr::null_mut();
        let hr = ((*sl_vtbl).query_interface)(shell_link, &IID_IPERSIST_FILE, &mut persist_file);
        if hr < 0 || persist_file.is_null() {
            ((*sl_vtbl).release)(shell_link);
            if co_result >= 0 {
                CoUninitialize();
            }
            return None;
        }

        let pf_vtbl = *(persist_file as *mut *const IPersistFileVtbl);
        let wide_path = to_wide(path);
        let hr = ((*pf_vtbl).load)(persist_file, wide_path.as_ptr(), 0);
        if hr < 0 {
            ((*pf_vtbl).release)(persist_file);
            ((*sl_vtbl).release)(shell_link);
            if co_result >= 0 {
                CoUninitialize();
            }
            return None;
        }

        let mut buf = [0u16; 1024];
        let hr = ((*sl_vtbl).get_path)(shell_link, buf.as_mut_ptr(), 1024, ptr::null_mut(), 0);
        if hr >= 0 {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            let target_path = String::from_utf16_lossy(&buf[..len]);
            if !target_path.is_empty() {
                let path_obj = Path::new(&target_path);
                let name = path_obj
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown")
                    .to_string();
                ((*pf_vtbl).release)(persist_file);
                ((*sl_vtbl).release)(shell_link);
                if co_result >= 0 {
                    CoUninitialize();
                }
                return Some((name, target_path));
            }
        }

        ((*pf_vtbl).release)(persist_file);
        ((*sl_vtbl).release)(shell_link);
        if co_result >= 0 {
            CoUninitialize();
        }
        None
    }
}

// ビルドスクリプトによって出力されたRustコードを取り込む
slint::include_modules!();

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct SavedApp {
    name: String,
    path: String,
}

thread_local! {
    static APPS_MODEL: RefCell<Option<Rc<VecModel<AppItem>>>> = const { RefCell::new(None) };
}

/// 実行ファイルの置かれているディレクトリ配下のパスを取得
fn get_data_path(file_name: &str) -> PathBuf {
    #[cfg(debug_assertions)]
    {
        // cargo run のときはカレントディレクトリ（プロジェクトルート）を使う
        PathBuf::from(file_name)
    }

    #[cfg(not(debug_assertions))]
    {
        // 本番ビルド (cargo build --release) のときは exe と同じフォルダを使う
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|parent| parent.join(file_name)))
            .unwrap_or_else(|| PathBuf::from(file_name))
    }
}

/// apps.json の読み込み（パース失敗時は None を返し、前回の状態を破壊しない）
fn load_apps() -> Option<Vec<SavedApp>> {
    let path = get_data_path("apps.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// apps.json の保存
fn save_apps(apps: &[SavedApp]) {
    let path = get_data_path("apps.json");
    if let Ok(json) = serde_json::to_string_pretty(apps) {
        let _ = std::fs::write(path, json);
    }
}

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
            show_edit_buttons: false,
            app_name_font_size: 14,
            app_path_font_size: 11,
        }
    }
}

fn load_settings() -> Settings {
    let path = get_data_path("settings.json");
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => {
            let defaults = Settings::default();
            if let Ok(json) = serde_json::to_string_pretty(&defaults) {
                let _ = std::fs::write(path, json);
            }
            defaults
        }
    }
}

/// 複数ファイル（ダイアログまたはDnD）からアプリを追加する共通関数（重複防止付き）
fn add_apps_from_paths(
    paths: &[PathBuf],
    shared_apps: &Arc<Mutex<Vec<SavedApp>>>,
    apps_model: &Rc<VecModel<AppItem>>,
) {
    let allowed_extensions = ["exe", "lnk"];
    let mut new_saved = Vec::new();
    let mut current_apps = shared_apps.lock().unwrap();

    for file_path in paths {
        let ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        if !allowed_extensions.contains(&ext.as_str()) {
            continue;
        }

        let path_str = file_path.to_string_lossy().to_string();
        let (name, target_path) = if ext == "lnk" {
            resolve_lnk(&path_str).unwrap_or_else(|| {
                let n = file_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown")
                    .to_string();
                (n, path_str)
            })
        } else {
            let n = file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown")
                .to_string();
            (n, path_str)
        };

        // 既に同じパスが登録されている場合はスキップ
        if current_apps.iter().any(|app| app.path == target_path) {
            continue;
        }

        new_saved.push(SavedApp {
            name: name.clone(),
            path: target_path.clone(),
        });

        apps_model.push(AppItem {
            name: SharedString::from(name),
            path: SharedString::from(target_path),
        });
    }

    if !new_saved.is_empty() {
        current_apps.extend(new_saved);
        save_apps(&current_apps);
    }
}

// 4. DnDハンドラ
struct DndHandler {
    shared_apps: Arc<Mutex<Vec<SavedApp>>>,
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

        // ファイルがドロップされたらタイマーを待たずにUIスレッド上で直接追加処理を実行
        if let winit::event::WindowEvent::DroppedFile(path) = event {
            let path = path.clone();
            let shared_apps = self.shared_apps.clone();
            let _ = slint::invoke_from_event_loop(move || {
                APPS_MODEL.with(|m| {
                    if let Some(ref model) = *m.borrow() {
                        add_apps_from_paths(&[path], &shared_apps, model);
                    }
                });
            });
        }
        EventResult::Propagate
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let loaded_apps = load_apps().unwrap_or_default();
    let shared_apps = Arc::new(Mutex::new(loaded_apps.clone()));

    // DnDバックエンドの設定
    let _backend = slint::BackendSelector::new()
        .with_winit_custom_application_handler(DndHandler {
            shared_apps: shared_apps.clone(),
            theme_set: false,
        })
        .select();

    let ui = MainWindow::new()?;

    // SlintのUIモデルを構築
    let apps_model = Rc::new(VecModel::<AppItem>::default());
    for app in &loaded_apps {
        apps_model.push(AppItem {
            name: SharedString::from(app.name.clone()),
            path: SharedString::from(app.path.clone()),
        });
    }

    APPS_MODEL.with(|m| {
        *m.borrow_mut() = Some(apps_model.clone());
    });

    ui.set_apps(apps_model.clone().into());

    let settings = load_settings();
    ui.set_app_name_font_size(settings.app_name_font_size);
    ui.set_app_path_font_size(settings.app_path_font_size);
    ui.set_show_edit_buttons(settings.show_edit_buttons);
    let confirm_on_delete = Arc::new(AtomicBool::new(settings.confirm_on_delete));

    // トースト通知の閉じるコールバック
    let ui_handle_for_toast = ui.as_weak();
    let toast_timer = Rc::new(Timer::default());
    let toast_timer_clone = toast_timer.clone();
    ui.on_toast_close(move || {
        if let Some(ui) = ui_handle_for_toast.upgrade() {
            ui.set_toast_text(SharedString::default());
        }
        toast_timer_clone.stop();
    });

    // 1. 起動処理（作業ディレクトリを実行ファイルの場所に設定）
    let ui_handle_launch = ui.as_weak();
    let toast_timer_clone = toast_timer.clone();
    ui.on_launch_app(move |path| {
        let path_str = path.to_string();
        let target_path = Path::new(&path_str);

        let mut cmd = std::process::Command::new(target_path);
        // カレントディレクトリをexeの親フォルダに設定
        if let Some(parent) = target_path.parent() {
            if parent.exists() && parent.is_dir() {
                cmd.current_dir(parent);
            }
        }

        if let Err(err) = cmd.spawn() {
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

    // 2. アプリの追加処理
    let apps_model_clone = apps_model.clone();
    let shared_apps_clone = shared_apps.clone();
    ui.on_add_app_clicked(move || {
        if let Some(file_paths) = FileDialog::new()
            .add_filter("実行可能ファイル", &["exe", "lnk"])
            .pick_files()
        {
            add_apps_from_paths(&file_paths, &shared_apps_clone, &apps_model_clone);
        }
    });

    // 3. アプリの削除処理
    let apps_model_clone = apps_model.clone();
    let shared_apps_clone = shared_apps.clone();
    let confirm_on_delete_clone = confirm_on_delete.clone();
    ui.on_delete_app(move |idx| {
        if idx >= 0 && (idx as usize) < apps_model_clone.row_count() {
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

            {
                let mut apps = shared_apps_clone.lock().unwrap();
                if (idx as usize) < apps.len() {
                    apps.remove(idx as usize);
                    save_apps(&apps);
                }
            }

            apps_model_clone.remove(idx as usize);
        }
    });

    // 4. ファイル変更監視（ホットリロード）
    let shared_apps_watcher = shared_apps.clone();
    let ui_handle = ui.as_weak();

    // 監視対象のディレクトリを取得
    let apps_path = get_data_path("apps.json");
    let watch_dir = apps_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            if event.paths.iter().any(|p| p.ends_with("apps.json")) {
                if let Some(file_apps) = load_apps() {
                    let mut current_apps = shared_apps_watcher.lock().unwrap();

                    if file_apps != *current_apps {
                        *current_apps = file_apps.clone();

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

                                    model.set_vec(new_items);
                                }
                            });
                        });
                    }
                }
            }
        }
    })?;

    watcher.watch(watch_dir, notify::RecursiveMode::NonRecursive)?;
    let _watcher = watcher;

    ui.run()?;
    Ok(())
}
