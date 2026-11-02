use std::{collections::BTreeMap, future::Future, time::Duration};

use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER},
    redirect::Policy,
};
use serde::Serialize;

use crate::{MAX_AUDIO_BYTES, SpeechError};

const ENDPOINT: &str = "https://openrouter.ai/api/v1/audio/speech";
const CATALOG: &str = "https://openrouter.ai/api/v1/models?output_modalities=speech";
const USER_CATALOG: &str = "https://openrouter.ai/api/v1/models/user?output_modalities=speech";
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRouterPricing {
    pub prompt: String,
    pub completion: String,
    #[serde(flatten)]
    pub other: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRouterModel {
    pub id: String,
    pub name: String,
    pub description: String,
    pub pricing: OpenRouterPricing,
    pub voices: Vec<String>,
    pub free: bool,
}

#[derive(Serialize)]
struct SpeechRequest<'a> {
    model: &'a str,
    input: &'a str,
    response_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<&'a str>,
}

pub fn client() -> Result<Client, SpeechError> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(55))
        .redirect(Policy::none())
        .build()
        .map_err(|_| {
            SpeechError::Engine("Не удалось инициализировать соединение OpenRouter".into())
        })
}

pub async fn models(
    client: &Client,
    api_key: Option<&str>,
) -> Result<Vec<OpenRouterModel>, SpeechError> {
    let mut request = client.get(if api_key.is_some() {
        USER_CATALOG
    } else {
        CATALOG
    });
    if let Some(key) = api_key {
        request = request.header(AUTHORIZATION, authorization(key)?);
    }
    let response = request.send().await.map_err(|_| {
        SpeechError::Engine("Не удалось загрузить каталог моделей OpenRouter".into())
    })?;
    if !response.status().is_success() {
        return Err(status_error(
            response.status(),
            response.headers().get(RETRY_AFTER),
        ));
    }
    let bytes = bounded_body(response, MAX_CATALOG_BYTES).await?;
    parse_catalog(&bytes)
}

fn authorization(api_key: &str) -> Result<HeaderValue, SpeechError> {
    if api_key.is_empty() || api_key.len() > 8192 || api_key.chars().any(char::is_whitespace) {
        return Err(SpeechError::Engine(
            "Укажите действительный ключ OpenRouter в настройках".into(),
        ));
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|_| SpeechError::Engine("Некорректный ключ OpenRouter".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

fn parse_catalog(bytes: &[u8]) -> Result<Vec<OpenRouterModel>, SpeechError> {
    let root: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| {
        SpeechError::Engine("OpenRouter вернул некорректный каталог моделей".into())
    })?;
    let entries = root
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            SpeechError::Engine("OpenRouter вернул некорректный каталог моделей".into())
        })?;
    let mut models = Vec::new();
    for entry in entries {
        let contains = |path: &serde_json::Value, value: &str| {
            path.as_array()
                .is_some_and(|list| list.iter().any(|item| item.as_str() == Some(value)))
        };
        if !contains(&entry["architecture"]["input_modalities"], "text")
            || !contains(&entry["architecture"]["output_modalities"], "speech")
            || !contains(&entry["endpoints"], "/api/v1/audio/speech")
        {
            continue;
        }
        let Some(id) = entry["id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let pricing = entry["pricing"].as_object();
        let price = |name: &str| {
            pricing
                .and_then(|prices| prices.get(name))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let prompt = price("prompt");
        let completion = price("completion");
        let zero = |value: &str| {
            value
                .parse::<f64>()
                .is_ok_and(|number| number.is_finite() && number == 0.0)
                && !value
                    .split(['e', 'E'])
                    .next()
                    .unwrap_or("")
                    .bytes()
                    .any(|digit| (b'1'..=b'9').contains(&digit))
        };
        let free = !prompt.is_empty()
            && !completion.is_empty()
            && pricing.is_some_and(|prices| {
                prices
                    .values()
                    .all(|value| value.as_str().is_some_and(zero))
            });
        let voices = entry["supported_voices"]
            .as_array()
            .map_or_else(Vec::new, |items| {
                items
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            });
        let other = pricing.map_or_else(BTreeMap::new, |prices| {
            prices
                .iter()
                .filter(|(name, _)| name.as_str() != "prompt" && name.as_str() != "completion")
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or("unknown").to_owned()))
                .collect()
        });
        models.push(OpenRouterModel {
            id: id.into(),
            name: entry["name"].as_str().unwrap_or(id).into(),
            description: entry["description"].as_str().unwrap_or("").into(),
            pricing: OpenRouterPricing {
                prompt,
                completion,
                other,
            },
            voices,
            free,
        });
    }
    Ok(models)
}

pub async fn synthesize<F, Fut>(
    client: &Client,
    text: &str,
    api_key: &str,
    model: &str,
    voice: Option<&str>,
    on_response: F,
) -> Result<(Vec<u8>, &'static str), SpeechError>
where
    F: FnOnce(Option<String>) -> Fut + Send,
    Fut: Future<Output = Result<(), SpeechError>> + Send,
{
    send_tracked(
        client,
        ENDPOINT,
        text,
        api_key,
        model,
        voice,
        MAX_AUDIO_BYTES,
        on_response,
    )
    .await
}

pub fn validate_request(
    api_key: &str,
    model: &str,
    voice: Option<&str>,
) -> Result<HeaderValue, SpeechError> {
    if model.is_empty() || model.len() > 256 || model.chars().any(char::is_whitespace) {
        return Err(SpeechError::Engine("Выберите модель OpenRouter".into()));
    }
    if voice.is_some_and(|value| {
        value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
    }) {
        return Err(SpeechError::Engine(
            "Некорректный идентификатор голоса OpenRouter".into(),
        ));
    }
    authorization(api_key)
}

fn pcm_only(model: &str) -> bool {
    model.starts_with("heygen/") || (model.starts_with("google/gemini-") && model.contains("tts"))
}
fn pcm_default_rate(model: &str) -> Option<u32> {
    if model.starts_with("heygen/heygen-voice-1") {
        Some(44_100)
    } else if [
        "google/gemini-2.5-",
        "google/gemini-3.1-",
        "google/gemini-3.8-",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
        && model.contains("tts")
    {
        Some(24_000)
    } else {
        None
    }
}

#[cfg(test)]
async fn send(
    client: &Client,
    endpoint: &str,
    text: &str,
    api_key: &str,
    model: &str,
    voice: Option<&str>,
    audio_limit: usize,
) -> Result<(Vec<u8>, &'static str), SpeechError> {
    send_tracked(
        client,
        endpoint,
        text,
        api_key,
        model,
        voice,
        audio_limit,
        |_| async { Ok(()) },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_tracked<F, Fut>(
    client: &Client,
    endpoint: &str,
    text: &str,
    api_key: &str,
    model: &str,
    voice: Option<&str>,
    audio_limit: usize,
    on_response: F,
) -> Result<(Vec<u8>, &'static str), SpeechError>
where
    F: FnOnce(Option<String>) -> Fut + Send,
    Fut: Future<Output = Result<(), SpeechError>> + Send,
{
    let auth = validate_request(api_key, model, voice)?;
    let response_format = if pcm_only(model) { "pcm" } else { "mp3" };
    let request = SpeechRequest {
        model,
        input: text,
        response_format,
        voice,
    };
    let response = client
        .post(endpoint)
        .header(AUTHORIZATION, auth)
        .json(&request)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                SpeechError::Engine("OpenRouter не ответил вовремя. Повторите попытку".into())
            } else {
                SpeechError::Engine(
                    "Не удалось подключиться к OpenRouter. Проверьте интернет и повторите попытку"
                        .into(),
                )
            }
        })?;
    let generation_id = response
        .headers()
        .get("x-generation-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| safe_generation_id(value) && *value != api_key)
        .map(str::to_owned);
    on_response(generation_id).await?;
    if !response.status().is_success() {
        return Err(status_error(
            response.status(),
            response.headers().get(RETRY_AFTER),
        ));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    let is_pcm = media_type.eq_ignore_ascii_case("audio/pcm");
    if !(is_pcm
        || media_type.eq_ignore_ascii_case("audio/mpeg")
        || media_type.eq_ignore_ascii_case("audio/wav")
        || media_type.eq_ignore_ascii_case("audio/x-wav"))
    {
        return Err(SpeechError::Engine(
            "OpenRouter вернул ответ без поддерживаемого аудио".into(),
        ));
    }
    let audio = bounded_body(response, audio_limit).await?;
    decode_audio(audio, &content_type, model)
}

fn safe_generation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.to_ascii_lowercase().starts_with("sk-")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn decode_audio(
    audio: Vec<u8>,
    content_type: &str,
    model: &str,
) -> Result<(Vec<u8>, &'static str), SpeechError> {
    if audio.is_empty() {
        return Err(SpeechError::Engine("OpenRouter вернул пустое аудио".into()));
    }
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    if media_type.eq_ignore_ascii_case("audio/mpeg") {
        return Ok((audio, "audio/mpeg"));
    }
    if audio.starts_with(b"RIFF") {
        if valid_wav(&audio) {
            return Ok((audio, "audio/wav"));
        }
        return Err(SpeechError::Engine(
            "OpenRouter вернул повреждённый WAV".into(),
        ));
    }
    if media_type.eq_ignore_ascii_case("audio/pcm") {
        let rate = pcm_rate(content_type, model)?;
        if !audio.len().is_multiple_of(2) {
            return Err(SpeechError::Engine(
                "OpenRouter вернул повреждённое PCM-аудио".into(),
            ));
        }
        return Ok((crate::pcm_to_wav(&audio, rate)?, "audio/wav"));
    }
    Err(SpeechError::Engine(
        "OpenRouter вернул повреждённый WAV".into(),
    ))
}

fn valid_wav(data: &[u8]) -> bool {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return false;
    }
    let length = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let Some(end) = length.checked_add(8) else {
        return false;
    };
    if end > data.len() || end < 12 {
        return false;
    }
    let mut cursor = 12;
    let mut has_format = false;
    let mut has_audio = false;
    while cursor + 8 <= end {
        let id = &data[cursor..cursor + 4];
        let size = u32::from_le_bytes([
            data[cursor + 4],
            data[cursor + 5],
            data[cursor + 6],
            data[cursor + 7],
        ]) as usize;
        let Some(next) = cursor
            .checked_add(8)
            .and_then(|offset| offset.checked_add(size))
        else {
            return false;
        };
        if next > end {
            return false;
        }
        if id == b"fmt " {
            has_format = size >= 16;
        }
        if id == b"data" {
            has_audio = size > 0;
        }
        let Some(padded) = next.checked_add(size & 1) else {
            return false;
        };
        if padded > end {
            return false;
        }
        cursor = padded;
    }
    cursor == end && has_format && has_audio
}

fn pcm_rate(content_type: &str, model: &str) -> Result<u32, SpeechError> {
    let mut rate = None;
    let mut channels = None;
    for parameter in content_type.split(';').skip(1) {
        let Some((key, value)) = parameter.trim().split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if key.trim().eq_ignore_ascii_case("rate") || key.trim().eq_ignore_ascii_case("samplerate")
        {
            rate = Some(
                value
                    .parse::<u32>()
                    .ok()
                    .filter(|rate| (8_000..=192_000).contains(rate))
                    .ok_or_else(|| {
                        SpeechError::Engine("Некорректная частота PCM от OpenRouter".into())
                    })?,
            );
        } else if key.trim().eq_ignore_ascii_case("channels") {
            channels = Some(value == "1");
        }
    }
    if channels == Some(false) || (channels.is_none() && pcm_default_rate(model).is_none()) {
        return Err(SpeechError::Engine(
            "OpenRouter не подтвердил моно PCM".into(),
        ));
    }
    rate.or_else(|| pcm_default_rate(model))
        .ok_or_else(|| SpeechError::Engine("OpenRouter не указал частоту PCM-аудио".into()))
}

async fn bounded_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, SpeechError> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(SpeechError::Engine(
            "Ответ OpenRouter слишком большой".into(),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        SpeechError::Engine("Соединение с OpenRouter прервалось при получении ответа".into())
    })? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(SpeechError::Engine(
                "Ответ OpenRouter слишком большой".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn status_error(status: StatusCode, retry_after: Option<&HeaderValue>) -> SpeechError {
    let message = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            "OpenRouter отклонил ключ. Проверьте ключ и разрешения в настройках".to_owned()
        }
        StatusCode::PAYMENT_REQUIRED => {
            "OpenRouter отклонил запрос из-за состояния счёта или лимита. Проверьте аккаунт"
                .to_owned()
        }
        StatusCode::TOO_MANY_REQUESTS => {
            let seconds = retry_after
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| *value <= 86_400);
            seconds.map_or_else(
                || "OpenRouter ограничил частоту запросов. Повторите попытку позже".to_owned(),
                |seconds| {
                    format!(
                        "OpenRouter ограничил частоту запросов. Повторите попытку через {seconds} с"
                    )
                },
            )
        }
        StatusCode::SERVICE_UNAVAILABLE => {
            "Модель OpenRouter сейчас недоступна. Повторите попытку позже".to_owned()
        }
        status if status.is_redirection() => {
            "OpenRouter вернул перенаправление. Соединение остановлено".to_owned()
        }
        status if status.is_server_error() => {
            "OpenRouter или поставщик модели временно недоступен. Повторите попытку позже"
                .to_owned()
        }
        status => format!(
            "OpenRouter отклонил запрос (HTTP {}). Проверьте доступность модели и настройки голоса",
            status.as_u16()
        ),
    };
    SpeechError::Engine(message)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    async fn server(reply: &'static [u8]) -> (String, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut input = Vec::new();
            let mut chunk = [0; 2048];
            let header_end = loop {
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                input.extend_from_slice(&chunk[..count]);
                if let Some(position) = input.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&input[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while input.len() < header_end + content_length {
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                input.extend_from_slice(&chunk[..count]);
            }
            socket.write_all(reply).await.unwrap();
            input
        });
        (format!("http://{address}/api/v1/audio/speech"), task)
    }

    fn test_client() -> Client {
        Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .unwrap()
    }

    const MODEL: &str = "some-provider/some-model";

    #[test]
    fn filters_speech_catalog_and_handles_missing_prices() {
        let input = serde_json::json!({"data": [
            {"id":"free/model", "name":"Free", "description":"test", "architecture":{"input_modalities":["text"],"output_modalities":["speech"]}, "endpoints":["/api/v1/audio/speech"], "supported_voices":["voice-a"], "pricing":{"prompt":"0","completion":"0"}},
            {"id":"paid/model", "architecture":{"input_modalities":["text"],"output_modalities":["speech"]}, "endpoints":["/api/v1/audio/speech"], "supported_voices":null, "pricing":{"prompt":"0.001","completion":"0"}},
            {"id":"unknown/model", "architecture":{"input_modalities":["text"],"output_modalities":["speech"]}, "endpoints":["/api/v1/audio/speech"], "pricing":{"prompt":"0"}},
            {"id":"chat/model", "architecture":{"input_modalities":["text"],"output_modalities":["text"]}, "endpoints":["/api/v1/chat/completions"], "pricing":{"prompt":"0","completion":"0"}}
        ]});
        let models = parse_catalog(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(models.len(), 3);
        assert!(models[0].free);
        assert_eq!(models[0].voices, ["voice-a"]);
        assert!(!models[1].free);
        assert_eq!(models[1].voices, Vec::<String>::new());
        assert!(!models[2].free);
        assert_eq!(models[2].pricing.completion, "");
    }

    #[test]
    fn detects_wave_container_in_pcm_response() {
        let wav = crate::pcm_to_wav(&[1, 2, 3, 4], 24_000).unwrap();
        let (decoded, mime) =
            decode_audio(wav.clone(), "audio/pcm", "google/gemini-3.8-flash-tts").unwrap();
        assert_eq!(mime, "audio/wav");
        assert_eq!(decoded, wav);
        let mut truncated = wav;
        truncated.pop();
        assert!(decode_audio(truncated, "audio/pcm", "google/gemini-3.8-flash-tts").is_err());
    }

    #[test]
    fn pcm_metadata_accepts_only_known_mono_rates() {
        assert_eq!(
            pcm_rate("audio/pcm; rate=48000; channels=1", "unknown").unwrap(),
            48_000
        );
        assert_eq!(
            pcm_rate("audio/pcm", "google/gemini-3.8-flash-tts").unwrap(),
            24_000
        );
        assert_eq!(
            pcm_rate("audio/pcm", "heygen/heygen-voice-1").unwrap(),
            44_100
        );
        assert!(pcm_rate("audio/pcm", "unknown").is_err());
        assert!(pcm_rate("audio/pcm", "google/gemini-9.0-tts").is_err());
        assert!(pcm_rate("audio/pcm", "heygen/voice-9").is_err());
        assert!(pcm_rate("audio/pcm; rate=24000; channels=2", "unknown").is_err());
    }

    #[tokio::test]
    async fn tracks_headers_before_truncated_body_and_error_status() {
        use std::sync::{Arc, Mutex};
        for reply in [
            b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nX-Generation-Id: gen-test-123\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx".as_slice(),
            b"HTTP/1.1 429 Too Many Requests\r\nX-Generation-Id: gen-test-123\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
        ] {
            let (url, server_task) = server(reply).await;
            let recorded = Arc::new(Mutex::new(None));
            let target = Arc::clone(&recorded);
            let result = send_tracked(&test_client(), &url, "text", "key", MODEL, None, 1024, move |id| async move {
                *target.lock().unwrap() = id;
                Ok(())
            }).await;
            assert!(result.is_err());
            assert_eq!(recorded.lock().unwrap().as_deref(), Some("gen-test-123"));
            server_task.await.unwrap();
        }
    }

    #[test]
    fn rejects_unsafe_generation_ids() {
        assert!(safe_generation_id("gen-123_abc.def"));
        assert!(!safe_generation_id("sk-or-v1-secret"));
        assert!(!safe_generation_id("gen secret"));
        assert!(!safe_generation_id(&"x".repeat(513)));
    }

    #[tokio::test]
    async fn sends_pcm_for_gemini_and_wraps_as_wav() {
        let (url, request) = server(b"HTTP/1.1 200 OK\r\nContent-Type: audio/pcm;rate=24000;channels=1\r\nContent-Length: 4\r\nConnection: close\r\n\r\n1234").await;
        let (audio, mime) = send(
            &test_client(),
            &url,
            "text",
            "key",
            "google/gemini-3.8-flash-tts",
            Some("Kore"),
            1024,
        )
        .await
        .unwrap();
        assert_eq!(mime, "audio/wav");
        assert_eq!(&audio[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(audio[24..28].try_into().unwrap()),
            24_000
        );
        assert_eq!(&audio[44..], b"1234");
        let bytes = request.await.unwrap();
        let split = bytes.windows(4).position(|x| x == b"\r\n\r\n").unwrap() + 4;
        let body: serde_json::Value = serde_json::from_slice(&bytes[split..]).unwrap();
        assert_eq!(body["response_format"], "pcm");
        assert_eq!(body["voice"], "Kore");
        assert_eq!(body["model"], "google/gemini-3.8-flash-tts");
    }

    #[test]
    fn serializes_an_explicit_voice_without_changing_model() {
        let request = SpeechRequest {
            model: MODEL,
            input: "text",
            response_format: "mp3",
            voice: Some("user-voice-id"),
        };
        let body = serde_json::to_value(request).unwrap();
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["voice"], "user-voice-id");
        assert!(body.get("speed").is_none());
    }

    #[tokio::test]
    async fn sends_selected_model_and_returns_mp3() {
        let (url, request) = server(b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 4\r\nConnection: close\r\n\r\nMP3!").await;
        let audio = send(
            &test_client(),
            &url,
            "Привет",
            "test-key",
            MODEL,
            None,
            1024,
        )
        .await
        .unwrap();
        assert_eq!(audio.0, b"MP3!");
        assert_eq!(audio.1, "audio/mpeg");
        let bytes = request.await.unwrap();
        let split = bytes.windows(4).position(|x| x == b"\r\n\r\n").unwrap() + 4;
        let headers = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer test-key"));
        let body: serde_json::Value = serde_json::from_slice(&bytes[split..]).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "model": MODEL, "input": "Привет", "response_format": "mp3" })
        );
    }

    #[tokio::test]
    async fn rejects_auth_and_rate_limit_with_no_secret_exposure() {
        for (reply, expected) in [
            (b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"secret\":\"test-key\"}".as_slice(), "ключ"),
            (b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 12\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(), "12 с"),
        ] {
            let (url, request) = server(reply).await;
            let error = send(&test_client(), &url, "text", "test-key", MODEL, None, 1024).await.unwrap_err().to_string();
            assert!(error.contains(expected));
            assert!(!error.contains("test-key"));
            request.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_non_audio_and_oversized_stream() {
        for reply in [
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\n12345\r\n0\r\n\r\n".as_slice(),
        ] {
            let (url, request) = server(reply).await;
            assert!(send(&test_client(), &url, "text", "key", MODEL, None, 4).await.is_err());
            request.await.unwrap();
        }
    }
    #[tokio::test]
    async fn stop_cancels_in_flight_response() {
        use std::{path::PathBuf, sync::Arc};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/api/v1/audio/speech",
            listener.local_addr().unwrap()
        );
        let (sent, ready) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 2048];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
                if request
                    .windows(b"\"response_format\":\"mp3\"}".len())
                    .any(|part| part == b"\"response_format\":\"mp3\"}")
                {
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 100\r\n\r\nx",
                )
                .await
                .unwrap();
            sent.send(()).unwrap();
            let mut byte = [0; 1];
            match tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
                .await
                .unwrap()
            {
                Ok(0) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    true
                }
                _ => false,
            }
        });
        let service = Arc::new(crate::SpeechService::new(PathBuf::new()).unwrap());
        let running = Arc::clone(&service);
        let client = test_client();
        let task = tokio::spawn(async move {
            running
                .run_cancellable(send(&client, &url, "text", "key", MODEL, None, 1024))
                .await
        });
        ready.await.unwrap();
        service.stop().await;
        assert!(matches!(task.await.unwrap(), Err(SpeechError::Cancelled)));
        assert!(server.await.unwrap());
    }
}
