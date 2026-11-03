use crate::ipc::OfflineJobState;
use crate::pricing::{
    CostEstimate, EstimateSource, PricingUnit, UnknownCostReason, decimal_add, decimal_add_compare,
    multiply_decimal, parse_decimal, unknown,
};
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
use tauri::{Emitter, State};
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::sync::Notify;
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
    pricing_source: Option<&'static str>,
    pricing_verified_at: Option<u64>,
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
    state: OfflineJobState,
    error: Option<String>,
    generation: u64,
    engine: String,
    budget: Option<BudgetState>,
}
#[derive(Clone)]
struct BudgetState {
    limits: OfflineBudget,
    pricing: Option<Price>,
    free: bool,
    billing_unit: Option<BillingUnit>,
    submitted: usize,
    actual: String,
    reserved: Option<(String, String)>,
    unknown: usize,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineStatus {
    book_id: String,
    profile_id: Option<String>,
    state: OfflineJobState,
    total_blocks: usize,
    cached_blocks: usize,
    bytes: u64,
    error: Option<String>,
    pending_requests: usize,
    actual_cost_usd: Option<String>,
    estimated_reserved_usd: Option<String>,
    unknown_cost_requests: usize,
    submitted_requests: usize,
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

fn cost(quote: &Quote) -> CostEstimate {
    estimate_cost(
        quote.pricing.as_ref(),
        quote.free,
        quote.billing_unit,
        quote.characters,
        quote.text_bytes,
    )
}
fn estimate_cost(
    pricing: Option<&Price>,
    free: bool,
    billing_unit: Option<BillingUnit>,
    characters: usize,
    text_bytes: usize,
) -> CostEstimate {
    if free {
        return CostEstimate {
            unit: PricingUnit::Free,
            units: None,
            amount: Some("0".into()),
            reason: None,
            source: Some(EstimateSource::Free),
        };
    }
    let Some(unit) = billing_unit else {
        return unknown(UnknownCostReason::ProviderUnitUnavailable);
    };
    let Some(price) = pricing else {
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
        BillingUnit::Character => (PricingUnit::Character, characters),
        BillingUnit::Utf8Byte => (PricingUnit::Utf8Byte, text_bytes),
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
type QuoteScan = (Vec<String>, usize, usize, usize, HashMap<String, String>);
fn scan_quote(
    plan: &EditionPlan,
    profile: &AudioProfile,
    journal: &std::sync::Mutex<GenerationJournal>,
    mut verify: impl FnMut(&str) -> IpcResult<bool>,
) -> IpcResult<QuoteScan> {
    let mut ids = Vec::new();
    let mut cached = 0;
    let mut chars = 0_usize;
    let mut bytes = 0_usize;
    let mut missing = HashSet::new();
    let mut retry_attempts = HashMap::new();
    for chapter in &plan.chapters {
        for block in &chapter.blocks {
            if verify(&block.id)? {
                cached += 1;
            } else if missing.insert(block.id.clone()) {
                chars = chars.saturating_add(block.characters);
                bytes = bytes.saturating_add(block.text.len());
                if profile.engine == "openrouter"
                    && let Some(attempt) = journal
                        .lock()
                        .map_err(|e| e.to_string())?
                        .latest(block, profile)
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

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineBudget {
    max_cost_usd: Option<String>,
    max_new_blocks: Option<usize>,
    allow_unknown_cost: bool,
}

type SharedPlanner = Arc<std::sync::Mutex<Option<(String, Arc<desk_core::ChapterPlanner>)>>>;

pub struct OfflineManager {
    store: Arc<std::sync::Mutex<Store>>,
    cache: Arc<AudioCache>,
    speech: Arc<SpeechService>,
    gate: Mutex<()>,
    interactive: AtomicUsize,
    credential_generation: AtomicU64,
    reconciliation_cursor: AtomicUsize,
    jobs: Mutex<HashMap<String, Job>>,
    quotes: Mutex<HashMap<String, Quote>>,
    credential_gate: Mutex<()>,
    inflight: Mutex<HashMap<String, tokio::sync::watch::Receiver<Option<InflightResult>>>>,
    journal: Arc<std::sync::Mutex<GenerationJournal>>,
    queue: WorkQueue,
    tasks: Arc<Tasks>,
    mutation_tasks: Arc<Tasks>,
    final_saves: Arc<Tasks>,
    chapter_plans: Arc<PlanCache<plans::ChapterPlan>>,
    edition_plans: Arc<PlanCache<EditionPlan>>,
    manifest_plans: Arc<PlanCache<OfflineManifest>>,
    planner: SharedPlanner,
    plan_gate: Arc<Mutex<()>>,
    event_handle: std::sync::Mutex<Option<tauri::AppHandle>>,
    #[cfg(test)]
    chapter_builds: Arc<AtomicUsize>,
    #[cfg(test)]
    coalesced_waiters: Arc<AtomicUsize>,
    #[cfg(test)]
    coalesced_signal: Arc<Notify>,
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
            speech: Arc::new(SpeechService::new(root).map_err(|e| e.to_string())?),
            gate: Mutex::new(()),
            interactive: AtomicUsize::new(0),
            credential_generation: AtomicU64::new(0),
            reconciliation_cursor: AtomicUsize::new(0),
            jobs: Mutex::new(HashMap::new()),
            quotes: Mutex::new(HashMap::new()),
            credential_gate: Mutex::new(()),
            inflight: Mutex::new(HashMap::new()),
            journal: Arc::new(std::sync::Mutex::new(journal)),
            queue: WorkQueue::default(),
            tasks: Tasks::new(),
            mutation_tasks: Tasks::new(),
            final_saves: Tasks::new(),
            chapter_plans: Arc::new(PlanCache::new(256 * 1024 * 1024)),
            edition_plans: Arc::new(PlanCache::new(256 * 1024 * 1024)),
            manifest_plans: Arc::new(PlanCache::new(16 * 1024 * 1024)),
            planner: Arc::new(std::sync::Mutex::new(None)),
            plan_gate: Arc::new(Mutex::new(())),
            event_handle: std::sync::Mutex::new(None),
            #[cfg(test)]
            chapter_builds: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            coalesced_waiters: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            coalesced_signal: Arc::new(Notify::new()),
            #[cfg(test)]
            test_provider: None,
        }))
    }
    pub(crate) fn set_event_handle(&self, handle: tauri::AppHandle) {
        if let Ok(mut event_handle) = self.event_handle.lock() {
            *event_handle = Some(handle);
        }
    }
    async fn emit_status(&self, book_id: &str) {
        let handle = self
            .event_handle
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        if let Some(handle) = handle
            && let Ok(status) = self.status(book_id).await
        {
            let _ = handle.emit("offline-status-changed", status);
        }
    }
    pub(crate) fn begin_shutdown(&self) {
        self.queue.close();
        self.tasks.close();
        self.mutation_tasks.close();
    }
    pub(crate) fn mutation_permit(&self) -> IpcResult<crate::tasks::TaskPermit> {
        self.mutation_tasks
            .enter()
            .ok_or_else(|| "Приложение закрывается".into())
    }
    pub(crate) async fn drain_mutations(&self) {
        self.final_saves.close();
        self.mutation_tasks.wait().await;
        self.final_saves.wait().await;
    }
    pub(crate) fn final_save_permit(&self) -> IpcResult<crate::tasks::TaskPermit> {
        self.final_saves
            .enter()
            .ok_or_else(|| "Приложение закрывается".into())
    }
    pub(crate) async fn run_mutation<T: Send + 'static>(
        &self,
        operation: impl std::future::Future<Output = IpcResult<T>> + Send + 'static,
    ) -> IpcResult<T> {
        let permit = self.mutation_permit()?;
        tauri::async_runtime::spawn(async move {
            let _permit = permit;
            operation.await
        })
        .await
        .map_err(|error| error.to_string())?
    }
    async fn invalidate_under_store(&self) -> IpcResult<()> {
        let store = Arc::clone(&self.store);
        let chapter = Arc::clone(&self.chapter_plans);
        let edition = Arc::clone(&self.edition_plans);
        let manifests = Arc::clone(&self.manifest_plans);
        tauri::async_runtime::spawn_blocking(move || {
            let _store = store.lock().map_err(|e| e.to_string())?;
            chapter.invalidate();
            edition.invalidate();
            manifests.invalidate();
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    pub(crate) async fn shutdown(&self) {
        self.begin_shutdown();
        {
            let mut jobs = self.jobs.lock().await;
            for job in jobs
                .values_mut()
                .filter(|job| job.state == OfflineJobState::Running)
            {
                job.state = OfflineJobState::Paused;
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
        let speech = Arc::clone(&self.speech);
        tauri::async_runtime::spawn_blocking(move || {
            let store = store.lock().map_err(|e| e.to_string())?;
            let book = store.book(&id).map_err(|e| e.to_string())?;
            let mut profile = if offline {
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
            drop(store);
            if !offline {
                Self::resolve_resource_revision(&speech, &mut profile)?;
            }
            Ok((book, profile))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    fn resolve_resource_revision(
        speech: &SpeechService,
        profile: &mut AudioProfile,
    ) -> IpcResult<()> {
        if profile.engine == "piper" {
            let revision = speech
                .piper_resource_revision(&profile.voice)
                .map_err(|e| e.to_string())?;
            profile
                .synthesis
                .get_or_insert_with(Default::default)
                .resource_revision = Some(revision);
        }
        Ok(())
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
    #[allow(clippy::too_many_lines)] // Status combines cache state and durable provider accounting.
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
                state: OfflineJobState::None,
                total_blocks: 0,
                cached_blocks: 0,
                bytes: 0,
                error: None,
                pending_requests: 0,
                actual_cost_usd: None,
                estimated_reserved_usd: None,
                unknown_cost_requests: 0,
                submitted_requests: 0,
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
        let history_ids = manifest.block_ids.clone();
        let attempts = self
            .journal(move |journal| journal.attempts_for_blocks(&history_ids))
            .await?;
        let mut costs: HashMap<String, Option<String>> = HashMap::new();
        let mut missing_ids = 0_usize;
        let mut submitted_history = 0_usize;
        for attempt in attempts {
            if attempt.state == GenerationState::Prepared {
                continue;
            }
            submitted_history = submitted_history.saturating_add(1);
            if let Some(id) = attempt.provider_response_id {
                let entry = costs.entry(id).or_insert(None);
                if attempt.provider_cost.is_some() {
                    *entry = attempt.provider_cost;
                }
            } else {
                missing_ids = missing_ids.saturating_add(1);
            }
        }
        let mut historical_cost = None::<String>;
        let mut historical_unknown = missing_ids;
        for cost in costs.into_values() {
            if let Some(cost) = cost {
                historical_cost = Some(
                    decimal_add(historical_cost.as_deref().unwrap_or("0"), &cost)
                        .ok_or("Не удалось точно сложить историю расходов")?,
                );
            } else {
                historical_unknown = historical_unknown.saturating_add(1);
            }
        }
        let missing_after_completion = cached_blocks != manifest.block_ids.len()
            && job
                .as_ref()
                .is_some_and(|job| job.state == OfflineJobState::Complete);
        let state = if job
            .as_ref()
            .is_some_and(|job| job.state == OfflineJobState::Running)
        {
            OfflineJobState::Running
        } else if cached_blocks == manifest.block_ids.len() {
            OfflineJobState::Complete
        } else if missing_after_completion {
            OfflineJobState::Error
        } else {
            job.as_ref()
                .map_or(OfflineJobState::Paused, |job| job.state)
        };
        let accounting = job.as_ref().and_then(|job| job.budget.as_ref());
        let actual_cost_usd = historical_cost;
        let estimated_reserved_usd = accounting
            .and_then(|budget| budget.reserved.as_ref().map(|(_, value)| value.clone()))
            .filter(|value| !value.is_empty());
        let unknown_cost_requests = historical_unknown;
        let submitted_requests = submitted_history;
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
            pending_requests: self.queue.pending_for_book(book_id),
            actual_cost_usd,
            estimated_reserved_usd,
            unknown_cost_requests,
            submitted_requests,
        })
    }
    async fn price(&self, profile: &AudioProfile) -> IpcResult<(Option<Price>, bool)> {
        if profile.engine != "openrouter" {
            return Ok((None, true));
        }
        #[cfg(test)]
        if let Some(provider) = &self.test_provider {
            let (prompt, completion, free) = provider.price();
            return Ok((
                Some(Price {
                    prompt,
                    completion,
                    other: std::collections::BTreeMap::default(),
                }),
                free,
            ));
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
            if jobs
                .get(&book_id)
                .is_some_and(|job| job.state == OfflineJobState::Running)
            {
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
                scan_quote(&plan_copy, &profile_copy, &journal, |id| {
                    cache
                        .verify(id)
                        .map(|entry| entry.is_some())
                        .map_err(|e| e.to_string())
                })
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
            pricing_source: quote.pricing.as_ref().map(|_| "openrouter_api"),
            pricing_verified_at: quote.pricing.as_ref().map(|_| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |v| v.as_millis().try_into().unwrap_or(u64::MAX))
            }),
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
        budget: OfflineBudget,
    ) -> BlockResult<OfflineStatus> {
        let permit = self.tasks.enter().ok_or("Приложение закрывается")?;
        let _credentials = self.credential_gate.lock().await;
        let quote =
            self.quotes.lock().await.remove(&quote_id).ok_or_else(|| {
                BlockError::stale_quote("Стоимость устарела; запросите оценку снова")
            })?;
        if budget
            .max_cost_usd
            .as_ref()
            .is_some_and(|value| parse_decimal(value).is_none())
        {
            return Err("Некорректный денежный лимит".into());
        }
        if quote.manifest.profile.engine == "openrouter" && quote.characters != 0 {
            if budget.max_new_blocks.is_none_or(|limit| limit == 0) {
                return Err("Укажите максимальное число новых запросов".into());
            }
            if cost(&quote).amount.is_none() && !budget.allow_unknown_cost {
                return Err("Подтвердите загрузку с неизвестной стоимостью".into());
            }
            if cost(&quote).amount.is_none() && budget.max_cost_usd.is_some() {
                return Err("Денежный лимит нельзя применить без оценки стоимости запроса".into());
            }
            if budget.max_cost_usd.is_some()
                && self.status(&book_id).await?.unknown_cost_requests > 0
            {
                return Err("Сначала проверьте неизвестный расход предыдущих запросов".into());
            }
        }
        if self.queue.pending_for_book(&book_id) != 0 {
            return Err("Дождитесь завершения отправленного запроса".into());
        }
        if quote.manifest.book_id != book_id || quote.expires <= Instant::now() {
            return Err(BlockError::stale_quote(
                "Стоимость устарела; запросите оценку снова",
            ));
        }
        if !quote.retry_attempts.is_empty() && !confirm_retries {
            return Err("Подтвердите повтор ранее отправленных запросов".into());
        }
        if quote.characters != 0 {
            let (pricing, free) = self.price(&quote.manifest.profile).await?;
            if !price_matches(&quote, pricing.as_ref(), free) {
                return Err(BlockError::stale_quote(
                    "Тариф изменился; запросите оценку снова",
                ));
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
            return Err(BlockError::stale_quote(
                "Офлайн-издание изменилось; запросите оценку снова",
            ));
        }
        if !existing && profile != quote.manifest.profile {
            return Err(BlockError::stale_quote(
                "Настройки озвучивания изменились; запросите оценку снова",
            ));
        }
        let mut jobs = self.jobs.lock().await;
        if jobs.get(&book_id).map_or(0, |job| job.generation) != quote.job_generation {
            return Err(BlockError::stale_quote(
                "Состояние загрузки изменилось; запросите оценку снова",
            ));
        }
        if jobs
            .get(&book_id)
            .is_some_and(|j| j.state == OfflineJobState::Running)
        {
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
            return Err(BlockError::stale_quote(
                "Текст книги или кеш изменились; запросите оценку снова",
            ));
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
                state: OfflineJobState::Running,
                error: None,
                generation,
                engine: quote.manifest.profile.engine.clone(),
                budget: Some(BudgetState {
                    limits: budget.clone(),
                    pricing: quote.pricing.clone(),
                    free: quote.free,
                    billing_unit: quote.billing_unit,
                    submitted: 0,
                    actual: "0".into(),
                    reserved: None,
                    unknown: 0,
                }),
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
                        plan: Arc::clone(&quote.plan),
                        profile: quote.manifest.profile.clone(),
                        generation,
                        started_at,
                        allowed,
                        retry_attempts: quote.retry_attempts.clone(),
                    })
                    .await;
            })
            .await;
            if outcome.is_err() {
                let mut jobs = manager.jobs.lock().await;
                if let Some(job) = jobs.get_mut(&id)
                    && job.generation == generation
                {
                    job.state = OfflineJobState::Error;
                    job.error =
                        Some("Загрузка завершилась аварийно; запросите новый расчёт".into());
                }
                manager.queue.stop_offline(&id, generation);
                drop(jobs);
            }
            manager.emit_status(&id).await;
        });
        self.emit_status(&book_id).await;
        self.status(&book_id).await.map_err(Into::into)
    }
    #[allow(clippy::too_many_lines, clippy::single_match_else)] // The worker owns sequential budget and state transitions.
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
                    if !jobs.get(&book_id).is_some_and(|j| {
                        j.generation == generation && j.state == OfflineJobState::Running
                    }) {
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
                self.emit_status(&book_id).await;
                if let Err(error) = result {
                    let mut jobs = self.jobs.lock().await;
                    if let Some(job) = jobs.get_mut(&book_id)
                        && job.generation == generation
                        && job.state == OfflineJobState::Running
                    {
                        if let Some(budget) = &mut job.budget
                            && budget
                                .reserved
                                .as_ref()
                                .is_some_and(|(id, _)| id == &block.id)
                        {
                            budget.unknown = budget.unknown.saturating_add(1);
                        }
                        job.state = OfflineJobState::Error;
                        job.error = Some(error.message);
                        self.queue.stop_offline(&book_id, generation);
                    }
                    drop(jobs);
                    return;
                }
                if profile.engine == "openrouter" {
                    let reserved = self
                        .jobs
                        .lock()
                        .await
                        .get(&book_id)
                        .and_then(|job| job.budget.as_ref())
                        .and_then(|budget| budget.reserved.as_ref())
                        .is_some_and(|(id, _)| id == &block.id);
                    if reserved {
                        let usage = self.reconcile_cost(block.clone(), profile.clone()).await;
                        let mut jobs = self.jobs.lock().await;
                        if let Some(job) = jobs.get_mut(&book_id)
                            && job.generation == generation
                            && let Some(budget) = &mut job.budget
                        {
                            match usage {
                                Ok(value) => {
                                    if let Some(sum) = decimal_add(&budget.actual, &value) {
                                        budget.actual = sum;
                                        budget.reserved = None;
                                    } else {
                                        budget.unknown += 1;
                                        job.state = OfflineJobState::Paused;
                                        job.error = Some("Не удалось точно сложить расход".into());
                                    }
                                }
                                Err(_) => {
                                    budget.unknown += 1;
                                    if budget.limits.max_cost_usd.is_some() {
                                        job.state = OfflineJobState::Paused;
                                        job.error = Some("Расход запроса ещё неизвестен; проверьте его перед продолжением".into());
                                    }
                                }
                            }
                            if job.state != OfflineJobState::Running {
                                self.queue.stop_offline(&book_id, generation);
                            }
                        }
                        drop(jobs);
                        self.emit_status(&book_id).await;
                    }
                }
            }
        }
        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get_mut(&book_id)
            && job.generation == generation
            && job.state == OfflineJobState::Running
        {
            job.state = OfflineJobState::Complete;
            self.queue.stop_offline(&book_id, generation);
        }
    }
    async fn reconcile_cost(&self, block: AudioBlock, profile: AudioProfile) -> IpcResult<String> {
        let attempt = self
            .journal(move |journal| journal.latest(&block, &profile))
            .await?
            .ok_or("Нет записи о запросе")?;
        self.reconcile_attempt(attempt).await
    }
    async fn reconcile_attempt(&self, attempt: desk_core::GenerationAttempt) -> IpcResult<String> {
        if let Some(cost) = attempt.provider_cost {
            return Ok(cost);
        }
        let id = attempt
            .provider_response_id
            .ok_or("Поставщик не сообщил ID запроса")?;
        #[cfg(test)]
        if let Some(provider) = &self.test_provider {
            let cost = provider.usage(&id)?;
            let attempt_id = attempt.id;
            let saved_cost = cost.clone();
            self.journal(move |journal| journal.note_cost(&attempt_id, &saved_cost))
                .await?;
            return Ok(cost);
        }
        let key = tauri::async_runtime::spawn_blocking(credentials::read)
            .await
            .map_err(|e| e.to_string())??
            .ok_or("Добавьте ключ OpenRouter в настройках")?;
        let cost = self
            .speech
            .openrouter_generation_cost(&key, &id)
            .await
            .map_err(|e| e.to_string())?;
        if parse_decimal(&cost).is_none() {
            return Err("Поставщик сообщил некорректный расход".into());
        }
        let attempt_id = attempt.id;
        let saved_cost = cost.clone();
        self.journal(move |journal| journal.note_cost(&attempt_id, &saved_cost))
            .await?;
        Ok(cost)
    }
    async fn cached(&self, id: String) -> IpcResult<Option<SpeechAudio>> {
        let cache = Arc::clone(&self.cache);
        tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
            cache.pin_active(&id).map_err(|e| e.to_string())?;
            let _pin = ActivePin {
                cache: Arc::clone(&cache),
                id: id.clone(),
            };
            let Some(entry) = cache.get_bytes(&id).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            Ok(Some(SpeechAudio {
                data: STANDARD.encode(entry.data),
                mime: entry.mime,
                playback_rate: 1.0,
                timings: Vec::new(),
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
        let permit = self.mutation_permit()?;
        self.journal(move |journal| {
            let _permit = permit;
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
            #[cfg(test)]
            {
                self.coalesced_waiters.fetch_add(1, Ordering::SeqCst);
                self.coalesced_signal.notify_waiters();
            }
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
                        timings: audio.timings.clone(),
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
    #[allow(clippy::too_many_lines)] // Resource revision is checked around synthesis and cache write.
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
            let hit = cache.get_bytes(&id).map_err(|e| e.to_string())?;
            Ok((hit, pin))
        })
        .await
        .map_err(|e| e.to_string())??;
        if let Some(hit) = hit {
            return Ok(Some(SpeechAudio {
                data: STANDARD.encode(hit.data),
                mime: hit.mime,
                playback_rate: 1.0,
                timings: Vec::new(),
            }));
        }
        let audio = if profile.engine == "openrouter" {
            self.generate_openrouter(&block, &profile, credential_generation, confirmed_attempt)
                .await?
        } else {
            if profile.engine == "piper" {
                let expected = profile
                    .synthesis
                    .as_ref()
                    .and_then(|s| s.resource_revision.clone())
                    .ok_or(
                        "Для старой версии Piper нельзя безопасно создать недостающий фрагмент",
                    )?;
                let speech = Arc::clone(&self.speech);
                let voice = profile.voice.clone();
                let actual = tauri::async_runtime::spawn_blocking(move || {
                    speech.piper_resource_revision(&voice)
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
                if actual != expected {
                    return Err("Файлы Piper изменились; создайте новое издание".into());
                }
            }
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
            if profile.engine == "piper" {
                let speech = Arc::clone(&self.speech);
                let voice = profile.voice.clone();
                let expected = profile
                    .synthesis
                    .as_ref()
                    .and_then(|s| s.resource_revision.clone());
                let actual = tauri::async_runtime::spawn_blocking(move || {
                    speech.piper_resource_revision(&voice)
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
                if Some(actual) != expected {
                    return Err("Файлы Piper изменились во время синтеза".into());
                }
            }
            self.save_audio(&block, &audio).await?;
            Some(audio)
        };
        Ok(audio)
    }
    async fn save_audio(&self, block: &AudioBlock, audio: &SpeechAudio) -> BlockResult<()> {
        let bytes = STANDARD
            .decode(&audio.data)
            .map_err(|_| "Некорректное аудио".to_owned())?;
        let cache = Arc::clone(&self.cache);
        let id = block.id.clone();
        let mime = audio.mime.clone();
        let text_length = block.text.encode_utf16().count();
        let timings = audio
            .timings
            .iter()
            .map(|mark| desk_core::AudioTiming {
                start_utf16: mark.start_utf16,
                end_utf16: mark.end_utf16,
                start_seconds: mark.start_seconds,
                end_seconds: mark.end_seconds,
            })
            .collect::<Vec<_>>();
        if timings.iter().any(|mark| mark.end_utf16 > text_length) {
            return Err("Метки синтеза выходят за границы текста".into());
        }
        tauri::async_runtime::spawn_blocking(move || -> IpcResult<()> {
            cache
                .put_with_timings(&id, &mime, &bytes, &timings)
                .map_err(|e| e.to_string())?;
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
                crate::transport_for(profile.synthesis.as_ref()),
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
    #[allow(clippy::too_many_lines)] // Submission, budget reservation and journal transitions form one transaction boundary.
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
        crate::transport_for(profile.synthesis.as_ref())
            .validate()
            .map_err(|e| e.to_string())?;
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
        let owners = self.queue.submitted_books(&block.id);
        let mut reserved_books = Vec::new();
        {
            let mut jobs = self.jobs.lock().await;
            for book in owners {
                let Some(job) = jobs.get_mut(&book) else {
                    continue;
                };
                let Some(budget) = &mut job.budget else {
                    continue;
                };
                if job.state != OfflineJobState::Running {
                    continue;
                }
                let estimate = estimate_cost(
                    budget.pricing.as_ref(),
                    budget.free,
                    budget.billing_unit,
                    block.characters,
                    block.text.len(),
                )
                .amount;
                let over_count = budget
                    .limits
                    .max_new_blocks
                    .is_some_and(|limit| budget.submitted >= limit);
                let over_cost = budget.limits.max_cost_usd.as_ref().is_some_and(|limit| {
                    estimate.as_deref().is_none_or(|amount| {
                        !matches!(
                            decimal_add_compare(&budget.actual, amount, limit),
                            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                        )
                    })
                });
                if over_count || over_cost || budget.reserved.is_some() {
                    job.state = OfflineJobState::Paused;
                    job.error = Some(
                        "Достигнут лимит загрузки или предыдущий расход ещё неизвестен".into(),
                    );
                    self.queue.stop_offline(&book, job.generation);
                } else {
                    budget.submitted += 1;
                    budget.reserved = Some((block.id.clone(), estimate.unwrap_or_default()));
                    reserved_books.push(book);
                }
            }
            drop(jobs);
        }
        if !self.queue.needs(&block.id) {
            self.queue.complete(&block.id);
            let mut jobs = self.jobs.lock().await;
            for book in reserved_books {
                if let Some(budget) = jobs.get_mut(&book).and_then(|job| job.budget.as_mut())
                    && budget
                        .reserved
                        .as_ref()
                        .is_some_and(|(id, _)| id == &block.id)
                {
                    budget.submitted = budget.submitted.saturating_sub(1);
                    budget.reserved = None;
                }
            }
            drop(jobs);
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
            self.save_audio(block, &audio).await?;
            let attempt_id = attempt.id.clone();
            self.journal(move |journal| journal.complete(&attempt_id, None))
                .await?;
            Ok(audio)
        }
        .await;
        match outcome {
            Ok(audio) => {
                #[cfg(not(test))]
                {
                    let speech = Arc::clone(&self.speech);
                    let journal = Arc::clone(&self.journal);
                    let attempt_id = attempt.id.clone();
                    let key = key.clone();
                    if let Some(permit) = self.tasks.enter() {
                        tauri::async_runtime::spawn(async move {
                            let _permit = permit;
                            let id_for_lookup = attempt_id.clone();
                            let journal_for_lookup = Arc::clone(&journal);
                            let record = tauri::async_runtime::spawn_blocking(move || {
                                journal_for_lookup
                                    .lock()
                                    .ok()?
                                    .attempt(&id_for_lookup)
                                    .ok()
                                    .flatten()
                            })
                            .await
                            .ok()
                            .flatten();
                            if let Some(record) = record
                                && record.provider_cost.is_none()
                                && let Some(response_id) = record.provider_response_id
                                && let Ok(cost) =
                                    speech.openrouter_generation_cost(&key, &response_id).await
                            {
                                let _ = tauri::async_runtime::spawn_blocking(move || {
                                    if let Ok(journal) = journal.lock() {
                                        let _ = journal.note_cost(&attempt_id, &cost);
                                    }
                                })
                                .await;
                            }
                        });
                    }
                }
                Ok(Some(audio))
            }
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
) -> Result<AudioPlan, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
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
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn audio_session(state: State<'_, AppState>) -> Result<String, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        state.audio_resources.clear();
        state
            .offline
            .queue
            .open_playback_session()
            .ok_or_else(|| "Приложение закрывается".into())
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn audio_interest(
    state: State<'_, AppState>,
    session: String,
    revision: u64,
    block_ids: Vec<String>,
) -> Result<bool, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
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
    .await;
    result.map_err(Into::into)
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
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioBlockResponse {
    resource_id: String,
    mime: String,
    playback_rate: f32,
    timings: Vec<desk_core::AudioTiming>,
}
#[tauri::command]
#[allow(clippy::too_many_lines)] // Validates session, source and resource lease in one operation.
pub async fn audio_block(
    state: State<'_, AppState>,
    request: AudioBlockRequest,
) -> BlockResult<AudioBlockResponse> {
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
    {
        let cache = Arc::clone(&manager.cache);
        let id = block.id.clone();
        let cached = tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
            cache.pin_active(&id).map_err(|e| e.to_string())?;
            let _pin = ActivePin {
                cache: Arc::clone(&cache),
                id: id.clone(),
            };
            cache.get_bytes(&id).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        if let Some(audio) = cached {
            manager.repair_cached_attempt(&block, &profile).await?;
            let resource_id = state
                .audio_resources
                .insert(audio.data, audio.mime.clone())?;
            return Ok(AudioBlockResponse {
                resource_id,
                mime: audio.mime,
                playback_rate: speed,
                timings: audio.timings,
            });
        }
        if offline {
            return Err("Этот фрагмент ещё не загружен".into());
        }
    }
    let block_id = block.id.clone();
    manager
        .cache
        .pin_active(&block_id)
        .map_err(|e| e.to_string())?;
    let _pin = ActivePin {
        cache: Arc::clone(&manager.cache),
        id: block_id.clone(),
    };
    Arc::clone(&manager)
        .request_block(
            block,
            profile,
            None,
            retry.unwrap_or(false).then(Instant::now),
            confirmed_attempt,
        )
        .await?
        .ok_or_else(BlockError::cancelled)?;
    let cache = Arc::clone(&manager.cache);
    let audio = tauri::async_runtime::spawn_blocking(move || cache.get_bytes(&block_id))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?
        .ok_or("Сохранённый аудиоблок недоступен")?;
    let resource_id = state
        .audio_resources
        .insert(audio.data, audio.mime.clone())?;
    Ok(AudioBlockResponse {
        resource_id,
        mime: audio.mime,
        playback_rate: speed,
        timings: audio.timings,
    })
}
#[tauri::command]
pub async fn set_openrouter_billing_unit(
    state: State<'_, AppState>,
    model: String,
    unit: Option<BillingUnit>,
) -> Result<desk_core::Preferences, crate::ipc::IpcError> {
    let manager = Arc::clone(&state.offline);
    let result = state
        .offline
        .run_mutation(async move { manager.set_billing_unit(model, unit).await })
        .await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn offline_quote(
    state: State<'_, AppState>,
    book_id: String,
) -> Result<QuoteResult, crate::ipc::IpcError> {
    let result: IpcResult<_> = async { state.offline.quote(book_id).await }.await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn reconcile_generation_cost(
    state: State<'_, AppState>,
    attempt_id: String,
) -> Result<String, crate::ipc::IpcError> {
    let manager = Arc::clone(&state.offline);
    let result: IpcResult<_> = state
        .offline
        .run_mutation(async move {
            let attempt = manager
                .journal(move |journal| journal.attempt(&attempt_id))
                .await?
                .ok_or("Попытка генерации не найдена")?;
            manager.reconcile_attempt(attempt).await
        })
        .await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn reconcile_book_cost(
    state: State<'_, AppState>,
    book_id: String,
) -> Result<OfflineStatus, crate::ipc::IpcError> {
    let manager = Arc::clone(&state.offline);
    let result = state
        .offline
        .run_mutation(async move { manager.reconcile_book_cost(&book_id).await })
        .await;
    result.map_err(Into::into)
}
impl OfflineManager {
    async fn reconcile_book_cost(&self, book_id: &str) -> IpcResult<OfflineStatus> {
        let manifest = self
            .manifests()
            .await?
            .into_iter()
            .find(|m| m.book_id == book_id)
            .ok_or("Для книги нет офлайн-версии")?;
        let ids = manifest.block_ids;
        let attempts = self
            .journal(move |journal| journal.attempts_for_blocks(&ids))
            .await?;
        let pending = attempts
            .into_iter()
            .filter(|attempt| {
                attempt.provider_cost.is_none() && attempt.provider_response_id.is_some()
            })
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            let start = self.reconciliation_cursor.fetch_add(20, Ordering::Relaxed) % pending.len();
            for index in 0..pending.len().min(20) {
                let attempt = pending[(start + index) % pending.len()].clone();
                let _ =
                    tokio::time::timeout(Duration::from_secs(3), self.reconcile_attempt(attempt))
                        .await;
            }
        }
        self.emit_status(book_id).await;
        self.status(book_id).await
    }
}
#[tauri::command]
pub async fn offline_start(
    state: State<'_, AppState>,
    book_id: String,
    quote_id: String,
    confirm_retries: Option<bool>,
    budget: Option<OfflineBudget>,
) -> BlockResult<OfflineStatus> {
    let permit = state.offline.mutation_permit()?;
    let manager = Arc::clone(&state.offline);
    tauri::async_runtime::spawn(async move {
        let _permit = permit;
        manager
            .start(
                book_id,
                quote_id,
                confirm_retries.unwrap_or(false),
                budget.unwrap_or_default(),
            )
            .await
    })
    .await
    .map_err(|error| BlockError::from(error.to_string()))?
}
#[tauri::command]
pub async fn offline_status(
    state: State<'_, AppState>,
    book_id: String,
) -> Result<OfflineStatus, crate::ipc::IpcError> {
    let result: IpcResult<_> = async { state.offline.status(&book_id).await }.await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn offline_diagnostics(
    state: State<'_, AppState>,
) -> Result<Vec<desk_core::ManifestIssue>, crate::ipc::IpcError> {
    let result: IpcResult<_> = async { state.offline.diagnostics().await }.await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn offline_restore_manifest(
    state: State<'_, AppState>,
    file: String,
    backup_path: String,
) -> Result<(), crate::ipc::IpcError> {
    let manager = Arc::clone(&state.offline);
    let result: IpcResult<_> = state
        .offline
        .run_mutation(async move {
            let _plan_lifecycle = manager.plan_gate.lock().await;
            {
                let mut jobs = manager.jobs.lock().await;
                for (book_id, job) in jobs.iter_mut() {
                    manager.queue.stop_offline(book_id, job.generation);
                    job.state = OfflineJobState::Paused;
                    job.generation = job.generation.wrapping_add(1);
                }
            }
            manager.quotes.lock().await.clear();
            let cache = Arc::clone(&manager.cache);
            let outcome = tauri::async_runtime::spawn_blocking(move || {
                cache
                    .restore_manifest(&file, std::path::Path::new(&backup_path))
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| e.to_string());
            manager.invalidate_under_store().await?;
            outcome?
        })
        .await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn offline_list(
    state: State<'_, AppState>,
) -> Result<Vec<OfflineStatus>, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
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
    .await;
    result.map_err(Into::into)
}
const fn broken_status(
    book_id: String,
    profile_id: Option<String>,
    error: String,
) -> OfflineStatus {
    OfflineStatus {
        book_id,
        profile_id,
        state: OfflineJobState::Error,
        total_blocks: 0,
        cached_blocks: 0,
        bytes: 0,
        error: Some(error),
        pending_requests: 0,
        actual_cost_usd: None,
        estimated_reserved_usd: None,
        unknown_cost_requests: 0,
        submitted_requests: 0,
    }
}
#[tauri::command]
pub async fn offline_pause(
    state: State<'_, AppState>,
    book_id: String,
) -> Result<OfflineStatus, crate::ipc::IpcError> {
    let result: IpcResult<_> = async {
        state.offline.pause(&book_id).await;
        state.offline.status(&book_id).await
    }
    .await;
    result.map_err(Into::into)
}
#[tauri::command]
pub async fn offline_remove(
    state: State<'_, AppState>,
    book_id: String,
) -> Result<OfflineStatus, crate::ipc::IpcError> {
    let manager = Arc::clone(&state.offline);
    let result: IpcResult<_> = state
        .offline
        .run_mutation(async move {
            manager.remove_all(&book_id).await?;
            manager.status(&book_id).await
        })
        .await;
    result.map_err(Into::into)
}
impl OfflineManager {
    async fn pause(&self, book_id: &str) {
        let mut jobs = self.jobs.lock().await;
        let job = jobs.entry(book_id.to_owned()).or_insert_with(|| Job {
            state: OfflineJobState::Paused,
            error: None,
            generation: 0,
            engine: String::new(),
            budget: None,
        });
        self.queue.stop_offline(book_id, job.generation);
        job.state = OfflineJobState::Paused;
        job.generation = job.generation.wrapping_add(1);
        drop(jobs);
        self.emit_status(book_id).await;
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
            job.state = OfflineJobState::Paused;
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
        self.invalidate_under_store().await?;
        let mut jobs = self.jobs.lock().await;
        let job = jobs.entry(book_id.to_owned()).or_insert_with(|| Job {
            state: OfflineJobState::Paused,
            error: None,
            generation: 0,
            engine: String::new(),
            budget: None,
        });
        self.queue.stop_offline(book_id, job.generation);
        job.state = OfflineJobState::Paused;
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
        self.invalidate_under_store().await?;
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

    #[tokio::test]
    async fn cancelled_mutation_keeps_gate_and_permit_until_blocking_work_finishes() -> IpcResult<()>
    {
        let (manager, root) = manager()?;
        let gate = Arc::clone(&manager.plan_gate);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let outer = Arc::clone(&manager);
        let runner = tokio::spawn(async move {
            outer
                .run_mutation(async move {
                    let guard = gate.lock_owned().await;
                    tauri::async_runtime::spawn_blocking(move || {
                        let _guard = guard;
                        let _ = started_tx.send(());
                        release_rx.recv().map_err(|e| e.to_string())
                    })
                    .await
                    .map_err(|e| e.to_string())?
                })
                .await
        });
        started_rx.await.map_err(|e| e.to_string())?;
        runner.abort();
        manager.begin_shutdown();
        assert!(manager.mutation_permit().is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), manager.plan_gate.lock())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), manager.drain_mutations())
                .await
                .is_err()
        );
        release_tx.send(()).map_err(|e| e.to_string())?;
        tokio::time::timeout(Duration::from_secs(2), manager.drain_mutations())
            .await
            .map_err(|e| e.to_string())?;
        cleanup(manager, root).await?;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_accepts_final_save_until_frontend_flush_and_waits_for_it() -> IpcResult<()> {
        let (manager, root) = manager()?;
        manager.begin_shutdown();
        assert!(manager.mutation_permit().is_err());
        let save = manager.final_save_permit()?;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), manager.drain_mutations())
                .await
                .is_err()
        );
        assert!(manager.final_save_permit().is_err());
        drop(save);
        tokio::time::timeout(Duration::from_secs(2), manager.drain_mutations())
            .await
            .map_err(|e| e.to_string())?;
        cleanup(manager, root).await?;
        Ok(())
    }

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
                    timings: Vec::new(),
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
            synthesis: None,
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
            synthesis: None,
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
            synthesis: None,
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
                .start(id.clone(), first.quote_id, false, OfflineBudget::default())
                .await
                .is_err()
        );
        assert!(manager.manifests().await?.is_empty());
        let second = manager.quote(id.clone()).await?;
        manager.remove_all(&id).await?;
        assert!(
            manager
                .start(id.clone(), second.quote_id, false, OfflineBudget::default())
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
        assert!(
            manager
                .start(id, quote.quote_id, false, OfflineBudget::default())
                .await
                .is_err()
        );
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
            synthesis: None,
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

#[cfg(test)]
#[path = "offline_budget_tests.rs"]
mod budget_tests;

#[cfg(test)]
#[path = "offline_memory_tests.rs"]
mod memory_tests;

#[path = "offline_plans.rs"]
mod plans;
