mod audio_resource;
mod block_error;
mod credentials;
mod instance;
mod ipc;
mod lifecycle;
mod offline;
mod plan_cache;
mod pricing;
mod request_queue;
mod tasks;
#[cfg(test)]
mod test_provider;

use desk_core::{BookSummary, BookmarkEntry, Mark, Preferences, ReaderBook, Store};
use readio_speech::{OpenRouterModel, SpeechAudio, SpeechService, Voice};
use serde::Serialize;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager, State};

struct AppState {
    _instance: instance::InstanceGuard,
    lifecycle: lifecycle::Lifecycle,
    store: Arc<Mutex<Store>>,
    speech: SpeechService,
    offline: Arc<offline::OfflineManager>,
    openrouter_catalog: tokio::sync::Mutex<Option<CachedCatalog>>,
    audio_resources: Arc<audio_resource::AudioResources>,
}
struct CachedCatalog {
    fetched_at: Instant,
    models: Vec<OpenRouterModel>,
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn audio_resource_release(state: State<'_, AppState>, token: String) {
    state.audio_resources.release(&token);
}
const CATALOG_TTL: Duration = Duration::from_mins(10);
const LEGACY_FISH_VOICE: &str = "openrouter:fish-audio/s2.1-pro-free:free";
fn migrate_provider(preferences: &mut Preferences) -> bool {
    if preferences.voice.as_deref() != Some(LEGACY_FISH_VOICE) {
        return false;
    }
    preferences.voice = Some("openrouter".into());
    if preferences.open_router_model.is_none() {
        preferences.open_router_model = Some("fish-audio/s2.1-pro-free:free".into());
    }
    true
}
async fn catalog(
    state: &AppState,
    refresh: bool,
    require_key: bool,
) -> IpcResult<(Vec<OpenRouterModel>, Option<zeroize::Zeroizing<String>>)> {
    let mut cache = state.openrouter_catalog.lock().await;
    if refresh {
        *cache = None;
    }
    let api_key = tauri::async_runtime::spawn_blocking(credentials::read)
        .await
        .map_err(|e| e.to_string())??;
    if require_key && api_key.is_none() {
        return Err("Добавьте ключ OpenRouter в настройках".into());
    }
    if !refresh
        && let Some(entry) = cache.as_ref()
        && entry.fetched_at.elapsed() < CATALOG_TTL
    {
        return Ok((entry.models.clone(), api_key));
    }
    let models = state
        .speech
        .openrouter_models(api_key.as_ref().map(|key| key.as_str()))
        .await
        .map_err(|e| e.to_string())?;
    *cache = Some(CachedCatalog {
        fetched_at: Instant::now(),
        models: models.clone(),
    });
    drop(cache);
    Ok((models, api_key))
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
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    books: Vec<BookSummary>,
    preferences: Preferences,
    voices: Vec<Voice>,
    warnings: Vec<String>,
    open_router: OpenRouterStatus,
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenRouterStatus {
    has_api_key: bool,
}

#[tauri::command]
async fn bootstrap(state: State<'_, AppState>) -> Result<Bootstrap, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        let store = Arc::clone(&state.store);
        let (books, preferences) = tauri::async_runtime::spawn_blocking(move || {
            let store = store.lock().map_err(|e| e.to_string())?;
            let mut preferences = store.preferences().map_err(|e| e.to_string())?;
            if migrate_provider(&mut preferences) {
                store
                    .save_preferences(&preferences)
                    .map_err(|e| e.to_string())?;
            }
            Ok::<_, String>((store.books().map_err(|e| e.to_string())?, preferences))
        })
        .await
        .map_err(|e| e.to_string())??;
        let (voices, mut warnings) = match state.speech.voices().await {
            Ok(voices) => (voices, Vec::new()),
            Err(e) => (Vec::new(), vec![e.to_string()]),
        };
        let open_router = match openrouter_status().await {
            Ok(status) => status,
            Err(error) => {
                warnings.push(error.to_string());
                OpenRouterStatus { has_api_key: false }
            }
        };
        Ok(Bootstrap {
            books,
            preferences,
            voices,
            warnings,
            open_router,
        })
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
async fn import_books(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<BookSummary>, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        let store = Arc::clone(&state.store);
        let offline = Arc::clone(&state.offline);
        tauri::async_runtime::spawn_blocking(move || {
            let store = store.lock().map_err(|e| e.to_string())?;
            let result = store.import(&paths).map_err(|e| e.to_string());
            offline.invalidate_plans();
            drop(store);
            result
        })
        .await
        .map_err(|e| e.to_string())?
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
async fn open_book(
    state: State<'_, AppState>,
    id: String,
) -> Result<ReaderBook, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        let store = Arc::clone(&state.store);
        tauri::async_runtime::spawn_blocking(move || {
            store
                .lock()
                .map_err(|e| e.to_string())?
                .reader_book(&id)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
async fn delete_book(state: State<'_, AppState>, id: String) -> Result<(), crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        state.offline.remove_all(&id).await?;
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let result = store.delete(&id).map_err(|e| e.to_string());
        state.offline.invalidate_plans();
        drop(store);
        result
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn save_position(
    state: State<'_, AppState>,
    id: String,
    chapter: usize,
    offset: usize,
    speed: f32,
    audio_position: Option<desk_core::AudioPosition>,
) -> Result<BookSummary, crate::ipc::IpcError> {
    let result: IpcResult<_> = with_store(&state, |s| {
        s.position_with_audio(&id, chapter, offset, speed, audio_position.as_ref())
    });
    result.map_err(Into::into)
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn add_bookmark(
    state: State<'_, AppState>,
    id: String,
    chapter: usize,
    offset: usize,
) -> Result<Mark, crate::ipc::IpcError> {
    let result: IpcResult<_> = with_store(&state, |s| s.add_mark(&id, chapter, offset));
    result.map_err(Into::into)
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn list_bookmarks(state: State<'_, AppState>) -> Result<Vec<BookmarkEntry>, crate::ipc::IpcError> {
    let result: IpcResult<_> = with_store(&state, Store::bookmarks);
    result.map_err(Into::into)
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn delete_bookmark(
    state: State<'_, AppState>,
    id: String,
    mark_id: String,
) -> Result<(), crate::ipc::IpcError> {
    let result: IpcResult<_> = with_store(&state, |s| s.delete_mark(&id, &mark_id));
    result.map_err(Into::into)
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn save_preferences(
    state: State<'_, AppState>,
    preferences: Preferences,
) -> Result<Preferences, crate::ipc::IpcError> {
    let result: IpcResult<_> = with_store(&state, |store| {
        let previous = store.preferences()?;
        let mut preferences = preferences;
        preferences.open_router_billing_units = previous.open_router_billing_units.clone();
        migrate_provider(&mut preferences);
        if previous.open_router_model != preferences.open_router_model {
            preferences.open_router_voice = None;
        }
        store.save_preferences(&preferences)
    });
    result.map_err(Into::into)
}
#[tauri::command]
async fn import_legacy(
    state: State<'_, AppState>,
    path: String,
) -> Result<usize, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        let store = Arc::clone(&state.store);
        let offline = Arc::clone(&state.offline);
        tauri::async_runtime::spawn_blocking(move || {
            let store = store.lock().map_err(|e| e.to_string())?;
            let result = store
                .import_legacy(Path::new(&path))
                .map_err(|e| e.to_string());
            offline.invalidate_plans();
            drop(store);
            result
        })
        .await
        .map_err(|e| e.to_string())?
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned command arguments.
fn set_background(
    state: State<'_, AppState>,
    path: Option<String>,
) -> Result<Preferences, crate::ipc::IpcError> {
    let result: IpcResult<_> = with_store(&state, |s| s.background(path.as_deref()));
    result.map_err(Into::into)
}
#[tauri::command]
async fn list_openrouter_models(
    state: State<'_, AppState>,
    refresh: Option<bool>,
) -> Result<Vec<OpenRouterModel>, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        catalog(&state, refresh.unwrap_or(false), false)
            .await
            .map(|(models, _)| models)
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
async fn openrouter_status() -> Result<OpenRouterStatus, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        tauri::async_runtime::spawn_blocking(|| {
            credentials::read().map(|key| OpenRouterStatus {
                has_api_key: key.is_some(),
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
async fn set_openrouter_key(
    state: State<'_, AppState>,
    api_key: String,
) -> Result<OpenRouterStatus, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        let api_key = zeroize::Zeroizing::new(api_key);
        credentials::validate_key(&api_key).map_err(str::to_owned)?;
        let mut cache = state.openrouter_catalog.lock().await;
        state.speech.stop().await;
        state.offline.change_credentials(Some(api_key)).await?;
        *cache = None;
        drop(cache);
        Ok(OpenRouterStatus { has_api_key: true })
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
async fn delete_openrouter_key(
    state: State<'_, AppState>,
) -> Result<OpenRouterStatus, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        let mut cache = state.openrouter_catalog.lock().await;
        state.speech.stop().await;
        state.offline.change_credentials(None).await?;
        *cache = None;
        drop(cache);
        Ok(OpenRouterStatus { has_api_key: false })
    }
    .await;
    result.map_err(Into::into)
}
async fn synthesize_openrouter(
    state: &State<'_, AppState>,
    text: &str,
    speed: f32,
) -> IpcResult<SpeechAudio> {
    let generation = state.speech.cancellation_generation();
    let preferences = with_store(state, Store::preferences)?;
    let model = preferences
        .open_router_model
        .as_deref()
        .ok_or_else(|| "Выберите TTS-модель OpenRouter".to_owned())?;
    let (models, key) = catalog(state, false, true).await?;
    if !models.iter().any(|candidate| candidate.id == model) {
        return Err("Выбранная TTS-модель OpenRouter недоступна; обновите список моделей".into());
    }
    let key = key.ok_or_else(|| "Добавьте ключ OpenRouter в настройках".to_owned())?;
    state
        .speech
        .synthesize_with_openrouter_transport_if_current(
            text,
            &key,
            model,
            preferences.open_router_voice.as_deref(),
            speed,
            generation,
            transport_for(preferences.open_router_model_settings.get(model)),
        )
        .await
        .map_err(|e| e.to_string())
}

fn transport_for(settings: Option<&desk_core::SynthesisSettings>) -> readio_speech::AudioTransport {
    settings
        .and_then(|settings| settings.transport.as_ref())
        .map_or_else(readio_speech::AudioTransport::default, |transport| {
            readio_speech::AudioTransport {
                format: match transport.format {
                    desk_core::AudioFormat::Mp3 => readio_speech::AudioFormat::Mp3,
                    desk_core::AudioFormat::Pcm => readio_speech::AudioFormat::Pcm,
                },
                pcm_sample_rate: transport.pcm_sample_rate,
                pcm_channels: transport.pcm_channels,
            }
        })
}
#[tauri::command]
async fn synthesize(
    state: State<'_, AppState>,
    text: String,
    voice: Option<String>,
    speed: f32,
) -> Result<SpeechAudio, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        if matches!(voice.as_deref(), Some("openrouter" | LEGACY_FISH_VOICE)) {
            return synthesize_openrouter(&state, &text, speed).await;
        }
        state
            .speech
            .synthesize(&text, voice.as_deref(), speed)
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
async fn synthesize_many(
    state: State<'_, AppState>,
    texts: Vec<String>,
    voice: Option<String>,
    speed: f32,
) -> Result<SpeechAudio, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        if matches!(voice.as_deref(), Some("openrouter" | LEGACY_FISH_VOICE)) {
            let joined = readio_speech::validate_and_join_texts(
                &texts.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .map_err(|e| e.to_string())?;
            return synthesize_openrouter(&state, &joined, speed).await;
        }
        state
            .speech
            .synthesize_many(
                &texts.iter().map(String::as_str).collect::<Vec<_>>(),
                voice.as_deref(),
                speed,
            )
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
async fn stop_speech(state: State<'_, AppState>) -> Result<(), crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        state.speech.stop().await;
        Ok(())
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
async fn prepare_shutdown(state: State<'_, AppState>) -> Result<(), crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        state.lifecycle.frontend_ready();
        state.offline.begin_shutdown();
        state.offline.shutdown().await;
        state.speech.stop().await;
        state.lifecycle.mark_drained();
        Ok(())
    }
    .await;
    result.map_err(Into::into)
}

/// Starts the desktop application.
///
/// # Errors
/// Returns an error when Tauri cannot initialize or run the application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> tauri::Result<()> {
    let audio_resources = Arc::new(audio_resource::AudioResources::default());
    let protocol_resources = Arc::clone(&audio_resources);
    tauri::Builder::default()
        .register_uri_scheme_protocol("readio-audio", move |_context, request| {
            protocol_resources.respond(&request)
        })
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            let root = app.path().app_data_dir()?;
            let instance = instance::InstanceGuard::acquire(&root)?;
            let store = Store::open(&root)?;
            let speech = SpeechService::new(root.clone())?;
            let store = Arc::new(Mutex::new(store));
            let offline = offline::OfflineManager::new(Arc::clone(&store), root)
                .map_err(std::io::Error::other)?;
            offline.set_event_handle(app.handle().clone());
            app.manage(AppState {
                _instance: instance,
                lifecycle: lifecycle::Lifecycle::new(),
                store,
                speech,
                offline,
                openrouter_catalog: tokio::sync::Mutex::new(None),
                audio_resources: Arc::clone(&audio_resources),
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
            list_openrouter_models,
            openrouter_status,
            set_openrouter_key,
            delete_openrouter_key,
            synthesize,
            synthesize_many,
            stop_speech,
            prepare_shutdown,
            offline::audio_session,
            offline::audio_interest,
            offline::audio_plan,
            offline::audio_block,
            audio_resource_release,
            offline::offline_quote,
            offline::reconcile_generation_cost,
            offline::reconcile_book_cost,
            offline::set_openrouter_billing_unit,
            offline::offline_start,
            offline::offline_status,
            offline::offline_list,
            offline::offline_diagnostics,
            offline::offline_restore_manifest,
            offline::offline_pause,
            offline::offline_remove
        ])
        .build(tauri::generate_context!())?
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                let state = app.state::<AppState>();
                if state.lifecycle.is_drained() {
                    return;
                }
                api.prevent_exit();
                state.offline.begin_shutdown();
                if !state.lifecycle.begin_exit() {
                    return;
                }
                let _ = app.emit("shutdown-requested", ());
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    state.offline.shutdown().await;
                    state.speech.stop().await;
                    let _ = tokio::time::timeout(
                        Duration::from_secs(3),
                        state.lifecycle.wait_frontend(),
                    )
                    .await;
                    state.lifecycle.mark_drained();
                    app.exit(code.unwrap_or(0));
                });
            }
        });
    Ok(())
}

#[cfg(all(test, not(dev)))]
mod tests {
    #[test]
    fn embeds_index_html() {
        let context: tauri::Context<tauri::Wry> = tauri::generate_context!();
        assert!(context.assets().get(&"/index.html".into()).is_some());
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    #[test]
    fn migrates_legacy_fish_selection_without_switching_existing_model() {
        let mut prefs = Preferences {
            voice: Some(LEGACY_FISH_VOICE.into()),
            ..Preferences::default()
        };
        assert!(migrate_provider(&mut prefs));
        assert_eq!(prefs.voice.as_deref(), Some("openrouter"));
        assert_eq!(
            prefs.open_router_model.as_deref(),
            Some("fish-audio/s2.1-pro-free:free")
        );
        prefs.voice = Some(LEGACY_FISH_VOICE.into());
        prefs.open_router_model = Some("other/model".into());
        assert!(migrate_provider(&mut prefs));
        assert_eq!(prefs.open_router_model.as_deref(), Some("other/model"));
        assert!(!migrate_provider(&mut prefs));
    }
}
