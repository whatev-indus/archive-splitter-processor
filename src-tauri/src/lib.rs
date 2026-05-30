mod commands;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::scan_folder,
            commands::detect_layout,
            commands::preview_split,
            commands::preview_rename,
            commands::do_rename,
            commands::bin_split,
            commands::verify_tracks,
            commands::create_zip,
            commands::upload_to_archive,
            commands::derive_identifier,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
