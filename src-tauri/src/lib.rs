mod adapter;
mod codex;
mod model;
mod monitor;
mod storage;

use monitor::{MonitorService, MonitorSignal};
use serde_json::json;
use std::sync::Arc;
use std::thread;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt as AutoStartManagerExt;
#[cfg(not(target_os = "macos"))]
use tauri_plugin_notification::NotificationExt;

pub struct AppState(pub MonitorService);

#[tauri::command]
fn get_dashboard_snapshot(state: State<'_, AppState>) -> Result<model::DashboardSnapshot, String> {
    state.0.snapshot().map_err(|error| error.to_string())
}

#[tauri::command]
fn get_task_detail(
    task_id: String,
    state: State<'_, AppState>,
) -> Result<model::TaskDetail, String> {
    state
        .0
        .task_detail(&task_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn sync_now(state: State<'_, AppState>) -> Result<model::DashboardSnapshot, String> {
    state.0.sync_now().map_err(|error| error.to_string())
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Result<model::Settings, String> {
    state.0.settings().map_err(|error| error.to_string())
}

#[tauri::command]
fn update_settings(
    settings: model::Settings,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<model::Settings, String> {
    let saved = state
        .0
        .update_settings(settings.clone())
        .map_err(|error| error.to_string())?;
    let autolaunch_result = if settings.launch_at_login {
        app.autolaunch().enable().map_err(|error| error.to_string())
    } else {
        app.autolaunch()
            .disable()
            .map_err(|error| error.to_string())
    };
    if let Err(error) = autolaunch_result {
        // The preference remains saved even when a development build cannot register a login item.
        let _ = app.emit(
            "monitor://sync-error",
            json!({ "message": format!("自动启动设置未应用: {error}") }),
        );
    }
    let _ = app.emit("monitor://settings-updated", saved.clone());
    Ok(saved)
}

#[tauri::command]
fn open_task(task_id: String, state: State<'_, AppState>) -> Result<(), String> {
    state
        .0
        .open_task(&task_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn clear_local_history(state: State<'_, AppState>) -> Result<(), String> {
    state.0.clear_history().map_err(|error| error.to_string())
}

#[tauri::command]
fn get_diagnostics(state: State<'_, AppState>) -> Result<model::Diagnostics, String> {
    state.0.diagnostics().map_err(|error| error.to_string())
}

fn emit_notification(app: &tauri::AppHandle, notification: model::NotificationEvent) {
    let _ = app.emit("monitor://notification", notification.clone());
    #[cfg(target_os = "macos")]
    {
        let app = app.clone();
        let task_id = notification.task_id;
        let title = notification.title;
        let body = notification.body;
        let _ = thread::Builder::new()
            .name("agent-monitor-notification".into())
            .spawn(move || {
                let mut native = notify_rust::Notification::new();
                native
                    .summary(&title)
                    .body(&body)
                    .action("open", "打开任务");
                if let Ok(handle) = native.show() {
                    handle.wait_for_action(|action| {
                        if action != "__closed" {
                            show_quick_window(&app);
                            let _ = app
                                .emit("monitor://open-task-detail", json!({ "taskId": task_id }));
                        }
                    });
                }
            });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app
            .notification()
            .builder()
            .title(notification.title)
            .body(notification.body)
            .show();
    }
}

fn update_tray_summary(app: &tauri::AppHandle, snapshot: &model::DashboardSnapshot) {
    let running = snapshot
        .tasks
        .iter()
        .filter(|task| !task.archived && matches!(task.status, model::TaskStatus::Running))
        .count();
    let needs_action = snapshot
        .tasks
        .iter()
        .filter(|task| !task.archived && matches!(task.status, model::TaskStatus::NeedsAction))
        .count();
    if let Some(tray) = app.tray_by_id("main-tray") {
        let title = [(running, "运行"), (needs_action, "待办")]
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, label)| format!("{count}{label}"))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = tray.set_title((!title.is_empty()).then_some(title));
        let _ = tray.set_tooltip(Some(format!(
            "Agent Monitor · 运行中 {} · 待处理 {}",
            running, needs_action
        )));
    }
}

fn show_quick_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    if let Some(window) = app.get_webview_window("quick") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("quick") {
        let _ = window.hide();
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[tauri::command]
fn show_quick(app: tauri::AppHandle) {
    show_quick_window(&app);
}

#[tauri::command]
fn show_main(app: tauri::AppHandle) {
    show_main_window(&app);
}

fn position_quick_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("quick") else {
        return;
    };
    let monitor = window
        .cursor_position()
        .ok()
        .and_then(|cursor| {
            window
                .available_monitors()
                .ok()?
                .into_iter()
                .find(|monitor| {
                    let position = monitor.position();
                    let size = monitor.size();
                    cursor.x >= position.x as f64
                        && cursor.x < (position.x + size.width as i32) as f64
                        && cursor.y >= position.y as f64
                        && cursor.y < (position.y + size.height as i32) as f64
                })
        })
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return;
    };
    let scale = monitor.scale_factor();
    let work_area = monitor.work_area();
    let margin = (16.0 * scale) as i32;
    let width = window
        .outer_size()
        .map(|size| size.width as i32)
        .unwrap_or((390.0 * scale) as i32);
    let right = work_area.position.x + work_area.size.width as i32;
    let x = (right - width - margin).max(work_area.position.x + margin);
    let y = work_area.position.y + margin;
    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
}

fn forward_signals(app: tauri::AppHandle, receiver: std::sync::mpsc::Receiver<MonitorSignal>) {
    thread::Builder::new()
        .name("agent-monitor-events".into())
        .spawn(move || {
            let mut pending: Option<MonitorSignal> = None;
            loop {
                let signal = match pending.take() {
                    Some(signal) => signal,
                    None => match receiver.recv() {
                        Ok(signal) => signal,
                        Err(_) => break,
                    },
                };
                match signal {
                    MonitorSignal::SnapshotUpdated(snapshot) => {
                        update_tray_summary(&app, &snapshot);
                        let _ = app.emit("monitor://snapshot-updated", snapshot);
                    }
                    MonitorSignal::TaskUpdated(task) => {
                        let _ = app.emit("monitor://task-updated", task);
                    }
                    MonitorSignal::Notification(notification) => {
                        let mut notifications = vec![notification];
                        let deadline = std::time::Duration::from_millis(350);
                        loop {
                            match receiver.recv_timeout(deadline) {
                                Ok(MonitorSignal::Notification(next)) => notifications.push(next),
                                Ok(other) => {
                                    pending = Some(other);
                                    break;
                                }
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        if notifications.len() == 1 {
                            emit_notification(&app, notifications.remove(0));
                        } else {
                            let body = notifications
                                .iter()
                                .map(|item| item.title.as_str())
                                .collect::<Vec<_>>()
                                .join("、");
                            emit_notification(
                                &app,
                                model::NotificationEvent {
                                    event_type: "batch".into(),
                                    task_id: notifications
                                        .first()
                                        .map(|item| item.task_id.clone())
                                        .unwrap_or_default(),
                                    turn_id: None,
                                    title: format!("{} 个任务状态更新", notifications.len()),
                                    body,
                                },
                            );
                        }
                    }
                    MonitorSignal::ConnectionChanged(sync) => {
                        let _ = app.emit("monitor://connection-changed", sync);
                    }
                    MonitorSignal::SyncError(error) => {
                        let _ = app.emit("monitor://sync-error", json!({ "message": error }));
                    }
                }
            }
        })
        .ok();
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                let _ = notify_rust::set_application(if tauri::is_dev() {
                    "com.apple.Terminal"
                } else {
                    "com.agentmonitor.desktop"
                });
            }
            let (store, storage_error) = match storage::SnapshotStore::open_default() {
                Ok(store) => (Arc::new(store), None),
                Err(error) => (
                    Arc::new(
                        storage::SnapshotStore::open_in_memory().map_err(|fallback| {
                            format!("无法打开本地快照数据库: {error}; 内存回退也失败: {fallback}")
                        })?,
                    ),
                    Some(error.to_string()),
                ),
            };
            let service = MonitorService::new(store).map_err(|error| error.to_string())?;
            if let Some(error) = storage_error {
                let _ = service.set_storage_degraded(format!("本地快照数据库不可用: {error}"));
            }
            let receiver = service.subscribe();
            service.start_background();
            app.manage(AppState(service));
            position_quick_window(app.handle());
            if !cfg!(debug_assertions)
                && app
                    .try_state::<AppState>()
                    .and_then(|state| state.0.settings().ok())
                    .map(|settings| settings.launch_at_login)
                    .unwrap_or(false)
            {
                let _ = app.autolaunch().enable();
            }
            forward_signals(app.handle().clone(), receiver);

            let show = MenuItem::with_id(app, "show", "打开任务雷达", true, None::<&str>)?;
            let console = MenuItem::with_id(app, "console", "打开完整控制台", true, None::<&str>)?;
            let sync = MenuItem::with_id(app, "sync", "立即同步", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &console, &sync, &quit])?;
            TrayIconBuilder::with_id("main-tray")
                .icon(
                    app.default_window_icon()
                        .cloned()
                        .expect("default icon is configured"),
                )
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("Agent Monitor")
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_quick_window(tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        show_quick_window(app);
                    }
                    "console" => {
                        show_main_window(app);
                    }
                    "sync" => {
                        if let Some(state) = app.try_state::<AppState>() {
                            let _ = state.0.sync_now();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_dashboard_snapshot,
            get_task_detail,
            sync_now,
            get_settings,
            update_settings,
            show_quick,
            show_main,
            open_task,
            clear_local_history,
            get_diagnostics
        ])
        .run(tauri::generate_context!())
        .expect("error while running Agent Monitor");
}
