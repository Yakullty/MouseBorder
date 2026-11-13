#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;
mod network;
mod protocol;
#[cfg(target_os="macos")]
mod tap;

#[tauri::command]
fn get_state(state: tauri::State<'_, std::sync::Arc<network::Core>>) -> network::UiState { state.ui.lock().unwrap().clone() }
#[tauri::command]
fn refresh_pin(state: tauri::State<'_, std::sync::Arc<network::Core>>) -> String { state.refresh_pin() }
#[tauri::command]
fn connect_peer(state: tauri::State<'_, std::sync::Arc<network::Core>>, host:String, pin:String) { state.connect(host,pin); }
#[tauri::command]
fn disconnect(state: tauri::State<'_, std::sync::Arc<network::Core>>) { state.disconnect(); }
#[tauri::command]
fn set_enabled(state: tauri::State<'_, std::sync::Arc<network::Core>>, enabled:bool) { state.set_edge_enabled(enabled); }
#[tauri::command]
fn set_clipboard(state: tauri::State<'_, std::sync::Arc<network::Core>>, enabled:bool) { state.set_clipboard_enabled(enabled); }
#[tauri::command]
fn set_edge(state: tauri::State<'_, std::sync::Arc<network::Core>>, side:String) { state.set_edge(side); }
#[tauri::command]
fn retry_hook(state: tauri::State<'_, std::sync::Arc<network::Core>>) { state.retry_hook(); }
fn main() {
 let core=network::Core::new(); let startup=core.clone();
 let result=tauri::Builder::default().manage(core).setup(move|app|{startup.set_app(app.handle().clone());startup.start();Ok(())})
  .invoke_handler(tauri::generate_handler![get_state,refresh_pin,connect_peer,set_edge,set_enabled,set_clipboard,disconnect,retry_hook])
  .run(tauri::generate_context!());
 // The cursor association is session-global: if the app quits while it is handed off, leaving it
 // disassociated would freeze the pointer for the whole login session.
 #[cfg(target_os="macos")]
 tap::emergency_restore();
 result.expect("MouseBorder failed to start");
}
