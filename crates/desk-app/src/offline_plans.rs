use super::{IpcResult, OfflineManager};
use desk_core::{AudioBlock, AudioProfile, Book, EditionPlan, OfflineManifest};
use desk_core::{ChapterPlanner, plan_chapter_v1};
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::{collections::HashMap, sync::Arc};

#[derive(Debug)]
pub(super) struct ChapterPlan {
    pub blocks: Vec<AudioBlock>,
    by_id: HashMap<String, usize>,
}
impl ChapterPlan {
    fn new(blocks: Vec<AudioBlock>) -> Self {
        let mut by_id = HashMap::new();
        for (index, block) in blocks.iter().enumerate() {
            by_id.entry(block.id.clone()).or_insert(index);
        }
        Self { blocks, by_id }
    }
    pub(super) fn block(&self, id: &str) -> Option<&AudioBlock> {
        self.by_id.get(id).and_then(|index| self.blocks.get(*index))
    }
    fn bytes(&self) -> usize {
        self.blocks.iter().fold(
            self.blocks.capacity() * std::mem::size_of::<AudioBlock>()
                + self.by_id.capacity() * (std::mem::size_of::<(String, usize)>() + 64),
            |total, block| {
                total
                    .saturating_add(block.id.capacity())
                    .saturating_add(block.text.capacity())
                    .saturating_add(
                        block.passages.capacity() * std::mem::size_of::<desk_core::AudioPart>(),
                    )
            },
        )
    }
}

fn manifest_bytes(manifest: &OfflineManifest) -> usize {
    let profile = &manifest.profile;
    std::mem::size_of_val(manifest)
        .saturating_add(manifest.book_id.capacity())
        .saturating_add(manifest.profile_id.capacity())
        .saturating_add(manifest.plan_digest.as_ref().map_or(0, String::capacity))
        .saturating_add(
            manifest
                .block_ids
                .capacity()
                .saturating_mul(std::mem::size_of::<String>()),
        )
        .saturating_add(
            manifest
                .block_ids
                .iter()
                .map(String::capacity)
                .sum::<usize>(),
        )
        .saturating_add(profile.engine.capacity())
        .saturating_add(profile.voice.capacity())
        .saturating_add(profile.model.as_ref().map_or(0, String::capacity))
        .saturating_add(profile.model_voice.as_ref().map_or(0, String::capacity))
        .saturating_add(
            profile
                .dictionary
                .iter()
                .map(|(key, value)| key.capacity() + value.capacity() + 128)
                .sum::<usize>(),
        )
}

impl OfflineManager {
    pub(crate) fn invalidate_plans(&self) {
        self.chapter_plans.invalidate();
        self.edition_plans.invalidate();
        self.manifest_plans.invalidate();
    }

    pub(super) async fn chapter_plan(
        &self,
        book_id: String,
        chapter: usize,
    ) -> IpcResult<(AudioProfile, Arc<ChapterPlan>)> {
        let _singleflight = self.plan_gate.lock().await;
        let store = Arc::clone(&self.store);
        let plans = Arc::clone(&self.chapter_plans);
        let planners = Arc::clone(&self.planner);
        #[cfg(test)]
        let builds = Arc::clone(&self.chapter_builds);
        let (profile, result, epoch) =
            tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
                let (profile, key, epoch, source) = {
                    let store = store.lock().map_err(|e| e.to_string())?;
                    let profile = AudioProfile::from_preferences(
                        &store.preferences().map_err(|e| e.to_string())?,
                    );
                    let key = format!("{}:{book_id}:{chapter}:{}", book_id.len(), profile.id());
                    let epoch = plans.epoch();
                    if let Some(cached) = plans.get(&key) {
                        return Ok((profile, cached, epoch));
                    }
                    let source = store
                        .chapter(&book_id, chapter)
                        .map_err(|e| e.to_string())?;
                    drop(store);
                    (profile, key, epoch, source)
                };
                let planner = {
                    let mut current = planners.lock().map_err(|e| e.to_string())?;
                    let id = profile.id();
                    if current
                        .as_ref()
                        .is_none_or(|(current_id, _)| *current_id != id)
                    {
                        *current = Some((id, Arc::new(ChapterPlanner::new(&profile))));
                    }
                    let result = Arc::clone(&current.as_ref().ok_or("Планировщик недоступен")?.1);
                    drop(current);
                    result
                };
                let result = Arc::new(ChapterPlan::new(planner.plan_chapter(chapter, &source)));
                #[cfg(test)]
                builds.fetch_add(1, Ordering::SeqCst);
                let bytes = result.bytes();
                if plans.epoch() != epoch {
                    return Err("Книга изменилась во время планирования".into());
                }
                if !plans.insert_if_epoch(epoch, key, Arc::clone(&result), bytes) {
                    return Err("План главы превышает лимит памяти приложения или устарел".into());
                }
                Ok((profile, result, epoch))
            })
            .await
            .map_err(|e| e.to_string())??;
        if self.chapter_plans.epoch() != epoch {
            return Err("Книга изменилась во время планирования".into());
        }
        Ok((profile, result))
    }

    pub(super) async fn saved_edition(
        &self,
        book_id: String,
    ) -> IpcResult<(Arc<OfflineManifest>, Arc<EditionPlan>)> {
        let cached = {
            let store = self.store.lock().map_err(|e| e.to_string())?;
            let epoch = self.edition_plans.epoch();
            let result = self.manifest_plans.get(&book_id).and_then(|manifest| {
                let plan = self.edition_plans.get(manifest.plan_digest.as_deref()?)?;
                Some((manifest, plan, epoch))
            });
            drop(store);
            result
        };
        if let Some((manifest, plan, epoch)) = cached
            && self.edition_plans.epoch() == epoch
        {
            return Ok((manifest, plan));
        }
        let manifest = self
            .manifests()
            .await?
            .into_iter()
            .find(|m| m.book_id == book_id)
            .ok_or(
                "Для книги нет доступной офлайн-версии; проверьте диагностику сохранённых изданий",
            )?;
        let (manifest, plan) = self.edition(manifest).await?;
        Ok((Arc::new(manifest), plan))
    }

    pub(super) async fn edition(
        &self,
        manifest: OfflineManifest,
    ) -> IpcResult<(OfflineManifest, Arc<EditionPlan>)> {
        let _singleflight = self.plan_gate.lock().await;
        let store = Arc::clone(&self.store);
        let cache = Arc::clone(&self.cache);
        let plans = Arc::clone(&self.edition_plans);
        let manifests = Arc::clone(&self.manifest_plans);
        let (manifest, plan, epoch) =
            tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
                let epoch = {
                    let store = store.lock().map_err(|e| e.to_string())?;
                    let epoch = plans.epoch();
                    if let Some(digest) = &manifest.plan_digest
                        && let Some(plan) = plans.get(digest)
                    {
                        let previously_validated =
                            manifests.get(&manifest.book_id).is_some_and(|old| {
                                old.plan_digest == manifest.plan_digest
                                    && old.profile_id == manifest.profile_id
                                    && old.block_ids == manifest.block_ids
                            });
                        if previously_validated {
                            drop(store);
                            return Ok((manifest, plan, epoch));
                        }
                    }
                    drop(store);
                    epoch
                };
                let book = store
                    .lock()
                    .map_err(|e| e.to_string())?
                    .book(&manifest.book_id)
                    .map_err(|e| e.to_string())?;
                let mut manifest = manifest;
                let plan = if let Some(digest) = &manifest.plan_digest {
                    cache.edition_plan(digest).map_err(|e| e.to_string())?
                } else {
                    let plan = EditionPlan::migrate_legacy(
                        &book,
                        &manifest.profile,
                        &manifest.block_ids,
                        plan_chapter_v1,
                    )
                    .map_err(|e| e.to_string())?;
                    let _store = store.lock().map_err(|e| e.to_string())?;
                    if plans.epoch() != epoch {
                        return Err("Книга изменилась во время миграции".into());
                    }
                    let current = cache
                        .manifest(&manifest.book_id, &manifest.profile_id)
                        .map_err(|e| e.to_string())?
                        .ok_or("Офлайн-издание было удалено во время миграции")?;
                    if current.plan_digest != manifest.plan_digest
                        || current.block_ids != manifest.block_ids
                    {
                        return Err("Офлайн-издание изменилось во время миграции".into());
                    }
                    manifest.plan_digest =
                        Some(cache.save_edition_plan(&plan).map_err(|e| e.to_string())?);
                    cache.save_manifest(&manifest).map_err(|e| e.to_string())?;
                    plan
                };
                plan.validate(&book, &manifest.profile)
                    .map_err(|e| e.to_string())?;
                if plan.block_ids() != manifest.block_ids {
                    return Err("Карта издания не совпадает с манифестом".into());
                }
                let plan = Arc::new(plan);
                let digest = manifest
                    .plan_digest
                    .as_ref()
                    .ok_or("В издании нет сохранённого плана")?
                    .clone();
                let _store = store.lock().map_err(|e| e.to_string())?;
                if plans.epoch() != epoch {
                    return Err("Книга изменилась во время загрузки издания".into());
                }
                if !plans.insert_if_epoch(epoch, digest, Arc::clone(&plan), plan.estimated_bytes())
                {
                    return Err("План издания превышает лимит памяти приложения или устарел".into());
                }
                let size = manifest_bytes(&manifest);
                manifests.insert(manifest.book_id.clone(), Arc::new(manifest.clone()), size);
                Ok((manifest, plan, epoch))
            })
            .await
            .map_err(|e| e.to_string())??;
        if self.edition_plans.epoch() != epoch {
            return Err("Книга изменилась во время загрузки издания".into());
        }
        Ok((manifest, plan))
    }

    pub(super) async fn new_edition(
        &self,
        book: Book,
        profile: AudioProfile,
    ) -> IpcResult<(OfflineManifest, Arc<EditionPlan>)> {
        let _singleflight = self.plan_gate.lock().await;
        let epoch = self.edition_plans.epoch();
        let (manifest, plan) = tauri::async_runtime::spawn_blocking(move || -> IpcResult<_> {
            let plan = Arc::new(EditionPlan::build(&book, &profile).map_err(|e| e.to_string())?);
            let (_, digest) = plan.encode().map_err(|e| e.to_string())?;
            Ok((
                OfflineManifest {
                    book_id: book.id,
                    profile_id: profile.id(),
                    profile,
                    block_ids: plan.block_ids(),
                    updated_at: 0,
                    plan_digest: Some(digest),
                },
                plan,
            ))
        })
        .await
        .map_err(|e| e.to_string())??;
        let _store = self.store.lock().map_err(|e| e.to_string())?;
        if self.edition_plans.epoch() != epoch {
            return Err("Книга изменилась во время планирования".into());
        }
        let digest = manifest
            .plan_digest
            .as_ref()
            .ok_or("В издании нет плана")?
            .clone();
        if !self.edition_plans.insert_if_epoch(
            epoch,
            digest,
            Arc::clone(&plan),
            plan.estimated_bytes(),
        ) {
            return Err("План издания превышает лимит памяти приложения или устарел".into());
        }
        Ok((manifest, plan))
    }
}
