//! TTS Client and Stream, SpeechConfig, Response Type.

use crate::error::{Error, Result};

/// Synthesis Config
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SpeechConfig {
    pub voice_name: String,
    pub audio_format: String,
    pub pitch: i32,
    pub rate: i32,
    pub volume: i32,
}

impl From<&crate::voice::Voice> for SpeechConfig {
    fn from(voice: &crate::voice::Voice) -> Self {
        let audio_format = if let Some(ref audio_format) = voice.suggested_codec {
            audio_format.clone()
        } else {
            "audio-24khz-48kbitrate-mono-mp3".to_owned()
        };
        Self {
            voice_name: voice.name.clone(),
            audio_format,
            pitch: 0,
            rate: 0,
            volume: 0,
        }
    }
}

/// Audio Metadata
#[derive(Debug)]
pub struct AudioMetadata {
    pub metadata_type: Option<String>,
    pub offset: u64,
    pub duration: u64,
    pub text: Option<String>,
    pub length: u64,
    pub boundary_type: Option<String>,
}

impl AudioMetadata {
    fn from_str(text: &str) -> Result<Vec<Self>> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        if let Some(items) = value["Metadata"].as_array() {
            let mut audio_metadata = Vec::new();
            for item in items {
                let metadata_type = item["Type"].as_str().map(|x| x.to_owned());
                let offset = item["Data"]["Offset"].as_u64().unwrap_or(0);
                let duration = item["Data"]["Duration"].as_u64().unwrap_or(0);
                let text = item["Data"]["text"]["Text"].as_str().map(|x| x.to_owned());
                let length = item["Data"]["text"]["Length"].as_u64().unwrap_or(0);
                let boundary_type = item["Data"]["text"]["BoundaryType"]
                    .as_str()
                    .map(|x| x.to_owned());
                audio_metadata.push(AudioMetadata {
                    metadata_type,
                    offset,
                    duration,
                    text,
                    length,
                    boundary_type,
                });
            }
            Ok(audio_metadata)
        } else {
            Err(Error::UnexpectedMessage(format!(
                "unexpected json text: {}",
                text
            )))
        }
    }
}

mod drm;
pub mod stream;

enum Payload {
    AudioBytes((tungstenite::Bytes, usize)),
    AudioMetadata(Vec<AudioMetadata>),
}

impl Payload {
    fn process(
        message: tungstenite::Message,
        turn_start: &mut bool,
        response: &mut bool,
        turn_end: &mut bool,
    ) -> Result<Option<Payload>> {
        match message {
            tungstenite::Message::Text(text) => {
                if text.contains("audio.metadata") {
                    if let Some(index) = text.find("\r\n\r\n") {
                        let metadata = AudioMetadata::from_str(&text[index + 4..])?;
                        Ok(Some(Payload::AudioMetadata(metadata)))
                    } else {
                        Ok(None)
                    }
                } else if text.contains("turn.start") {
                    *turn_start = true;
                    Ok(None)
                } else if text.contains("response") {
                    *response = true;
                    Ok(None)
                } else if text.contains("turn.end") {
                    *turn_end = true;
                    Ok(None)
                } else {
                    Err(Error::UnexpectedMessage(format!(
                        "unexpected text message: {}",
                        text
                    )))
                }
            }
            tungstenite::Message::Binary(bytes) => {
                if *turn_start || *response {
                    if bytes.len() < 2 {
                        return Err(Error::UnexpectedMessage("short audio frame".to_owned()));
                    }
                    let header_len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                    if header_len + 2 > bytes.len() {
                        return Err(Error::UnexpectedMessage(
                            "invalid audio frame header".to_owned(),
                        ));
                    }
                    Ok(Some(Payload::AudioBytes((bytes, header_len + 2))))
                } else {
                    Ok(None)
                }
            }
            tungstenite::Message::Close(_) => Err(Error::UnexpectedMessage(
                "connection closed before turn.end".to_owned(),
            )),
            _ => Err(Error::UnexpectedMessage(format!(
                "unexpected message: {}",
                message
            ))),
        }
    }
}

fn build_config_message(config: &SpeechConfig) -> tungstenite::Message {
    static SPEECH_CONFIG_HEAD: &str = r#"{"context":{"synthesis":{"audio":{"metadataoptions":{"sentenceBoundaryEnabled":"false","wordBoundaryEnabled":"true"},"outputFormat":""#;
    static SPEECH_CONFIG_TAIL: &str = r#""}}}}"#;
    let speech_config_message = format!(
        "X-Timestamp:{}\r\nContent-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n{}{}{}",
        chrono::Local::now().to_rfc2822(),
        SPEECH_CONFIG_HEAD,
        config.audio_format,
        SPEECH_CONFIG_TAIL
    );
    tungstenite::Message::Text(speech_config_message.into())
}

fn build_ssml_message(text: &str, config: &SpeechConfig) -> tungstenite::Message {
    let ssml = format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'><voice name='{}'><prosody pitch='{:+}Hz' rate='{:+}%' volume='{:+}%'>{}</prosody></voice></speak>",
        config.voice_name, config.pitch, config.rate, config.volume, text,
    );
    let ssml_message = format!(
        "X-RequestId:{}\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:{}\r\nPath:ssml\r\n\r\n{}",
        uuid::Uuid::new_v4().simple(),
        chrono::Local::now().to_rfc2822(),
        ssml,
    );
    tungstenite::Message::Text(ssml_message.into())
}

// Sec-MS-GEC v2 token is generated by the `drm` module. Microsoft made v2
// mandatory in October 2026.
// Solution source:
//   https://github.com/rany2/edge-tts/issues/490
//   https://github.com/rany2/edge-tts/pull/492
fn build_websocket_request() -> tungstenite::Result<tungstenite::handshake::client::Request> {
    use crate::constants;
    use tungstenite::client::IntoClientRequest;
    use tungstenite::http::header;

    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let sec_ms_gec = drm::generate_sec_ms_gec();
    let sec_ms_gec_version = constants::SEC_MS_GEC_VERSION;
    let mut request = format!(
        "{}{}&Sec-MS-GEC={}&Sec-MS-GEC-Version={}",
        constants::WSS_URL,
        uuid,
        sec_ms_gec,
        sec_ms_gec_version
    )
    .into_client_request()?;
    let headers = request.headers_mut();
    headers.insert(header::PRAGMA, "no-cache".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    headers.insert(header::USER_AGENT, constants::USER_AGENT.parse().unwrap());
    headers.insert(header::ORIGIN, constants::ORIGIN.parse().unwrap());
    Ok(request)
}

async fn websocket_connect_tokio_async() -> Result<
    async_tungstenite::WebSocketStream<
        async_tungstenite::tokio::ClientStream<tokio::net::TcpStream>,
    >,
> {
    let request = build_websocket_request()?;
    let (websocket, _) = async_tungstenite::tokio::connect_async(request).await?;
    Ok(websocket)
}

#[cfg(test)]
mod protocol_tests {
    use super::Payload;
    use tungstenite::Message;

    #[test]
    fn close_does_not_complete_a_partial_turn() {
        let (mut started, mut response, mut ended) = (true, true, false);
        let result = Payload::process(
            Message::Close(None),
            &mut started,
            &mut response,
            &mut ended,
        );
        assert!(result.is_err());
        assert!(!ended);
    }

    #[test]
    fn rejects_short_and_invalid_audio_headers() {
        for bytes in [vec![1], vec![0, 5, 0]] {
            let (mut started, mut response, mut ended) = (true, true, false);
            let result = Payload::process(
                Message::Binary(bytes.into()),
                &mut started,
                &mut response,
                &mut ended,
            );
            assert!(result.is_err());
        }
    }
}
