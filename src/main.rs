#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::rc::Rc;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use slint::{Model, VecModel, SharedString};
use rfd::FileDialog;
use serde::{Serialize, Deserialize};
use notify::{Watcher, Event};

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
    static APPS_MODEL: RefCell<Option<Rc<VecModel<AppItem>>>> = RefCell::new(None);
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    // --- コールバック処理の実装 ---

    // 1. 起動処理
    ui.on_launch_app(|path| {
        let path_str = path.to_string();
        if let Err(err) = std::process::Command::new(&path_str).spawn() {
            eprintln!("Failed to launch {}: {}", path_str, err);
        }
    });

    // 2. アプリの追加処理
    let apps_model_clone = apps_model.clone();
    let shared_apps_clone = shared_apps.clone();
    ui.on_add_app_clicked(move || {
        if let Some(file_path) = FileDialog::new()
            .add_filter("実行ファイル", &["exe"])
            .pick_file()
        {
            let name = file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown")
                .to_string();
            let path = file_path.to_string_lossy().to_string();

            let new_app = SavedApp { name: name.clone(), path: path.clone() };

            // 共有状態を更新してファイルへ保存
            {
                let mut apps = shared_apps_clone.lock().unwrap();
                apps.push(new_app);
                save_apps(&apps);
            }

            // UIモデルも更新
            apps_model_clone.push(AppItem {
                name: SharedString::from(name),
                path: SharedString::from(path),
            });
        }
    });

    // 3. アプリの削除処理（確認ダイアログ付き）
    let apps_model_clone = apps_model.clone();
    let shared_apps_clone = shared_apps.clone();
    ui.on_delete_app(move |idx| {
        if idx >= 0 && (idx as usize) < apps_model_clone.row_count() {
            let app_name = apps_model_clone
                .row_data(idx as usize)
                .map(|app| app.name.to_string())
                .unwrap_or_else(|| "アプリケーション".to_string());

            let confirm = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("削除の確認")
                .set_description(format!("「{}」を一覧から削除してもよろしいですか？", app_name))
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();

            if confirm == rfd::MessageDialogResult::Yes {
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

    ui.run()?;
    Ok(())
}