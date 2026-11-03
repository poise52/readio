mod openrouter;
mod timing;
pub use openrouter::{AudioFormat, AudioTransport, OpenRouterModel, OpenRouterPricing};
pub use timing::{SpeechTiming, collect_edge_timings};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use msedge_tts::tts::{
    SpeechConfig,
    stream::{SynthesizedResponse, tokio_runtime::msedge_tts_split_async},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::watch,
    time::timeout,
};

const MAX_TEXT_BYTES: usize = 8_000;
const MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;
const MAX_EDGE_ESCAPED_BYTES: usize = 4096;

/// Joins a non-empty batch after checking its total UTF-8 byte length.
///
/// # Errors
/// Returns `SpeechError::InvalidText` for empty or oversized batches.
pub fn validate_and_join_texts(texts: &[&str]) -> Result<String, SpeechError> {
    if texts.is_empty() || texts.iter().all(|text| text.trim().is_empty()) {
        return Err(SpeechError::InvalidText);
    }
    let length = texts
        .iter()
        .try_fold(0_usize, |sum, text| sum.checked_add(text.len()))
        .and_then(|sum| {
            texts
                .len()
                .saturating_sub(1)
                .checked_mul(2)
                .and_then(|separators| sum.checked_add(separators))
        })
        .filter(|size| *size <= MAX_TEXT_BYTES)
        .ok_or(SpeechError::InvalidText)?;
    let mut joined = String::with_capacity(length);
    for (index, text) in texts.iter().enumerate() {
        if index > 0 {
            joined.push_str("\n\n");
        }
        joined.push_str(text);
    }
    Ok(joined)
}
pub const OPENROUTER_VOICE: &str = "openrouter";
const EDGE_DMITRY: &str = "ru-RU-DmitryNeural";
const EDGE_SVETLANA: &str = "ru-RU-SvetlanaNeural";

#[derive(Debug, Error)]
pub enum SpeechError {
    #[error("Текст для озвучивания пуст или превышает 8000 байт")]
    InvalidText,
    #[error("Скорость речи должна быть от 0,5 до 3")]
    InvalidSpeed,
    #[error("Фрагмент слишком длинный для Edge. Разделите текст на части")]
    EdgeTextTooLong,
    #[error("Выбранный голос недоступен: {0}")]
    UnknownVoice(String),
    #[error("Синтез речи отменён")]
    Cancelled,
    #[error("Превышено время ожидания синтеза речи")]
    Timeout,
    #[error("Ошибка синтеза речи: {0}")]
    Engine(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Voice {
    pub id: String,
    pub name: String,
    pub locale: String,
    pub engine: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechAudio {
    pub data: String,
    pub mime: String,
    pub playback_rate: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub timings: Vec<SpeechTiming>,
}

#[derive(Debug)]
pub struct SpeechService {
    data_dir: PathBuf,
    openrouter_client: reqwest::Client,
    generation: watch::Sender<u64>,
    generation_lock: Mutex<()>,
    piper_revisions: Mutex<HashMap<String, (Vec<ResourceStamp>, String)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceStamp {
    path: PathBuf,
    size: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(windows)]
    windows_times: (u64, u64),
    #[cfg(windows)]
    windows_identity: (u64, u64),
}

fn resource_stamp(path: &Path) -> Result<ResourceStamp, SpeechError> {
    let file = std::fs::File::open(path)
        .map_err(|_| SpeechError::Engine("Не удалось открыть файлы Piper".into()))?;
    let metadata = file
        .metadata()
        .map_err(|_| SpeechError::Engine("Не удалось прочитать файлы Piper".into()))?;
    #[cfg(windows)]
    let identity = {
        let info = winapi_util::file::information(&file)
            .map_err(|_| SpeechError::Engine("Не удалось проверить файлы Piper".into()))?;
        (info.volume_serial_number(), info.file_index())
    };
    Ok(ResourceStamp {
        path: path.to_owned(),
        size: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        #[cfg(windows)]
        windows_times: (metadata.creation_time(), metadata.last_write_time()),
        #[cfg(windows)]
        windows_identity: identity,
    })
}

#[derive(Debug)]
struct PiperModel {
    id: String,
    path: PathBuf,
    name: String,
    locale: String,
}

impl SpeechService {
    /// Creates a speech service using the application data directory.
    ///
    /// # Errors
    /// Returns an error if the HTTP client cannot be initialized.
    pub fn new(data_dir: PathBuf) -> Result<Self, SpeechError> {
        let (generation, _) = watch::channel(0);
        Ok(Self {
            data_dir,
            openrouter_client: openrouter::client()?,
            generation,
            generation_lock: Mutex::new(()),
            piper_revisions: Mutex::new(HashMap::new()),
        })
    }

    /// Lists Edge voices and locally installed Piper voices.
    ///
    /// # Errors
    /// Reserved for voice discovery failures.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn voices(&self) -> Result<Vec<Voice>, SpeechError> {
        let mut voices = vec![
            Voice {
                id: EDGE_DMITRY.into(),
                name: "Edge Дмитрий · онлайн".into(),
                locale: "ru-RU".into(),
                engine: "edge".into(),
            },
            Voice {
                id: EDGE_SVETLANA.into(),
                name: "Edge Светлана · онлайн".into(),
                locale: "ru-RU".into(),
                engine: "edge".into(),
            },
            Voice {
                id: OPENROUTER_VOICE.into(),
                name: "OpenRouter · онлайн".into(),
                locale: "multilingual".into(),
                engine: "openrouter".into(),
            },
        ];
        if self.piper_binary().is_some() {
            voices.extend(self.piper_models().into_iter().map(|model| Voice {
                id: model.id,
                name: format!("Piper {} · офлайн", model.name),
                locale: model.locale,
                engine: "piper".into(),
            }));
        }
        Ok(voices)
    }

    /// Synthesizes a single text chunk.
    ///
    /// # Errors
    /// Returns an error for invalid input, unavailable engines, cancellation, or timeout.
    pub async fn synthesize(
        &self,
        text: &str,
        voice: Option<&str>,
        speed: f32,
    ) -> Result<SpeechAudio, SpeechError> {
        self.synthesize_inner(text, voice, speed).await
    }

    /// Synthesizes with the selected voice without switching engines.
    ///
    /// # Errors
    /// Returns an error for invalid input, unavailable engines, cancellation, or timeout.
    pub async fn synthesize_exact(
        &self,
        text: &str,
        voice: Option<&str>,
        speed: f32,
    ) -> Result<SpeechAudio, SpeechError> {
        self.synthesize_inner(text, voice, speed).await
    }

    async fn synthesize_inner(
        &self,
        text: &str,
        voice: Option<&str>,
        speed: f32,
    ) -> Result<SpeechAudio, SpeechError> {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(SpeechError::InvalidText);
        }
        if !speed.is_finite() || !(0.5..=3.0).contains(&speed) {
            return Err(SpeechError::InvalidSpeed);
        }
        let selected = voice.unwrap_or(EDGE_DMITRY);
        if (selected == EDGE_DMITRY || selected == EDGE_SVETLANA)
            && xml_escape(text).len() > MAX_EDGE_ESCAPED_BYTES
        {
            return Err(SpeechError::EdgeTextTooLong);
        }
        let operation = async {
            if selected == EDGE_DMITRY || selected == EDGE_SVETLANA {
                edge(text, selected, speed)
                    .await
                    .map(|(audio, timings)| (audio, "audio/mpeg", timings))
            } else {
                let model = self
                    .piper_models()
                    .into_iter()
                    .find(|model| model.id == selected)
                    .ok_or_else(|| SpeechError::UnknownVoice(selected.into()))?;
                let binary = self.piper_binary().ok_or_else(|| {
                    SpeechError::Engine("Piper не найден. Укажите READIO_PIPER_BIN".into())
                })?;
                piper(&binary, &model.path, text, speed)
                    .await
                    .map(|audio| (audio, "audio/wav", Vec::new()))
            }
        };
        let result = self.run_cancellable(Box::pin(operation)).await?;
        if result.0.is_empty() || result.0.len() > MAX_AUDIO_BYTES {
            return Err(SpeechError::Engine(
                "Получено пустое или слишком большое аудио".into(),
            ));
        }
        Ok(SpeechAudio {
            data: STANDARD.encode(result.0),
            mime: result.1.into(),
            playback_rate: 1.0,
            timings: result.2,
        })
    }

    /// Synthesizes several texts in a single request to keep one voice and
    /// avoid the per-line latency of a separate request for every passage.
    /// Texts are joined with paragraph breaks; the joined text must satisfy
    /// the same size limits as a single request.
    ///
    /// # Errors
    /// Returns an error for empty or oversized batches, invalid input,
    /// unavailable engines, cancellation, or timeout.
    pub async fn synthesize_many(
        &self,
        texts: &[&str],
        voice: Option<&str>,
        speed: f32,
    ) -> Result<SpeechAudio, SpeechError> {
        let joined = validate_and_join_texts(texts)?;
        self.synthesize(&joined, voice, speed).await
    }

    /// Fetches available text-to-speech models from `OpenRouter`.
    ///
    /// # Errors
    /// Returns an error if the catalogue cannot be retrieved or parsed.
    pub async fn openrouter_models(
        &self,
        api_key: Option<&str>,
    ) -> Result<Vec<OpenRouterModel>, SpeechError> {
        openrouter::models(&self.openrouter_client, api_key).await
    }

    /// Fetches provider-reported USD cost for an existing generation.
    ///
    /// # Errors
    /// Returns an error when credentials, response or provider usage are unavailable.
    pub async fn openrouter_generation_cost(
        &self,
        api_key: &str,
        generation_id: &str,
    ) -> Result<String, SpeechError> {
        openrouter::generation_cost(&self.openrouter_client, api_key, generation_id).await
    }

    /// Validates a speech request without contacting the provider.
    ///
    /// # Errors
    /// Returns an error for invalid text, speed, model, voice or credentials.
    pub fn validate_openrouter_request(
        &self,
        text: &str,
        api_key: &str,
        model: &str,
        voice: Option<&str>,
        speed: f32,
    ) -> Result<(), SpeechError> {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(SpeechError::InvalidText);
        }
        if !speed.is_finite() || !(0.5..=3.0).contains(&speed) {
            return Err(SpeechError::InvalidSpeed);
        }
        openrouter::validate_request(api_key, model, voice)?;
        Ok(())
    }

    /// Synthesizes speech with the selected `OpenRouter` model.
    ///
    /// # Errors
    /// Returns an error for invalid input, credentials, network/API errors, cancellation or timeout.
    pub async fn synthesize_with_openrouter(
        &self,
        text: &str,
        api_key: &str,
        model: &str,
        voice: Option<&str>,
        speed: f32,
    ) -> Result<SpeechAudio, SpeechError> {
        self.synthesize_openrouter(
            text,
            api_key,
            model,
            voice,
            speed,
            None,
            AudioTransport::default(),
            |_| async { Ok(()) },
        )
        .await
    }

    /// Returns a cancellation marker to use when an operation starts after asynchronous preparation.
    pub fn cancellation_generation(&self) -> u64 {
        let _guard = self
            .generation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *self.generation.borrow()
    }

    /// Synthesizes speech only if no cancellation or newer request occurred since `generation`.
    ///
    /// # Errors
    /// Returns an error for stale generation, invalid input, credentials, network/API errors or timeout.
    pub async fn synthesize_with_openrouter_if_current(
        &self,
        text: &str,
        api_key: &str,
        model: &str,
        voice: Option<&str>,
        speed: f32,
        generation: u64,
    ) -> Result<SpeechAudio, SpeechError> {
        self.synthesize_openrouter(
            text,
            api_key,
            model,
            voice,
            speed,
            Some(generation),
            AudioTransport::default(),
            |_| async { Ok(()) },
        )
        .await
    }

    /// Synthesizes using explicitly selected output transport.
    ///
    /// # Errors
    /// Returns an error for invalid transport, input, credentials or network failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn synthesize_with_openrouter_transport_if_current(
        &self,
        text: &str,
        api_key: &str,
        model: &str,
        voice: Option<&str>,
        speed: f32,
        generation: u64,
        transport: AudioTransport,
    ) -> Result<SpeechAudio, SpeechError> {
        self.synthesize_openrouter(
            text,
            api_key,
            model,
            voice,
            speed,
            Some(generation),
            transport,
            |_| async { Ok(()) },
        )
        .await
    }

    /// Synthesizes speech and persists response metadata before reading the body.
    ///
    /// # Errors
    /// Returns an error for invalid input, network or API errors, cancellation, timeout, or callback failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn synthesize_with_openrouter_tracked_if_current<F, Fut>(
        &self,
        text: &str,
        api_key: &str,
        model: &str,
        voice: Option<&str>,
        speed: f32,
        generation: u64,
        transport: AudioTransport,
        on_response: F,
    ) -> Result<SpeechAudio, SpeechError>
    where
        F: FnOnce(Option<String>) -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), SpeechError>> + Send,
    {
        self.synthesize_openrouter(
            text,
            api_key,
            model,
            voice,
            speed,
            Some(generation),
            transport,
            on_response,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn synthesize_openrouter<F, Fut>(
        &self,
        text: &str,
        api_key: &str,
        model: &str,
        voice: Option<&str>,
        speed: f32,
        generation: Option<u64>,
        transport: AudioTransport,
        on_response: F,
    ) -> Result<SpeechAudio, SpeechError>
    where
        F: FnOnce(Option<String>) -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), SpeechError>> + Send,
    {
        self.validate_openrouter_request(text, api_key, model, voice, speed)?;
        let (audio, mime) = self
            .run_cancellable_if_current(
                openrouter::synthesize(
                    &self.openrouter_client,
                    text,
                    api_key,
                    model,
                    voice,
                    transport,
                    on_response,
                ),
                generation,
            )
            .await?;
        Ok(SpeechAudio {
            data: STANDARD.encode(audio),
            mime: mime.into(),
            playback_rate: speed,
            timings: Vec::new(),
        })
    }

    async fn run_cancellable<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T, SpeechError>>,
    ) -> Result<T, SpeechError> {
        self.run_cancellable_if_current(operation, None).await
    }

    async fn run_cancellable_if_current<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T, SpeechError>>,
        generation: Option<u64>,
    ) -> Result<T, SpeechError> {
        let mut cancelled = self.begin_generation(generation)?;
        tokio::select! {
            biased;
            _ = cancelled.changed() => Err(SpeechError::Cancelled),
            result = timeout(Duration::from_secs(60), operation) => result.map_err(|_| SpeechError::Timeout)?,
        }
    }

    fn begin_generation(&self, expected: Option<u64>) -> Result<watch::Receiver<u64>, SpeechError> {
        let _guard = self
            .generation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if expected.is_some_and(|value| value != *self.generation.borrow()) {
            return Err(SpeechError::Cancelled);
        }
        let mut receiver = self.generation.subscribe();
        self.generation
            .send_modify(|value| *value = value.wrapping_add(1));
        receiver.borrow_and_update();
        Ok(receiver)
    }

    #[allow(clippy::unused_async)]
    pub async fn stop(&self) {
        let _guard = self
            .generation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.generation
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    fn piper_binary(&self) -> Option<PathBuf> {
        if let Some(value) = env::var_os("READIO_PIPER_BIN") {
            return is_file(Path::new(&value)).then(|| PathBuf::from(value));
        }
        let name = if cfg!(windows) { "piper.exe" } else { "piper" };
        let local = self.data_dir.join("piper").join(name);
        if is_file(&local) {
            return Some(local);
        }
        env::var_os("PATH").and_then(|paths| {
            env::split_paths(&paths)
                .map(|path| path.join(name))
                .find(|path| is_file(path))
        })
    }

    fn piper_models(&self) -> Vec<PiperModel> {
        let root = env::var_os("READIO_PIPER_MODELS")
            .map_or_else(|| self.data_dir.join("models").join("piper"), PathBuf::from);
        let mut models = Vec::new();
        collect_models(&root, &root, 0, &mut models);
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }

    /// Hashes the selected Piper model, configuration and binary for cache identity.
    ///
    /// # Errors
    /// Returns an error if the voice or one of its resources is unavailable.
    pub fn piper_resource_revision(&self, voice: &str) -> Result<String, SpeechError> {
        let model = self
            .piper_models()
            .into_iter()
            .find(|model| model.id == voice)
            .ok_or_else(|| SpeechError::UnknownVoice(voice.into()))?;
        let binary = self
            .piper_binary()
            .ok_or_else(|| SpeechError::Engine("Piper не найден".into()))?;
        let mut config: OsString = model.path.as_os_str().into();
        config.push(".json");
        let resources = [
            (b"model".as_slice(), model.path.as_path()),
            (b"config".as_slice(), Path::new(&config)),
            (b"binary".as_slice(), binary.as_path()),
        ];
        let stamps = resources
            .iter()
            .map(|(_, path)| resource_stamp(path))
            .collect::<Result<Vec<_>, _>>()?;
        let mut revisions = self
            .piper_revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, revision)) = revisions
            .get(voice)
            .filter(|(previous, _)| *previous == stamps)
        {
            return Ok(revision.clone());
        }
        let mut digest = Sha256::new();
        for (kind, path) in resources {
            digest.update(kind);
            let mut file = std::fs::File::open(path)
                .map_err(|_| SpeechError::Engine("Не удалось прочитать файлы Piper".into()))?;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|_| SpeechError::Engine("Не удалось прочитать файлы Piper".into()))?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
            }
        }
        let after = resources
            .iter()
            .map(|(_, path)| resource_stamp(path))
            .collect::<Result<Vec<_>, _>>()?;
        if stamps != after {
            return Err(SpeechError::Engine(
                "Файлы Piper изменились во время чтения".into(),
            ));
        }
        let revision = format!("{:x}", digest.finalize());
        revisions.insert(voice.to_owned(), (stamps, revision.clone()));
        drop(revisions);
        Ok(revision)
    }
}

fn is_file(path: &Path) -> bool {
    path.is_file()
}

fn collect_models(root: &Path, dir: &Path, depth: usize, models: &mut Vec<PiperModel>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && depth < 2 {
            collect_models(root, &path, depth + 1, models);
        } else if kind.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "onnx")
        {
            let mut config: OsString = path.as_os_str().into();
            config.push(".json");
            if !Path::new(&config).is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let Some(relative) = relative.to_str() else {
                continue;
            };
            let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            let Ok(file) = std::fs::File::open(&config) else {
                continue;
            };
            let mut bytes = Vec::new();
            if file.take(1024 * 1024 + 1).read_to_end(&mut bytes).is_err() {
                continue;
            }
            if bytes.len() > 1024 * 1024 {
                continue;
            }
            let Ok(config_value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(locale) = config_value["language"]["code"]
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 64)
            else {
                continue;
            };
            let id = format!("piper:{}", relative.replace('\\', "/"));
            let name = name.to_owned();
            models.push(PiperModel {
                id,
                path,
                name,
                locale: locale.replace('_', "-"),
            });
        }
    }
}

fn xml_escape(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '\'' => result.push_str("&apos;"),
            '"' => result.push_str("&quot;"),
            '\t' | '\n' | '\r' => result.push(c),
            c if !c.is_control() && c != '\u{fffe}' && c != '\u{ffff}' => result.push(c),
            _ => result.push(' '),
        }
    }
    result
}

#[derive(Debug)]
enum EdgeFailure {
    Transient(&'static str),
    AudioTooLarge,
}

async fn edge(
    text: &str,
    voice: &str,
    speed: f32,
) -> Result<(Vec<u8>, Vec<SpeechTiming>), SpeechError> {
    retry_edge(|| edge_once(text, voice, speed)).await
}

async fn retry_edge<F, Fut, T>(mut attempt: F) -> Result<T, SpeechError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, EdgeFailure>>,
{
    for index in 0..3 {
        let result = timeout(Duration::from_secs(45), attempt()).await;
        match result {
            Ok(Ok(audio)) => return Ok(audio),
            Ok(Err(EdgeFailure::AudioTooLarge)) => {
                return Err(SpeechError::Engine(
                    "Edge вернул слишком большое аудио".into(),
                ));
            }
            Ok(Err(EdgeFailure::Transient(message))) if index == 2 => {
                return Err(SpeechError::Engine(format!(
                    "{message} Проверьте интернет и повторите попытку."
                )));
            }
            Err(_) if index == 2 => {
                return Err(SpeechError::Engine(
                    "Edge не ответил вовремя. Проверьте интернет и повторите попытку.".into(),
                ));
            }
            _ => tokio::time::sleep(Duration::from_millis(300 * (index + 1))).await,
        }
    }
    Err(SpeechError::Engine("Не удалось получить аудио Edge".into()))
}

#[allow(clippy::cast_possible_truncation)]
async fn edge_once(
    text: &str,
    voice: &str,
    speed: f32,
) -> Result<(Vec<u8>, Vec<SpeechTiming>), EdgeFailure> {
    let config = SpeechConfig {
        voice_name: voice.into(),
        audio_format: "audio-24khz-48kbitrate-mono-mp3".into(),
        pitch: 0,
        rate: ((speed - 1.0) * 100.0).round() as i32,
        volume: 0,
    };
    let (mut sender, mut receiver) = timeout(Duration::from_secs(10), msedge_tts_split_async())
        .await
        .map_err(|_| EdgeFailure::Transient("Edge не ответил при подключении."))?
        .map_err(|_| EdgeFailure::Transient("Не удалось подключиться к Edge."))?;
    sender
        .send(&xml_escape(text), &config)
        .await
        .map_err(|_| EdgeFailure::Transient("Соединение с Edge прервалось при отправке текста."))?;
    let mut audio = Vec::new();
    let mut metadata = Vec::new();
    let mut metadata_overflow = false;
    loop {
        let chunk = timeout(Duration::from_secs(15), receiver.read())
            .await
            .map_err(|_| EdgeFailure::Transient("Edge перестал передавать аудио."))?
            .map_err(|_| {
                EdgeFailure::Transient("Соединение с Edge прервалось во время синтеза.")
            })?;
        match chunk {
            Some(SynthesizedResponse::AudioBytes(bytes)) => {
                if audio.len().saturating_add(bytes.len()) > MAX_AUDIO_BYTES {
                    return Err(EdgeFailure::AudioTooLarge);
                }
                audio.extend_from_slice(&bytes);
            }
            Some(SynthesizedResponse::AudioMetadata(items)) => {
                if metadata.len().saturating_add(items.len()) > 4096 {
                    metadata.clear();
                    metadata_overflow = true;
                } else if !metadata_overflow {
                    metadata.extend(items);
                }
            }
            _ => {}
        }
        if !receiver.can_read().await {
            break;
        }
        tokio::task::yield_now().await;
    }
    if audio.is_empty() {
        return Err(EdgeFailure::Transient("Edge вернул пустое аудио."));
    }
    Ok((
        audio,
        collect_edge_timings(text, &metadata).unwrap_or_default(),
    ))
}

async fn piper(
    binary: &Path,
    model: &Path,
    text: &str,
    speed: f32,
) -> Result<Vec<u8>, SpeechError> {
    let mut config = model.as_os_str().to_os_string();
    config.push(".json");
    let config = tokio::fs::read(config).await.map_err(|error| {
        SpeechError::Engine(format!("не удалось прочитать конфигурацию Piper: {error}"))
    })?;
    let config: serde_json::Value = serde_json::from_slice(&config).map_err(|error| {
        SpeechError::Engine(format!("некорректная конфигурация Piper: {error}"))
    })?;
    let sample_rate = config["audio"]["sample_rate"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| (8_000..=192_000).contains(value))
        .ok_or_else(|| SpeechError::Engine("неверная частота аудио Piper".into()))?;
    let mut command = Command::new(binary);
    command
        .arg("--model")
        .arg(model)
        .arg("--output_raw")
        .arg("--length_scale")
        .arg(format!("{}", 1.0 / speed))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let audio = piper_exchange(command, text).await?;
    pcm_to_wav(&audio, sample_rate)
}

async fn piper_exchange(mut command: Command, text: &str) -> Result<Vec<u8>, SpeechError> {
    let mut child = command
        .spawn()
        .map_err(|error| SpeechError::Engine(format!("не удалось запустить Piper: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| SpeechError::Engine("Piper не открыл stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SpeechError::Engine("Piper не открыл stdout".into()))?;
    let single_line = text.replace(['\r', '\n'], " ");
    let write_input = async move {
        stdin.write_all(single_line.as_bytes()).await?;
        drop(stdin);
        Ok::<(), std::io::Error>(())
    };
    let read_audio = async {
        let mut audio = Vec::new();
        stdout
            .take((MAX_AUDIO_BYTES + 1) as u64)
            .read_to_end(&mut audio)
            .await?;
        if audio.len() > MAX_AUDIO_BYTES {
            return Err(std::io::Error::other("Piper вернул слишком большое аудио"));
        }
        Ok::<_, std::io::Error>(audio)
    };
    let ((), audio) = tokio::try_join!(write_input, read_audio)
        .map_err(|error| SpeechError::Engine(format!("ошибка обмена с Piper: {error}")))?;
    if audio.len() > MAX_AUDIO_BYTES {
        return Err(SpeechError::Engine(
            "Piper вернул слишком большое аудио".into(),
        ));
    }
    let status = child
        .wait()
        .await
        .map_err(|error| SpeechError::Engine(format!("ошибка Piper: {error}")))?;
    if !status.success() || audio.is_empty() || audio.len() % 2 != 0 {
        return Err(SpeechError::Engine(format!(
            "Piper не создал PCM (код: {status})"
        )));
    }
    Ok(audio)
}

fn pcm_to_wav(pcm: &[u8], sample_rate: u32) -> Result<Vec<u8>, SpeechError> {
    let size = u32::try_from(pcm.len())
        .map_err(|_| SpeechError::Engine("слишком большое аудио Piper".into()))?;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + size).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&size.to_le_bytes());
    wav.extend_from_slice(pcm);
    Ok(wav)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    const PIPER_EOF_CHILD: &str = "READIO_TEST_PIPER_EOF_CHILD";
    const PIPER_EOF_PCM: &[u8] = b"\x01\x02\x03\x04";

    #[test]
    fn piper_eof_child() {
        use std::io::Write as _;
        if std::env::var_os(PIPER_EOF_CHILD).is_none() {
            return;
        }
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
        if input != "Привет Мир".as_bytes() {
            std::process::exit(2);
        }
        std::io::stdout().write_all(PIPER_EOF_PCM).unwrap();
        std::io::stdout().flush().unwrap();
        std::process::exit(0);
    }

    #[tokio::test]
    async fn piper_closes_stdin_after_writing_to_allow_eof() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "tests::piper_eof_child", "--nocapture"])
            .env(PIPER_EOF_CHILD, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let audio = timeout(
            Duration::from_secs(3),
            piper_exchange(command, "Привет\nМир"),
        )
        .await
        .expect("Piper did not receive EOF")
        .unwrap();
        assert!(audio.ends_with(PIPER_EOF_PCM));
    }

    #[test]
    fn discovers_piper_config_locale_and_detects_resource_replacement() {
        let root =
            std::env::temp_dir().join(format!("readio-piper-resources-{}", std::process::id()));
        let models = root.join("models/piper");
        let binary_dir = root.join("piper");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&binary_dir).unwrap();
        let name = if cfg!(windows) { "piper.exe" } else { "piper" };
        std::fs::write(binary_dir.join(name), b"binary").unwrap();
        let model = models.join("arbitrary.onnx");
        std::fs::write(&model, b"model-a").unwrap();
        std::fs::write(
            models.join("arbitrary.onnx.json"),
            br#"{"language":{"code":"en_US"},"audio":{"sample_rate":22050}}"#,
        )
        .unwrap();
        let discovered = {
            let mut entries = Vec::new();
            collect_models(&models, &models, 0, &mut entries);
            entries
        };
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].locale, "en-US");
        if std::env::var_os("READIO_PIPER_BIN").is_none()
            && std::env::var_os("READIO_PIPER_MODELS").is_none()
        {
            let service = SpeechService::new(root.clone()).unwrap();
            let before = service
                .piper_resource_revision("piper:arbitrary.onnx")
                .unwrap();
            std::fs::write(&model, b"model-b-changed").unwrap();
            let after = service
                .piper_resource_revision("piper:arbitrary.onnx")
                .unwrap();
            assert_ne!(before, after);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn escapes_xml_and_removes_invalid_controls() {
        assert_eq!(
            xml_escape("<&>\"'\0\nтекст"),
            "&lt;&amp;&gt;&quot;&apos; \nтекст"
        );
    }
    #[test]
    fn validates_batch_size_including_separators() {
        assert_eq!(
            validate_and_join_texts(&["Первая строка.", "Вторая строка"]).unwrap(),
            "Первая строка.\n\nВторая строка"
        );
        assert!(matches!(
            validate_and_join_texts(&[]),
            Err(SpeechError::InvalidText)
        ));
        let within_limit = "я".repeat(3_999);
        let joined = validate_and_join_texts(&[&within_limit, ""]).unwrap();
        assert_eq!(joined.len(), MAX_TEXT_BYTES);
        assert!(matches!(
            validate_and_join_texts(&[&within_limit, "x"]),
            Err(SpeechError::InvalidText)
        ));
        assert!(matches!(
            validate_and_join_texts(&[" ", "\t"]),
            Err(SpeechError::InvalidText)
        ));
    }

    #[tokio::test]
    async fn edge_limit_counts_escaped_utf8_before_network() {
        let service = SpeechService::new(PathBuf::new()).unwrap();
        let text = "&".repeat(820);
        assert_eq!(text.len(), 820);
        assert_eq!(xml_escape(&text).len(), 4_100);
        assert!(matches!(
            service
                .synthesize_many(&[&text], Some(EDGE_DMITRY), 1.0)
                .await,
            Err(SpeechError::EdgeTextTooLong)
        ));
        let text = "я".repeat(2049);
        assert!(matches!(
            service
                .synthesize_many(&[&text], Some(EDGE_SVETLANA), 1.0)
                .await,
            Err(SpeechError::EdgeTextTooLong)
        ));
    }

    #[tokio::test]
    async fn rejects_empty_or_oversized_batches_before_network() {
        let service = SpeechService::new(PathBuf::new()).unwrap();
        assert!(matches!(
            service.synthesize_many(&[], None, 1.0).await,
            Err(SpeechError::InvalidText)
        ));
        assert!(matches!(
            service.synthesize_many(&["  ", "\n"], None, 1.0).await,
            Err(SpeechError::InvalidText)
        ));
        assert!(matches!(
            service.synthesize_many(&vec!["т"; 4_100], None, 1.0).await,
            Err(SpeechError::InvalidText)
        ));
    }
    #[tokio::test]
    async fn accepts_valid_batch_then_checks_voice() {
        let service = SpeechService::new(PathBuf::new()).unwrap();
        assert!(matches!(
            service
                .synthesize_many(&["текст", "ещё текст"], Some("missing"), 1.0)
                .await,
            Err(SpeechError::UnknownVoice(_))
        ));
        assert!(matches!(
            service
                .synthesize_many(&["текст", "ещё текст"], None, f32::NAN)
                .await,
            Err(SpeechError::InvalidSpeed)
        ));
    }
    #[test]
    fn openrouter_preflight_validates_without_network() {
        let service = SpeechService::new(PathBuf::new()).unwrap();
        assert!(
            service
                .validate_openrouter_request("hello", "key", "provider/model", Some("voice"), 1.0)
                .is_ok()
        );
        assert!(
            service
                .validate_openrouter_request("hello", "invalid key", "provider/model", None, 1.0)
                .is_err()
        );
        assert!(
            service
                .validate_openrouter_request("hello", "key", "invalid model", None, 1.0)
                .is_err()
        );
        assert!(
            service
                .validate_openrouter_request("hello", "key", "provider/model", Some("voice\n"), 1.0)
                .is_err()
        );
        assert!(
            service
                .validate_openrouter_request(" ", "key", "provider/model", None, 1.0)
                .is_err()
        );
        assert!(
            service
                .validate_openrouter_request("hello", "key", "provider/model", None, f32::NAN)
                .is_err()
        );
    }

    #[tokio::test]
    async fn stop_invalidates_prepared_openrouter_request() {
        let service = SpeechService::new(PathBuf::new()).unwrap();
        let generation = service.cancellation_generation();
        service.stop().await;
        assert!(matches!(
            service
                .synthesize_with_openrouter_if_current(
                    "text",
                    "key",
                    "example/model",
                    None,
                    1.0,
                    generation
                )
                .await,
            Err(SpeechError::Cancelled)
        ));
    }

    #[test]
    fn wraps_pcm_in_a_mono_wav_header() {
        let wav = pcm_to_wav(&[0x12, 0x34, 0x56, 0x78], 22_050).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 40);
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 22_050);
        assert_eq!(&wav[40..], &[4, 0, 0, 0, 0x12, 0x34, 0x56, 0x78]);
    }

    #[tokio::test]
    async fn rejects_invalid_requests_before_network() {
        let service = SpeechService::new(PathBuf::new()).unwrap();
        assert!(matches!(
            service.synthesize(" ", None, 1.0).await,
            Err(SpeechError::InvalidText)
        ));
        assert!(matches!(
            service.synthesize("текст", None, f32::NAN).await,
            Err(SpeechError::InvalidSpeed)
        ));
        assert!(matches!(
            service.synthesize("текст", Some("missing"), 1.0).await,
            Err(SpeechError::UnknownVoice(_))
        ));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancels_piper_process_on_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let folder = std::env::temp_dir().join(format!(
            "readio-piper-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        let script = folder.join("piper");
        let pid_file = folder.join("pid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let model = folder.join("voice.onnx");
        std::fs::write(
            folder.join("voice.onnx.json"),
            r#"{"audio":{"sample_rate":22050}}"#,
        )
        .unwrap();
        assert!(
            timeout(
                Duration::from_millis(1500),
                piper(&script, &model, "тест", 1.0)
            )
            .await
            .is_err()
        );
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let mut exited = false;
        for _ in 0..30 {
            let status = std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap();
            if !status.success() {
                exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::remove_dir_all(folder).unwrap();
        assert!(exited, "Piper process survived cancellation");
    }

    #[tokio::test]
    async fn retries_transient_edge_errors() {
        let mut attempts = 0;
        let audio = retry_edge(|| {
            attempts += 1;
            std::future::ready(if attempts < 3 {
                Err(EdgeFailure::Transient("offline"))
            } else {
                Ok(vec![1, 2])
            })
        })
        .await
        .unwrap();
        assert_eq!(audio, [1, 2]);
        assert_eq!(attempts, 3);
    }

    #[tokio::test]
    async fn does_not_retry_oversized_edge_audio() {
        let mut attempts = 0;
        let result = retry_edge(|| {
            attempts += 1;
            std::future::ready(Err::<Vec<u8>, _>(EdgeFailure::AudioTooLarge))
        })
        .await;
        assert!(matches!(result, Err(SpeechError::Engine(_))));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    #[ignore = "requires network access to Microsoft Edge TTS"]
    async fn edge_batch_network_smoke() {
        let service = SpeechService::new(PathBuf::new()).unwrap();
        let result = service
            .synthesize_many(
                &[
                    "Первая строка проверяет работу пакетной озвучки.",
                    "Вторая строка содержит знаки & кавычки и русские буквы.",
                    "Третье предложение завершает проверку.",
                ],
                Some(EDGE_DMITRY),
                1.0,
            )
            .await
            .expect("Edge batch synthesis");
        assert_eq!(result.mime, "audio/mpeg");
        assert!(STANDARD.decode(result.data).unwrap().len() > 1000);
    }

    #[tokio::test]
    #[ignore = "requires network access to Microsoft Edge TTS"]
    async fn edge_network_smoke() {
        for text in [
            "Привет, мир & проверка.",
            "Вторая фраза с улыбкой 😀 и знаками препинания.",
            "Третья фраза после эмодзи.",
            "Четвёртая фраза завершает проверку.",
        ] {
            let audio = edge(text, EDGE_DMITRY, 1.0).await.expect("Edge synthesis");
            assert!(
                audio.0.len() > 1000,
                "Edge returned too little audio for {text}"
            );
        }
    }
}
