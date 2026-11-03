use crate::{Book, Chapter, Error, Preferences, Result, split_passages};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub const AUDIO_BLOCK_MAX_ESCAPED_BYTES: usize = 3_000;
pub const AUDIO_BLOCK_MAX_PASSAGES: usize = 6;
const TRANSIENT_LIMIT: u64 = 1024 * 1024 * 1024;
const MAX_AUDIO: usize = 32 * 1024 * 1024;
const MAX_AUDIO_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_AUDIO_TIMINGS: usize = 4096;
/// Application safety limit for a lightweight offline manifest.
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    Mp3,
    Pcm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioTransport {
    pub format: AudioFormat,
    pub pcm_sample_rate: Option<u32>,
    pub pcm_channels: Option<u16>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputUnit {
    Character,
    Utf8Byte,
    XmlEscapedByte,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    User,
    Provider,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InputLimit {
    pub unit: InputUnit,
    pub value: usize,
    pub source: LimitSource,
    pub verified_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SynthesisSettings {
    pub resource_revision: Option<String>,
    pub transport: Option<AudioTransport>,
    pub input_limit: Option<InputLimit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioProfile {
    pub engine: String,
    pub voice: String,
    pub model: Option<String>,
    pub model_voice: Option<String>,
    pub dictionary: BTreeMap<String, String>,
    #[serde(default)]
    pub max_escaped_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<SynthesisSettings>,
}

impl AudioProfile {
    #[must_use]
    pub fn escaped_byte_limit(&self) -> usize {
        self.max_escaped_bytes
            .unwrap_or(AUDIO_BLOCK_MAX_ESCAPED_BYTES)
            .clamp(6, AUDIO_BLOCK_MAX_ESCAPED_BYTES)
    }

    #[must_use]
    pub fn from_preferences(prefs: &Preferences) -> Self {
        let voice = prefs
            .voice
            .as_deref()
            .filter(|voice| !voice.is_empty())
            .unwrap_or("ru-RU-DmitryNeural")
            .to_owned();
        let engine = if voice == "openrouter" {
            "openrouter"
        } else if voice.starts_with("piper:") {
            "piper"
        } else {
            "edge"
        }
        .to_owned();
        let (model, model_voice) = if engine == "openrouter" {
            (
                prefs.open_router_model.clone(),
                prefs
                    .open_router_voice
                    .clone()
                    .filter(|voice| !voice.trim().is_empty()),
            )
        } else {
            (None, None)
        };
        let synthesis = model
            .as_ref()
            .and_then(|id| prefs.open_router_model_settings.get(id).cloned());
        Self {
            engine,
            voice,
            model,
            model_voice,
            dictionary: prefs.dictionary.clone(),
            max_escaped_bytes: None,
            synthesis,
        }
    }

    #[must_use]
    pub fn id(&self) -> String {
        let serialized = serde_json::to_vec(self).unwrap_or_default();
        hash_parts(&[b"readio-audio-profile-v1", &serialized])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioPart {
    pub index: usize,
    pub start: usize,
    pub end: usize,
    pub weight: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis_start: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis_end: Option<usize>,
}

fn prepared_part(
    index: usize,
    start: usize,
    end: usize,
    current: &str,
    texts: &[String],
) -> AudioPart {
    let synthesis_start = texts
        .iter()
        .map(|text| text.encode_utf16().count())
        .sum::<usize>()
        + texts.len() * 2;
    AudioPart {
        index,
        start,
        end,
        weight: weight(current),
        synthesis_start: Some(synthesis_start),
        synthesis_end: Some(synthesis_start + current.encode_utf16().count()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioBlock {
    pub id: String,
    pub chapter: usize,
    pub start: usize,
    pub end: usize,
    pub passages: Vec<AudioPart>,
    pub text: String,
    pub characters: usize,
}

#[derive(Clone)]
struct Unit {
    ch: char,
    start: usize,
    end: usize,
}
fn word_char(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ('А'..='я').contains(&ch) || ch == 'Ё' || ch == 'ё'
}
const fn escaped_size(ch: char) -> usize {
    match ch {
        '&' => 5,
        '<' | '>' => 4,
        '"' | '\'' => 6,
        _ => ch.len_utf8(),
    }
}
fn weight(text: &str) -> usize {
    text.encode_utf16().count()
        + text
            .chars()
            .map(|c| match c {
                '.' | '!' | '?' | '…' | ':' | ';' => 14,
                ',' => 6,
                _ => 0,
            })
            .sum::<usize>()
}

fn prepare_units(text: &str, offset: usize, rules: &[(Regex, String)]) -> Vec<Unit> {
    let mut position = offset;
    let mut units: Vec<Unit> = text
        .chars()
        .map(|ch| {
            let start = position;
            position += ch.len_utf16();
            Unit {
                ch,
                start,
                end: position,
            }
        })
        .collect();
    for (pattern, replacement) in rules {
        let current: String = units.iter().map(|unit| unit.ch).collect();
        let mut boundaries = BTreeMap::new();
        for (index, (byte, _)) in current.char_indices().enumerate() {
            boundaries.insert(byte, index);
        }
        boundaries.insert(current.len(), units.len());
        let mut next = Vec::new();
        let mut cursor = 0;
        for matched in pattern.find_iter(&current) {
            let (Some(&start), Some(&end)) = (
                boundaries.get(&matched.start()),
                boundaries.get(&matched.end()),
            ) else {
                continue;
            };
            if start > 0 && word_char(units[start - 1].ch)
                || end < units.len() && word_char(units[end].ch)
            {
                continue;
            }
            next.extend_from_slice(&units[cursor..start]);
            let source_start = units[start].start;
            let source_end = units[end - 1].end;
            next.extend(replacement.chars().map(|ch| Unit {
                ch,
                start: source_start,
                end: source_end,
            }));
            cursor = end;
        }
        next.extend_from_slice(&units[cursor..]);
        units = next;
    }
    units
}

const MAX_DICTIONARY_ENTRIES: usize = 128;
const MAX_PREPARED_UNITS: usize = 250_000;
const MAX_PLANNING_SOURCE_BYTES: usize = 8 * 1024 * 1024;

/// Validates the bounded pronunciation dictionary before storing or compiling it.
///
/// # Errors
/// Returns an error for oversized or malformed entries.
pub fn validate_dictionary(dictionary: &BTreeMap<String, String>) -> Result<()> {
    if dictionary.len() > MAX_DICTIONARY_ENTRIES {
        return Err(Error::Format(
            "Слишком много записей словаря произношения".into(),
        ));
    }
    for (key, replacement) in dictionary {
        if key.trim().is_empty()
            || key.len() > 128
            || replacement.len() > 1024
            || key.chars().any(char::is_control)
            || replacement.chars().any(char::is_control)
        {
            return Err(Error::Format(
                "Некорректная запись словаря произношения".into(),
            ));
        }
        RegexBuilder::new(&regex::escape(key.trim()))
            .case_insensitive(true)
            .build()
            .map_err(|_| Error::Format("Не удалось обработать слово словаря".into()))?;
    }
    Ok(())
}

fn unicode_word_char(ch: char) -> bool {
    static WORD: std::sync::OnceLock<Option<Regex>> = std::sync::OnceLock::new();
    if ch.is_alphanumeric() || ch == '_' {
        return true;
    }
    if ch.is_whitespace() || ch.is_ascii() {
        return false;
    }
    WORD.get_or_init(|| Regex::new(r"^\w$").ok())
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(ch.encode_utf8(&mut [0; 4])))
}

fn prepare_units_checked(
    text: &str,
    offset: usize,
    rules: &[(Regex, String)],
    legacy: bool,
) -> Result<Vec<Unit>> {
    if text.chars().count() > MAX_PREPARED_UNITS {
        return Err(Error::Format(
            "Фрагмент текста слишком велик для подготовки озвучивания".into(),
        ));
    }
    let mut position = offset;
    let mut units: Vec<Unit> = text
        .chars()
        .map(|ch| {
            let start = position;
            position += ch.len_utf16();
            Unit {
                ch,
                start,
                end: position,
            }
        })
        .collect();
    for (pattern, replacement) in rules {
        let current: String = units.iter().map(|unit| unit.ch).collect();
        let mut boundaries = BTreeMap::new();
        for (index, (byte, _)) in current.char_indices().enumerate() {
            boundaries.insert(byte, index);
        }
        boundaries.insert(current.len(), units.len());
        let mut next = Vec::new();
        let mut cursor = 0;
        for matched in pattern.find_iter(&current) {
            let (Some(&start), Some(&end)) = (
                boundaries.get(&matched.start()),
                boundaries.get(&matched.end()),
            ) else {
                continue;
            };
            let is_word = if legacy { word_char } else { unicode_word_char };
            if start > 0 && is_word(units[start - 1].ch)
                || end < units.len() && is_word(units[end].ch)
            {
                continue;
            }
            let count = replacement.chars().count();
            if next
                .len()
                .saturating_add(start - cursor)
                .saturating_add(count)
                > MAX_PREPARED_UNITS
            {
                return Err(Error::Format(
                    "Замены словаря слишком сильно увеличивают текст".into(),
                ));
            }
            next.extend_from_slice(&units[cursor..start]);
            let (source_start, source_end) = (units[start].start, units[end - 1].end);
            next.extend(replacement.chars().map(|ch| Unit {
                ch,
                start: source_start,
                end: source_end,
            }));
            cursor = end;
        }
        if next.len().saturating_add(units.len() - cursor) > MAX_PREPARED_UNITS {
            return Err(Error::Format(
                "Замены словаря слишком сильно увеличивают текст".into(),
            ));
        }
        next.extend_from_slice(&units[cursor..]);
        units = next;
    }
    Ok(units)
}

fn hash_parts(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    format!("{:x}", hash.finalize())
}

pub fn canonical_block_id(text: &str, profile: &AudioProfile) -> String {
    if let Some(settings) = &profile.synthesis
        && (settings.resource_revision.is_some() || settings.transport.is_some())
    {
        let identity = serde_json::to_vec(&(&settings.resource_revision, &settings.transport))
            .unwrap_or_default();
        return hash_parts(&[
            b"readio-audio-block-v2",
            profile.engine.as_bytes(),
            profile.voice.as_bytes(),
            profile.model.as_deref().unwrap_or("").as_bytes(),
            profile.model_voice.as_deref().unwrap_or("").as_bytes(),
            &identity,
            text.as_bytes(),
        ]);
    }
    hash_parts(&[
        b"readio-audio-block-v1",
        profile.engine.as_bytes(),
        profile.voice.as_bytes(),
        profile.model.as_deref().unwrap_or("").as_bytes(),
        profile.model_voice.as_deref().unwrap_or("").as_bytes(),
        text.as_bytes(),
    ])
}

fn finish_block(
    result: &mut Vec<AudioBlock>,
    chapter: usize,
    profile: &AudioProfile,
    parts: &mut Vec<AudioPart>,
    texts: &mut Vec<String>,
    legacy: bool,
) {
    if parts.is_empty() {
        return;
    }
    let text = texts.join("\n\n");
    let Some(first) = parts.first() else {
        return;
    };
    let Some(last) = parts.last() else {
        return;
    };
    let id = if legacy {
        hash_parts(&[
            b"readio-audio-block-v1",
            profile.engine.as_bytes(),
            profile.voice.as_bytes(),
            profile.model.as_deref().unwrap_or("").as_bytes(),
            profile.model_voice.as_deref().unwrap_or("").as_bytes(),
            text.as_bytes(),
        ])
    } else {
        canonical_block_id(&text, profile)
    };
    result.push(AudioBlock {
        id,
        chapter,
        start: first.start,
        end: last.end,
        characters: text.chars().count(),
        text,
        passages: std::mem::take(parts),
    });
    texts.clear();
}

#[derive(Debug, Clone)]
pub struct ChapterPlanner {
    profile: AudioProfile,
    rules: Vec<(Regex, String)>,
}

impl ChapterPlanner {
    /// Compiles bounded pronunciation rules and validates model constraints.
    ///
    /// # Errors
    /// Returns an error for an invalid dictionary or provider limit.
    pub fn try_new(profile: &AudioProfile) -> Result<Self> {
        validate_dictionary(&profile.dictionary)?;
        if profile
            .synthesis
            .as_ref()
            .and_then(|settings| settings.input_limit.as_ref())
            .is_some_and(|limit| limit.value == 0)
        {
            return Err(Error::Format("Лимит модели должен быть больше нуля".into()));
        }
        Ok(Self::new(profile))
    }
    #[must_use]
    pub fn new(profile: &AudioProfile) -> Self {
        let rules = profile
            .dictionary
            .iter()
            .filter_map(|(word, replacement)| {
                let key = word.trim();
                if key.is_empty() {
                    return None;
                }
                RegexBuilder::new(&regex::escape(key))
                    .case_insensitive(true)
                    .build()
                    .ok()
                    .map(|pattern| (pattern, replacement.clone()))
            })
            .collect();
        Self {
            profile: profile.clone(),
            rules,
        }
    }

    #[must_use]
    pub fn plan_chapter(&self, chapter: usize, source: &Chapter) -> Vec<AudioBlock> {
        self.plan_text(chapter, &source.text)
    }

    /// Plans a chapter with bounded expansion and explicit input-limit checks.
    ///
    /// # Errors
    /// Returns an error when source or expanded text exceeds application limits.
    pub fn try_plan_chapter(&self, chapter: usize, source: &Chapter) -> Result<Vec<AudioBlock>> {
        self.try_plan_text(chapter, &source.text)
    }

    /// Bounds legacy pronunciation expansion before invoking the frozen v1 planner.
    ///
    /// # Errors
    /// Returns an error when a chapter or dictionary expansion exceeds the application budget.
    pub fn validate_legacy_source(&self, text: &str) -> Result<()> {
        if text.len() > MAX_PLANNING_SOURCE_BYTES {
            return Err(Error::Format(
                "Глава превышает допустимый размер планирования".into(),
            ));
        }
        if self.rules.is_empty() {
            return Ok(());
        }
        for passage in split_passages(text) {
            prepare_units_checked(&passage.text, passage.start, &self.rules, true)?;
        }
        Ok(())
    }

    /// Plans prepared text without exceeding configured provider or application limits.
    ///
    /// # Errors
    /// Returns an error when source, dictionary expansion, or one replacement cannot fit.
    #[allow(clippy::too_many_lines)] // Keep sizing and source mapping in one bounded pass.
    pub fn try_plan_text(&self, chapter: usize, text: &str) -> Result<Vec<AudioBlock>> {
        if text.len() > MAX_PLANNING_SOURCE_BYTES {
            return Err(Error::Format(
                "Глава превышает допустимый размер планирования".into(),
            ));
        }
        let provider = self
            .profile
            .synthesis
            .as_ref()
            .and_then(|settings| settings.input_limit.as_ref());
        let mut blocks = Vec::new();
        let mut parts = Vec::new();
        let mut texts = Vec::new();
        let mut app_size = 0_usize;
        let mut provider_size = 0_usize;
        for (index, passage) in split_passages(text).iter().enumerate() {
            let mut current = String::new();
            let mut start = 0;
            let mut end = 0;
            let mut previous_span = None;
            let mut remaining = passage.text.as_str();
            let mut source_offset = passage.start;
            while !remaining.is_empty() {
                let chunk_units = if self.rules.is_empty() {
                    MAX_PREPARED_UNITS
                } else {
                    MAX_PREPARED_UNITS / 8
                };
                let mut end_byte = remaining
                    .char_indices()
                    .nth(chunk_units)
                    .map_or(remaining.len(), |(byte, _)| byte);
                if end_byte < remaining.len() && !self.rules.is_empty() {
                    if self
                        .profile
                        .dictionary
                        .keys()
                        .any(|key| key.chars().any(char::is_whitespace))
                    {
                        return Err(Error::Format(
                            "Слишком длинный фрагмент для словаря с составными словами".into(),
                        ));
                    }
                    end_byte = remaining[..end_byte]
                        .char_indices()
                        .rev()
                        .find(|(_, ch)| ch.is_whitespace())
                        .map(|(byte, ch)| byte + ch.len_utf8())
                        .ok_or_else(|| {
                            Error::Format("Слишком длинное слово для обработки словаря".into())
                        })?;
                }
                let chunk = &remaining[..end_byte];
                let units = prepare_units_checked(chunk, source_offset, &self.rules, false)?;
                for (at, unit) in units.iter().enumerate() {
                    if unicode_word_char(unit.ch)
                        && (at == 0 || !unicode_word_char(units[at - 1].ch))
                        && app_size > 0
                    {
                        let word = units[at..]
                            .iter()
                            .take_while(|item| unicode_word_char(item.ch));
                        let (word_app, word_provider) =
                            word.fold((0_usize, 0_usize), |(app, model), item| {
                                let escaped = escaped_size(item.ch);
                                let size = provider.map_or(0, |limit| match limit.unit {
                                    InputUnit::Character => 1,
                                    InputUnit::Utf8Byte => item.ch.len_utf8(),
                                    InputUnit::XmlEscapedByte => escaped,
                                });
                                (app.saturating_add(escaped), model.saturating_add(size))
                            });
                        let separator = usize::from(current.is_empty() && !parts.is_empty()) * 2;
                        let fits_empty = word_app <= self.profile.escaped_byte_limit()
                            && provider.is_none_or(|limit| word_provider <= limit.value);
                        let exceeds = app_size.saturating_add(separator).saturating_add(word_app)
                            > self.profile.escaped_byte_limit()
                            || provider.is_some_and(|limit| {
                                provider_size
                                    .saturating_add(separator)
                                    .saturating_add(word_provider)
                                    > limit.value
                            });
                        if fits_empty && exceeds {
                            if previous_span == Some((unit.start, unit.end)) {
                                return Err(Error::Format(
                                    "Замена словаря не помещается в один аудиоблок".into(),
                                ));
                            }
                            if !current.is_empty() {
                                parts.push(prepared_part(index, start, end, &current, &texts));
                                texts.push(std::mem::take(&mut current));
                            }
                            finish_block(
                                &mut blocks,
                                chapter,
                                &self.profile,
                                &mut parts,
                                &mut texts,
                                false,
                            );
                            app_size = 0;
                            provider_size = 0;
                        }
                    }
                    let separator = usize::from(current.is_empty() && !parts.is_empty()) * 2;
                    let size = escaped_size(unit.ch);
                    let provider_char = provider.map_or(0, |limit| match limit.unit {
                        InputUnit::Character => 1,
                        InputUnit::Utf8Byte => unit.ch.len_utf8(),
                        InputUnit::XmlEscapedByte => size,
                    });
                    if provider.is_some_and(|limit| provider_char > limit.value) {
                        return Err(Error::Format(
                            "Символ не помещается в заданный лимит модели".into(),
                        ));
                    }
                    let over_provider = provider.is_some_and(|limit| {
                        provider_size
                            .saturating_add(separator)
                            .saturating_add(provider_char)
                            > limit.value
                    });
                    if app_size.saturating_add(separator).saturating_add(size)
                        > self.profile.escaped_byte_limit()
                        || over_provider
                        || current.is_empty() && parts.len() >= AUDIO_BLOCK_MAX_PASSAGES
                    {
                        if previous_span == Some((unit.start, unit.end)) {
                            return Err(Error::Format(
                                "Замена словаря не помещается в один аудиоблок".into(),
                            ));
                        }
                        if !current.is_empty() {
                            parts.push(prepared_part(index, start, end, &current, &texts));
                            texts.push(std::mem::take(&mut current));
                        }
                        finish_block(
                            &mut blocks,
                            chapter,
                            &self.profile,
                            &mut parts,
                            &mut texts,
                            false,
                        );
                        app_size = 0;
                        provider_size = 0;
                    }
                    if current.is_empty() {
                        if !parts.is_empty() {
                            app_size += 2;
                            provider_size += 2;
                        }
                        start = unit.start;
                    }
                    app_size += size;
                    provider_size += provider_char;
                    end = unit.end;
                    previous_span = Some((unit.start, unit.end));
                    current.push(unit.ch);
                }
                source_offset += chunk.encode_utf16().count();
                remaining = &remaining[end_byte..];
            }
            if !current.is_empty() {
                parts.push(prepared_part(index, start, end, &current, &texts));
                texts.push(current);
            }
        }
        finish_block(
            &mut blocks,
            chapter,
            &self.profile,
            &mut parts,
            &mut texts,
            false,
        );
        blocks.retain(|block| !block.text.trim().is_empty());
        Ok(blocks)
    }

    #[must_use]
    pub fn plan_book(&self, book: &Book) -> Vec<AudioBlock> {
        book.chapters
            .iter()
            .enumerate()
            .flat_map(|(chapter, source)| self.plan_chapter(chapter, source))
            .collect()
    }

    #[must_use]
    pub fn plan_text(&self, chapter: usize, text: &str) -> Vec<AudioBlock> {
        self.plan_impl(chapter, text, false)
    }

    // Keep this algorithm frozen for editions created before planner versioning.
    fn plan_v1_text(&self, chapter: usize, text: &str) -> Vec<AudioBlock> {
        self.plan_impl(chapter, text, true)
    }

    fn plan_impl(&self, chapter: usize, text: &str, legacy: bool) -> Vec<AudioBlock> {
        let limit = self.profile.escaped_byte_limit();
        let mut blocks = Vec::new();
        let mut parts = Vec::new();
        let mut texts = Vec::new();
        let mut bytes = 0;
        for (index, passage) in split_passages(text).iter().enumerate() {
            let units = prepare_units(&passage.text, passage.start, &self.rules);
            let mut current = String::new();
            let mut start = 0;
            let mut end = 0;
            for unit in units {
                let separator = if current.is_empty() && !parts.is_empty() {
                    2
                } else {
                    0
                };
                let size = escaped_size(unit.ch);
                if bytes + separator + size > limit
                    || current.is_empty() && parts.len() >= AUDIO_BLOCK_MAX_PASSAGES
                {
                    if !current.is_empty() {
                        parts.push(AudioPart {
                            index,
                            start,
                            end,
                            weight: weight(&current),
                            synthesis_start: None,
                            synthesis_end: None,
                        });
                        texts.push(std::mem::take(&mut current));
                    }
                    finish_block(
                        &mut blocks,
                        chapter,
                        &self.profile,
                        &mut parts,
                        &mut texts,
                        legacy,
                    );
                    bytes = 0;
                }
                if current.is_empty() {
                    if !parts.is_empty() {
                        bytes += 2;
                    }
                    start = unit.start;
                }
                bytes += size;
                end = unit.end;
                current.push(unit.ch);
            }
            if !current.is_empty() {
                parts.push(AudioPart {
                    index,
                    start,
                    end,
                    weight: weight(&current),
                    synthesis_start: None,
                    synthesis_end: None,
                });
                texts.push(current);
            }
        }
        finish_block(
            &mut blocks,
            chapter,
            &self.profile,
            &mut parts,
            &mut texts,
            legacy,
        );
        blocks.retain(|block| block.text.chars().any(char::is_alphanumeric));
        blocks
    }
}

/// The v1 planner preserves the IDs and offsets used by legacy offline editions.
#[must_use]
pub fn plan_chapter_v1(book: &Book, chapter: usize, profile: &AudioProfile) -> Vec<AudioBlock> {
    book.chapters.get(chapter).map_or_else(Vec::new, |source| {
        ChapterPlanner::new(profile).plan_v1_text(chapter, &source.text)
    })
}

#[must_use]
pub fn plan_chapter(book: &Book, chapter: usize, profile: &AudioProfile) -> Vec<AudioBlock> {
    book.chapters.get(chapter).map_or_else(Vec::new, |source| {
        ChapterPlanner::new(profile).plan_chapter(chapter, source)
    })
}

#[must_use]
pub fn plan_book(book: &Book, profile: &AudioProfile) -> Vec<AudioBlock> {
    ChapterPlanner::new(profile).plan_book(book)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineManifest {
    pub book_id: String,
    pub profile_id: String,
    pub profile: AudioProfile,
    pub block_ids: Vec<String>,
    #[serde(default)]
    pub plan_digest: Option<String>,
    #[serde(default)]
    pub updated_at: u64,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestIssue {
    pub file: String,
    pub book_id: Option<String>,
    pub profile_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestReport {
    pub manifests: Vec<OfflineManifest>,
    pub issues: Vec<ManifestIssue>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedAudio {
    pub path: PathBuf,
    pub mime: String,
    pub size: u64,
}
#[derive(Debug, Clone)]
pub struct CachedAudioBytes {
    pub data: Vec<u8>,
    pub mime: String,
    pub timings: Vec<AudioTiming>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioTiming {
    pub start_utf16: usize,
    pub end_utf16: usize,
    pub start_seconds: f64,
    pub end_seconds: f64,
}
#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    mime: String,
    size: u64,
    digest: String,
    accessed: u64,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_timings"
    )]
    timings: Vec<AudioTiming>,
}

fn deserialize_timings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<AudioTiming>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

fn valid_timings(timings: &[AudioTiming]) -> bool {
    if timings.len() > MAX_AUDIO_TIMINGS {
        return false;
    }
    let mut previous_end = 0;
    let mut previous_time = 0.0;
    for timing in timings {
        if timing.start_utf16 < previous_end
            || timing.end_utf16 <= timing.start_utf16
            || !timing.start_seconds.is_finite()
            || !timing.end_seconds.is_finite()
            || timing.start_seconds < previous_time
            || timing.end_seconds < timing.start_seconds
        {
            return false;
        }
        previous_end = timing.end_utf16;
        previous_time = timing.start_seconds;
    }
    true
}
#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    accessed: u64,
    size: u64,
}
#[derive(Debug)]
struct CacheIndex {
    entries: HashMap<String, CacheEntry>,
    manifest_pins: HashSet<String>,
    eviction_blocked: bool,
    transient: u64,
    manifests_modified: Option<SystemTime>,
    blocks_modified: Option<SystemTime>,
}

#[derive(Debug)]
pub struct AudioCache {
    root: PathBuf,
    transient_limit: u64,
    index: Mutex<Option<CacheIndex>>,
    active: Mutex<HashMap<String, usize>>,
    lock: Mutex<()>,
}

fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn validate_id(id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(Error::Format("Некорректный идентификатор аудио".into()))
    }
}
fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let outcome = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        sync_parent_directory(path)?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    outcome
}
#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Format("Некорректный путь кеша".into()))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > limit as u64 {
        return Err(Error::Format(
            "Файл превышает допустимый размер приложения".into(),
        ));
    }
    let mut data = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(Error::Format(
            "Файл превышает допустимый размер приложения".into(),
        ));
    }
    Ok(data)
}

fn stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|info| info.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= std::time::Duration::from_hours(24))
}

fn directory_stamp(path: &Path) -> Result<Option<SystemTime>> {
    Ok(fs::metadata(path)?.modified().ok())
}

impl AudioCache {
    /// Creates the persistent cache under the application data directory.
    ///
    /// # Errors
    /// Returns an error if the cache directories cannot be created.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        let root = data_dir.join("audio-cache");
        fs::create_dir_all(root.join("blocks"))?;
        fs::create_dir_all(root.join("manifests"))?;
        fs::create_dir_all(root.join("editions"))?;
        Ok(Self {
            root,
            transient_limit: TRANSIENT_LIMIT,
            index: Mutex::new(None),
            active: Mutex::new(HashMap::new()),
            lock: Mutex::new(()),
        })
    }
    fn paths(&self, id: &str) -> (PathBuf, PathBuf) {
        let root = self.root.join("blocks");
        (
            root.join(format!("{id}.audio")),
            root.join(format!("{id}.json")),
        )
    }
    fn manifest_path(&self, book_id: &str, profile_id: &str) -> PathBuf {
        self.root.join("manifests").join(format!(
            "{}.json",
            hash_parts(&[book_id.as_bytes(), profile_id.as_bytes()])
        ))
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reads and verifies an audio block.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers or filesystem errors.
    pub fn get(&self, id: &str) -> Result<Option<CachedAudio>> {
        validate_id(id)?;
        let _guard = self.lock();
        let Some((audio, metadata, _)) = self.verified_locked(id)? else {
            return Ok(None);
        };
        self.touch_locked(id, &metadata);
        Ok(Some(CachedAudio {
            path: audio,
            mime: metadata.mime,
            size: metadata.size,
        }))
    }

    /// Reads and verifies cached audio in one pass without requiring a metadata write.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers or audio read failures.
    pub fn get_bytes(&self, id: &str) -> Result<Option<CachedAudioBytes>> {
        validate_id(id)?;
        let _guard = self.lock();
        let Some((_, metadata, data)) = self.verified_locked(id)? else {
            return Ok(None);
        };
        self.touch_locked(id, &metadata);
        Ok(Some(CachedAudioBytes {
            data,
            mime: metadata.mime,
            timings: metadata.timings,
        }))
    }

    fn touch_locked(&self, id: &str, metadata: &Metadata) {
        const TOUCH_INTERVAL_MS: u64 = 60_000;
        let now = timestamp();
        let updated = Metadata {
            mime: metadata.mime.clone(),
            size: metadata.size,
            digest: metadata.digest.clone(),
            accessed: now,
            timings: metadata.timings.clone(),
        };
        let (_, metadata_path) = self.paths(id);
        let blocks_dir = self.root.join("blocks");
        let mut stamps = None;
        if now.saturating_sub(metadata.accessed) >= TOUCH_INTERVAL_MS {
            let before = directory_stamp(&blocks_dir).ok().flatten();
            if let Ok(data) = serde_json::to_vec(&updated)
                && atomic_write(&metadata_path, &data).is_ok()
            {
                stamps = Some((before, directory_stamp(&blocks_dir).ok().flatten()));
            }
        }
        let mut guard = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = guard.as_mut() {
            if let Some((before, after)) = stamps {
                if before.is_some() && index.blocks_modified == before {
                    index.blocks_modified = after;
                } else {
                    *guard = None;
                    return;
                }
            }
            if let Some(entry) = index.entries.get_mut(id) {
                entry.accessed = now;
            }
        }
        drop(guard);
    }

    /// Verifies one cached block without changing its LRU timestamp.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers or filesystem failures.
    pub fn verify(&self, id: &str) -> Result<Option<u64>> {
        validate_id(id)?;
        let _guard = self.lock();
        Ok(self
            .verified_locked(id)?
            .map(|(_, metadata, _)| metadata.size))
    }

    fn verified_locked(&self, id: &str) -> Result<Option<(PathBuf, Metadata, Vec<u8>)>> {
        let (audio, metadata_path) = self.paths(id);
        match fs::metadata(&metadata_path) {
            Ok(info) if info.len() <= MAX_AUDIO_METADATA_BYTES => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let metadata = match fs::read(metadata_path) {
            Ok(data) => data,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Ok(mut metadata) = serde_json::from_slice::<Metadata>(&metadata) else {
            return Ok(None);
        };
        if metadata.size == 0
            || metadata.size > MAX_AUDIO as u64
            || !valid_id(&metadata.digest)
            || !valid_mime(&metadata.mime)
        {
            return Ok(None);
        }
        if !valid_timings(&metadata.timings) {
            metadata.timings.clear();
        }
        match fs::metadata(&audio) {
            Ok(info) if info.len() == metadata.size => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let data = match fs::read(&audio) {
            Ok(data) => data,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if u64::try_from(data.len()).ok() != Some(metadata.size)
            || hash_parts(&[&data]) != metadata.digest
        {
            return Ok(None);
        }
        Ok(Some((audio, metadata, data)))
    }

    /// Returns cached audio sizes without reading audio data or updating LRU.
    ///
    /// This fast check validates metadata and file length; `get` additionally verifies the content digest.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers or filesystem failures.
    pub fn stats(&self, ids: &[String]) -> Result<Vec<Option<u64>>> {
        let _guard = self.lock();
        let mut sizes = Vec::with_capacity(ids.len());
        for id in ids {
            validate_id(id)?;
            let (audio, meta) = self.paths(id);
            let meta_info = match fs::metadata(&meta) {
                Ok(info) if info.len() <= MAX_AUDIO_METADATA_BYTES => info,
                Ok(_) => {
                    sizes.push(None);
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    sizes.push(None);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let _ = meta_info;
            let Ok(metadata) = serde_json::from_slice::<Metadata>(&fs::read(meta)?) else {
                sizes.push(None);
                continue;
            };
            if metadata.size == 0
                || metadata.size > MAX_AUDIO as u64
                || !valid_id(&metadata.digest)
                || !valid_mime(&metadata.mime)
            {
                sizes.push(None);
                continue;
            }
            let size = match fs::metadata(audio) {
                Ok(info) if info.len() == metadata.size => Some(metadata.size),
                Ok(_) => None,
                Err(error) if error.kind() == ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            sizes.push(size);
        }
        Ok(sizes)
    }

    /// Persists a generated block; use a manifest or active pin before writing blocks to retain them.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers, audio, or filesystem failures.
    #[allow(clippy::significant_drop_tightening)]
    pub fn put(&self, id: &str, mime: &str, data: &[u8]) -> Result<CachedAudio> {
        self.put_with_timings(id, mime, data, &[])
    }

    /// Persists an audio block and validated word timings together.
    ///
    /// # Errors
    /// Returns an error for invalid audio, timings or filesystem failures.
    #[allow(clippy::significant_drop_tightening)]
    pub fn put_with_timings(
        &self,
        id: &str,
        mime: &str,
        data: &[u8],
        timings: &[AudioTiming],
    ) -> Result<CachedAudio> {
        validate_id(id)?;
        if data.is_empty() || data.len() > MAX_AUDIO || !valid_mime(mime) || !valid_timings(timings)
        {
            return Err(Error::Format("Некорректное аудио для кеша".into()));
        }
        let _guard = self.lock();
        self.ensure_index_locked()?;
        let (path, meta) = self.paths(id);
        let metadata = Metadata {
            mime: mime.to_owned(),
            size: data.len() as u64,
            digest: hash_parts(&[data]),
            accessed: timestamp(),
            timings: timings.to_vec(),
        };
        let write_result = atomic_write(&path, data)
            .and_then(|()| atomic_write(&meta, &serde_json::to_vec(&metadata)?));
        if let Err(error) = write_result {
            *self
                .index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            return Err(error);
        }
        {
            let active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut guard = self
                .index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(index) = guard.as_mut() {
                index.blocks_modified = directory_stamp(&self.root.join("blocks"))?;
                let pinned = index.manifest_pins.contains(id) || active.contains_key(id);
                if let Some(old) = index.entries.insert(
                    id.to_owned(),
                    CacheEntry {
                        accessed: metadata.accessed,
                        size: metadata.size,
                    },
                ) && !pinned
                {
                    index.transient = index.transient.saturating_sub(old.size);
                }
                if !pinned {
                    index.transient = index.transient.saturating_add(metadata.size);
                }
            }
        }
        self.evict_locked()?;
        Ok(CachedAudio {
            path,
            mime: mime.into(),
            size: metadata.size,
        })
    }

    /// Stores an immutable content-addressed edition plan.
    ///
    /// # Errors
    /// Returns an error if the plan is oversized or the file cannot be written.
    pub fn save_edition_plan(&self, plan: &crate::EditionPlan) -> Result<String> {
        let (bytes, digest) = plan.encode()?;
        let _guard = self.lock();
        let path = self.root.join("editions").join(format!("{digest}.json"));
        match read_bounded(&path, crate::MAX_EDITION_PLAN_BYTES) {
            Ok(existing) if existing == bytes => return Ok(digest),
            Ok(_) => return Err(Error::Format("Повреждён файл плана издания".into())),
            Err(Error::Io(error)) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        atomic_write(&path, &bytes)?;
        Ok(digest)
    }

    /// Loads and verifies a content-addressed edition plan.
    ///
    /// # Errors
    /// Returns an error if the plan is missing, oversized, corrupt or has an unsupported version.
    pub fn edition_plan(&self, digest: &str) -> Result<crate::EditionPlan> {
        validate_id(digest)?;
        let _guard = self.lock();
        let bytes = read_bounded(
            &self.root.join("editions").join(format!("{digest}.json")),
            crate::MAX_EDITION_PLAN_BYTES,
        )?;
        crate::EditionPlan::decode(&bytes, digest)
    }

    fn parse_manifest(&self, path: &Path, bytes: &[u8]) -> Result<OfflineManifest> {
        let manifest: OfflineManifest = serde_json::from_slice(bytes)?;
        validate_manifest(&manifest)?;
        if path.file_name()
            != self
                .manifest_path(&manifest.book_id, &manifest.profile_id)
                .file_name()
        {
            return Err(Error::Format(
                "Имя файла манифеста не соответствует изданию".into(),
            ));
        }
        Ok(manifest)
    }

    fn scan_manifests_locked(&self) -> Result<ManifestReport> {
        let mut report = ManifestReport {
            manifests: Vec::new(),
            issues: Vec::new(),
        };
        for entry in fs::read_dir(self.root.join("manifests"))? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let file = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let bytes = read_bounded(&path, MAX_MANIFEST_BYTES);
            let parsed = bytes
                .as_ref()
                .map_err(std::string::ToString::to_string)
                .and_then(|bytes| {
                    self.parse_manifest(&path, bytes)
                        .map_err(|error| error.to_string())
                });
            match parsed {
                Ok(manifest) => report.manifests.push(manifest),
                Err(message) => {
                    let identity = bytes
                        .ok()
                        .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok());
                    let book_id = identity
                        .as_ref()
                        .and_then(|value| value.get("bookId"))
                        .and_then(|value| value.as_str())
                        .filter(|id| {
                            !id.is_empty() && id.len() <= 4096 && !id.chars().any(char::is_control)
                        })
                        .map(str::to_owned);
                    let profile_id = identity
                        .as_ref()
                        .and_then(|value| value.get("profileId"))
                        .and_then(|value| value.as_str())
                        .filter(|id| valid_id(id))
                        .map(str::to_owned);
                    report.issues.push(ManifestIssue {
                        file,
                        book_id,
                        profile_id,
                        message,
                    });
                }
            }
        }
        report.manifests.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.book_id.cmp(&right.book_id))
                .then_with(|| left.profile_id.cmp(&right.profile_id))
        });
        report.issues.sort_by(|a, b| a.file.cmp(&b.file));
        Ok(report)
    }

    /// Lists healthy manifests and diagnostics for damaged files.
    ///
    /// # Errors
    /// Returns an error if the manifest directory cannot be enumerated.
    pub fn scan_manifests(&self) -> Result<ManifestReport> {
        let _guard = self.lock();
        self.scan_manifests_locked()
    }

    /// Lists healthy persisted offline manifests, newest first.
    ///
    /// # Errors
    /// Returns an error if the manifest directory cannot be enumerated.
    pub fn list_manifests(&self) -> Result<Vec<OfflineManifest>> {
        Ok(self.scan_manifests()?.manifests)
    }

    /// Loads an offline manifest.
    ///
    /// # Errors
    /// Returns an error for a malformed manifest, invalid identifier or filesystem failure.
    pub fn manifest(&self, book_id: &str, profile_id: &str) -> Result<Option<OfflineManifest>> {
        validate_id(profile_id)?;
        let _guard = self.lock();
        let path = self.manifest_path(book_id, profile_id);
        let data = match read_bounded(&path, MAX_MANIFEST_BYTES) {
            Ok(data) => data,
            Err(Error::Io(error)) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(self.parse_manifest(&path, &data)?))
    }

    /// Atomically stores an offline manifest and pins every referenced block.
    ///
    /// # Errors
    /// Returns an error for an invalid manifest, damaged existing manifest or filesystem failure.
    pub fn save_manifest(&self, manifest: &OfflineManifest) -> Result<()> {
        validate_manifest(manifest)?;
        let _guard = self.lock();
        let path = self.manifest_path(&manifest.book_id, &manifest.profile_id);
        match read_bounded(&path, MAX_MANIFEST_BYTES) {
            Ok(bytes) => {
                self.parse_manifest(&path, &bytes)?;
            }
            Err(Error::Io(error)) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let mut persisted = manifest.clone();
        persisted.updated_at = timestamp();
        let bytes = serde_json::to_vec(&persisted)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Format(
                "Манифест превышает допустимый размер приложения".into(),
            ));
        }
        atomic_write(&path, &bytes)?;
        *self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }

    /// Restores a corrupt manifest from a matching validated backup.
    ///
    /// # Errors
    /// Returns an error for invalid paths, a healthy or absent target, invalid backup or filesystem failure.
    pub fn restore_manifest(&self, issue_file: &str, backup: &Path) -> Result<()> {
        let Some(id) = issue_file.strip_suffix(".json") else {
            return Err(Error::Format("Некорректное имя манифеста".into()));
        };
        validate_id(id)?;
        let _guard = self.lock();
        let target = self.root.join("manifests").join(issue_file);
        let issues = self.scan_manifests_locked()?.issues;
        if !issues.iter().any(|issue| issue.file == issue_file) {
            return Err(Error::Format("Повреждённый манифест не найден".into()));
        }
        let bytes = read_bounded(backup, MAX_MANIFEST_BYTES)?;
        self.parse_manifest(&target, &bytes)?;
        atomic_write(&target, &bytes)?;
        *self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }

    /// Removes an offline manifest and releases its pins.
    ///
    /// # Errors
    /// Returns an error for an invalid, damaged or inaccessible manifest.
    pub fn remove_manifest(&self, book_id: &str, profile_id: &str) -> Result<()> {
        validate_id(profile_id)?;
        let _guard = self.lock();
        let path = self.manifest_path(book_id, profile_id);
        let digest = match read_bounded(&path, MAX_MANIFEST_BYTES) {
            Ok(bytes) => {
                let manifest = self.parse_manifest(&path, &bytes)?;
                fs::remove_file(path)?;
                manifest.plan_digest
            }
            Err(Error::Io(error)) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        *self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.remove_unreferenced_plans_locked(digest.into_iter().collect())?;
        self.evict_locked()
    }

    /// Removes readable offline editions belonging to a book, preserving damaged files.
    ///
    /// # Errors
    /// Returns an error if manifests cannot be enumerated or removed.
    pub fn remove_book_manifests(&self, book_id: &str) -> Result<()> {
        let _guard = self.lock();
        let mut digests = HashSet::new();
        for manifest in self.scan_manifests_locked()?.manifests {
            if manifest.book_id == book_id {
                fs::remove_file(self.manifest_path(&manifest.book_id, &manifest.profile_id))?;
                if let Some(digest) = manifest.plan_digest {
                    digests.insert(digest);
                }
            }
        }
        *self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.remove_unreferenced_plans_locked(digests)?;
        self.evict_locked()
    }

    fn remove_unreferenced_plans_locked(&self, digests: HashSet<String>) -> Result<()> {
        if digests.is_empty() {
            return Ok(());
        }
        let report = self.scan_manifests_locked()?;
        if !report.issues.is_empty() {
            return Ok(());
        }
        let referenced: HashSet<_> = report
            .manifests
            .iter()
            .filter_map(|manifest| manifest.plan_digest.as_deref())
            .collect();
        for digest in digests {
            if referenced.contains(digest.as_str()) {
                continue;
            }
            let path = self.root.join("editions").join(format!("{digest}.json"));
            match fs::remove_file(&path) {
                Ok(()) => {
                    #[cfg(unix)]
                    sync_parent_directory(&path)?;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Pins an active playback block until it is released.
    ///
    /// # Errors
    /// Returns an error for an invalid identifier.
    pub fn pin_active(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let _guard = self.lock();
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = active.entry(id.to_owned()).or_default();
        if *count == 0 {
            let mut guard = self
                .index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(index) = guard.as_mut()
                && !index.manifest_pins.contains(id)
                && let Some(entry) = index.entries.get(id)
            {
                index.transient = index.transient.saturating_sub(entry.size);
            }
        }
        *count = count.saturating_add(1);
        drop(active);
        Ok(())
    }
    /// Releases an active playback pin.
    #[allow(clippy::significant_drop_tightening)]
    pub fn unpin_active(&self, id: &str) {
        let _guard = self.lock();
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = active.get_mut(id) {
            *count -= 1;
            if *count == 0 {
                active.remove(id);
                let mut guard = self
                    .index
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(index) = guard.as_mut()
                    && !index.manifest_pins.contains(id)
                    && let Some(entry) = index.entries.get(id)
                {
                    index.transient = index.transient.saturating_add(entry.size);
                }
            }
        }
    }
    /// Evicts least-recently-used transient audio over the cache limit.
    ///
    /// # Errors
    /// Returns an error if directory scanning or file deletion fails.
    pub fn evict(&self) -> Result<()> {
        let _guard = self.lock();
        *self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.evict_locked()
    }
    #[allow(clippy::significant_drop_tightening)]
    fn ensure_index_locked(&self) -> Result<()> {
        let mut guard = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let manifests_modified = directory_stamp(&self.root.join("manifests"))?;
        let blocks_modified = directory_stamp(&self.root.join("blocks"))?;
        let report = self.scan_manifests_locked()?;
        let current_pins: HashSet<String> = report
            .manifests
            .iter()
            .flat_map(|manifest| manifest.block_ids.iter().cloned())
            .collect();
        let eviction_blocked = !report.issues.is_empty();
        let needs_scan = guard.as_ref().is_none_or(|index| {
            index.eviction_blocked != eviction_blocked
                || index.manifest_pins != current_pins
                || manifests_modified.is_none()
                || blocks_modified.is_none()
                || index.manifests_modified != manifests_modified
                || index.blocks_modified != blocks_modified
        });
        if needs_scan {
            *guard = Some(self.scan_index_locked()?);
        }
        Ok(())
    }

    #[allow(clippy::significant_drop_tightening)]
    fn scan_index_locked(&self) -> Result<CacheIndex> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut pinned: HashSet<String> = active.keys().cloned().collect();
        let mut manifest_pins = HashSet::new();
        let report = self.scan_manifests_locked()?;
        let eviction_blocked = !report.issues.is_empty();
        for entry in fs::read_dir(self.root.join("manifests"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "tmp") && stale(&path) {
                fs::remove_file(path)?;
            }
        }
        for manifest in report.manifests {
            pinned.extend(manifest.block_ids.iter().cloned());
            manifest_pins.extend(manifest.block_ids);
        }
        let blocks = self.root.join("blocks");
        for entry in fs::read_dir(&blocks)? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "tmp") {
                if stale(&path) {
                    fs::remove_file(path)?;
                }
            } else if path
                .extension()
                .is_some_and(|extension| extension == "audio")
            {
                let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                if !eviction_blocked && valid_id(id) && !self.paths(id).1.is_file() && stale(&path)
                {
                    fs::remove_file(path)?;
                }
            }
        }
        let mut entries = HashMap::new();
        let mut total = 0_u64;
        for entry in fs::read_dir(&blocks)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            let (audio, _) = self.paths(id);
            let size = match fs::metadata(&audio) {
                Ok(info) => info.len(),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    if !eviction_blocked {
                        fs::remove_file(path)?;
                    }
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let parsed = if fs::metadata(&path)?.len() <= MAX_AUDIO_METADATA_BYTES {
                serde_json::from_slice::<Metadata>(&fs::read(&path)?).ok()
            } else {
                None
            };
            let Some(metadata) = parsed.filter(|metadata| {
                metadata.size == size
                    && metadata.size > 0
                    && metadata.size <= MAX_AUDIO as u64
                    && valid_id(&metadata.digest)
                    && valid_mime(&metadata.mime)
            }) else {
                if !eviction_blocked && !pinned.contains(id) {
                    fs::remove_file(audio)?;
                    fs::remove_file(path)?;
                }
                continue;
            };
            if !pinned.contains(id) {
                total = total.saturating_add(size);
            }
            entries.insert(
                id.to_owned(),
                CacheEntry {
                    accessed: metadata.accessed,
                    size,
                },
            );
        }
        Ok(CacheIndex {
            entries,
            manifest_pins,
            eviction_blocked,
            transient: total,
            manifests_modified: directory_stamp(&self.root.join("manifests"))?,
            blocks_modified: directory_stamp(&self.root.join("blocks"))?,
        })
    }

    #[allow(clippy::significant_drop_tightening)]
    fn evict_locked(&self) -> Result<()> {
        self.ensure_index_locked()?;
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut guard = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(index) = guard.as_mut() else {
            return Ok(());
        };
        if index.eviction_blocked || index.transient <= self.transient_limit {
            return Ok(());
        }
        let mut candidates: Vec<_> = index
            .entries
            .iter()
            .filter(|(id, _)| !index.manifest_pins.contains(*id) && !active.contains_key(*id))
            .map(|(id, entry)| (entry.accessed, id.clone(), entry.size))
            .collect();
        candidates.sort_unstable();
        for (_, id, size) in candidates {
            if index.transient <= self.transient_limit {
                break;
            }
            for path in <[PathBuf; 2]>::from(self.paths(&id)) {
                match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => {
                        *guard = None;
                        return Err(error.into());
                    }
                }
            }
            index.entries.remove(&id);
            index.transient = index.transient.saturating_sub(size);
        }
        index.blocks_modified = directory_stamp(&self.root.join("blocks"))?;
        Ok(())
    }
}
fn valid_mime(mime: &str) -> bool {
    matches!(mime, "audio/mpeg" | "audio/wav")
}
fn validate_manifest(manifest: &OfflineManifest) -> Result<()> {
    if manifest.book_id.is_empty()
        || manifest.book_id.len() > 4096
        || manifest.book_id.chars().any(char::is_control)
        || manifest
            .plan_digest
            .as_deref()
            .is_some_and(|digest| !valid_id(digest))
        || manifest.profile_id != manifest.profile.id()
        || manifest.block_ids.iter().any(|id| !valid_id(id))
    {
        return Err(Error::Format("Некорректный манифест аудио".into()));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Chapter;

    fn book(text: &str) -> Book {
        Book {
            id: "book".into(),
            chapters: vec![Chapter {
                title: "chapter".into(),
                text: text.into(),
            }],
            ..Book::default()
        }
    }
    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!("readio-audio-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn plan_is_stable_and_splits_long_unicode_passages() {
        let source = format!("Первое. Второе. {} Конец.", "😀я".repeat(1100));
        let book = book(&source);
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let first = plan_chapter(&book, 0, &profile);
        let repeat = plan_chapter(&book, 0, &profile);
        assert!(first.len() >= 3);
        assert_eq!(
            first.iter().map(|x| &x.id).collect::<Vec<_>>(),
            repeat.iter().map(|x| &x.id).collect::<Vec<_>>()
        );
        assert!(first.iter().all(|block| block.passages.len() <= 6
            && block.text.chars().map(escaped_size).sum::<usize>()
                <= AUDIO_BLOCK_MAX_ESCAPED_BYTES));
        let split: Vec<_> = first
            .iter()
            .flat_map(|block| &block.passages)
            .filter(|part| part.index == 2)
            .collect();
        assert!(split.len() >= 2);
        assert!(split.windows(2).all(|parts| parts[0].end == parts[1].start));
        assert!(split[0].start < split[0].end);
    }

    #[test]
    fn pronunciation_matches_word_boundaries_and_changes_key() {
        let book = book("Мир, МИР и мирный мир. hello HELLO helloing.");
        let base = AudioProfile::from_preferences(&Preferences::default());
        let mut profile = base.clone();
        profile.dictionary.insert("мир".into(), "мiр".into());
        profile.dictionary.insert("hello".into(), "привет".into());
        let first = plan_chapter(&book, 0, &profile);
        assert_eq!(
            first[0].text,
            "мiр, мiр и мирный мiр.\n\nпривет привет helloing."
        );
        assert_ne!(first[0].id, plan_chapter(&book, 0, &base)[0].id);
        profile.voice = "ru-RU-SvetlanaNeural".into();
        assert_ne!(first[0].id, plan_chapter(&book, 0, &profile)[0].id);
    }

    #[test]
    fn checked_planner_honors_typed_limits_and_unicode_boundaries() {
        let mut profile = AudioProfile::from_preferences(&Preferences::default());
        profile.dictionary.insert("мир".into(), "СВІТ".into());
        profile.synthesis = Some(SynthesisSettings {
            input_limit: Some(InputLimit {
                unit: InputUnit::Character,
                value: 12,
                source: LimitSource::User,
                verified_at: None,
            }),
            ..SynthesisSettings::default()
        });
        let planner = ChapterPlanner::try_new(&profile).unwrap();
        let blocks = planner
            .try_plan_text(0, "мир мирний. мир! 😀 тағы.")
            .unwrap();
        assert!(blocks.iter().all(|block| block.text.chars().count() <= 12));
        for block in &blocks {
            for part in &block.passages {
                let (start, end) = (part.synthesis_start.unwrap(), part.synthesis_end.unwrap());
                assert!(start < end && end <= block.text.encode_utf16().count());
            }
        }
        let text = blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("СВІТ мирний"));
        assert!(!text.contains("СВІТний"));
    }

    #[test]
    fn checked_planner_respects_utf8_limit_and_keeps_fitting_words_whole() {
        let mut profile = AudioProfile::from_preferences(&Preferences::default());
        profile.synthesis = Some(SynthesisSettings {
            input_limit: Some(InputLimit {
                unit: InputUnit::Utf8Byte,
                value: 7,
                source: LimitSource::Provider,
                verified_at: Some(1),
            }),
            ..SynthesisSettings::default()
        });
        let blocks = ChapterPlanner::try_new(&profile)
            .unwrap()
            .try_plan_text(0, "one two three 😀")
            .unwrap();
        assert!(blocks.iter().all(|block| block.text.len() <= 7));
        let joined = blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>();
        assert!(joined.iter().any(|text| text.contains("three")));
        assert!(joined.iter().any(|text| text.contains('😀')));
        if let Some(settings) = profile.synthesis.as_mut() {
            settings.input_limit.as_mut().unwrap().value = 3;
        }
        assert!(
            ChapterPlanner::try_new(&profile)
                .unwrap()
                .try_plan_text(0, "😀")
                .is_err()
        );
    }

    #[test]
    fn checked_planner_rejects_large_dictionary_expansion_and_unsplittable_replacement() {
        let mut profile = AudioProfile::from_preferences(&Preferences::default());
        profile.dictionary.insert("x".into(), "y".repeat(200));
        profile.synthesis = Some(SynthesisSettings {
            input_limit: Some(InputLimit {
                unit: InputUnit::Character,
                value: 20,
                source: LimitSource::User,
                verified_at: None,
            }),
            ..SynthesisSettings::default()
        });
        assert!(
            ChapterPlanner::try_new(&profile)
                .unwrap()
                .try_plan_text(0, "x")
                .is_err()
        );
        profile.dictionary = (0..=MAX_DICTIONARY_ENTRIES)
            .map(|i| (format!("x{i}"), "y".into()))
            .collect();
        assert!(ChapterPlanner::try_new(&profile).is_err());
    }

    #[test]
    fn long_passage_with_single_word_dictionary_is_chunked_without_losing_text() {
        let mut profile = AudioProfile::from_preferences(&Preferences::default());
        profile.dictionary.insert("кот".into(), "кіт".into());
        let source = "кот ".repeat(65_000);
        let blocks = ChapterPlanner::try_new(&profile)
            .unwrap()
            .try_plan_text(0, &source)
            .unwrap();
        let actual: String = blocks.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(actual, "кіт ".repeat(65_000).trim());
        let source_len = source.encode_utf16().count();
        assert!(blocks.iter().all(|block| block.end <= source_len));
    }

    #[test]
    fn resource_identity_changes_audio_id_without_changing_legacy_profiles() {
        let old = AudioProfile::from_preferences(&Preferences::default());
        let serialized = serde_json::to_value(&old).unwrap();
        assert!(serialized.get("synthesis").is_none());
        let text = "Пример.";
        let legacy_id = canonical_block_id(text, &old);
        let mut changed = old;
        changed.synthesis = Some(SynthesisSettings {
            resource_revision: Some("v2".into()),
            ..SynthesisSettings::default()
        });
        assert_ne!(canonical_block_id(text, &changed), legacy_id);
        changed.synthesis = Some(SynthesisSettings {
            input_limit: Some(InputLimit {
                unit: InputUnit::Character,
                value: 7,
                source: LimitSource::User,
                verified_at: None,
            }),
            ..SynthesisSettings::default()
        });
        assert_eq!(canonical_block_id(text, &changed), legacy_id);
    }

    #[test]
    fn respects_model_limit_and_reuses_equal_audio_across_books() {
        let mut profile = AudioProfile::from_preferences(&Preferences::default());
        profile.max_escaped_bytes = Some(20);
        let text = "& ааааааа. Ббббббббб.";
        let a = book(text);
        let mut b = book(text);
        b.id = "different".into();
        let a = plan_chapter(&a, 0, &profile);
        let b = plan_chapter(&b, 0, &profile);
        assert!(a.len() > 1);
        assert!(
            a.iter()
                .all(|block| block.text.chars().map(escaped_size).sum::<usize>() <= 20)
        );
        assert_eq!(
            a.iter().map(|block| &block.id).collect::<Vec<_>>(),
            b.iter().map(|block| &block.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn disk_cache_detects_tampering_and_restores_manifest() {
        let root = fixture();
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let id = plan_chapter(&book("Пример."), 0, &profile)[0].id.clone();
        let manifest = OfflineManifest {
            book_id: "b/../id".into(),
            profile_id: profile.id(),
            profile,
            block_ids: vec![id.clone()],
            plan_digest: None,
            updated_at: 0,
        };
        let cache = AudioCache::new(root.clone()).unwrap();
        cache.save_manifest(&manifest).unwrap();
        let saved = cache.put(&id, "audio/mpeg", b"mp3-data").unwrap();
        assert_eq!(
            cache.stats(std::slice::from_ref(&id)).unwrap(),
            vec![Some(8)]
        );
        assert_eq!(cache.get(&id).unwrap().unwrap().mime, "audio/mpeg");
        assert!(cache.get("../../escape").is_err());
        drop(cache);
        let cache = AudioCache::new(root.clone()).unwrap();
        assert_eq!(
            cache.list_manifests().unwrap()[0].block_ids,
            vec![id.clone()]
        );
        assert_eq!(
            cache
                .manifest(&manifest.book_id, &manifest.profile_id)
                .unwrap()
                .unwrap()
                .block_ids,
            vec![id.clone()]
        );
        fs::write(saved.path, b"mp3-datA").unwrap();
        assert_eq!(
            cache.stats(std::slice::from_ref(&id)).unwrap(),
            vec![Some(8)]
        );
        assert!(cache.get(&id).unwrap().is_none());
        cache
            .remove_manifest(&manifest.book_id, &manifest.profile_id)
            .unwrap();
        assert!(cache.list_manifests().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verified_audio_bytes_do_not_rewrite_recent_lru_metadata() {
        let root = fixture();
        let cache = AudioCache::new(root.clone()).unwrap();
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let id = plan_chapter(&book("Текст."), 0, &profile)[0].id.clone();
        cache.put(&id, "audio/mpeg", b"audio-data").unwrap();
        let (_, metadata) = cache.paths(&id);
        let before = fs::read(&metadata).unwrap();
        let result = cache.get_bytes(&id).unwrap().unwrap();
        assert_eq!(result.data, b"audio-data");
        assert_eq!(result.mime, "audio/mpeg");
        assert_eq!(result.timings, Vec::<AudioTiming>::new());
        assert_eq!(fs::read(&metadata).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_timings_roundtrip_and_reject_invalid_alignment() {
        let root = fixture();
        let cache = AudioCache::new(root.clone()).unwrap();
        let id = "a".repeat(64);
        let timing = AudioTiming {
            start_utf16: 0,
            end_utf16: 3,
            start_seconds: 0.1,
            end_seconds: 0.5,
        };
        cache
            .put_with_timings(&id, "audio/mpeg", b"audio", std::slice::from_ref(&timing))
            .unwrap();
        assert_eq!(
            cache.get_bytes(&id).unwrap().unwrap().timings,
            vec![timing.clone()]
        );
        let invalid = AudioTiming {
            start_seconds: f64::NAN,
            ..timing
        };
        assert!(
            cache
                .put_with_timings(&id, "audio/mpeg", b"other", &[invalid])
                .is_err()
        );
        assert_eq!(cache.get_bytes(&id).unwrap().unwrap().data, b"audio");
        let (_, path) = cache.paths(&id);
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        metadata["timings"][0]["endSeconds"] = serde_json::json!(-1.0);
        fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let recovered = cache.get_bytes(&id).unwrap().unwrap();
        assert_eq!(recovered.data, b"audio");
        assert_eq!(recovered.timings, Vec::<AudioTiming>::new());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verified_read_survives_read_only_lru_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture();
        let cache = AudioCache::new(root.clone()).unwrap();
        let id = "b".repeat(64);
        cache.put(&id, "audio/mpeg", b"audio").unwrap();
        let (_, path) = cache.paths(&id);
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        metadata["accessed"] = serde_json::json!(0);
        fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let blocks = cache.root.join("blocks");
        fs::set_permissions(&blocks, fs::Permissions::from_mode(0o500)).unwrap();
        let readonly = fs::File::create(blocks.join("probe.tmp")).is_err();
        let result = cache.get_bytes(&id);
        fs::set_permissions(&blocks, fs::Permissions::from_mode(0o700)).unwrap();
        if readonly {
            assert_eq!(result.unwrap().unwrap().data, b"audio");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn damaged_manifest_isolated_and_blocks_eviction_until_restored() {
        let root = fixture();
        let mut cache = AudioCache::new(root.clone()).unwrap();
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let ids: Vec<String> = (1..=4).map(|n| format!("{n:064x}")).collect();
        let manifests: Vec<_> = (0..3)
            .map(|n| OfflineManifest {
                book_id: format!("book-{n}"),
                profile_id: profile.id(),
                profile: profile.clone(),
                block_ids: vec![ids[n].clone()],
                plan_digest: None,
                updated_at: 0,
            })
            .collect();
        for (n, manifest) in manifests.iter().enumerate() {
            cache.save_manifest(manifest).unwrap();
            cache.put(&ids[n], "audio/mpeg", b"sound").unwrap();
        }
        let bad_path = cache.manifest_path(&manifests[1].book_id, &manifests[1].profile_id);
        let backup = root.join("backup.json");
        fs::copy(&bad_path, &backup).unwrap();
        fs::write(&bad_path, b"broken json").unwrap();
        let report = cache.scan_manifests().unwrap();
        assert_eq!(report.manifests.len(), 2);
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0].book_id.is_none());
        cache.transient_limit = 0;
        cache.put(&ids[3], "audio/mpeg", b"transient").unwrap();
        for id in &ids {
            assert!(cache.verify(id).unwrap().is_some());
        }
        assert!(cache.save_manifest(&manifests[1]).is_err());
        let filename = bad_path.file_name().unwrap().to_str().unwrap();
        assert!(cache.restore_manifest("../backup.json", &backup).is_err());
        cache.restore_manifest(filename, &backup).unwrap();
        assert!(cache.scan_manifests().unwrap().issues.is_empty());
        cache.evict().unwrap();
        for id in &ids[..3] {
            assert!(cache.verify(id).unwrap().is_some());
        }
        assert!(cache.verify(&ids[3]).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_and_wrong_filename_manifest_are_isolated() {
        let root = fixture();
        let cache = AudioCache::new(root.clone()).unwrap();
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let manifest = OfflineManifest {
            book_id: "valid".into(),
            profile_id: profile.id(),
            profile,
            block_ids: Vec::new(),
            plan_digest: None,
            updated_at: 0,
        };
        cache.save_manifest(&manifest).unwrap();
        let wrong = cache
            .root
            .join("manifests")
            .join(format!("{}.json", "a".repeat(64)));
        fs::write(&wrong, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let oversized = cache
            .root
            .join("manifests")
            .join(format!("{}.json", "b".repeat(64)));
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_MANIFEST_BYTES as u64 + 1)
            .unwrap();
        let report = cache.scan_manifests().unwrap();
        assert_eq!(report.manifests.len(), 1);
        assert_eq!(report.issues.len(), 2);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.book_id.as_deref() == Some("valid"))
        );
        assert!(
            cache
                .restore_manifest(wrong.file_name().unwrap().to_str().unwrap(), &wrong)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persists_and_verifies_content_addressed_plan() {
        let root = fixture();
        let cache = AudioCache::new(root.clone()).unwrap();
        let book = book("Первое предложение. Второе предложение.");
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let plan = crate::EditionPlan::build(&book, &profile).unwrap();
        let digest = cache.save_edition_plan(&plan).unwrap();
        assert_eq!(cache.save_edition_plan(&plan).unwrap(), digest);
        let loaded = cache.edition_plan(&digest).unwrap();
        loaded.validate(&book, &profile).unwrap();
        assert_eq!(loaded.block_ids(), plan.block_ids());
        fs::write(
            cache.root.join("editions").join(format!("{digest}.json")),
            b"bad",
        )
        .unwrap();
        assert!(cache.edition_plan(&digest).is_err());
        assert!(cache.save_edition_plan(&plan).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plan_cleanup_preserves_shared_and_unknown_references() {
        let root = fixture();
        let cache = AudioCache::new(root.clone()).unwrap();
        let source = book("Текст первой главы.");
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let plan = crate::EditionPlan::build(&source, &profile).unwrap();
        let digest = cache.save_edition_plan(&plan).unwrap();
        let plan_path = cache.root.join("editions").join(format!("{digest}.json"));
        let first = OfflineManifest {
            book_id: "first".into(),
            profile_id: profile.id(),
            profile,
            block_ids: plan.block_ids(),
            plan_digest: Some(digest),
            updated_at: 0,
        };
        let second = OfflineManifest {
            book_id: "second".into(),
            ..first.clone()
        };
        cache.save_manifest(&first).unwrap();
        cache.save_manifest(&second).unwrap();
        cache
            .remove_manifest(&first.book_id, &first.profile_id)
            .unwrap();
        assert!(plan_path.exists());
        let unknown = cache
            .root
            .join("manifests")
            .join(format!("{}.json", "b".repeat(64)));
        fs::write(&unknown, b"not json").unwrap();
        cache.remove_book_manifests(&second.book_id).unwrap();
        assert!(plan_path.exists());
        fs::remove_file(unknown).unwrap();
        cache.save_manifest(&first).unwrap();
        cache.remove_book_manifests(&first.book_id).unwrap();
        assert!(!plan_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn eviction_preserves_recent_temps_and_removes_stale_ones() {
        let root = fixture();
        let cache = AudioCache::new(root.clone()).unwrap();
        let fresh = cache.root.join("blocks").join("fresh.tmp");
        let old = cache.root.join("manifests").join("old.tmp");
        fs::write(&fresh, b"in-progress").unwrap();
        fs::write(&old, b"abandoned").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&old).unwrap();
        let modified = SystemTime::now() - std::time::Duration::from_hours(25);
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        drop(file);
        cache.evict().unwrap();
        assert!(fresh.exists());
        assert!(!old.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn eviction_removes_corrupt_unpinned_metadata_and_audio() {
        let root = fixture();
        let cache = AudioCache::new(root.clone()).unwrap();
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let id = plan_chapter(&book("Текст."), 0, &profile)[0].id.clone();
        cache.put(&id, "audio/mpeg", b"data").unwrap();
        let (audio, meta) = cache.paths(&id);
        fs::write(&meta, b"not-json").unwrap();
        cache.evict().unwrap();
        assert!(!audio.exists());
        assert!(!meta.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_index_refreshes_after_external_manifest_changes() {
        let root = fixture();
        let mut first = AudioCache::new(root.clone()).unwrap();
        first.transient_limit = 2;
        let second = AudioCache::new(root.clone()).unwrap();
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let pinned = plan_chapter(&book("Первый."), 0, &profile)[0].id.clone();
        let other = plan_chapter(&book("Второй."), 0, &profile)[0].id.clone();
        first.pin_active(&pinned).unwrap();
        first.put(&pinned, "audio/mpeg", b"first").unwrap();
        first.unpin_active(&pinned);
        second
            .save_manifest(&OfflineManifest {
                book_id: "book".into(),
                profile_id: profile.id(),
                profile,
                block_ids: vec![pinned.clone()],
                plan_digest: None,
                updated_at: 0,
            })
            .unwrap();
        first.put(&other, "audio/mpeg", b"other").unwrap();
        assert!(first.get(&pinned).unwrap().is_some());
        assert!(first.get(&other).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_index_tracks_pins_overwrites_and_restart() {
        let root = fixture();
        let mut cache = AudioCache::new(root.clone()).unwrap();
        cache.transient_limit = 100;
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let id = plan_chapter(&book("Текст."), 0, &profile)[0].id.clone();
        cache.put(&id, "audio/mpeg", b"first").unwrap();
        assert_eq!(cache.index.lock().unwrap().as_ref().unwrap().transient, 5);
        cache.pin_active(&id).unwrap();
        cache.pin_active(&id).unwrap();
        assert_eq!(cache.index.lock().unwrap().as_ref().unwrap().transient, 0);
        cache.unpin_active(&id);
        assert_eq!(cache.index.lock().unwrap().as_ref().unwrap().transient, 0);
        cache.unpin_active(&id);
        assert_eq!(cache.index.lock().unwrap().as_ref().unwrap().transient, 5);
        cache.put(&id, "audio/mpeg", b"second").unwrap();
        assert_eq!(cache.index.lock().unwrap().as_ref().unwrap().transient, 6);
        drop(cache);
        let cache = AudioCache::new(root.clone()).unwrap();
        cache.evict().unwrap();
        assert_eq!(cache.index.lock().unwrap().as_ref().unwrap().transient, 6);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pinned_blocks_survive_eviction_and_reference_counts() {
        let root = fixture();
        let mut cache = AudioCache::new(root.clone()).unwrap();
        cache.transient_limit = 2;
        let profile = AudioProfile::from_preferences(&Preferences::default());
        let ids: Vec<_> = (0..3)
            .map(|index| {
                plan_chapter(&book(&format!("Текст {index}.")), 0, &profile)[0]
                    .id
                    .clone()
            })
            .collect();
        let manifest = OfflineManifest {
            book_id: "book".into(),
            profile_id: profile.id(),
            profile,
            block_ids: vec![ids[0].clone()],
            plan_digest: None,
            updated_at: 0,
        };
        cache.save_manifest(&manifest).unwrap();
        cache.put(&ids[0], "audio/mpeg", b"first").unwrap();
        cache.pin_active(&ids[1]).unwrap();
        cache.pin_active(&ids[1]).unwrap();
        cache.put(&ids[1], "audio/mpeg", b"second").unwrap();
        cache.unpin_active(&ids[1]);
        cache.put(&ids[2], "audio/mpeg", b"third").unwrap();
        assert!(cache.get(&ids[0]).unwrap().is_some());
        assert!(cache.get(&ids[1]).unwrap().is_some());
        assert!(cache.get(&ids[2]).unwrap().is_none());
        cache.unpin_active(&ids[1]);
        cache.evict().unwrap();
        assert!(cache.get(&ids[1]).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
