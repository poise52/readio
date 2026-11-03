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
/// Application safety limit for a lightweight offline manifest.
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

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
        Self {
            engine,
            voice,
            model,
            model_voice,
            dictionary: prefs.dictionary.clone(),
            max_escaped_bytes: None,
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

fn hash_parts(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    format!("{:x}", hash.finalize())
}

fn finish_block(
    result: &mut Vec<AudioBlock>,
    chapter: usize,
    profile: &AudioProfile,
    parts: &mut Vec<AudioPart>,
    texts: &mut Vec<String>,
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
    let id = hash_parts(&[
        b"readio-audio-block-v1",
        profile.engine.as_bytes(),
        profile.voice.as_bytes(),
        profile.model.as_deref().unwrap_or("").as_bytes(),
        profile.model_voice.as_deref().unwrap_or("").as_bytes(),
        text.as_bytes(),
    ]);
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
        self.plan_v1_text(chapter, text)
    }

    // Keep this algorithm frozen for editions created before planner versioning.
    fn plan_v1_text(&self, chapter: usize, text: &str) -> Vec<AudioBlock> {
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
                        });
                        texts.push(std::mem::take(&mut current));
                    }
                    finish_block(&mut blocks, chapter, &self.profile, &mut parts, &mut texts);
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
                });
                texts.push(current);
            }
        }
        finish_block(&mut blocks, chapter, &self.profile, &mut parts, &mut texts);
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
    plan_chapter_v1(book, chapter, profile)
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
#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    mime: String,
    size: u64,
    digest: String,
    accessed: u64,
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
        let Some((audio, mut metadata)) = self.verified_locked(id)? else {
            return Ok(None);
        };
        let (_, metadata_path) = self.paths(id);
        let blocks_dir = self.root.join("blocks");
        let before = directory_stamp(&blocks_dir)?;
        metadata.accessed = timestamp();
        atomic_write(&metadata_path, &serde_json::to_vec(&metadata)?)?;
        let after = directory_stamp(&blocks_dir)?;
        let mut guard = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = guard.as_mut() {
            if before.is_some() && index.blocks_modified == before {
                index.blocks_modified = after;
                if let Some(entry) = index.entries.get_mut(id) {
                    entry.accessed = metadata.accessed;
                }
            } else {
                *guard = None;
            }
        }
        drop(guard);
        Ok(Some(CachedAudio {
            path: audio,
            mime: metadata.mime,
            size: metadata.size,
        }))
    }

    /// Verifies one cached block without changing its LRU timestamp.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers or filesystem failures.
    pub fn verify(&self, id: &str) -> Result<Option<u64>> {
        validate_id(id)?;
        let _guard = self.lock();
        Ok(self.verified_locked(id)?.map(|(_, metadata)| metadata.size))
    }

    fn verified_locked(&self, id: &str) -> Result<Option<(PathBuf, Metadata)>> {
        let (audio, metadata_path) = self.paths(id);
        match fs::metadata(&metadata_path) {
            Ok(info) if info.len() <= 16 * 1024 => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let metadata = match fs::read(metadata_path) {
            Ok(data) => data,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Ok(metadata) = serde_json::from_slice::<Metadata>(&metadata) else {
            return Ok(None);
        };
        if metadata.size == 0
            || metadata.size > MAX_AUDIO as u64
            || !valid_id(&metadata.digest)
            || !valid_mime(&metadata.mime)
        {
            return Ok(None);
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
        Ok(Some((audio, metadata)))
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
                Ok(info) if info.len() <= 16 * 1024 => info,
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
        validate_id(id)?;
        if data.is_empty() || data.len() > MAX_AUDIO || !valid_mime(mime) {
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
            let parsed = if fs::metadata(&path)?.len() <= 16 * 1024 {
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
