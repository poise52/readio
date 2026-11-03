use crate::{AudioBlock, AudioPart, AudioProfile, Book, ChapterPlanner, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    mem::size_of,
    sync::OnceLock,
};

pub const EDITION_PLAN_VERSION: u32 = 1;
/// Application safety limit, independent of provider request limits.
pub const MAX_EDITION_PLAN_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditionChapter {
    pub title: String,
    pub source_utf16_len: usize,
    pub blocks: Vec<AudioBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditionPlan {
    pub version: u32,
    pub book_id: String,
    pub profile_id: String,
    pub source_fingerprint: String,
    pub chapters: Vec<EditionChapter>,
    #[serde(skip)]
    block_index: OnceLock<Vec<HashMap<String, usize>>>,
}

fn source_fingerprint(book: &Book) -> String {
    let mut hash = Sha256::new();
    let add = |hash: &mut Sha256, bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    };
    add(&mut hash, b"readio-edition-source-v1");
    for chapter in &book.chapters {
        add(&mut hash, chapter.title.as_bytes());
        add(&mut hash, chapter.text.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn block_id(block: &AudioBlock, profile: &AudioProfile) -> String {
    crate::audio::canonical_block_id(&block.text, profile)
}

fn invalid() -> Error {
    Error::Format("Повреждённый или несовместимый план офлайн-издания".into())
}

impl EditionPlan {
    /// Creates a snapshot using the current planner.
    ///
    /// # Errors
    /// Returns an error when the generated plan is invalid or exceeds the application limit.
    pub fn build(book: &Book, profile: &AudioProfile) -> Result<Self> {
        let planner = ChapterPlanner::try_new(profile)?;
        Self::with_planner(book, profile, |book, index, _| {
            planner.try_plan_chapter(index, &book.chapters[index])
        })
    }

    fn with_planner(
        book: &Book,
        profile: &AudioProfile,
        planner: impl Fn(&Book, usize, &AudioProfile) -> Result<Vec<AudioBlock>>,
    ) -> Result<Self> {
        let plan = Self {
            version: EDITION_PLAN_VERSION,
            book_id: book.id.clone(),
            profile_id: profile.id(),
            source_fingerprint: source_fingerprint(book),
            block_index: OnceLock::new(),
            chapters: book
                .chapters
                .iter()
                .enumerate()
                .map(|(index, chapter)| {
                    Ok(EditionChapter {
                        title: chapter.title.clone(),
                        source_utf16_len: chapter.text.encode_utf16().count(),
                        blocks: planner(book, index, profile)?,
                    })
                })
                .collect::<Result<_>>()?,
        };
        plan.validate(book, profile)?;
        plan.encode()?;
        Ok(plan)
    }

    /// Migrates a legacy manifest only if the frozen legacy planner reproduces every saved ID.
    ///
    /// # Errors
    /// Returns an error if any ID differs, or the plan is invalid or too large.
    pub fn migrate_legacy(
        book: &Book,
        profile: &AudioProfile,
        saved_ids: &[String],
        frozen_planner: impl Fn(&Book, usize, &AudioProfile) -> Vec<AudioBlock>,
    ) -> Result<Self> {
        let bounded = ChapterPlanner::try_new(profile)?;
        for chapter in &book.chapters {
            bounded.validate_legacy_source(&chapter.text)?;
        }
        let plan = Self::with_planner(book, profile, |book, chapter, profile| {
            Ok(frozen_planner(book, chapter, profile))
        })?;
        if !plan.block_ids().iter().eq(saved_ids.iter()) {
            return Err(Error::Format(
                "План старого издания не совпадает с сохранёнными аудиоблоками".into(),
            ));
        }
        Ok(plan)
    }

    /// Validates identity and source offsets without invoking a planner.
    ///
    /// # Errors
    /// Returns an error if the version, source, profile, block IDs or ranges are invalid.
    pub fn validate(&self, book: &Book, profile: &AudioProfile) -> Result<()> {
        if self.version != EDITION_PLAN_VERSION
            || self.book_id != book.id
            || self.profile_id != profile.id()
            || self.source_fingerprint != source_fingerprint(book)
            || self.chapters.len() != book.chapters.len()
        {
            return Err(invalid());
        }
        for (index, (saved, source)) in self.chapters.iter().zip(&book.chapters).enumerate() {
            if saved.title != source.title
                || saved.source_utf16_len != source.text.encode_utf16().count()
            {
                return Err(invalid());
            }
            let mut boundaries = HashSet::new();
            for block in &saved.blocks {
                for part in &block.passages {
                    boundaries.insert(part.start);
                    boundaries.insert(part.end);
                }
            }
            boundaries.remove(&0);
            let mut position = 0;
            for ch in source.text.chars() {
                position += ch.len_utf16();
                boundaries.remove(&position);
            }
            if !boundaries.is_empty() {
                return Err(invalid());
            }
            let mut previous_start = 0;
            for block in &saved.blocks {
                if block.chapter != index
                    || block.start >= block.end
                    || block.end > saved.source_utf16_len
                    || block.start < previous_start
                    || block.text.is_empty()
                    || block.characters != block.text.chars().count()
                    || block.id != block_id(block, profile)
                    || block.passages.is_empty()
                {
                    return Err(invalid());
                }
                let mut previous_part_start = block.start;
                let mut previous_part_index = 0;
                let mut previous_synthesis_end = 0;
                let mut synthesis_boundaries = HashSet::from([0]);
                let mut synthesis_offset = 0;
                for ch in block.text.chars() {
                    synthesis_offset += ch.len_utf16();
                    synthesis_boundaries.insert(synthesis_offset);
                }
                for part in &block.passages {
                    let passage_index_invalid = part.index >= saved.source_utf16_len;
                    if part.start < block.start
                        || part.end > block.end
                        || part.start >= part.end
                        || part.start < previous_part_start
                        || part.weight == 0
                        || passage_index_invalid
                        || part.index < previous_part_index
                    {
                        return Err(invalid());
                    }
                    match (part.synthesis_start, part.synthesis_end) {
                        (None, None) => (),
                        (Some(start), Some(end))
                            if start >= previous_synthesis_end
                                && start < end
                                && synthesis_boundaries.contains(&start)
                                && synthesis_boundaries.contains(&end) =>
                        {
                            previous_synthesis_end = end;
                        }
                        _ => return Err(invalid()),
                    }
                    previous_part_start = part.start;
                    previous_part_index = part.index;
                }
                if block
                    .passages
                    .first()
                    .is_none_or(|part| part.start != block.start)
                    || block
                        .passages
                        .last()
                        .is_none_or(|part| part.end != block.end)
                {
                    return Err(invalid());
                }
                previous_start = block.start;
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn block_ids(&self) -> Vec<String> {
        self.chapters
            .iter()
            .flat_map(|chapter| chapter.blocks.iter().map(|block| block.id.clone()))
            .collect()
    }

    /// Looks up a block by its canonical ID within a chapter.
    #[must_use]
    pub fn block(&self, chapter: usize, id: &str) -> Option<&AudioBlock> {
        let index = self.block_index.get_or_init(|| {
            self.chapters
                .iter()
                .map(|chapter| {
                    let mut blocks = HashMap::new();
                    for (position, block) in chapter.blocks.iter().enumerate() {
                        blocks.entry(block.id.clone()).or_insert(position);
                    }
                    blocks
                })
                .collect()
        });
        let position = *index.get(chapter)?.get(id)?;
        self.chapters
            .get(chapter)?
            .blocks
            .get(position)
            .filter(|block| block.id == id)
    }

    /// Conservative in-memory cache weight, including the eventual lookup index.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let mut bytes = size_of::<Self>()
            .saturating_add(self.book_id.capacity())
            .saturating_add(self.profile_id.capacity())
            .saturating_add(self.source_fingerprint.capacity())
            .saturating_add(
                self.chapters
                    .capacity()
                    .saturating_mul(size_of::<EditionChapter>()),
            )
            .saturating_add(self.chapters.len().saturating_mul(128));
        for chapter in &self.chapters {
            bytes = bytes
                .saturating_add(chapter.title.capacity())
                .saturating_add(
                    chapter
                        .blocks
                        .capacity()
                        .saturating_mul(size_of::<AudioBlock>()),
                );
            for block in &chapter.blocks {
                bytes = bytes
                    .saturating_add(block.id.capacity())
                    .saturating_add(block.text.capacity())
                    .saturating_add(
                        block
                            .passages
                            .capacity()
                            .saturating_mul(size_of::<AudioPart>()),
                    )
                    .saturating_add(block.id.len())
                    .saturating_add(256);
            }
        }
        bytes
    }

    /// Serializes the immutable plan and returns its content address.
    ///
    /// # Errors
    /// Returns an error if serialization fails or exceeds the application safety limit.
    pub fn encode(&self) -> Result<(Vec<u8>, String)> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_EDITION_PLAN_BYTES {
            return Err(Error::Format(
                "План издания превышает допустимый размер приложения".into(),
            ));
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        Ok((bytes, digest))
    }

    /// Verifies a content-addressed plan before deserializing it.
    ///
    /// # Errors
    /// Returns an error for an oversized or corrupted plan, invalid JSON or an unsupported version.
    pub fn decode(bytes: &[u8], expected_digest: &str) -> Result<Self> {
        if bytes.len() > MAX_EDITION_PLAN_BYTES
            || format!("{:x}", Sha256::digest(bytes)) != expected_digest
        {
            return Err(invalid());
        }
        let plan: Self = serde_json::from_slice(bytes)?;
        if plan.version != EDITION_PLAN_VERSION {
            return Err(invalid());
        }
        Ok(plan)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Chapter, Preferences, plan_chapter};

    fn fixture() -> (Book, AudioProfile) {
        let text = "Привет, мир! 😀 Повторим фразу. Привет, мир! 😀 Повторим фразу.";
        let book = Book {
            id: "book-1".into(),
            chapters: vec![
                Chapter {
                    title: "Первая".into(),
                    text: text.into(),
                },
                Chapter {
                    title: "Вторая".into(),
                    text: text.into(),
                },
            ],
            ..Book::default()
        };
        let mut profile = AudioProfile::from_preferences(&Preferences::default());
        profile
            .dictionary
            .insert("Привет".into(), "Здравствуй".into());
        (book, profile)
    }

    #[test]
    fn snapshot_survives_different_segmentation_and_duplicate_ids() {
        let (book, profile) = fixture();
        let old = EditionPlan::build(&book, &profile).unwrap();
        let (bytes, digest) = old.encode().unwrap();
        let mut future_profile = profile.clone();
        future_profile.max_escaped_bytes = Some(30);
        assert_ne!(
            plan_chapter(&book, 0, &future_profile).len(),
            old.chapters[0].blocks.len()
        );
        let loaded = EditionPlan::decode(&bytes, &digest).unwrap();
        loaded.validate(&book, &profile).unwrap();
        let first = &loaded.chapters[0].blocks[0];
        assert_eq!(loaded.block(0, &first.id).unwrap().text, first.text);
        assert!(loaded.block(0, &"0".repeat(64)).is_none());
        assert!(loaded.block(usize::MAX, &first.id).is_none());
        assert_eq!(loaded.encode().unwrap(), (bytes, digest));
        assert_eq!(loaded.block_ids(), old.block_ids());
        assert_eq!(
            loaded.chapters[0].blocks[0].id,
            loaded.chapters[1].blocks[0].id
        );
        assert_eq!(
            loaded.chapters[0].blocks[0].text,
            old.chapters[0].blocks[0].text
        );
        assert_eq!(
            loaded.chapters[0].blocks[0].passages,
            old.chapters[0].blocks[0].passages
        );
    }

    #[test]
    fn legacy_migration_never_accepts_different_ids() {
        let (book, profile) = fixture();
        let original = EditionPlan::build(&book, &profile).unwrap();
        let ids = original.block_ids();
        let migrated = EditionPlan::migrate_legacy(&book, &profile, &ids, plan_chapter).unwrap();
        assert_eq!(migrated.block_ids(), ids);
        let mut different = ids.clone();
        different.pop();
        assert!(EditionPlan::migrate_legacy(&book, &profile, &different, plan_chapter).is_err());
        assert!(EditionPlan::migrate_legacy(&book, &profile, &ids, |_, _, _| Vec::new()).is_err());
    }

    #[test]
    fn duplicate_ids_use_first_occurrence_and_index_does_not_change_weight() {
        let (book, profile) = fixture();
        let mut plan = EditionPlan::build(&book, &profile).unwrap();
        let first = plan.chapters[0].blocks[0].clone();
        plan.chapters[0].blocks.push(first.clone());
        let estimate = plan.estimated_bytes();
        let selected = plan.block(0, &first.id).unwrap();
        assert!(std::ptr::eq(
            std::ptr::from_ref(selected),
            std::ptr::from_ref(&plan.chapters[0].blocks[0])
        ));
        assert_eq!(plan.estimated_bytes(), estimate);
    }

    #[test]
    fn rejects_corrupt_or_future_plan_and_source_or_offsets_changed() {
        let (mut book, profile) = fixture();
        let mut plan = EditionPlan::build(&book, &profile).unwrap();
        let (bytes, digest) = plan.encode().unwrap();
        assert!(EditionPlan::decode(&bytes, &"0".repeat(64)).is_err());
        plan.version += 1;
        let (future, future_digest) = plan.encode().unwrap();
        assert!(EditionPlan::decode(&future, &future_digest).is_err());
        plan.version = EDITION_PLAN_VERSION;
        plan.chapters[0].blocks[0].id = "0".repeat(64);
        assert!(plan.validate(&book, &profile).is_err());
        let mut plan = EditionPlan::decode(&bytes, &digest).unwrap();
        plan.chapters[0].blocks[0].passages[0].index = usize::MAX;
        assert!(plan.validate(&book, &profile).is_err());
        let mut plan = EditionPlan::decode(&bytes, &digest).unwrap();
        plan.chapters[0].blocks[0].passages[0].weight = 0;
        assert!(plan.validate(&book, &profile).is_err());
        book.chapters[0].text.push('!');
        assert!(
            EditionPlan::decode(&bytes, &digest)
                .unwrap()
                .validate(&book, &profile)
                .is_err()
        );
    }
}
