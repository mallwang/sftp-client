mod sftp;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(sftp::AppState::default())
        .invoke_handler(tauri::generate_handler![
            sftp::connect,
            sftp::disconnect,
            sftp::reconnect,
            sftp::check_connection,
            sftp::resolve_path,
            sftp::list_dir,
            sftp::download,
            sftp::upload,
            sftp::cancel_transfer,
            sftp::folder_size,
            sftp::local_exists,
            sftp::mkdir,
            sftp::remove,
            sftp::rename,
        ])
        .run(tauri::generate_context!())
        .expect("Fehler beim Starten der Tauri-Anwendung");
}
