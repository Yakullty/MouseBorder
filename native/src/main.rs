#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Manager;

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

fn show_main(app:&tauri::AppHandle){
    if let Some(w)=app.get_webview_window("main"){let _=w.show();let _=w.unminimize();let _=w.set_focus();}
}
/// The only real exit. `AppHandle::exit` never returns to `main`, so the session-global cursor
/// association has to be restored here too, not just after the event loop ends.
fn quit(app:&tauri::AppHandle,core:&std::sync::Arc<network::Core>){
    core.disconnect();
    #[cfg(target_os="macos")]
    tap::emergency_restore();
    app.exit(0);
}
fn install_tray(app:&tauri::AppHandle,core:std::sync::Arc<network::Core>)->tauri::Result<()>{
    use tauri::{menu::{Menu,MenuItem},tray::{MouseButton,MouseButtonState,TrayIconBuilder,TrayIconEvent}};
    let menu=Menu::with_items(app,&[
        &MenuItem::with_id(app,"show","显示主界面",true,None::<&str>)?,
        &MenuItem::with_id(app,"quit","退出 MouseBorder",true,None::<&str>)?,
    ])?;
    TrayIconBuilder::with_id("mouseborder")
        .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?)
        .icon_as_template(false)
        .tooltip("MouseBorder")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move|app,item|match item.id().as_ref(){
            "show"=>show_main(app),
            "quit"=>quit(app,&core),
            _=>{}
        })
        .on_tray_icon_event(|tray,event|{
            if let TrayIconEvent::Click{button:MouseButton::Left,button_state:MouseButtonState::Up,..}=event{show_main(tray.app_handle());}
        })
        .build(app)?;
    Ok(())
}
fn main() {
 let core=network::Core::new(); let startup=core.clone();
 let app=tauri::Builder::default().manage(core).setup(move|app|{
        startup.set_app(app.handle().clone());
        startup.start();
        install_tray(app.handle(),startup.clone())?;
        // Closing the window parks the app in the tray; the hook and the link keep running.
        if let Some(w)=app.get_webview_window("main"){
            let window=w.clone();
            w.on_window_event(move|event|if let tauri::WindowEvent::CloseRequested{api,..}=event{api.prevent_close();let _=window.hide();});
        }
        Ok(())
    })
  .invoke_handler(tauri::generate_handler![get_state,refresh_pin,connect_peer,set_edge,set_enabled,set_clipboard,disconnect,retry_hook])
  .build(tauri::generate_context!())
  .expect("MouseBorder failed to start");
 app.run(|handle,event|match event{
     // A hidden window is not a closed one, but the runtime still asks to exit when the last
     // window goes away. Only the tray's 退出 (which carries an exit code) may end the process.
     tauri::RunEvent::ExitRequested{code,api,..}=>{if code.is_none(){api.prevent_exit();}}
     #[cfg(target_os="macos")]
     tauri::RunEvent::Reopen{has_visible_windows,..}=>{if !has_visible_windows{show_main(handle);}}
     _=>{}
 });
 // The cursor association is session-global: if the app quits while it is handed off, leaving it
 // disassociated would freeze the pointer for the whole login session.
 #[cfg(target_os="macos")]
 tap::emergency_restore();
}

#[cfg(test)]
mod tests {
    /// The tray is built during setup, so an icon that cannot decode would stop the app from
    /// starting at all rather than just leaving the tray empty.
    #[test]
    fn the_tray_icon_decodes_to_rgba() {
        let icon=tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png")).expect("tray icon must decode");
        assert_eq!((icon.width(),icon.height()),(32,32));
        assert_eq!(icon.rgba().len(),32*32*4);
    }
}
