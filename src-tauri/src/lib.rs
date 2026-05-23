pub mod desktop;
pub mod locales;
pub mod services;

use locales::Locale;
use rodio::{Decoder, OutputStream, Sink};
use services::notes::{default_store, AppConfig, AppError, Note, NoteMetadata, SaveNoteRequest};
use std::{
    fs,
    io::{BufReader, Cursor},
    path::PathBuf,
    sync::{
        mpsc::{channel, Sender},
        OnceLock,
    },
};
use tauri::{AppHandle, Emitter, Manager};

#[tauri::command]
fn app_name() -> Result<String, AppError> {
    let locale = Locale::from_tag(&default_store()?.load_config()?.locale);
    Ok(locales::app_name(locale).to_string())
}

struct ReminderAudio {
    _stream: OutputStream,
    sink: Sink,
}

enum ReminderAudioCommand {
    Play(Option<String>, Sender<Result<(), String>>),
    Stop(Sender<Result<(), String>>),
}

static REMINDER_AUDIO: OnceLock<Sender<ReminderAudioCommand>> = OnceLock::new();

fn reminder_audio_sender() -> &'static Sender<ReminderAudioCommand> {
    REMINDER_AUDIO.get_or_init(|| {
        let (tx, rx) = channel::<ReminderAudioCommand>();
        std::thread::spawn(move || {
            let mut current: Option<ReminderAudio> = None;
            while let Ok(command) = rx.recv() {
                match command {
                    ReminderAudioCommand::Play(path, reply) => {
                        if let Some(audio) = current.take() {
                            audio.sink.stop();
                        }
                        let result = start_reminder_audio(path).map(|audio| {
                            current = Some(audio);
                        });
                        let _ = reply.send(result);
                    }
                    ReminderAudioCommand::Stop(reply) => {
                        if let Some(audio) = current.take() {
                            audio.sink.stop();
                        }
                        let _ = reply.send(Ok(()));
                    }
                }
            }
        });
        tx
    })
}

fn start_reminder_audio(path: Option<String>) -> Result<ReminderAudio, String> {
    let (stream, handle) = OutputStream::try_default().map_err(|error| error.to_string())?;
    let sink = Sink::try_new(&handle).map_err(|error| error.to_string())?;

    if let Some(path) = path.filter(|value| !value.trim().is_empty()) {
        let file = std::fs::File::open(&path)
            .map_err(|error| format!("failed to open ringtone {path}: {error}"))?;
        let source = Decoder::new_looped(BufReader::new(file))
            .map_err(|error| format!("failed to decode ringtone {path}: {error}"))?;
        sink.append(source);
    } else {
        let cursor = Cursor::new(default_reminder_wav());
        let source = Decoder::new_looped(cursor)
            .map_err(|error| format!("failed to decode default reminder sound: {error}"))?;
        sink.append(source);
    }

    sink.play();
    Ok(ReminderAudio {
        _stream: stream,
        sink,
    })
}

fn default_reminder_wav() -> Vec<u8> {
    const SAMPLE_RATE: u32 = 44_100;
    const DURATION_MS: u32 = 220;
    const SAMPLES: u32 = SAMPLE_RATE * DURATION_MS / 1000;
    let mut data = Vec::with_capacity(44 + SAMPLES as usize * 2);
    let data_size = SAMPLES * 2;
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + data_size).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    data.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&data_size.to_le_bytes());

    for i in 0..SAMPLES {
        let t = i as f32 / SAMPLE_RATE as f32;
        let envelope = 1.0 - i as f32 / SAMPLES as f32;
        let sample = (t * 880.0 * std::f32::consts::TAU).sin() * envelope * 0.28;
        let sample_i16 = (sample * i16::MAX as f32)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        data.extend_from_slice(&sample_i16.to_le_bytes());
    }

    data
}

#[tauri::command]
fn stop_reminder_sound() -> Result<(), AppError> {
    let (reply_tx, reply_rx) = channel();
    reminder_audio_sender()
        .send(ReminderAudioCommand::Stop(reply_tx))
        .map_err(|error| AppError {
            code: "audioCommand".into(),
            message: error.to_string(),
            details: Default::default(),
        })?;
    reply_rx
        .recv()
        .map_err(|error| AppError {
            code: "audioCommand".into(),
            message: error.to_string(),
            details: Default::default(),
        })?
        .map_err(|message| AppError {
            code: "audioStop".into(),
            message,
            details: Default::default(),
        })
}

#[tauri::command]
fn play_reminder_sound(path: Option<String>) -> Result<(), AppError> {
    let (reply_tx, reply_rx) = channel();
    reminder_audio_sender()
        .send(ReminderAudioCommand::Play(path, reply_tx))
        .map_err(|error| AppError {
            code: "audioCommand".into(),
            message: error.to_string(),
            details: Default::default(),
        })?;
    reply_rx
        .recv()
        .map_err(|error| AppError {
            code: "audioCommand".into(),
            message: error.to_string(),
            details: Default::default(),
        })?
        .map_err(|message| AppError {
            code: "audioPlay".into(),
            message,
            details: Default::default(),
        })
}

#[tauri::command]
fn notes_list() -> Result<Vec<NoteMetadata>, AppError> {
    default_store()?.list_notes()
}

#[tauri::command]
fn notes_get(id: String) -> Result<Note, AppError> {
    default_store()?.read_note(&id)
}

#[tauri::command]
fn notes_create(app: AppHandle, request: SaveNoteRequest) -> Result<Note, AppError> {
    let note = default_store()?.create_note(request)?;
    let _ = app.emit("notes-changed", ());
    Ok(note)
}

#[tauri::command]
fn notes_update(app: AppHandle, id: String, request: SaveNoteRequest) -> Result<Note, AppError> {
    let note = default_store()?.update_note(&id, request)?;
    let _ = app.emit("notes-changed", ());
    Ok(note)
}

#[tauri::command]
fn notes_delete(app: AppHandle, id: String) -> Result<(), AppError> {
    default_store()?.delete_note(&id)?;
    let _ = app.emit("notes-changed", ());
    Ok(())
}

#[tauri::command]
fn notes_import_markdown(
    app: AppHandle,
    path: String,
    category: Option<String>,
) -> Result<Note, AppError> {
    let note = default_store()?
        .import_markdown_file(&PathBuf::from(path), &category.unwrap_or_default())?;
    let _ = app.emit("notes-changed", ());
    Ok(note)
}

#[tauri::command]
fn notes_export_markdown(id: String, path: String) -> Result<(), AppError> {
    default_store()?.export_markdown_file(&id, &PathBuf::from(path))
}

#[tauri::command]
fn read_external_file(path: String) -> Result<String, AppError> {
    std::fs::read_to_string(&path).map_err(|e| AppError {
        code: "io".into(),
        message: e.to_string(),
        details: Default::default(),
    })
}

#[tauri::command]
fn get_file_modified_time(path: String) -> Result<f64, AppError> {
    let metadata = std::fs::metadata(&path).map_err(|e| AppError {
        code: "io".into(),
        message: e.to_string(),
        details: Default::default(),
    })?;
    let modified = metadata.modified().map_err(|e| AppError {
        code: "io".into(),
        message: e.to_string(),
        details: Default::default(),
    })?;
    let duration = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(duration.as_secs_f64() * 1000.0)
}

#[tauri::command]
fn save_external_file(path: String, content: String) -> Result<(), AppError> {
    if let Some(parent) = PathBuf::from(&path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| AppError {
            code: "io".into(),
            message: e.to_string(),
            details: Default::default(),
        })?;
    }
    std::fs::write(&path, content).map_err(|e| AppError {
        code: "io".into(),
        message: e.to_string(),
        details: Default::default(),
    })
}

#[tauri::command]
fn categories_list() -> Result<Vec<String>, AppError> {
    default_store()?.list_categories()
}

#[tauri::command]
fn categories_create(app: AppHandle, name: String) -> Result<(), AppError> {
    default_store()?.create_category(&name)?;
    let _ = app.emit("notes-changed", ());
    Ok(())
}

#[tauri::command]
fn categories_rename(app: AppHandle, old_name: String, new_name: String) -> Result<(), AppError> {
    default_store()?.rename_category(&old_name, &new_name)?;
    let _ = app.emit("notes-changed", ());
    Ok(())
}

#[tauri::command]
fn categories_delete(app: AppHandle, name: String) -> Result<(), AppError> {
    default_store()?.delete_category(&name)?;
    let _ = app.emit("notes-changed", ());
    Ok(())
}

#[tauri::command]
fn notes_move_category(
    app: AppHandle,
    id: String,
    category: String,
) -> Result<NoteMetadata, AppError> {
    let result = default_store()?.move_note_to_category(&id, &category)?;
    let _ = app.emit("notes-changed", ());
    Ok(result)
}

#[tauri::command]
fn config_get() -> Result<AppConfig, AppError> {
    default_store()?.load_config()
}

#[tauri::command]
fn copy_background_image(app: AppHandle, source_path: String) -> Result<String, AppError> {
    let source = PathBuf::from(source_path.trim());
    if !source.is_file() {
        return Err(AppError {
            code: "invalidSource".into(),
            message: "background image source not found".into(),
            details: Default::default(),
        });
    }

    let app_data = app.path().app_data_dir().map_err(|error| AppError {
        code: "path".into(),
        message: error.to_string(),
        details: Default::default(),
    })?;
    let dir = app_data.join("backgrounds");
    fs::create_dir_all(&dir)?;

    let ext = source
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("png");
    let dest = dir.join(format!("bg-{}.{}", uuid::Uuid::new_v4(), ext));
    fs::copy(&source, &dest)?;

    dest.to_str().map(str::to_string).ok_or_else(|| AppError {
        code: "path".into(),
        message: "invalid destination path".into(),
        details: Default::default(),
    })
}

#[tauri::command]
fn config_save(app: AppHandle, config: AppConfig) -> Result<AppConfig, AppError> {
    let store = default_store()?;
    let previous = store.load_config()?;
    desktop::apply_runtime_config(&app, &previous, &config).map_err(|error| {
        match error.downcast::<AppError>() {
            Ok(app_error) => *app_error,
            Err(error) => AppError {
                code: "desktopConfig".into(),
                message: error.to_string(),
                details: Default::default(),
            },
        }
    })?;
    let saved = store.save_config(config)?;
    if let Err(error) = desktop::refresh_shell_state(&app, &saved) {
        eprintln!("failed to refresh desktop shell state: {error}");
    }
    let _ = app.emit("config-changed", &saved);
    Ok(saved)
}

#[tauri::command]
fn global_shortcut_check(
    app: AppHandle,
    shortcut: String,
) -> Result<desktop::ShortcutCheckResult, AppError> {
    desktop::check_global_shortcut(&app, &shortcut)
}

#[tauri::command]
async fn open_notepad_window(
    app: AppHandle,
    note_id: Option<String>,
    bounds: Option<desktop::WindowBounds>,
) -> Result<String, AppError> {
    desktop::open_notepad_window(app, note_id, bounds).await
}

#[tauri::command]
async fn recycle_notepad_window(app: AppHandle, label: String) -> Result<(), AppError> {
    desktop::recycle_notepad_window(&app, &label)
}

#[tauri::command]
async fn open_tile_window(
    app: AppHandle,
    note_id: String,
    bounds: Option<desktop::WindowBounds>,
) -> Result<String, AppError> {
    desktop::open_tile_window(app, note_id, bounds).await
}

#[tauri::command]
async fn toggle_tile_window(
    app: AppHandle,
    note_id: String,
    bounds: Option<desktop::WindowBounds>,
) -> Result<bool, AppError> {
    desktop::toggle_tile_window(app, note_id, bounds).await
}

#[tauri::command]
async fn open_note_in_editor(app: AppHandle, note_id: String) -> Result<(), AppError> {
    desktop::show_main_window(&app)?;
    let _ = app.emit("open-note", &note_id);
    Ok(())
}

#[tauri::command]
async fn open_main_window(app: AppHandle) -> Result<(), AppError> {
    desktop::show_main_window(&app)
}

#[tauri::command]
async fn open_reminder_alarm_window(
    app: AppHandle,
    reminder_id: String,
) -> Result<String, AppError> {
    desktop::open_reminder_alarm_window(app, reminder_id).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if let Some(file_path) = desktop::extract_file_arg(&args) {
                let _ = app.emit("open-external-file", file_path);
            }
            let _ = desktop::show_main_window(app);
        }))
        .setup(|app| {
            desktop::setup_desktop(app)?;
            Ok(())
        })
        .on_window_event(desktop::handle_window_event)
        .invoke_handler(tauri::generate_handler![
            app_name,
            play_reminder_sound,
            stop_reminder_sound,
            notes_list,
            notes_get,
            notes_create,
            notes_update,
            notes_delete,
            notes_import_markdown,
            notes_export_markdown,
            notes_move_category,
            read_external_file,
            save_external_file,
            get_file_modified_time,
            categories_list,
            categories_create,
            categories_rename,
            categories_delete,
            config_get,
            copy_background_image,
            config_save,
            global_shortcut_check,
            open_notepad_window,
            recycle_notepad_window,
            open_tile_window,
            toggle_tile_window,
            open_note_in_editor,
            open_main_window,
            open_reminder_alarm_window
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
