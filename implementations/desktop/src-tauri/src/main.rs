// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{CustomMenuItem, SystemTray, SystemTrayMenu, SystemTrayMenuItem, SystemTrayEvent};
use tauri::Manager;
use std::process::Command;

#[tauri::command]
fn node_list() -> String {
  println!("listing...");
  let output = if cfg!(target_os = "windows") {
    println!("on windows");
    Command::new("ockam")
            .args(["node", "list"])
            .output()
            .expect("failed to execute process")
  } else {
    println!("not windows");
    Command::new("sh")
            .arg("-c")
            .arg("ockam node list")
            .output()
            .expect("failed to execute process")
  };
  let out = String::from_utf8(output.stdout).expect("Found invalid UTF-8");
  format!("{}", out)
}

fn main() {
  let quit = CustomMenuItem::new("quit".to_string(), "Quit");
  let hide = CustomMenuItem::new("hide".to_string(), "Hide");
  let tray_menu = SystemTrayMenu::new()
      .add_item(quit)
      .add_native_item(SystemTrayMenuItem::Separator)
      .add_item(hide);
  let system_tray = SystemTray::new()
      .with_menu(tray_menu);
      

  tauri::Builder::default()
      .system_tray(system_tray)
      .on_system_tray_event(|app, event| match event {
          SystemTrayEvent::LeftClick {
            position: _,
            size: _,
            ..
          } => {
            println!("system tray received a left click");
          }
          SystemTrayEvent::RightClick {
            position: _,
            size: _,
            ..
          } => {
            println!("system tray received a right click");
          }
          SystemTrayEvent::DoubleClick {
            position: _,
            size: _,
            ..
          } => {
            println!("system tray received a double click");
          }
          SystemTrayEvent::MenuItemClick { id, .. } => {
            match id.as_str() {
              "quit" => {
                std::process::exit(0);
              }
              "hide" => {
                let window = app.get_window("main").unwrap();
                window.hide().unwrap();
              }
              _ => {}
            }
          }
          _ => {}
        })
      .invoke_handler(tauri::generate_handler![node_list])
      .build(tauri::generate_context!())
      .expect("error while running tauri application")
      .run(|_app_handle, event| match event {
          tauri::RunEvent::ExitRequested { api, .. } => {
            api.prevent_exit();
          }
          _ => {}
        });
}