use crate::{AppState, IpcResult, credentials};
use crate::{
    block_error::{BlockError, BlockErrorCode, BlockResult},
    plan_cache::PlanCache,
    request_queue::WorkQueue,
    tasks::Tasks,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use desk_core::{
    AUDIO_BLOCK_MAX_PASSAGES, AudioBlock, AudioCache, AudioProfile, BillingUnit, Book, EditionPlan,
    GenerationJournal, GenerationPrepare, GenerationState, OfflineManifest, Store,
};
use readio_speech::{SpeechAudio, SpeechService};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::State;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

#[derive(Clone)]
struct Quote {
    manifest: OfflineManifest,
    plan: Arc<desk_core::EditionPlan>,
    cached_blocks: usize,
    characters: usize,
    text_bytes: usize,
    pricing: Option<Price>,
    free: bool,
    expires: Instant,
    job_generation: u64,
    billing_unit: Option<BillingUnit>,
    retry_attempts: HashMap<String, String>,
}
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Price {
    prompt: String,
    completion: String,
    #[serde(flatten)]
    other: std::collections::BTreeMap<String, String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResult {
    quote_id: String,
    profile_id: String,
    model: Option<String>,
    engine: String,
    voice: String,
    model_voice: Option<String>,
    total_blocks: usize,
    cached_blocks: usize,
    retry_required_blocks: usize,
    characters: usize,
    pricing: Option<Price>,
    estimated_cost: Option<String>,
    billable_units: Option<usize>,
    cost_unit: PricingUnit,
    cost_reason: Option<UnknownCostReason>,
    estimate_source: Option<EstimateSource>,
    max_block_bytes: usize,
    max_passages: usize,
    limit_source: &'static str,
    expires_at: u64,
}
#[derive(Clone)]
struct Job {
    state: &'static str,
    error: Option<String>,
    generation: u64,
    engine: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineStatus {
    book_id: String,
    profile_id: Option<String>,
    state: &'static str,
    total_blocks: usize,
    cached_blocks: usize,
    bytes: u64,
    error: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioPlan {
    profile_id: String,
    blocks: Vec<AudioBlockInfo>,
    offline: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioBlockInfo {
    id: String,
    chapter: usize,
    start: usize,
    end: usize,
    passages: Vec<desk_core::AudioPart>,
    characters: usize,
}
impl From<&AudioBlock> for AudioBlockInfo {
    fn from(block: &AudioBlock) -> Self {
        Self {
            id: block.id.clone(),
            chapter: block.chapter,
            start: block.start,
            end: block.end,
            passages: block.passages.clone(),
            characters: block.characters,
        }
    }
}

#[derive(Clone)]
enum InflightResult {
    Audio(Arc<SpeechAudio>),
    Failed(BlockError, Instant),
    Paused,
}
struct ActivePin {
    cache: Arc<AudioCache>,
    id: String,
}
struct Submission<'a> {
    queue: &'a WorkQueue,
    id: &'a str,
}
impl Drop for Submission<'_> {
    fn drop(&mut self) {
        self.queue.complete(self.id);
    }
}
struct InteractiveWaiter<'a>(&'a AtomicUsize);
impl Drop for InteractiveWaiter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Drop for ActivePin {
    fn drop(&mut self) {
        self.cache.unpin_active(&self.id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PricingUnit {
    Free,
    Unknown,
    Character,
    Utf8Byte,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum UnknownCostReason {
    ProviderUnitUnavailable,
    AdditionalRateUnitUnavailable,
    InvalidRate,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum EstimateSource {
    Free,
    User,
}
struct CostEstimate {
    unit: PricingUnit,
    units: Option<usize>,
    amount: Option<String>,
    reason: Option<UnknownCostReason>,
    source: Option<EstimateSource>,
}
const fn unknown(reason: UnknownCostReason) -> CostEstimate {
    CostEstimate {
        unit: PricingUnit::Unknown,
        units: None,
        amount: None,
        reason: Some(reason),
        source: None,
    }
}
fn parse_decimal(value: &str) -> Option<(u128, i32)> {
    let value = value.trim();
    let (mantissa, exponent) = value
        .split_once(['e', 'E'])
        .map_or((value, "0"), |(a, b)| (a, b));
    let exponent: i32 = exponent.parse().ok()?;
    if !(-128..=128).contains(&exponent) {
        return None;
    }
    let (whole, fraction) = mantissa
        .split_once('.')
        .map_or((mantissa, ""), |(a, b)| (a, b));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let coefficient: u128 = digits
        .trim_start_matches('0')
        .parse()
        .or_else(|_| {
            if digits.bytes().all(|c| c == b'0') {
                Ok(0_u128)
            } else {
                Err(())
            }
        })
        .ok()?;
    Some((
        coefficient,
        exponent.checked_sub(i32::try_from(fraction.len()).ok()?)?,
    ))
}
fn multiply_decimal(rate: &str, units: usize) -> Option<String> {
    let (coefficient, exponent) = parse_decimal(rate)?;
    let product = coefficient.checked_mul(u128::try_from(units).ok()?)?;
    if product == 0 {
        return Some("0".into());
    }
    let mut digits = product.to_string();
    if exponent >= 0 {
        let count = usize::try_from(exponent).ok()?;
        if digits.len().checked_add(count)? > 128 {
            return None;
        }
        digits.push_str(&"0".repeat(count));
        return Some(digits);
    }
    let places = usize::try_from(exponent.checked_neg()?).ok()?;
    if places > 128 {
        return None;
    }
    if places >= digits.len() {
        digits = format!("0.{}{}", "0".repeat(places - digits.len()), digits);
    } else {
        digits.insert(digits.len() - places, '.');
    }
    while digits.ends_with('0') {
        digits.pop();
    }
    if digits.ends_with('.') {
        digits.pop();
    }
    Some(digits)
}
fn cost(quote: &Quote) -> CostEstimate {
    if quote.free {
        return CostEstimate {
            unit: PricingUnit::Free,
            units: None,
            amount: Some("0".into()),
            reason: None,
            source: Some(EstimateSource::Free),
        };
    }
    let Some(unit) = quote.billing_unit else {
        return unknown(UnknownCostReason::ProviderUnitUnavailable);
    };
    let Some(price) = quote.pricing.as_ref() else {
        return unknown(UnknownCostReason::InvalidRate);
    };
    for value in std::iter::once(&price.completion).chain(price.other.values()) {
        match parse_decimal(value) {
            Some((0, _)) => (),
            Some(_) => return unknown(UnknownCostReason::AdditionalRateUnitUnavailable),
            None => return unknown(UnknownCostReason::InvalidRate),
        }
    }
    let (unit, units) = match unit {
        BillingUnit::Character => (PricingUnit::Character, quote.characters),
        BillingUnit::Utf8Byte => (PricingUnit::Utf8Byte, quote.text_bytes),
    };
    let Some(amount) = multiply_decimal(&price.prompt, units) else {
        return unknown(UnknownCostReason::InvalidRate);
    };
    CostEstimate {
        unit,
        units: Some(units),
        amount: Some(amount),
        reason: None,
        source: Some(EstimateSource::User),
    }
}
fn price_matches(quote: &Quote, price: Option<&Price>, free: bool) -> bool {
    quote.pricing.as_ref() == price && quote.free == free
}

struct OfflineWork {
    book_id: String,
    plan: Arc<desk_core::EditionPlan>,
    profile: AudioProfile,
    generation: u64,
    started_at: Instant,
    allowed: HashSet<String>,
    retry_attempts: HashMap<String, String>,
}

type SharedPlanner = Arc<std::sync::Mutex<Option<(String, Arc<desk_core::ChapterPlanner>)>>>;

pub struct OfflineManager {
    store: Arc<std::sync::Mutex<Store>>,
    cache: Arc<AudioCache>,
    speech: SpeechService,
    gate: Mutex<()>,
    interactive: AtomicUsize,
    credential_generation: AtomicU64,
    wake: Notify,
    jobs: Mutex<HashMap<String, Job>>,
    quotes: Mutex<HashMap<String, Quote>>,
    credential_gate: Mutex<()>,
    inflight: Mutex<HashMap<String, tokio::sync::watch::Receiver<Option<InflightResult>>>>,
    journal: Arc<std::sync::Mutex<GenerationJournal>>,
    queue: WorkQueue,
    tasks: Arc<Tasks>,
    chapter_plans: Arc<PlanCache<plans::ChapterPlan>>,
    edition_plans: Arc<PlanCache<EditionPlan>>,
    manifest_plans: Arc<PlanCache<OfflineManifest>>,
    planner: SharedPlanner,
    plan_gate: Mutex<()>,
    #[cfg(test)]
    chapter_builds: Arc<AtomicUsize>,
    #[cfg(test)]
    test_provider: Option<Arc<crate::test_provider::MockSpeech>>,
}
impl OfflineManager {
    pub(crate) fn new(
        store: Arc<std::sync::Mutex<Store>>,
        root: std::path::PathBuf,
    ) -> IpcResult<Arc<Self>> {
        let journal = GenerationJournal::open(&root).map_err(|e| e.to_string())?;
        journal.recover_submitted().map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            store,
            cache: Arc::new(AudioCache::new(root.clone()).map_err(|e| e.to_string())?),
            speech: SpeechService::new(root).map_err(|e| e.to_string())?,
            gate: Mutex::new(()),
            interactive: AtomicUsize::new(0),
            credential_generation: AtomicU64::new(0),
            wake: Notify::new(),
            jobs: Mutex::new(HashMap::new()),
            quotes: Mutex::new(HashMap::new()),
            credential_gate: Mutex::new(()),
            inflight: Mutex::new(HashMap::new()),
            journal: Arc::new(std::sync::Mutex::new(journal)),
            queue: WorkQueue::default(),
            tasks: Tasks::new(),
            chapter_plans: Arc::new(PlanCache::new(256 * 1024 * 1024)),
            edition_plans: Arc::new(PlanCache::new(256 * 1024 * 1024)),
            manifest_plans: Arc::new(PlanCache::new(16 * 1024 * 1024)),
            planner: Arc::new(std::sync::Mutex::new(None)),
            plan_gate: Mutex::new(()),
            #[cfg(test)]
            chapter_builds: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            test_provider: None,
        }))
    }
    pub(crate) fn begin_shutdown(&self) {
        self.queue.close();
        self.tasks.close();
    }
    pub(crate) async fn shutdown(&self) {
        self.begin_shutdown();
        {
            let mut jobs = self.jobs.lock().await;
            for job in jobs.values_mut().filter(|job| job.state == "running") {
                job.state = "paused";
                job.generation = job.generation.wrapping_add(1);
            }
        }
        self.tasks.wait().await;
    }
    async fn journal<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&GenerationJournal) -> desk_core::Result<T> + Send + 'static,
    ) -> BlockResult<T> {
        let journal = Arc::clone(&self.journal);
        tauri::async_runtime::spawn_blocking(move || {
            let journal = journal.lock().map_err(|e| e.to_string())?;
            operation(&journal).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(Into::into)
    }
    async fn book_and_profile(&self, id: String, offline: bool) -> IpcResult<(Book, AudioProfile)> {
        let store = Arc::clone(&self.store);
        let cache = Arc::clone(&self.cache);
        tauri::async_runtime::spawn_blocking(move || {
            let store = store.lock().map_err(|e| e.to_string())?;
            let book = store.book(&id).map_err(|e| e.to_string())?;
            let profile = if offline {
                cache
                    .list_manifests()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|m| m.book_id == id)
                    .ok_or_else(|| "Для книги нет офлайн-версии".to_owned())?
                    .profile
            } else {
                AudioProfile::from_preferences(&store.preferences().map_err(|e| e.to_string())?)
            };
            Ok((book, profile))
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn manifests(&self) -> IpcResult<Vec<OfflineManifest>> {
        let cache = Arc::clone(&self.cache);
        tauri::async_runtime::spawn_blocking(move || {
            cache.list_manifests().map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn diagnostics(&self) -> IpcResult<Vec<desk_core::ManifestIssue>> {
        let cache = Arc::clone(&self.cache);
        tauri::async_runtime::spawn_blocking(move || {
            cache
                .scan_manifests()
                .map(|report| report.issues)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn status(&self, book_id: &str) -> IpcResult<OfflineStatus> {
        let manifest = self
            .manifests()
            .await?
            .into_iter()
            .find(|m| m.book_id == book_id);
        let Some(manifest) = manifest else {
            if let Some(issue) = self
                .diagnostics()
                .await?
                .into_iter()
                .find(|issue| issue.book_id.as_deref() == Some(book_id))
            {
                return Ok(broken_status(
                    book_id.into(),
                    issue.profile_id,
                    issue.message,
                ));
            }
            return Ok(OfflineStatus {
                book_id: book_id.into(),
                profile_id: None,
                state: "none",
                total_blocks: 0,
                cached_blocks: 0,
                bytes: 0,
                error: None,
            });
        };
        let cache = Arc::clone(&self.cache);
        let ids = manifest.block_ids.clone();
        let (cached_blocks, bytes) =
            tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
                let sizes = cache.stats(&ids).map_err(|e| e.to_string())?;
                let mut cached = 0;
                let mut bytes = 0_u64;
                let mut seen = HashSet::new();
                for (id, size) in ids.iter().zip(sizes) {
                    if let Some(size) = size {
                        cached += 1;
                        if seen.insert(id) {
                            bytes = bytes.saturating_add(size);
                        }
                    }
                }
                Ok((cached, bytes))
            })
            .await
            .map_err(|e| e.to_string())??;
        let job = self.jobs.lock().await.get(book_id).cloned();
        let missing_after_completion = cached_blocks != manifest.block_ids.len()
            && job.as_ref().is_some_and(|job| job.state == "complete");
        let state = if cached_blocks == manifest.block_ids.len() {
            "complete"
        } else if missing_after_completion {
            "error"
        } else {
            job.as_ref().map_or("paused", |job| job.state)
        };
        Ok(OfflineStatus {
            book_id: book_id.into(),
            profile_id: Some(manifest.profile_id),
            state,
            total_blocks: manifest.block_ids.len(),
            cached_blocks,
            bytes,
            error: if missing_after_completion {
                Some("Часть аудио отсутствует; запросите новую оценку загрузки".into())
            } else {
                job.and_then(|j| j.error)
            },
        })
    }
    async fn price(&self, profile: &AudioProfile) -> IpcResult<(Option<Price>, bool)> {
        if profile.engine != "openrouter" {
            return Ok((None, true));
        }
        let model = profile
            .model
            .as_deref()
            .ok_or_else(|| "Выберите модель OpenRouter".to_owned())?;
        let key = tauri::async_runtime::spawn_blocking(credentials::read)
            .await
            .map_err(|e| e.to_string())??
            .ok_or_else(|| "Добавьте ключ OpenRouter в настройках".to_owned())?;
        let models = self
            .speech
            .openrouter_models(Some(&key))
            .await
            .map_err(|e| e.to_string())?;
        let item = models
            .into_iter()
            .find(|item| item.id == model)
            .ok_or_else(|| "Модель OpenRouter недоступна".to_owned())?;
        Ok((
            Some(Price {
                prompt: item.pricing.prompt,
                completion: item.pricing.completion,
                other: item.pricing.other,
            }),
            item.free,
        ))
    }
    #[allow(clippy::too_many_lines)] // Keep the estimate tied to one profile and job revision.
    async fn quote(&self, book_id: String) -> IpcResult<QuoteResult> {
        let _credentials = self.credential_gate.lock().await;
        let job_generation = {
            let jobs = self.jobs.lock().await;
            if jobs.get(&book_id).is_some_and(|job| job.state == "running") {
                return Err("Сначала приостановите загрузку".into());
            }
            jobs.get(&book_id).map_or(0, |job| job.generation)
        };
        let existing = self
            .manifests()
            .await?
            .into_iter()
            .find(|m| m.book_id == book_id);
        let (manifest, plan) = if let Some(manifest) = existing {
            self.edition(manifest).await?
        } else {
            let (book, profile) = self.book_and_profile(book_id.clone(), false).await?;
            self.new_edition(book, profile).await?
        };
        let profile = manifest.profile.clone();
        let cache = Arc::clone(&self.cache);
        let profile_copy = profile.clone();
        let plan_copy = Arc::clone(&plan);
        let journal = Arc::clone(&self.journal);
        let (block_ids, cached_blocks, characters, text_bytes, retry_attempts) =
            tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
                let mut ids = Vec::new();
                let mut cached = 0;
                let mut chars = 0_usize;
                let mut bytes = 0_usize;
                let mut missing = HashSet::new();
                let mut retry_attempts = HashMap::new();
                let journal = journal.lock().map_err(|e| e.to_string())?;
                for chapter in &plan_copy.chapters {
                    for block in &chapter.blocks {
                        if cache
                            .verify(&block.id)
                            .map_err(|e| e.to_string())?
                            .is_some()
                        {
                            cached += 1;
                        } else if missing.insert(block.id.clone()) {
                            chars = chars.saturating_add(block.characters);
                            bytes = bytes.saturating_add(block.text.len());
                            if profile_copy.engine == "openrouter"
                                && let Some(attempt) = journal
                                    .latest(block, &profile_copy)
                                    .map_err(|e| e.to_string())?
                                && matches!(
                                    attempt.state,
                                    GenerationState::Uncertain | GenerationState::Completed
                                )
                            {
                                retry_attempts.insert(block.id.clone(), attempt.id);
                            }
                        }
                        ids.push(block.id.clone());
                    }
                }
                Ok((ids, cached, chars, bytes, retry_attempts))
            })
            .await
            .map_err(|e| e.to_string())??;
        if block_ids.is_empty() {
            return Err("В книге нет текста для озвучивания".into());
        }
        if manifest.block_ids != block_ids {
            return Err("План издания не совпадает с манифестом".into());
        }
        let (pricing, free) = if characters == 0 {
            (None, true)
        } else {
            self.price(&profile).await?
        };
        let billing_unit = {
            let store = Arc::clone(&self.store);
            let model = profile.model.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let store = store.lock().map_err(|e| e.to_string())?;
                let prefs = store.preferences().map_err(|e| e.to_string())?;
                drop(store);
                Ok::<_, String>(
                    model.and_then(|id| prefs.open_router_billing_units.get(&id).copied()),
                )
            })
            .await
            .map_err(|e| e.to_string())??
        };
        let id = Uuid::new_v4().to_string();
        let quote = Quote {
            manifest,
            plan,
            cached_blocks,
            characters,
            text_bytes,
            pricing,
            free,
            expires: Instant::now() + Duration::from_mins(5),
            job_generation,
            billing_unit,
            retry_attempts,
        };
        let estimate = cost(&quote);
        let result = QuoteResult {
            quote_id: id.clone(),
            profile_id: quote.manifest.profile_id.clone(),
            model: quote.manifest.profile.model.clone(),
            engine: quote.manifest.profile.engine.clone(),
            voice: quote.manifest.profile.voice.clone(),
            model_voice: quote.manifest.profile.model_voice.clone(),
            total_blocks: quote.manifest.block_ids.len(),
            cached_blocks: quote.cached_blocks,
            retry_required_blocks: quote.retry_attempts.len(),
            characters: quote.characters,
            pricing: quote.pricing.clone(),
            estimated_cost: estimate.amount,
            billable_units: estimate.units,
            cost_unit: estimate.unit,
            cost_reason: estimate.reason,
            estimate_source: estimate.source,
            max_block_bytes: quote.manifest.profile.escaped_byte_limit(),
            max_passages: AUDIO_BLOCK_MAX_PASSAGES,
            limit_source: "app",
            expires_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |v| v.as_millis().try_into().unwrap_or(u64::MAX))
                .saturating_add(300_000),
        };
        let jobs = self.jobs.lock().await;
        if jobs
            .get(&quote.manifest.book_id)
            .map_or(0, |job| job.generation)
            != job_generation
        {
            return Err("Состояние загрузки изменилось; запросите оценку снова".into());
        }
        let mut quotes = self.quotes.lock().await;
        quotes.retain(|_, q| {
            q.expires > Instant::now() && q.manifest.book_id != quote.manifest.book_id
        });
        if quotes.len() >= 2 {
            let oldest = quotes
                .iter()
                .min_by_key(|(_, q)| q.expires)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                quotes.remove(&oldest);
            }
        }
        quotes.insert(id, quote);
        drop(quotes);
        drop(jobs);
        Ok(result)
    }
    #[allow(clippy::too_many_lines)] // Keep quote revalidation and manifest pinning atomic with job creation.
    async fn start(
        self: &Arc<Self>,
        book_id: String,
        quote_id: String,
        confirm_retries: bool,
    ) -> IpcResult<OfflineStatus> {
        let permit = self.tasks.enter().ok_or("Приложение закрывается")?;
        let _credentials = self.credential_gate.lock().await;
        let quote = self
            .quotes
            .lock()
            .await
            .remove(&quote_id)
            .ok_or_else(|| "Стоимость устарела; запросите оценку снова".to_owned())?;
        if quote.manifest.book_id != book_id || quote.expires <= Instant::now() {
            return Err("Стоимость устарела; запросите оценку снова".into());
        }
        if !quote.retry_attempts.is_empty() && !confirm_retries {
            return Err("Подтвердите повтор ранее отправленных запросов".into());
        }
        if quote.characters != 0 {
            let (pricing, free) = self.price(&quote.manifest.profile).await?;
            if !price_matches(&quote, pricing.as_ref(), free) {
                return Err("Тариф изменился; запросите оценку снова".into());
            }
        }
        let _plan_lifecycle = self.plan_gate.lock().await;
        let (book, profile) = self.book_and_profile(book_id.clone(), false).await?;
        let existing_manifest = self
            .manifests()
            .await?
            .into_iter()
            .find(|m| m.book_id == book_id && m.profile_id == quote.manifest.profile_id);
        let existing = existing_manifest.is_some();
        if existing_manifest.as_ref().is_some_and(|manifest| {
            manifest.plan_digest != quote.manifest.plan_digest
                || manifest.block_ids != quote.manifest.block_ids
        }) {
            return Err("Офлайн-издание изменилось; запросите оценку снова".into());
        }
        if !existing && profile != quote.manifest.profile {
            return Err("Настройки озвучивания изменились; запросите оценку снова".into());
        }
        let mut jobs = self.jobs.lock().await;
        if jobs.get(&book_id).map_or(0, |job| job.generation) != quote.job_generation {
            return Err("Состояние загрузки изменилось; запросите оценку снова".into());
        }
        if jobs.get(&book_id).is_some_and(|j| j.state == "running") {
            return Err("Загрузка уже выполняется".into());
        }
        let cache = Arc::clone(&self.cache);
        let manifest = quote.manifest.clone();
        let plan_check = Arc::clone(&quote.plan);
        let result = tauri::async_runtime::spawn_blocking(
            move || -> IpcResult<(Vec<String>, usize, usize, HashSet<String>)> {
                plan_check
                    .validate(&book, &manifest.profile)
                    .map_err(|e| e.to_string())?;
                let digest = cache
                    .save_edition_plan(&plan_check)
                    .map_err(|e| e.to_string())?;
                if manifest.plan_digest.as_deref() != Some(&digest) {
                    return Err("План издания не соответствует манифесту".into());
                }
                cache.save_manifest(&manifest).map_err(|e| e.to_string())?;
                let outcome = (|| {
                    let mut ids = Vec::new();
                    let mut missing = HashSet::new();
                    let mut chars = 0_usize;
                    let mut bytes = 0_usize;
                    for chapter in &plan_check.chapters {
                        for block in &chapter.blocks {
                            if cache
                                .verify(&block.id)
                                .map_err(|e| e.to_string())?
                                .is_none()
                                && missing.insert(block.id.clone())
                            {
                                chars = chars.saturating_add(block.characters);
                                bytes = bytes.saturating_add(block.text.len());
                            }
                            ids.push(block.id.clone());
                        }
                    }
                    Ok((ids, chars, bytes, missing))
                })();
                if outcome.is_err() && !existing {
                    let _ = cache.remove_manifest(&manifest.book_id, &manifest.profile_id);
                }
                outcome
            },
        )
        .await
        .map_err(|e| e.to_string())??;
        if result.0 != quote.manifest.block_ids
            || result.1 > quote.characters
            || result.2 > quote.text_bytes
        {
            if !existing {
                let cache = Arc::clone(&self.cache);
                let manifest = quote.manifest.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    cache.remove_manifest(&manifest.book_id, &manifest.profile_id)
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            }
            return Err("Текст книги или кеш изменились; запросите оценку снова".into());
        }
        let generation = jobs
            .get(&book_id)
            .map_or(1, |j| j.generation.wrapping_add(1));
        if !self.queue.start_offline(&book_id, generation) {
            return Err("Приложение закрывается".into());
        }
        jobs.insert(
            book_id.clone(),
            Job {
                state: "running",
                error: None,
                generation,
                engine: quote.manifest.profile.engine.clone(),
            },
        );
        drop(jobs);
        let manager = Arc::clone(self);
        let id = book_id.clone();
        let allowed = result.3;
        let started_at = Instant::now();
        tauri::async_runtime::spawn(async move {
            let _permit = permit;
            let worker = Arc::clone(&manager);
            let worker_id = id.clone();
            let outcome = tauri::async_runtime::spawn(async move {
                worker
                    .worker(OfflineWork {
                        book_id: worker_id,
                        plan: quote.plan,
                        profile: quote.manifest.profile,
                        generation,
                        started_at,
                        allowed,
                        retry_attempts: quote.retry_attempts,
                    })
                    .await;
            })
            .await;
            if outcome.is_err() {
                let mut jobs = manager.jobs.lock().await;
                if let Some(job) = jobs.get_mut(&id)
                    && job.generation == generation
                {
                    job.state = "error";
                    job.error =
                        Some("Загрузка завершилась аварийно; запросите новый расчёт".into());
                }
                manager.queue.stop_offline(&id, generation);
                drop(jobs);
            }
        });
        self.status(&book_id).await
    }
    async fn worker(self: Arc<Self>, work: OfflineWork) {
        let OfflineWork {
            book_id,
            plan,
            profile,
            generation,
            started_at,
            allowed,
            retry_attempts,
        } = work;
        let mut sequence = 0_u64;
        for chapter in &plan.chapters {
            for block in &chapter.blocks {
                if !allowed.contains(&block.id) {
                    continue;
                }
                {
                    let jobs = self.jobs.lock().await;
                    if !jobs
                        .get(&book_id)
                        .is_some_and(|j| j.generation == generation && j.state == "running")
                    {
                        return;
                    }
                    sequence = sequence.saturating_add(1);
                    if !self.queue.update_offline(
                        &book_id,
                        generation,
                        sequence,
                        HashSet::from([block.id.clone()]),
                    ) {
                        return;
                    }
                    drop(jobs);
                }
                let consent = retry_attempts.get(&block.id).cloned();
                let result = Arc::clone(&self)
                    .request_block(
                        block.clone(),
                        profile.clone(),
                        Some((book_id.clone(), generation)),
                        Some(started_at),
                        consent,
                    )
                    .await;
                if let Err(error) = result {
                    let mut jobs = self.jobs.lock().await;
                    if let Some(job) = jobs.get_mut(&book_id)
                        && job.generation == generation
                        && job.state == "running"
                    {
                        job.state = "error";
                        job.error = Some(error.message);
                        self.queue.stop_offline(&book_id, generation);
                    }
                    drop(jobs);
                    return;
                }
            }
        }
        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get_mut(&book_id)
            && job.generation == generation
            && job.state == "running"
        {
            job.state = "complete";
            self.queue.stop_offline(&book_id, generation);
        }
    }
    async fn cached(&self, id: String) -> IpcResult<Option<SpeechAudio>> {
        let cache = Arc::clone(&self.cache);
        tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
            cache.pin_active(&id).map_err(|e| e.to_string())?;
            let _pin = ActivePin {
                cache: Arc::clone(&cache),
                id: id.clone(),
            };
            let Some(entry) = cache.get(&id).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            let bytes = std::fs::read(entry.path).map_err(|e| e.to_string())?;
            Ok(Some(SpeechAudio {
                data: STANDARD.encode(bytes),
                mime: entry.mime,
                playback_rate: 1.0,
            }))
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn request_block(
        self: Arc<Self>,
        block: AudioBlock,
        profile: AudioProfile,
        job: Option<(String, u64)>,
        retry_before: Option<Instant>,
        confirmed_attempt: Option<String>,
    ) -> BlockResult<Option<SpeechAudio>> {
        if let Some(audio) = self.cached(block.id.clone()).await? {
            self.repair_cached_attempt(&block, &profile).await?;
            return Ok(Some(audio));
        }
        let credential_generation = self.credential_generation.load(Ordering::SeqCst);
        let id = block.id.clone();
        let manager = Arc::clone(&self);
        let first_block = block.clone();
        let first_profile = profile.clone();
        let first_consent = confirmed_attempt.clone();
        let interactive = job.is_none();
        let result = self
            .coalesce(id.clone(), retry_before, async move {
                manager
                    .produce_block(
                        first_block,
                        first_profile,
                        interactive,
                        credential_generation,
                        first_consent,
                    )
                    .await
            })
            .await?;
        if result.is_some() {
            return Ok(result);
        }
        if let Some(audio) = self.cached(id.clone()).await? {
            self.repair_cached_attempt(&block, &profile).await?;
            return Ok(Some(audio));
        }
        if !self.queue.needs(&id) {
            return Ok(None);
        }
        let manager = Arc::clone(&self);
        self.coalesce(id, retry_before, async move {
            manager
                .produce_block(
                    block,
                    profile,
                    interactive,
                    credential_generation,
                    confirmed_attempt,
                )
                .await
        })
        .await
    }
    async fn repair_cached_attempt(
        &self,
        block: &AudioBlock,
        profile: &AudioProfile,
    ) -> BlockResult<()> {
        if profile.engine != "openrouter" {
            return Ok(());
        }
        let block = block.clone();
        let profile = profile.clone();
        self.journal(move |journal| {
            if let Some(attempt) = journal.latest(&block, &profile)?
                && attempt.state == GenerationState::Uncertain
            {
                journal.complete(&attempt.id, None)?;
            }
            Ok(())
        })
        .await
    }
    #[allow(clippy::option_if_let_else)] // The vacant branch inserts into the same map.
    async fn coalesce<F>(
        self: &Arc<Self>,
        id: String,
        retry_before: Option<Instant>,
        operation: F,
    ) -> BlockResult<Option<SpeechAudio>>
    where
        F: std::future::Future<Output = BlockResult<Option<SpeechAudio>>> + Send + 'static,
    {
        let mut inflight = self.inflight.lock().await;
        let retry = inflight
            .get(&id)
            .and_then(|rx| rx.borrow().clone())
            .is_some_and(|result| match result {
                InflightResult::Failed(_, at) => retry_before.is_some_and(|cutoff| at <= cutoff),
                InflightResult::Paused => true,
                InflightResult::Audio(_) => false,
            });
        if retry {
            inflight.remove(&id);
        }
        let mut receiver = if let Some(receiver) = inflight.get(&id) {
            receiver.clone()
        } else {
            if inflight.len() >= 1024 {
                return Err("Слишком много ошибок озвучивания. Перезапустите приложение".into());
            }
            let permit = self.tasks.enter().ok_or_else(BlockError::cancelled)?;
            let (sender, receiver) = tokio::sync::watch::channel(None);
            inflight.insert(id.clone(), receiver.clone());
            let marker = receiver.clone();
            let manager = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                let _permit = permit;
                let outcome = if let Ok(result) = tauri::async_runtime::spawn(operation).await {
                    result
                } else {
                    let failed_id = id.clone();
                    let _ = manager
                        .journal(move |journal| journal.recover_block(&failed_id))
                        .await;
                    Err(
                        "Задача озвучивания завершилась аварийно; запустите воспроизведение снова"
                            .into(),
                    )
                };
                let result = match outcome {
                    Ok(Some(audio)) => InflightResult::Audio(Arc::new(audio)),
                    Ok(None) => InflightResult::Paused,
                    Err(error) => InflightResult::Failed(error, Instant::now()),
                };
                let remove = !matches!(&result, InflightResult::Failed(error, _) if error.code != BlockErrorCode::Cancelled);
                sender.send_replace(Some(result));
                if remove {
                    let mut inflight = manager.inflight.lock().await;
                    if inflight
                        .get(&id)
                        .is_some_and(|current| current.same_channel(&marker))
                    {
                        inflight.remove(&id);
                    }
                }
            });
            receiver
        };
        drop(inflight);
        loop {
            let value = receiver.borrow().clone();
            if let Some(result) = value {
                return match result {
                    InflightResult::Audio(audio) => Ok(Some(SpeechAudio {
                        data: audio.data.clone(),
                        mime: audio.mime.clone(),
                        playback_rate: audio.playback_rate,
                    })),
                    InflightResult::Failed(error, _) => Err(error),
                    InflightResult::Paused => Ok(None),
                };
            }
            receiver
                .changed()
                .await
                .map_err(|_| "Задача озвучивания прервана".to_owned())?;
        }
    }
    async fn produce_block(
        &self,
        block: AudioBlock,
        profile: AudioProfile,
        interactive: bool,
        credential_generation: u64,
        confirmed_attempt: Option<String>,
    ) -> BlockResult<Option<SpeechAudio>> {
        let waiting = interactive.then(|| {
            self.interactive.fetch_add(1, Ordering::SeqCst);
            InteractiveWaiter(&self.interactive)
        });
        let _guard = loop {
            if !self.queue.needs(&block.id) {
                return Ok(None);
            }
            if !interactive && self.interactive.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            let guard = self.gate.lock().await;
            if interactive || self.interactive.load(Ordering::SeqCst) == 0 {
                break guard;
            }
            drop(guard);
            tokio::task::yield_now().await;
        };
        drop(waiting);
        if !self.queue.needs(&block.id) {
            return Ok(None);
        }
        let cache = Arc::clone(&self.cache);
        let id = block.id.clone();
        let (hit, _pin) = tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
            cache.pin_active(&id).map_err(|e| e.to_string())?;
            let pin = ActivePin {
                cache: Arc::clone(&cache),
                id: id.clone(),
            };
            let hit = cache.get(&id).map_err(|e| e.to_string())?;
            Ok((hit, pin))
        })
        .await
        .map_err(|e| e.to_string())??;
        if let Some(hit) = hit {
            let data = tokio::fs::read(hit.path).await.map_err(|e| e.to_string())?;
            return Ok(Some(SpeechAudio {
                data: STANDARD.encode(data),
                mime: hit.mime,
                playback_rate: 1.0,
            }));
        }
        let audio = if profile.engine == "openrouter" {
            self.generate_openrouter(&block, &profile, credential_generation, confirmed_attempt)
                .await?
        } else {
            if !self.queue.try_submit(&block.id) {
                return Ok(None);
            }
            let _submission = Submission {
                queue: &self.queue,
                id: &block.id,
            };
            let audio = self
                .speech
                .synthesize_exact(&block.text, Some(&profile.voice), 1.0)
                .await
                .map_err(|e| e.to_string())?;
            self.save_audio(&block.id, &audio).await?;
            Some(audio)
        };
        Ok(audio)
    }
    async fn save_audio(&self, id: &str, audio: &SpeechAudio) -> BlockResult<()> {
        let bytes = STANDARD
            .decode(&audio.data)
            .map_err(|_| "Некорректное аудио".to_owned())?;
        let cache = Arc::clone(&self.cache);
        let id = id.to_owned();
        let mime = audio.mime.clone();
        tauri::async_runtime::spawn_blocking(move || -> IpcResult<()> {
            cache.put(&id, &mime, &bytes).map_err(|e| e.to_string())?;
            if cache.verify(&id).map_err(|e| e.to_string())?.is_none() {
                return Err("Не удалось проверить сохранённое аудио".into());
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok(())
    }
    async fn call_openrouter<F, Fut>(
        &self,
        block: &AudioBlock,
        profile: &AudioProfile,
        key: &str,
        callback: F,
    ) -> Result<SpeechAudio, readio_speech::SpeechError>
    where
        F: FnOnce(Option<String>) -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), readio_speech::SpeechError>> + Send,
    {
        #[cfg(test)]
        if let Some(provider) = &self.test_provider {
            return provider.synthesize(&block.id, callback).await;
        }
        self.speech
            .synthesize_with_openrouter_tracked_if_current(
                &block.text,
                key,
                profile.model.as_deref().ok_or_else(|| {
                    readio_speech::SpeechError::Engine("Выберите модель OpenRouter".into())
                })?,
                profile.model_voice.as_deref(),
                1.0,
                self.speech.cancellation_generation(),
                callback,
            )
            .await
    }
    async fn prepare_attempt(
        &self,
        block: &AudioBlock,
        profile: &AudioProfile,
        confirmed_attempt: Option<String>,
    ) -> BlockResult<desk_core::GenerationAttempt> {
        let block = block.clone();
        let profile = profile.clone();
        let prepared = self
            .journal(move |journal| {
                let prepared =
                    journal.prepare_with_consent(&block, &profile, confirmed_attempt.as_deref())?;
                if let GenerationPrepare::Ready(attempt) = &prepared {
                    journal.mark_submitted(&attempt.id)?;
                }
                Ok(prepared)
            })
            .await?;
        match prepared {
            GenerationPrepare::Ready(attempt) => Ok(attempt),
            GenerationPrepare::RequiresConfirmation(attempt) => {
                Err(BlockError::retry_required(attempt))
            }
        }
    }
    async fn generate_openrouter(
        &self,
        block: &AudioBlock,
        profile: &AudioProfile,
        credential_generation: u64,
        confirmed_attempt: Option<String>,
    ) -> BlockResult<Option<SpeechAudio>> {
        let model = profile
            .model
            .as_deref()
            .ok_or("Выберите модель OpenRouter")?;
        let credential_guard = self.credential_gate.lock().await;
        if self.credential_generation.load(Ordering::SeqCst) != credential_generation {
            return Err("Ключ OpenRouter изменился; запустите воспроизведение заново".into());
        }
        #[cfg(test)]
        let key = if self.test_provider.is_some() {
            Some(zeroize::Zeroizing::new("test-key".to_owned()))
        } else {
            tauri::async_runtime::spawn_blocking(credentials::read)
                .await
                .map_err(|e| e.to_string())??
        };
        #[cfg(not(test))]
        let key = tauri::async_runtime::spawn_blocking(credentials::read)
            .await
            .map_err(|e| e.to_string())??;
        let key = key.ok_or("Добавьте ключ OpenRouter в настройках")?;
        self.speech
            .validate_openrouter_request(
                &block.text,
                &key,
                model,
                profile.model_voice.as_deref(),
                1.0,
            )
            .map_err(|e| e.to_string())?;
        let attempt = self
            .prepare_attempt(block, profile, confirmed_attempt)
            .await?;
        if !self.queue.try_submit(&block.id) {
            self.journal(move |journal| journal.return_to_prepared(&attempt.id))
                .await?;
            return Ok(None);
        }
        let _submission = Submission {
            queue: &self.queue,
            id: &block.id,
        };
        let journal = Arc::clone(&self.journal);
        let attempt_id = attempt.id.clone();
        let callback = move |response_id: Option<String>| async move {
            tauri::async_runtime::spawn_blocking(move || {
                let journal = journal.lock().map_err(|e| e.to_string())?;
                journal
                    .note_response(&attempt_id, response_id.as_deref())
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| readio_speech::SpeechError::Engine(e.to_string()))?
            .map_err(readio_speech::SpeechError::Engine)
        };
        let mut future = std::pin::pin!(self.call_openrouter(block, profile, &key, callback));
        let first =
            std::future::poll_fn(|context| std::task::Poll::Ready(future.as_mut().poll(context)))
                .await;
        drop(credential_guard);
        let result = match first {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => future.await,
        };
        let outcome: BlockResult<SpeechAudio> = async {
            let audio = result.map_err(|e| e.to_string())?;
            self.save_audio(&block.id, &audio).await?;
            let attempt_id = attempt.id.clone();
            self.journal(move |journal| journal.complete(&attempt_id, None))
                .await?;
            Ok(audio)
        }
        .await;
        match outcome {
            Ok(audio) => Ok(Some(audio)),
            Err(error) => {
                let attempt_id = attempt.id.clone();
                self.journal(move |journal| journal.mark_uncertain(&attempt_id, None))
                    .await?;
                let mut blocked = BlockError::retry_required(attempt);
                blocked.state = Some(GenerationState::Uncertain);
                blocked.message = format!("{}. {}", error.message, blocked.message);
                Err(blocked)
            }
        }
    }
}

#[tauri::command]
pub async fn audio_plan(
    state: State<'_, AppState>,
    book_id: String,
    chapter: usize,
    offline: bool,
) -> IpcResult<AudioPlan> {
    let manager = Arc::clone(&state.offline);
    let (profile_id, blocks) = if offline {
        let (manifest, plan) = manager.saved_edition(book_id).await?;
        let chapter = plan.chapters.get(chapter).ok_or("Глава не найдена")?;
        (
            manifest.profile_id.clone(),
            chapter.blocks.iter().map(AudioBlockInfo::from).collect(),
        )
    } else {
        let (profile, plan) = manager.chapter_plan(book_id, chapter).await?;
        (
            profile.id(),
            plan.blocks.iter().map(AudioBlockInfo::from).collect(),
        )
    };
    Ok(AudioPlan {
        profile_id,
        blocks,
        offline,
    })
}
#[tauri::command]
pub async fn audio_session(state: State<'_, AppState>) -> IpcResult<String> {
    state
        .offline
        .queue
        .open_playback_session()
        .ok_or_else(|| "Приложение закрывается".into())
}
#[tauri::command]
pub async fn audio_interest(
    state: State<'_, AppState>,
    session: String,
    revision: u64,
    block_ids: Vec<String>,
) -> IpcResult<bool> {
    if session.len() > 64
        || block_ids.len() > 2
        || block_ids
            .iter()
            .any(|id| id.len() != 64 || !id.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err("Некорректный интерес к аудиоблокам".into());
    }
    Ok(state
        .offline
        .queue
        .update_playback(&session, revision, block_ids.into_iter().collect()))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioBlockRequest {
    book_id: String,
    chapter: usize,
    block_id: String,
    offline: bool,
    speed: f32,
    retry: Option<bool>,
    session: String,
    revision: u64,
    confirmed_attempt: Option<String>,
}
#[tauri::command]
pub async fn audio_block(
    state: State<'_, AppState>,
    request: AudioBlockRequest,
) -> BlockResult<SpeechAudio> {
    let AudioBlockRequest {
        book_id,
        chapter,
        block_id,
        offline,
        speed,
        retry,
        session,
        revision,
        confirmed_attempt,
    } = request;
    if !speed.is_finite() || !(0.5..=3.0).contains(&speed) {
        return Err("Скорость речи должна быть от 0,5 до 3".into());
    }
    let manager = Arc::clone(&state.offline);
    if !manager
        .queue
        .is_current_playback(&session, revision, &block_id)
    {
        return Err(BlockError::cancelled());
    }
    let (profile, block) = if offline {
        let (manifest, plan) = manager.saved_edition(book_id).await?;
        let block = plan
            .block(chapter, &block_id)
            .cloned()
            .ok_or("Аудиоблок не найден в сохранённом плане")?;
        (manifest.profile.clone(), block)
    } else {
        let (profile, plan) = manager.chapter_plan(book_id, chapter).await?;
        let block = plan
            .block(&block_id)
            .cloned()
            .ok_or("Аудиоблок не найден")?;
        (profile, block)
    };
    if !manager
        .queue
        .is_current_playback(&session, revision, &block_id)
    {
        return Err(BlockError::cancelled());
    }
    if offline {
        let cache = Arc::clone(&manager.cache);
        let id = block.id.clone();
        let audio = tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
            cache.pin_active(&id).map_err(|e| e.to_string())?;
            let _pin = ActivePin {
                cache: Arc::clone(&cache),
                id: id.clone(),
            };
            let entry = cache
                .get(&id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "Этот фрагмент ещё не загружен".to_owned())?;
            let bytes = std::fs::read(&entry.path).map_err(|e| e.to_string())?;
            Ok(SpeechAudio {
                data: STANDARD.encode(bytes),
                mime: entry.mime,
                playback_rate: speed,
            })
        })
        .await
        .map_err(|e| e.to_string())??;
        return Ok(audio);
    }
    let mut audio = manager
        .request_block(
            block,
            profile,
            None,
            retry.unwrap_or(false).then(Instant::now),
            confirmed_attempt,
        )
        .await?
        .ok_or_else(BlockError::cancelled)?;
    audio.playback_rate = speed;
    Ok(audio)
}
#[tauri::command]
pub async fn set_openrouter_billing_unit(
    state: State<'_, AppState>,
    model: String,
    unit: Option<BillingUnit>,
) -> IpcResult<desk_core::Preferences> {
    state.offline.set_billing_unit(model, unit).await
}
#[tauri::command]
pub async fn offline_quote(state: State<'_, AppState>, book_id: String) -> IpcResult<QuoteResult> {
    state.offline.quote(book_id).await
}
#[tauri::command]
pub async fn offline_start(
    state: State<'_, AppState>,
    book_id: String,
    quote_id: String,
    confirm_retries: Option<bool>,
) -> IpcResult<OfflineStatus> {
    state
        .offline
        .start(book_id, quote_id, confirm_retries.unwrap_or(false))
        .await
}
#[tauri::command]
pub async fn offline_status(
    state: State<'_, AppState>,
    book_id: String,
) -> IpcResult<OfflineStatus> {
    state.offline.status(&book_id).await
}
#[tauri::command]
pub async fn offline_diagnostics(
    state: State<'_, AppState>,
) -> IpcResult<Vec<desk_core::ManifestIssue>> {
    state.offline.diagnostics().await
}
#[tauri::command]
pub async fn offline_restore_manifest(
    state: State<'_, AppState>,
    file: String,
    backup_path: String,
) -> IpcResult<()> {
    let _permit = state
        .offline
        .tasks
        .enter()
        .ok_or("Приложение закрывается")?;
    let _plan_lifecycle = state.offline.plan_gate.lock().await;
    {
        let mut jobs = state.offline.jobs.lock().await;
        for (book_id, job) in jobs.iter_mut() {
            state.offline.queue.stop_offline(book_id, job.generation);
            job.state = "paused";
            job.generation = job.generation.wrapping_add(1);
        }
    }
    state.offline.quotes.lock().await.clear();
    let cache = Arc::clone(&state.offline.cache);
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        cache
            .restore_manifest(&file, std::path::Path::new(&backup_path))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?;
    {
        let _store = state.store.lock().map_err(|e| e.to_string())?;
        state.offline.invalidate_plans();
    }
    outcome
}
#[tauri::command]
pub async fn offline_list(state: State<'_, AppState>) -> IpcResult<Vec<OfflineStatus>> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for manifest in state.offline.manifests().await? {
        if seen.insert(manifest.book_id.clone()) {
            let status = state
                .offline
                .status(&manifest.book_id)
                .await
                .unwrap_or_else(|error| {
                    broken_status(manifest.book_id, Some(manifest.profile_id), error)
                });
            result.push(status);
        }
    }
    for issue in state.offline.diagnostics().await? {
        if let Some(book_id) = issue.book_id
            && seen.insert(book_id.clone())
        {
            result.push(broken_status(
                book_id,
                issue.profile_id,
                format!("Манифест издания повреждён: {}", issue.message),
            ));
        }
    }
    Ok(result)
}
const fn broken_status(
    book_id: String,
    profile_id: Option<String>,
    error: String,
) -> OfflineStatus {
    OfflineStatus {
        book_id,
        profile_id,
        state: "error",
        total_blocks: 0,
        cached_blocks: 0,
        bytes: 0,
        error: Some(error),
    }
}
#[tauri::command]
pub async fn offline_pause(
    state: State<'_, AppState>,
    book_id: String,
) -> IpcResult<OfflineStatus> {
    state.offline.pause(&book_id).await;
    state.offline.status(&book_id).await
}
#[tauri::command]
pub async fn offline_remove(
    state: State<'_, AppState>,
    book_id: String,
) -> IpcResult<OfflineStatus> {
    state.offline.remove_all(&book_id).await?;
    state.offline.status(&book_id).await
}
impl OfflineManager {
    async fn pause(&self, book_id: &str) {
        let mut jobs = self.jobs.lock().await;
        let job = jobs.entry(book_id.to_owned()).or_insert_with(|| Job {
            state: "paused",
            error: None,
            generation: 0,
            engine: String::new(),
        });
        self.queue.stop_offline(book_id, job.generation);
        job.state = "paused";
        job.generation = job.generation.wrapping_add(1);
        drop(jobs);
        self.wake.notify_waiters();
    }
    pub(crate) async fn set_billing_unit(
        &self,
        model: String,
        unit: Option<BillingUnit>,
    ) -> IpcResult<desk_core::Preferences> {
        if model.is_empty()
            || model.len() > 256
            || model.chars().any(char::is_whitespace)
            || model.chars().any(char::is_control)
        {
            return Err("Некорректный идентификатор модели".into());
        }
        let _credentials = self.credential_gate.lock().await;
        let store = Arc::clone(&self.store);
        let preferences = tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
            let store = store.lock().map_err(|e| e.to_string())?;
            let mut prefs = store.preferences().map_err(|e| e.to_string())?;
            if let Some(unit) = unit {
                if prefs.open_router_billing_units.len() >= 512
                    && !prefs.open_router_billing_units.contains_key(&model)
                {
                    return Err("Слишком много настроенных тарифов".into());
                }
                prefs.open_router_billing_units.insert(model, unit);
            } else {
                prefs.open_router_billing_units.remove(&model);
            }
            store.save_preferences(&prefs).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        self.quotes.lock().await.clear();
        Ok(preferences)
    }
    pub(crate) async fn change_credentials(
        &self,
        key: Option<zeroize::Zeroizing<String>>,
    ) -> IpcResult<()> {
        let _credentials = self.credential_gate.lock().await;
        self.credential_generation.fetch_add(1, Ordering::SeqCst);
        let mut jobs = self.jobs.lock().await;
        for (book_id, job) in jobs
            .iter_mut()
            .filter(|(_, job)| job.engine == "openrouter")
        {
            self.queue.stop_offline(book_id, job.generation);
            job.state = "paused";
            job.generation = job.generation.wrapping_add(1);
        }
        self.quotes.lock().await.clear();
        drop(jobs);
        tauri::async_runtime::spawn_blocking(move || {
            key.map_or_else(credentials::delete, |key| credentials::write(&key))
        })
        .await
        .map_err(|e| e.to_string())?
    }
    pub(crate) async fn remove_all(&self, book_id: &str) -> IpcResult<()> {
        let _plan_lifecycle = self.plan_gate.lock().await;
        {
            let _store = self.store.lock().map_err(|e| e.to_string())?;
            self.invalidate_plans();
        }
        let mut jobs = self.jobs.lock().await;
        let job = jobs.entry(book_id.to_owned()).or_insert_with(|| Job {
            state: "paused",
            error: None,
            generation: 0,
            engine: String::new(),
        });
        self.queue.stop_offline(book_id, job.generation);
        job.state = "paused";
        job.generation = job.generation.wrapping_add(1);
        let manifests = self.manifests().await?;
        let id = book_id.to_owned();
        let cache = Arc::clone(&self.cache);
        let outcome = tauri::async_runtime::spawn_blocking(move || -> IpcResult<()> {
            for manifest in manifests.into_iter().filter(|m| m.book_id == id) {
                cache
                    .remove_manifest(&id, &manifest.profile_id)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?;
        if let Some(job) = jobs.get_mut(book_id) {
            job.generation = job.generation.wrapping_add(1);
        }
        drop(jobs);
        self.quotes
            .lock()
            .await
            .retain(|_, quote| quote.manifest.book_id != book_id);
        {
            let _store = self.store.lock().map_err(|e| e.to_string())?;
            self.invalidate_plans();
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn manager() -> IpcResult<(Arc<OfflineManager>, std::path::PathBuf)> {
        let root = std::env::temp_dir().join(format!("readio-offline-test-{}", Uuid::new_v4()));
        let store = Arc::new(std::sync::Mutex::new(
            Store::open(&root).map_err(|e| e.to_string())?,
        ));
        Ok((OfflineManager::new(store, root.clone())?, root))
    }
    async fn cleanup(manager: Arc<OfflineManager>, root: std::path::PathBuf) -> IpcResult<()> {
        manager.shutdown().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while Arc::strong_count(&manager) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "Не завершились задачи тестового менеджера".to_owned())?;
        drop(manager);
        std::fs::remove_dir_all(root).map_err(|e| e.to_string())
    }
    #[tokio::test]
    async fn concurrent_failed_requests_share_error_and_require_explicit_retry() -> IpcResult<()> {
        let (manager, root) = manager()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let first_calls = Arc::clone(&calls);
        let second_calls = Arc::clone(&calls);
        let first = manager.coalesce("same".into(), None, async move {
            first_calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            Err("server failed".into())
        });
        let second = manager.coalesce("same".into(), None, async move {
            second_calls.fetch_add(1, Ordering::SeqCst);
            Err("duplicate".into())
        });
        let (first, second) = tokio::join!(first, second);
        assert_eq!(
            first.err().map(|e| e.message).as_deref(),
            Some("server failed")
        );
        assert_eq!(
            second.err().map(|e| e.message).as_deref(),
            Some("server failed")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let next_calls = Arc::clone(&calls);
        let next = manager
            .coalesce("same".into(), None, async move {
                next_calls.fetch_add(1, Ordering::SeqCst);
                Err("automatic retry".into())
            })
            .await;
        assert_eq!(
            next.err().map(|e| e.message).as_deref(),
            Some("server failed")
        );
        let retry_calls = Arc::clone(&calls);
        let retry = manager
            .coalesce("same".into(), Some(Instant::now()), async move {
                retry_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some(SpeechAudio {
                    data: "YQ==".into(),
                    mime: "audio/mpeg".into(),
                    playback_rate: 1.0,
                }))
            })
            .await?;
        assert!(retry.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        cleanup(manager, root).await?;
        Ok(())
    }
    #[tokio::test]
    async fn cached_openrouter_audio_needs_no_credentials_or_network() -> IpcResult<()> {
        let (manager, root) = manager()?;
        let profile = AudioProfile {
            engine: "openrouter".into(),
            voice: "openrouter".into(),
            model: Some("missing/model".into()),
            model_voice: None,
            dictionary: BTreeMap::default(),
            max_escaped_bytes: None,
        };
        let block = AudioBlock {
            id: "a".repeat(64),
            chapter: 0,
            start: 0,
            end: 1,
            passages: Vec::new(),
            text: "текст".into(),
            characters: 5,
        };
        manager
            .cache
            .pin_active(&block.id)
            .map_err(|e| e.to_string())?;
        manager
            .cache
            .put(&block.id, "audio/mpeg", b"cached")
            .map_err(|e| e.to_string())?;
        manager.cache.unpin_active(&block.id);
        let gate = manager.gate.lock().await;
        let audio = tokio::time::timeout(
            Duration::from_millis(500),
            Arc::clone(&manager).request_block(block, profile, None, None, None),
        )
        .await
        .map_err(|_| "Кэш заблокирован сетевым запросом".to_owned())??
        .ok_or_else(|| "Отменено".to_owned())?;
        drop(gate);
        assert_eq!(audio.data, STANDARD.encode(b"cached"));
        cleanup(manager, root).await?;
        Ok(())
    }
    #[test]
    fn quote_requires_documented_units_and_detects_rate_changes() {
        let profile = AudioProfile {
            engine: "openrouter".into(),
            voice: "openrouter".into(),
            model: Some("any/model".into()),
            model_voice: None,
            dictionary: BTreeMap::new(),
            max_escaped_bytes: None,
        };
        let price = Price {
            prompt: "0.000015".into(),
            completion: "0".into(),
            other: BTreeMap::new(),
        };
        let mut quote = Quote {
            plan: Arc::new(
                desk_core::EditionPlan::build(
                    &Book {
                        id: "book".into(),
                        ..Book::default()
                    },
                    &profile,
                )
                .unwrap(),
            ),
            manifest: OfflineManifest {
                book_id: "book".into(),
                profile_id: profile.id(),
                profile,
                block_ids: Vec::new(),
                plan_digest: None,
                updated_at: 0,
            },
            cached_blocks: 0,
            characters: 8,
            text_bytes: 17,
            pricing: Some(price.clone()),
            free: false,
            expires: Instant::now(),
            job_generation: 0,
            billing_unit: None,
            retry_attempts: HashMap::new(),
        };
        let result = cost(&quote);
        assert_eq!(result.unit, PricingUnit::Unknown);
        assert_eq!(
            result.reason,
            Some(UnknownCostReason::ProviderUnitUnavailable)
        );
        assert_eq!(result.amount, None);
        quote
            .pricing
            .as_mut()
            .map(|p| p.other.insert("audio".into(), "0.1".into()));
        assert!(!price_matches(&quote, Some(&price), false));
        quote.free = true;
        let result = cost(&quote);
        assert_eq!(result.unit, PricingUnit::Free);
        assert_eq!(result.amount.as_deref(), Some("0"));
        assert_eq!(result.reason, None);
    }
    #[test]
    fn decimal_costs_are_exact_and_never_underflow_to_free() {
        assert_eq!(
            multiply_decimal("0.000015", 17).as_deref(),
            Some("0.000255")
        );
        assert_eq!(multiply_decimal("1.25e-7", 8).as_deref(), Some("0.000001"));
        assert_eq!(multiply_decimal("0.0000", 200).as_deref(), Some("0"));
        assert!(multiply_decimal("1e-400", 1).is_none());
        assert!(multiply_decimal("-1", 1).is_none());
        assert!(multiply_decimal("1e128", 100).is_none());
    }
    #[test]
    fn explicit_units_compute_generic_decimal_cost() {
        let profile = AudioProfile {
            engine: "openrouter".into(),
            voice: "openrouter".into(),
            model: Some("any/provider-model".into()),
            model_voice: None,
            dictionary: BTreeMap::new(),
            max_escaped_bytes: None,
        };
        let mut quote = Quote {
            plan: Arc::new(
                desk_core::EditionPlan::build(
                    &Book {
                        id: "book".into(),
                        ..Book::default()
                    },
                    &profile,
                )
                .unwrap(),
            ),
            manifest: OfflineManifest {
                book_id: "book".into(),
                profile_id: profile.id(),
                profile,
                block_ids: Vec::new(),
                plan_digest: None,
                updated_at: 0,
            },
            cached_blocks: 0,
            characters: 8,
            text_bytes: 17,
            pricing: Some(Price {
                prompt: "0.000015".into(),
                completion: "0".into(),
                other: BTreeMap::new(),
            }),
            free: false,
            expires: Instant::now(),
            job_generation: 0,
            billing_unit: Some(BillingUnit::Utf8Byte),
            retry_attempts: HashMap::new(),
        };
        let result = cost(&quote);
        assert_eq!(result.unit, PricingUnit::Utf8Byte);
        assert_eq!(result.units, Some(17));
        assert_eq!(result.amount.as_deref(), Some("0.000255"));
        assert_eq!(result.source, Some(EstimateSource::User));
        quote.billing_unit = Some(BillingUnit::Character);
        let result = cost(&quote);
        assert_eq!(result.unit, PricingUnit::Character);
        assert_eq!(result.units, Some(8));
        assert_eq!(result.amount.as_deref(), Some("0.00012"));
        if let Some(price) = quote.pricing.as_mut() {
            price.completion = "1e-400".into();
        }
        let result = cost(&quote);
        assert_eq!(result.amount, None);
        assert_eq!(result.reason, Some(UnknownCostReason::InvalidRate));
        if let Some(price) = quote.pricing.as_mut() {
            price.completion = "0.1".into();
        }
        let result = cost(&quote);
        assert_eq!(
            result.reason,
            Some(UnknownCostReason::AdditionalRateUnitUnavailable)
        );
    }
    #[tokio::test]
    async fn pause_or_remove_invalidates_prepared_quotes() -> IpcResult<()> {
        let (manager, root) = manager()?;
        let source = root.join("sample.txt");
        std::fs::write(&source, "Привет.").map_err(|e| e.to_string())?;
        let id = manager
            .store
            .lock()
            .map_err(|e| e.to_string())?
            .import(&[source.to_string_lossy().into_owned()])
            .map_err(|e| e.to_string())?
            .remove(0)
            .id;
        let first = manager.quote(id.clone()).await?;
        manager.pause(&id).await;
        assert!(
            manager
                .start(id.clone(), first.quote_id, false)
                .await
                .is_err()
        );
        assert!(manager.manifests().await?.is_empty());
        let second = manager.quote(id.clone()).await?;
        manager.remove_all(&id).await?;
        assert!(
            manager
                .start(id.clone(), second.quote_id, false)
                .await
                .is_err()
        );
        assert!(manager.manifests().await?.is_empty());
        cleanup(manager, root).await?;
        Ok(())
    }
    #[tokio::test]
    async fn billing_unit_persists_and_invalidates_quotes() -> IpcResult<()> {
        let (manager, root) = manager()?;
        let source = root.join("sample.txt");
        std::fs::write(&source, "Привет.").map_err(|e| e.to_string())?;
        let id = manager
            .store
            .lock()
            .map_err(|e| e.to_string())?
            .import(&[source.to_string_lossy().into_owned()])
            .map_err(|e| e.to_string())?
            .remove(0)
            .id;
        let quote = manager.quote(id.clone()).await?;
        manager
            .set_billing_unit("any/model".into(), Some(BillingUnit::Utf8Byte))
            .await?;
        assert!(manager.start(id, quote.quote_id, false).await.is_err());
        assert_eq!(
            manager
                .store
                .lock()
                .map_err(|e| e.to_string())?
                .preferences()
                .map_err(|e| e.to_string())?
                .open_router_billing_units
                .get("any/model"),
            Some(&BillingUnit::Utf8Byte)
        );
        cleanup(manager, root).await?;
        Ok(())
    }
    #[tokio::test]
    async fn foreground_recovers_after_queued_background_is_paused() -> IpcResult<()> {
        let (manager, root) = manager()?;
        let profile = AudioProfile {
            engine: "openrouter".into(),
            voice: "openrouter".into(),
            model: Some("missing/model".into()),
            model_voice: None,
            dictionary: BTreeMap::default(),
            max_escaped_bytes: None,
        };
        let block = AudioBlock {
            id: "b".repeat(64),
            chapter: 0,
            start: 0,
            end: 1,
            passages: Vec::new(),
            text: "текст".into(),
            characters: 5,
        };
        let id = block.id.clone();
        let cache = Arc::clone(&manager.cache);
        let background = manager.coalesce(id.clone(), None, async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cache.pin_active(&id).map_err(|e| e.to_string())?;
            cache
                .put(&id, "audio/mpeg", b"cached")
                .map_err(|e| e.to_string())?;
            cache.unpin_active(&id);
            Ok(None)
        });
        let foreground = Arc::clone(&manager).request_block(block, profile, None, None, None);
        let (background, foreground) = tokio::join!(background, foreground);
        assert!(background?.is_none());
        assert_eq!(
            foreground?.ok_or_else(|| "Нет аудио".to_owned())?.data,
            STANDARD.encode(b"cached")
        );
        cleanup(manager, root).await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "offline_pipeline_tests.rs"]
mod pipeline_tests;

#[cfg(test)]
#[path = "offline_edition_tests.rs"]
mod edition_tests;

#[path = "offline_plans.rs"]
mod plans;
