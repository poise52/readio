use desk_core::{Book, BookSummary, BookmarkEntry, Mark, Preferences, Store};
use readio_speech::{SpeechAudio, SpeechService, Voice};
use serde::Serialize;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tauri::{Manager, State};

struct AppState {
    store: Arc<Mutex<Store>>,
    speech: SpeechService,
}
type IpcResult<T> = Result<T, String>;
fn with_store<T>(
    state: &State<'_, AppState>,
    f: impl FnOnce(&Store) -> desk_core::Result<T>,
) -> IpcResult<T> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    f(&store).map_err(|e| e.to_string())
}
#[derive(Serialize)]
struct Bootstrap {
    books: Vec<BookSummary>,
    preferences: Preferences,
    voices: Vec<Voice>,
    warnings: Vec<String>,
}
#[tauri::command]
async fn bootstrap(state: State<'_, AppState>) -> IpcResult<Bootstrap> {
    let store = Arc::clone(&state.store);
    let (books, preferences) = tauri::async_runtime::spawn_blocking(move || {
        let store = store.lock().map_err(|e| e.to_string())?;
        Ok::<_, String>((
            store.books().map_err(|e| e.to_string())?,
            store.preferences().map_err(|e| e.to_string())?,
        ))
    })
    .await
    .map_err(|e| e.to_string())??;
    let (voices, warnings) = match state.speech.voices().await {
        Ok(voices) => (voices, Vec::new()),
        Err(e) => (Vec::new(), vec![e.to_string()]),
    };
    Ok(Bootstrap {
        books,
        preferences,
        voices,
        warnings,
    })
}
#[tauri::command]
async fn import_books(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> IpcResult<Vec<BookSummary>> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || {
        let books = paths
            .iter()
            .map(|p| desk_core::import_book(Path::new(p)))
            .collect::<desk_core::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        store
            .lock()
            .map_err(|e| e.to_string())?
            .import_parsed(&books)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn open_book(state: State<'_, AppState>, id: String) -> IpcResult<Book> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || {
        store
            .lock()
            .map_err(|e| e.to_string())?
            .book(&id)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn delete_book(state: State<'_, AppState>, id: String) -> IpcResult<()> {
    with_store(&state, |s| s.delete(&id))
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn save_position(
    state: State<'_, AppState>,
    id: String,
    chapter: usize,
    offset: usize,
    speed: f32,
) -> IpcResult<BookSummary> {
    with_store(&state, |s| s.position(&id, chapter, offset, speed))
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn add_bookmark(
    state: State<'_, AppState>,
    id: String,
    chapter: usize,
    offset: usize,
) -> IpcResult<Mark> {
    with_store(&state, |s| s.add_mark(&id, chapter, offset))
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn list_bookmarks(state: State<'_, AppState>) -> IpcResult<Vec<BookmarkEntry>> {
    with_store(&state, Store::bookmarks)
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn delete_bookmark(state: State<'_, AppState>, id: String, mark_id: String) -> IpcResult<()> {
    with_store(&state, |s| s.delete_mark(&id, &mark_id))
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn save_preferences(
    state: State<'_, AppState>,
    preferences: Preferences,
) -> IpcResult<Preferences> {
    with_store(&state, |s| s.save_preferences(&preferences))
}
#[tauri::command]
async fn import_legacy(state: State<'_, AppState>, path: String) -> IpcResult<usize> {
    let store = Arc::clone(&state.store);
    tauri::async_runtime::spawn_blocking(move || {
        store
            .lock()
            .map_err(|e| e.to_string())?
            .import_legacy(Path::new(&path))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn set_background(state: State<'_, AppState>, path: Option<String>) -> IpcResult<Preferences> {
    with_store(&state, |s| s.background(path.as_deref()))
}
#[tauri::command]
async fn synthesize(
    state: State<'_, AppState>,
    text: String,
    voice: Option<String>,
    speed: f32,
) -> IpcResult<SpeechAudio> {
    state
        .speech
        .synthesize(&text, voice.as_deref(), speed)
        .await
        .map_err(|e| e.to_string())
}
#[tauri::command]
async fn stop_speech(state: State<'_, AppState>) -> IpcResult<()> {
    state.speech.stop().await;
    Ok(())
}
/// Starts the desktop application.
///
/// # Errors
/// Returns an error when Tauri cannot initialize or run the application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> tauri::Result<()> {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let root = app.path().app_data_dir()?;
            let store = Store::open(&root)?;
            let speech = SpeechService::new(root)?;
            app.manage(AppState {
                store: Arc::new(Mutex::new(store)),
                speech,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            import_books,
            open_book,
            delete_book,
            save_position,
            add_bookmark,
            delete_bookmark,
            list_bookmarks,
            save_preferences,
            import_legacy,
            set_background,
            synthesize,
            stop_speech
        ])
        .run(tauri::generate_context!())
}

#[cfg(all(test, not(dev)))]
mod tests {
    #[test]
    fn embeds_index_html() {
        let context: tauri::Context<tauri::Wry> = tauri::generate_context!();
        assert!(context.assets().get(&"/index.html".into()).is_some());
    }
}
