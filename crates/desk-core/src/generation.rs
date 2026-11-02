use crate::{AudioBlock, AudioProfile, Error, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    Prepared,
    Submitted,
    Completed,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationAttempt {
    pub id: String,
    pub block_id: String,
    pub profile_id: String,
    pub text_hash: String,
    pub state: GenerationState,
    pub prepared_at: i64,
    pub submitted_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub provider_response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationPrepare {
    Ready(GenerationAttempt),
    RequiresConfirmation(GenerationAttempt),
}

#[derive(Debug)]
pub struct GenerationJournal {
    conn: Connection,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn text_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn read_attempt(row: &rusqlite::Row<'_>) -> rusqlite::Result<GenerationAttempt> {
    let value: String = row.get(4)?;
    let state = match value.as_str() {
        "prepared" => GenerationState::Prepared,
        "submitted" => GenerationState::Submitted,
        "completed" => GenerationState::Completed,
        "uncertain" => GenerationState::Uncertain,
        _ => {
            return Err(rusqlite::Error::InvalidColumnType(
                4,
                "state".into(),
                rusqlite::types::Type::Text,
            ));
        }
    };
    Ok(GenerationAttempt {
        id: row.get(0)?,
        block_id: row.get(1)?,
        profile_id: row.get(2)?,
        text_hash: row.get(3)?,
        state,
        prepared_at: row.get(5)?,
        submitted_at: row.get(6)?,
        finished_at: row.get(7)?,
        provider_response_id: row.get(8)?,
    })
}

const SELECT_LATEST: &str = "SELECT id,block_id,profile_id,text_hash,state,prepared_at,submitted_at,finished_at,provider_response_id FROM generation_attempts WHERE block_id=?1 AND profile_id=?2 AND text_hash=?3 ORDER BY rowid DESC LIMIT 1";

impl GenerationJournal {
    /// Opens the journal. Recovery must be run once before starting workers.
    ///
    /// # Errors
    /// Returns an error if the database cannot be initialized.
    pub fn open(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join("readio.sqlite3"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS generation_attempts (
                id TEXT PRIMARY KEY, block_id TEXT NOT NULL, profile_id TEXT NOT NULL,
                text_hash TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('prepared','submitted','completed','uncertain')),
                prepared_at INTEGER NOT NULL, submitted_at INTEGER, finished_at INTEGER, provider_response_id TEXT
            );
            CREATE INDEX IF NOT EXISTS generation_attempts_key ON generation_attempts(block_id,profile_id,text_hash);")?;
        Ok(Self { conn })
    }

    /// Marks interrupted submissions as uncertain. Call only when no workers are active.
    ///
    /// # Errors
    /// Returns an error if the recovery update fails.
    pub fn recover_submitted(&self) -> Result<usize> {
        Ok(self.conn.execute("UPDATE generation_attempts SET state='uncertain',finished_at=?1 WHERE state='submitted'", [now()])?)
    }

    /// Marks submitted attempts for a block uncertain after its worker has stopped unexpectedly.
    ///
    /// The caller must ensure no submission for this block is still running.
    ///
    /// # Errors
    /// Returns an error if the recovery update fails.
    pub fn recover_block(&self, block_id: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE generation_attempts SET state='uncertain',finished_at=?2 WHERE state='submitted' AND block_id=?1",
            params![block_id, now()],
        )?)
    }

    /// Returns the newest attempt for this exact block, profile and text.
    ///
    /// # Errors
    /// Returns an error if the database cannot be read.
    pub fn latest(
        &self,
        block: &AudioBlock,
        profile: &AudioProfile,
    ) -> Result<Option<GenerationAttempt>> {
        Ok(self
            .conn
            .query_row(
                SELECT_LATEST,
                params![block.id, profile.id(), text_hash(&block.text)],
                read_attempt,
            )
            .optional()?)
    }

    /// Creates or resumes an attempt. Retrying uncertain or completed attempts requires explicit consent.
    ///
    /// # Errors
    /// Returns an error for an unresolved submission or database failure.
    pub fn prepare(
        &self,
        block: &AudioBlock,
        profile: &AudioProfile,
        allow_uncertain_retry: bool,
    ) -> Result<GenerationPrepare> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let profile_id = profile.id();
        let hash = text_hash(&block.text);
        let latest = tx
            .query_row(
                SELECT_LATEST,
                params![block.id, profile_id, hash],
                read_attempt,
            )
            .optional()?;
        if let Some(attempt) = latest {
            match attempt.state {
                GenerationState::Prepared => {
                    tx.commit()?;
                    return Ok(GenerationPrepare::Ready(attempt));
                }
                GenerationState::Uncertain | GenerationState::Completed
                    if allow_uncertain_retry => {}
                GenerationState::Uncertain | GenerationState::Completed => {
                    tx.commit()?;
                    return Ok(GenerationPrepare::RequiresConfirmation(attempt));
                }
                GenerationState::Submitted => {
                    return Err(Error::Format("Генерация уже отправлена".into()));
                }
            }
        }
        let attempt = GenerationAttempt {
            id: Uuid::new_v4().to_string(),
            block_id: block.id.clone(),
            profile_id,
            text_hash: hash,
            state: GenerationState::Prepared,
            prepared_at: now(),
            submitted_at: None,
            finished_at: None,
            provider_response_id: None,
        };
        tx.execute("INSERT INTO generation_attempts(id,block_id,profile_id,text_hash,state,prepared_at) VALUES (?1,?2,?3,?4,'prepared',?5)", params![attempt.id, attempt.block_id, attempt.profile_id, attempt.text_hash, attempt.prepared_at])?;
        tx.commit()?;
        Ok(GenerationPrepare::Ready(attempt))
    }

    /// Persists submission before the provider request is sent.
    ///
    /// # Errors
    /// Returns an error if the attempt is not prepared or the update fails.
    pub fn mark_submitted(&self, id: &str) -> Result<()> {
        let changed = self.conn.execute("UPDATE generation_attempts SET state='submitted',submitted_at=?2 WHERE id=?1 AND state='prepared'", params![id, now()])?;
        Self::require_transition(changed)
    }

    /// Returns a submitted attempt to prepared only when the HTTP future has never been polled.
    ///
    /// The caller must establish this precondition; the journal cannot infer it.
    ///
    /// # Errors
    /// Returns an error if a response ID is already recorded, the attempt is not submitted,
    /// or the database update fails.
    pub fn return_to_prepared(&self, id: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE generation_attempts SET state='prepared',submitted_at=NULL,finished_at=NULL,provider_response_id=NULL WHERE id=?1 AND state='submitted' AND provider_response_id IS NULL",
            [id],
        )?;
        Self::require_transition(changed)
    }

    /// Stores a response identifier while the attempt is still submitted.
    ///
    /// # Errors
    /// Returns an error for an invalid identifier, non-submitted attempt, or database failure.
    pub fn note_response(&self, id: &str, provider_response_id: Option<&str>) -> Result<()> {
        Self::validate_response_id(provider_response_id)?;
        let changed = self.conn.execute(
            "UPDATE generation_attempts SET provider_response_id=COALESCE(?2,provider_response_id) WHERE id=?1 AND state='submitted'",
            params![id, provider_response_id],
        )?;
        Self::require_transition(changed)
    }

    fn validate_response_id(value: Option<&str>) -> Result<()> {
        if value.is_some_and(|value| {
            value.is_empty()
                || value.len() > 512
                || value.to_ascii_lowercase().starts_with("sk-")
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        }) {
            return Err(Error::Format(
                "Некорректный идентификатор ответа генерации".into(),
            ));
        }
        Ok(())
    }

    /// Records an unknown provider outcome.
    ///
    /// # Errors
    /// Returns an error if the attempt is not submitted or the update fails.
    pub fn mark_uncertain(&self, id: &str, provider_response_id: Option<&str>) -> Result<()> {
        Self::validate_response_id(provider_response_id)?;
        let changed = self.conn.execute("UPDATE generation_attempts SET state='uncertain',finished_at=?2,provider_response_id=COALESCE(?3,provider_response_id) WHERE id=?1 AND state='submitted'", params![id, now(), provider_response_id])?;
        Self::require_transition(changed)
    }

    /// Completes an attempt after its cached audio has been durably written and verified.
    ///
    /// # Errors
    /// Returns an error if the attempt is not submitted or uncertain, or the update fails.
    pub fn complete(&self, id: &str, provider_response_id: Option<&str>) -> Result<()> {
        Self::validate_response_id(provider_response_id)?;
        let changed = self.conn.execute("UPDATE generation_attempts SET state='completed',finished_at=?2,provider_response_id=COALESCE(?3,provider_response_id) WHERE id=?1 AND state IN ('submitted','uncertain')", params![id, now(), provider_response_id])?;
        Self::require_transition(changed)
    }

    fn require_transition(changed: usize) -> Result<()> {
        if changed == 1 {
            Ok(())
        } else {
            Err(Error::Format(
                "Недопустимый переход состояния генерации".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        sync::{Arc, Barrier},
        thread,
    };

    fn fixture() -> (AudioBlock, AudioProfile) {
        (
            AudioBlock {
                id: "a".repeat(64),
                chapter: 0,
                start: 0,
                end: 4,
                passages: vec![],
                text: "text".into(),
                characters: 4,
            },
            AudioProfile {
                engine: "test".into(),
                voice: "voice".into(),
                model: None,
                model_voice: None,
                dictionary: BTreeMap::new(),
                max_escaped_bytes: None,
            },
        )
    }

    fn ready(outcome: GenerationPrepare) -> GenerationAttempt {
        match outcome {
            GenerationPrepare::Ready(attempt) => attempt,
            GenerationPrepare::RequiresConfirmation(_) => panic!("expected ready"),
        }
    }

    #[test]
    fn recovery_preserves_unknown_and_requires_explicit_retry() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let attempt = ready(journal.prepare(&block, &profile, false)?);
        assert_eq!(
            ready(journal.prepare(&block, &profile, false)?).id,
            attempt.id
        );
        drop(journal);
        let journal = GenerationJournal::open(&dir)?;
        assert_eq!(journal.recover_submitted()?, 0);
        assert_eq!(
            ready(journal.prepare(&block, &profile, false)?).id,
            attempt.id
        );
        journal.mark_submitted(&attempt.id)?;
        drop(journal);
        let journal = GenerationJournal::open(&dir)?;
        assert_eq!(journal.recover_submitted()?, 1);
        assert_eq!(journal.recover_submitted()?, 0);
        let recovered = journal.latest(&block, &profile)?.unwrap();
        assert_eq!(recovered.state, GenerationState::Uncertain);
        assert!(recovered.submitted_at.is_some());
        assert!(recovered.finished_at.is_some());
        assert!(matches!(
            journal.prepare(&block, &profile, false)?,
            GenerationPrepare::RequiresConfirmation(_)
        ));
        let retry = ready(journal.prepare(&block, &profile, true)?);
        assert_ne!(retry.id, attempt.id);
        assert_eq!(retry.state, GenerationState::Prepared);
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn recovery_after_worker_failure_is_limited_to_one_block() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let first = ready(journal.prepare(&block, &profile, false)?);
        let mut other = block.clone();
        other.id = "b".repeat(64);
        let second = ready(journal.prepare(&other, &profile, false)?);
        journal.mark_submitted(&first.id)?;
        journal.mark_submitted(&second.id)?;
        assert_eq!(journal.recover_block(&block.id)?, 1);
        assert_eq!(journal.recover_block(&block.id)?, 0);
        assert_eq!(
            journal.latest(&block, &profile)?.unwrap().state,
            GenerationState::Uncertain
        );
        assert_eq!(
            journal.latest(&other, &profile)?.unwrap().state,
            GenerationState::Submitted
        );
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn unpolled_submission_can_be_reused_but_recorded_response_cannot_reset() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let attempt = ready(journal.prepare(&block, &profile, false)?);
        journal.mark_submitted(&attempt.id)?;
        journal.return_to_prepared(&attempt.id)?;
        let resumed = ready(journal.prepare(&block, &profile, false)?);
        assert_eq!(resumed.id, attempt.id);
        assert!(resumed.submitted_at.is_none());
        journal.mark_submitted(&attempt.id)?;
        journal.note_response(&attempt.id, Some("gen-1"))?;
        assert!(journal.return_to_prepared(&attempt.id).is_err());
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn completed_cache_recovery_and_exact_identity() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (mut block, mut profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let attempt = ready(journal.prepare(&block, &profile, false)?);
        assert!(journal.complete(&attempt.id, None).is_err());
        journal.mark_submitted(&attempt.id)?;
        assert!(
            journal
                .note_response(&attempt.id, Some("sk-or-v1-secret"))
                .is_err()
        );
        assert!(
            journal
                .note_response(&attempt.id, Some(&"x".repeat(513)))
                .is_err()
        );
        journal.note_response(&attempt.id, Some("response-1"))?;
        journal.mark_uncertain(&attempt.id, None)?;
        journal.complete(&attempt.id, None)?;
        let completed = journal.latest(&block, &profile)?.unwrap();
        assert_eq!(completed.state, GenerationState::Completed);
        assert_eq!(
            completed.provider_response_id.as_deref(),
            Some("response-1")
        );
        assert!(matches!(
            journal.prepare(&block, &profile, false)?,
            GenerationPrepare::RequiresConfirmation(_)
        ));
        let retry = ready(journal.prepare(&block, &profile, true)?);
        assert_ne!(retry.id, attempt.id);
        block.text.push('!');
        assert!(journal.latest(&block, &profile)?.is_none());
        let changed = ready(journal.prepare(&block, &profile, false)?);
        assert_ne!(changed.text_hash, attempt.text_hash);
        profile.voice.push('2');
        assert!(journal.latest(&block, &profile)?.is_none());
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn completed_with_missing_or_corrupt_cache_requires_confirmation() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let cache = crate::AudioCache::new(dir.clone())?;
        let attempt = ready(journal.prepare(&block, &profile, false)?);
        journal.mark_submitted(&attempt.id)?;
        let cached = cache.put(&block.id, "audio/mpeg", b"audio")?;
        assert!(cache.verify(&block.id)?.is_some());
        journal.complete(&attempt.id, None)?;
        fs::write(&cached.path, b"bad")?;
        assert!(cache.verify(&block.id)?.is_none());
        assert!(matches!(
            journal.prepare(&block, &profile, false)?,
            GenerationPrepare::RequiresConfirmation(_)
        ));
        fs::remove_file(cached.path)?;
        assert!(cache.verify(&block.id)?.is_none());
        assert!(matches!(
            journal.prepare(&block, &profile, false)?,
            GenerationPrepare::RequiresConfirmation(_)
        ));
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn only_one_worker_can_submit_a_prepared_attempt() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let attempt = ready(journal.prepare(&block, &profile, false)?);
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let dir = dir.clone();
                let barrier = Arc::clone(&barrier);
                let id = attempt.id.clone();
                thread::spawn(move || {
                    let journal = GenerationJournal::open(&dir).unwrap();
                    barrier.wait();
                    journal.mark_submitted(&id).is_ok()
                })
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|succeeded| *succeeded)
                .count(),
            1
        );
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
