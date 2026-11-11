// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright 2026 Bogdan Shapovalov and the Fury authors

//! Fury desktop — the native shell.
//!
//! Thin by design. The shell owns the window, the connection to the server and
//! the session token; everything an operator sees is the React app under
//! `../src`, and everything that touches a profile will be the agent
//! (docs/01-architecture.md). What lives here is what a web page cannot do:
//! keep a credential out of the document, reach a self-hosted server without
//! asking it for CORS, and refuse to run twice.

// No console window behind the app on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod assistants;
mod commands;
mod crypto;
mod session;
mod settings;
mod update;

use std::sync::Mutex;
use std::time::Duration;

use std::sync::atomic::Ordering;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, RunEvent, WindowEvent};

use commands::AppState;
use session::Session;
use settings::Settings;

/// Brings the main window back from the tray: unminimise, show, focus — the
/// same three calls the single-instance handler makes when a second launch is
/// folded into this one.
fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        // Registered first, because plugins run in the order they are added and
        // this one has to decide whether the process should exist at all.
        //
        // What it is for: one settings file and one keychain binding per
        // machine, and the behaviour people expect from a dock icon. What it is
        // *not* for is protecting the profile lock — that lock belongs to the
        // agent (docs/01-architecture.md), and the case it would have to prevent
        // is the same user on two machines, where a single-instance guard on one
        // of them cannot help. The lock has to defend itself server-side.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        // Updates are checked against the public key in tauri.conf.json; the
        // private half never leaves the release machine. See update.rs.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Size, position and maximised, and nothing else: restoring VISIBLE
        // would bring the window back hidden if it was ever saved while
        // hidden, and nothing here hides it on purpose. Saved by the plugin on
        // close; a position on a monitor that is gone is moved back on screen.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .build(),
        )
        .setup(|app| {
            update::maybe_install_on_start(app.handle());
            let config_dir = app.path().app_config_dir()?;
            let settings = Settings::load(&config_dir);
            // The tray is built below from a plain string, and `settings` moves
            // into the managed state — so keep the language out first.
            let tray_language = settings.language.clone();

            let session = Session::new();
            if let Some(url) = settings.server_url.as_deref() {
                session.bind(url);
            }

            let http = reqwest::Client::builder()
                // Long enough for a loaded self-hosted box, short enough that a
                // black-holed address reports itself rather than hanging the UI.
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("fury-desktop/", env!("CARGO_PKG_VERSION")))
                .build()?;

            // Touch the credential store once, here, where a failure is
            // visible. keyring 4 marks its store "initialised" *before* it
            // builds one (v1.rs), so a single transient failure — a locked
            // keychain, a denied prompt — poisons the process: every later
            // Entry::new skips setup and fails against a store that was never
            // installed. Probing at startup turns that into one line in the log
            // at the moment it happens, instead of a session that silently
            // stops being remembered.
            if let Err(e) = keyring::Entry::new("dev.fury.desktop", "startup-probe") {
                eprintln!("keychain unavailable — sessions will not be remembered: {e}");
            }

            app.manage(AppState {
                http,
                settings: Mutex::new(settings),
                config_dir,
                session,
                locks: Default::default(),
                org_key: Default::default(),
                force_quit: Default::default(),
            });

            // A tray icon: the way back in once the window has been hidden to
            // the tray, and a Quit that reaches `RunEvent::Exit` (and so the
            // agent shutdown) cleanly. Built unconditionally rather than only
            // when minimize_to_tray is on, because that setting can be flipped
            // on mid-session — a window hidden before the icon existed would
            // otherwise have no way back.
            //
            // Two items, labelled in the language the operator chose (persisted
            // by the webview through `set_language`). The tray is native, outside
            // the React i18n, so it reads the persisted language here rather than
            // the live one; a change takes effect on the next launch.
            let (show_label, quit_label) = match tray_language.as_str() {
                "zh" => ("主窗口", "退出"),
                _ => ("Show", "Quit"),
            };
            let show = MenuItem::with_id(app, "show", show_label, true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", quit_label, true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show, &quit])?;
            let mut tray = TrayIconBuilder::with_id("fury-tray")
                .icon(app.default_window_icon().expect("window icon").clone())
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    // Quit means quit: flag a force shutdown so the exit handler
                    // stops every open profile's browser too, then exit.
                    "quit" => {
                        app.state::<AppState>()
                            .force_quit
                            .store(true, Ordering::SeqCst);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // Left click restores the window — the usual tray convention.
                    // A right click ALSO arrives as `Click`, but it must do
                    // nothing here: `show_main` steals focus, and that focus
                    // change dismisses the context menu the platform opens for
                    // the right click, so the menu never stayed up. Filtering to
                    // the left button leaves the right click to open the menu.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;
            tray.set_tooltip(Some("Monica"))?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::shell_state,
            commands::download_core,
            commands::set_server,
            commands::invitation,
            commands::verify_code,
            commands::own_verify_code,
            commands::invitation_email,
            commands::invitation_email_verify,
            commands::server_info,
            commands::enrol,
            commands::signup,
            commands::server_allows_signup,
            commands::disconnect_server,
            commands::save_server_kit,
            commands::login,
            commands::login_totp,
            commands::totp_status,
            commands::totp_setup,
            commands::totp_confirm,
            commands::totp_verify,
            commands::totp_disable,
            commands::org_security,
            commands::set_org_security,
            commands::login_events,
            commands::sessions,
            commands::revoke_session,
            commands::revoke_member_sessions,
            commands::org_domain_lists,
            commands::access_roles,
            commands::save_access_role,
            commands::delete_access_role,
            commands::save_org_domain_list,
            commands::delete_org_domain_list,
            commands::logout,
            commands::me,
            commands::billing_status,
            commands::billing_checkout,
            commands::billing_portal,
            commands::projects,
            commands::profiles,
            commands::share_profile,
            commands::upload_profile,
            commands::profile_shares,
            commands::revoke_share,
            commands::shared_with_me,
            commands::move_profiles,
            update::check_update,
            update::install_update,
            update::update_progress,
            assistants::assistants_state,
            assistants::assistants_connect,
            assistants::assistants_disconnect,
            assistants::assistants_install_skill,
            update::open_url,
            commands::set_remember_org_key,
            commands::set_minimize_to_tray,
            commands::set_language,
            commands::audit,
            commands::org_members,
            commands::invite,
            commands::set_member_role,
            commands::hand_over_key,
            commands::grants,
            commands::grant_access,
            commands::revoke_access,
            commands::remove_member,
            commands::launch,
            commands::stop,
            commands::personas,
            commands::preview,
            commands::proxies,
            commands::credentials,
            commands::save_credential,
            commands::delete_credential,
            commands::totp_code,
            commands::save_proxy,
            commands::check_proxy,
            commands::diagnose_proxy,
            commands::rotate_proxy,
            commands::delete_proxy,
            commands::save_profile,
            commands::create_profiles,
            commands::templates,
            commands::save_template,
            commands::delete_template,
            commands::clone_profile,
            commands::import_proxies,
            commands::parse_proxy_line,
            commands::sniff_proxy_kind,
            commands::export_cookies,
            commands::extensions,
            commands::install_extension,
            commands::remove_extension,
            commands::copy_extension_with_state,
            commands::all_extensions,
            commands::install_extension_many,
            commands::extension_catalogue,
            commands::install_extension_from_store,
            commands::install_extension_from_store_team,
            commands::all_extensions_with,
            commands::profile_usage,
            commands::trim_profile,
            commands::reseed_profile,
            commands::capture_persona,
            commands::mirror_start,
            commands::mirror_stop,
            commands::mirror_status,
            commands::mirror_typing,
            commands::warm_start,
            commands::vault_status,
            commands::warm_status,
            commands::warm_stop,
            commands::warm_clear,
            commands::warm_defaults,
            commands::bookmarks_list,
            commands::bookmarks_save,
            commands::bookmarks_delete,
            commands::farm_list,
            commands::farm_create,
            commands::farm_delete,
            commands::farm_pause,
            commands::farm_due,
            commands::farm_claim,
            commands::farm_fail,
            commands::farm_stage_applied,
            commands::save_persona_file,
            commands::use_persona_here,
            commands::share_persona_with_team,
            commands::kept_personas,
            commands::blocklists,
            commands::read_blocklist,
            commands::save_blocklist,
            commands::delete_blocklist,
            commands::import_cookies,
            commands::vendor_list,
            commands::vendor_profile,
            commands::delete_profile,
            commands::export_project,
            commands::import_project,
            commands::trash,
            commands::restore_profile,
            commands::purge_profile,
            commands::rename_project,
            commands::delete_project,
            commands::create_project,
        ])
        .build(tauri::generate_context!())
        .expect("error while running Fury")
        .run(|app, event| {
            // The agent this shell started outlives it on purpose: open
            // profiles keep their locks and relays. With nothing open it had
            // no reason to, and on macOS a live child kept Fury in the Dock
            // as "running in background" after the person had quit
            // (lsappinfo: exited-with-subordinates; reported 10.10.2026).
            // So on the way out the agent is told to go once it is idle:
            // now if nothing is open, after the last profile otherwise.
            // An agent older than 0.3.3 does not know the parameter and
            // refuses while profiles are open, which is the old behaviour.
            //
            // On a thread of its own with a runtime of its own, and waited for
            // with a deadline the main thread keeps itself. block_on on the
            // shared runtime needs a worker to drive its socket and its timer,
            // and a worker stuck in a task means none may: the 0.4.17 update
            // hung here for good (see update.rs, request_restart). Whatever
            // happens to the call, the exit goes on after 1.5 s.
            match event {
                // Intercept the close so minimize-to-tray can hide the window
                // instead of letting it quit. The agent keeps running either
                // way; hiding leaves the shell reachable from the tray while
                // open profiles stay up. The setting is read here, on every
                // close, rather than once at startup, so flipping it in
                // Settings takes effect on the very next close.
                RunEvent::WindowEvent {
                    label,
                    event: WindowEvent::CloseRequested { api, .. },
                    ..
                } if label == "main" => {
                    let minimize = app.state::<AppState>().settings.lock().unwrap().minimize_to_tray;
                    if minimize {
                        api.prevent_close();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.hide();
                        }
                    }
                }
                RunEvent::Exit => {
                    // The tray menu's Quit sets force_quit: quitting means
                    // quitting, so every open profile's browser is stopped too,
                    // not just this window. Every other exit keeps the polite
                    // `when_idle` shutdown, which lets the agent outlive the
                    // window while profiles are open.
                    //
                    // The force call returns at once — the agent answers before
                    // it stops the profiles, then finishes on its own — so a
                    // short deadline is enough even with profiles open. The
                    // when_idle call keeps its 1.5 s bound.
                    let force = app.state::<AppState>().force_quit.load(Ordering::SeqCst);
                    let (params, deadline) = if force {
                        (serde_json::json!({ "force": true }), Duration::from_millis(5000))
                    } else {
                        (serde_json::json!({ "when_idle": true }), Duration::from_millis(1500))
                    };
                    let (done, wait) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                            rt.block_on(async {
                                let _ = tokio::time::timeout(
                                    deadline,
                                    agent::call::<serde_json::Value>("agent.shutdown", params),
                                )
                                .await;
                            });
                        }
                        let _ = done.send(());
                    });
                    let _ = wait.recv_timeout(deadline + Duration::from_millis(100));
                }
                _ => {}
            }
        });
}
