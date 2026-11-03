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
    pub retry_of: Option<String>,
    pub provider_cost: Option<String>,
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

#[derive(Clone, Copy)]
enum RetryConsent<'a> {
    Any(bool),
    Exact(Option<&'a str>),
}

impl RetryConsent<'_> {
    fn permits(&self, id: &str) -> bool {
        match self {
            Self::Any(allowed) => *allowed,
            Self::Exact(confirmed) => *confirmed == Some(id),
        }
    }
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
        retry_of: row.get(9)?,
        provider_cost: row.get(10)?,
    })
}

const SELECT_FIELDS: &str = "id,block_id,profile_id,text_hash,state,prepared_at,submitted_at,finished_at,provider_response_id,retry_of,provider_cost";

fn effective_attempt(
    conn: &Connection,
    block_id: &str,
    hash: &str,
) -> Result<Option<GenerationAttempt>> {
    let query = |suffix: &str| -> Result<Option<GenerationAttempt>> {
        Ok(conn.query_row(&format!("SELECT {SELECT_FIELDS} FROM generation_attempts WHERE block_id=?1 AND text_hash=?2 {suffix} ORDER BY rowid DESC LIMIT 1"), params![block_id, hash], read_attempt).optional()?)
    };
    if let Some(submitted) = query("AND state='submitted'")? {
        return Ok(Some(submitted));
    }
    let latest = query("")?;
    let danger = query("AND state IN ('uncertain','completed')")?;
    match (latest, danger) {
        (Some(prepared), Some(danger)) if prepared.state == GenerationState::Prepared => {
            if prepared.retry_of.as_deref() == Some(danger.id.as_str()) {
                Ok(Some(prepared))
            } else {
                Ok(Some(danger))
            }
        }
        (latest, _) => Ok(latest),
    }
}

impl GenerationJournal {
    /// Opens the journal. Recovery must be run once before starting workers.
    ///
    /// # Errors
    /// Returns an error if the database cannot be initialized.
    pub fn open(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join("readio.sqlite3"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        tx.execute_batch("
            CREATE TABLE IF NOT EXISTS generation_attempts (
                id TEXT PRIMARY KEY, block_id TEXT NOT NULL, profile_id TEXT NOT NULL,
                text_hash TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('prepared','submitted','completed','uncertain')),
                prepared_at INTEGER NOT NULL, submitted_at INTEGER, finished_at INTEGER, provider_response_id TEXT, retry_of TEXT, provider_cost TEXT
            );
            CREATE INDEX IF NOT EXISTS generation_attempts_key ON generation_attempts(block_id,profile_id,text_hash);")?;
        let columns = {
            let mut stmt = tx.prepare("PRAGMA table_info(generation_attempts)")?;
            let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
            columns.collect::<std::result::Result<Vec<_>, _>>()?
        };
        if !columns.iter().any(|name| name == "retry_of") {
            tx.execute_batch("ALTER TABLE generation_attempts ADD COLUMN retry_of TEXT;")?;
        }
        if !columns.iter().any(|name| name == "provider_cost") {
            tx.execute_batch("ALTER TABLE generation_attempts ADD COLUMN provider_cost TEXT;")?;
        }
        tx.execute_batch("CREATE INDEX IF NOT EXISTS generation_attempts_canonical ON generation_attempts(block_id,text_hash);")?;
        tx.commit()?;
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

    /// Returns the effective attempt for the canonical block and text across profiles.
    ///
    /// # Errors
    /// Returns an error if the database cannot be read.
    pub fn latest(
        &self,
        block: &AudioBlock,
        _profile: &AudioProfile,
    ) -> Result<Option<GenerationAttempt>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let attempt = effective_attempt(&tx, &block.id, &text_hash(&block.text))?;
        tx.commit()?;
        Ok(attempt)
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
        self.prepare_inner(block, profile, RetryConsent::Any(allow_uncertain_retry))
    }

    /// Creates or resumes an attempt using consent bound to the exact prior attempt ID.
    ///
    /// # Errors
    /// Returns an error for an unresolved submission or database failure.
    pub fn prepare_with_consent(
        &self,
        block: &AudioBlock,
        profile: &AudioProfile,
        confirmed_attempt: Option<&str>,
    ) -> Result<GenerationPrepare> {
        self.prepare_inner(block, profile, RetryConsent::Exact(confirmed_attempt))
    }

    fn prepare_inner(
        &self,
        block: &AudioBlock,
        profile: &AudioProfile,
        consent: RetryConsent<'_>,
    ) -> Result<GenerationPrepare> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let profile_id = profile.id();
        let hash = text_hash(&block.text);
        let latest = effective_attempt(&tx, &block.id, &hash)?;
        let mut retry_of = None;
        if let Some(attempt) = latest {
            match attempt.state {
                GenerationState::Prepared => {
                    tx.execute(
                        "UPDATE generation_attempts SET profile_id=?2 WHERE id=?1",
                        params![attempt.id, profile_id],
                    )?;
                    let mut attempt = attempt;
                    attempt.profile_id = profile_id;
                    tx.commit()?;
                    return Ok(GenerationPrepare::Ready(attempt));
                }
                GenerationState::Uncertain | GenerationState::Completed
                    if consent.permits(&attempt.id) =>
                {
                    retry_of = Some(attempt.id);
                }
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
            retry_of,
            provider_cost: None,
        };
        tx.execute("INSERT INTO generation_attempts(id,block_id,profile_id,text_hash,state,prepared_at,retry_of) VALUES (?1,?2,?3,?4,'prepared',?5,?6)", params![attempt.id, attempt.block_id, attempt.profile_id, attempt.text_hash, attempt.prepared_at, attempt.retry_of])?;
        tx.commit()?;
        Ok(GenerationPrepare::Ready(attempt))
    }

    /// Persists submission before the provider request is sent.
    ///
    /// # Errors
    /// Returns an error if the attempt is not prepared or the update fails.
    pub fn mark_submitted(&self, id: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let attempt = tx
            .query_row(
                &format!("SELECT {SELECT_FIELDS} FROM generation_attempts WHERE id=?1"),
                [id],
                read_attempt,
            )
            .optional()?;
        let Some(attempt) = attempt else {
            return Self::require_transition(0);
        };
        if attempt.state != GenerationState::Prepared
            || effective_attempt(&tx, &attempt.block_id, &attempt.text_hash)?
                .is_none_or(|latest| latest.id != id)
        {
            return Self::require_transition(0);
        }
        let changed = tx.execute("UPDATE generation_attempts SET state='submitted',submitted_at=?2 WHERE id=?1 AND state='prepared'", params![id, now()])?;
        Self::require_transition(changed)?;
        tx.commit()?;
        Ok(())
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

    /// Reads an individual generation attempt for usage reconciliation.
    ///
    /// # Errors
    /// Returns an error if the database cannot be read.
    pub fn attempt(&self, id: &str) -> Result<Option<GenerationAttempt>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SELECT_FIELDS} FROM generation_attempts WHERE id=?1"),
                [id],
                read_attempt,
            )
            .optional()?)
    }

    /// Reads attempt history for a bounded set of canonical block IDs, in creation order.
    ///
    /// # Errors
    /// Returns an error for an oversized request/history or database failure.
    pub fn attempts_for_blocks(&self, ids: &[String]) -> Result<Vec<GenerationAttempt>> {
        const MAX_IDS: usize = 100_000;
        const MAX_ATTEMPTS: usize = 100_000;
        if ids.len() > MAX_IDS
            || ids.iter().any(|id| {
                id.len() != 64
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            return Err(Error::Format(
                "Некорректный список аудиоблоков для сверки расходов".into(),
            ));
        }
        let unique: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let mut rows = Vec::new();
        {
            let mut statement = tx.prepare(&format!("SELECT rowid,{SELECT_FIELDS} FROM generation_attempts WHERE block_id=?1 ORDER BY rowid"))?;
            for id in unique {
                let mut found = statement.query([id])?;
                while let Some(row) = found.next()? {
                    if rows.len() == MAX_ATTEMPTS {
                        return Err(Error::Format(
                            "Слишком много попыток для сверки расходов".into(),
                        ));
                    }
                    let rowid: i64 = row.get(0)?;
                    let attempt = GenerationAttempt {
                        id: row.get(1)?,
                        block_id: row.get(2)?,
                        profile_id: row.get(3)?,
                        text_hash: row.get(4)?,
                        state: match row.get::<_, String>(5)?.as_str() {
                            "prepared" => GenerationState::Prepared,
                            "submitted" => GenerationState::Submitted,
                            "completed" => GenerationState::Completed,
                            "uncertain" => GenerationState::Uncertain,
                            _ => {
                                return Err(Error::Format("Некорректное состояние журнала".into()));
                            }
                        },
                        prepared_at: row.get(6)?,
                        submitted_at: row.get(7)?,
                        finished_at: row.get(8)?,
                        provider_response_id: row.get(9)?,
                        retry_of: row.get(10)?,
                        provider_cost: row.get(11)?,
                    };
                    rows.push((rowid, attempt));
                }
            }
        }
        tx.commit()?;
        rows.sort_unstable_by_key(|(rowid, _)| *rowid);
        Ok(rows.into_iter().map(|(_, attempt)| attempt).collect())
    }

    /// Records immutable provider-reported USD usage without floating-point conversion.
    ///
    /// # Errors
    /// Returns an error for invalid cost, a conflicting value, or an ineligible attempt.
    pub fn note_cost(&self, id: &str, cost: &str) -> Result<()> {
        if !valid_decimal_cost(cost) {
            return Err(Error::Format("Некорректная стоимость генерации".into()));
        }
        let changed = self.conn.execute("UPDATE generation_attempts SET provider_cost=?2 WHERE id=?1 AND state IN ('submitted','completed','uncertain') AND (provider_cost IS NULL OR provider_cost=?2)", params![id, cost])?;
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

fn valid_decimal_cost(cost: &str) -> bool {
    if cost.is_empty() || cost.len() > 128 {
        return false;
    }
    let mut parts = cost.split(['e', 'E']);
    let mantissa = parts.next().unwrap_or("");
    let exponent = parts.next();
    if parts.next().is_some() {
        return false;
    }
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if exponent.is_empty() || !exponent.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
    }
    let mut pieces = mantissa.split('.');
    let integer = pieces.next().unwrap_or("");
    let fractional = pieces.next();
    !integer.is_empty()
        && integer.bytes().all(|byte| byte.is_ascii_digit())
        && fractional.is_none_or(|value| {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        })
        && pieces.next().is_none()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
                synthesis: None,
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
    fn legacy_cross_profile_prepared_cannot_hide_uncertain_attempt() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        fs::create_dir_all(&dir)?;
        let book = crate::Book {
            id: "book".into(),
            chapters: vec![crate::Chapter {
                title: "chapter".into(),
                text: "Небольшой текст.".into(),
            }],
            ..crate::Book::default()
        };
        let (_, mut first_profile) = fixture();
        first_profile.engine = "openrouter".into();
        first_profile.model = Some("test/model".into());
        let mut other_profile = first_profile.clone();
        other_profile
            .dictionary
            .insert("отсутствует".into(), "замена".into());
        other_profile.max_escaped_bytes = Some(2500);
        let block = crate::plan_chapter(&book, 0, &first_profile).remove(0);
        let other_block = crate::plan_chapter(&book, 0, &other_profile).remove(0);
        assert_eq!(block.id, other_block.id);
        assert_ne!(first_profile.id(), other_profile.id());
        let conn = Connection::open(dir.join("readio.sqlite3"))?;
        conn.execute_batch("CREATE TABLE generation_attempts (id TEXT PRIMARY KEY,block_id TEXT NOT NULL,profile_id TEXT NOT NULL,text_hash TEXT NOT NULL,state TEXT NOT NULL,prepared_at INTEGER NOT NULL,submitted_at INTEGER,finished_at INTEGER,provider_response_id TEXT);")?;
        conn.execute(
            "INSERT INTO generation_attempts VALUES (?1,?2,?3,?4,'uncertain',1,2,3,NULL)",
            params![
                "old-risk",
                block.id,
                first_profile.id(),
                text_hash(&block.text)
            ],
        )?;
        conn.execute(
            "INSERT INTO generation_attempts VALUES (?1,?2,?3,?4,'prepared',4,NULL,NULL,NULL)",
            params![
                "unsafe-prepared",
                block.id,
                other_profile.id(),
                text_hash(&block.text)
            ],
        )?;
        drop(conn);
        let journal = GenerationJournal::open(&dir)?;
        assert_eq!(
            journal.latest(&block, &other_profile)?.unwrap().id,
            "old-risk"
        );
        assert!(journal.mark_submitted("unsafe-prepared").is_err());
        assert!(
            matches!(journal.prepare(&block, &other_profile, false)?, GenerationPrepare::RequiresConfirmation(attempt) if attempt.id == "old-risk")
        );
        let retry = ready(journal.prepare(&block, &other_profile, true)?);
        assert_eq!(retry.retry_of.as_deref(), Some("old-risk"));
        journal.mark_submitted(&retry.id)?;
        journal.return_to_prepared(&retry.id)?;
        drop(journal);
        let journal = GenerationJournal::open(&dir)?;
        let resumed = ready(journal.prepare(&block, &first_profile, false)?);
        assert_eq!(resumed.id, retry.id);
        assert_eq!(resumed.retry_of.as_deref(), Some("old-risk"));
        assert_eq!(resumed.profile_id, first_profile.id());
        journal.mark_submitted(&resumed.id)?;
        assert!(journal.prepare(&block, &other_profile, true).is_err());
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn legacy_prepared_cannot_hide_completed_attempt() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let completed = ready(journal.prepare(&block, &profile, false)?);
        journal.mark_submitted(&completed.id)?;
        journal.complete(&completed.id, None)?;
        journal.conn.execute("INSERT INTO generation_attempts(id,block_id,profile_id,text_hash,state,prepared_at) VALUES (?1,?2,?3,?4,'prepared',?5)", params!["legacy-prepared", block.id, "old-profile", text_hash(&block.text), now()])?;
        assert_eq!(journal.latest(&block, &profile)?.unwrap().id, completed.id);
        assert!(journal.mark_submitted("legacy-prepared").is_err());
        assert!(
            matches!(journal.prepare(&block, &profile, false)?, GenerationPrepare::RequiresConfirmation(attempt) if attempt.id == completed.id)
        );
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn consent_for_older_attempt_does_not_authorize_newer_unknown_result() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let first = ready(journal.prepare_with_consent(&block, &profile, None)?);
        journal.mark_submitted(&first.id)?;
        journal.mark_uncertain(&first.id, None)?;
        let second = ready(journal.prepare_with_consent(&block, &profile, Some(&first.id))?);
        journal.mark_submitted(&second.id)?;
        journal.mark_uncertain(&second.id, None)?;
        assert!(matches!(
            journal.prepare_with_consent(&block, &profile, Some(&first.id))?,
            GenerationPrepare::RequiresConfirmation(attempt) if attempt.id == second.id
        ));
        let third = ready(journal.prepare_with_consent(&block, &profile, Some(&second.id))?);
        assert_eq!(third.retry_of.as_deref(), Some(second.id.as_str()));
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn provider_cost_is_decimal_and_immutable_across_reopen() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let journal = GenerationJournal::open(&dir)?;
        let attempt = ready(journal.prepare(&block, &profile, false)?);
        assert!(journal.note_cost(&attempt.id, "0.01").is_err());
        journal.mark_submitted(&attempt.id)?;
        for bad in ["-1", "NaN", "1e", "0.1.2", "0; DROP TABLE"] {
            assert!(journal.note_cost(&attempt.id, bad).is_err());
        }
        journal.note_cost(&attempt.id, "1.25e-6")?;
        journal.note_cost(&attempt.id, "1.25e-6")?;
        assert!(journal.note_cost(&attempt.id, "0.01").is_err());
        journal.mark_uncertain(&attempt.id, None)?;
        drop(journal);
        let journal = GenerationJournal::open(&dir)?;
        assert_eq!(
            journal
                .attempt(&attempt.id)?
                .unwrap()
                .provider_cost
                .as_deref(),
            Some("1.25e-6")
        );
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn history_is_deduplicated_and_keeps_submission_order() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (first_block, profile) = fixture();
        let mut second_block = first_block.clone();
        second_block.id = "b".repeat(64);
        let journal = GenerationJournal::open(&dir)?;
        let first = ready(journal.prepare(&first_block, &profile, false)?);
        journal.mark_submitted(&first.id)?;
        journal.note_response(&first.id, Some("gen-first"))?;
        journal.note_cost(&first.id, "0.001")?;
        journal.mark_uncertain(&first.id, None)?;
        let second = ready(journal.prepare(&second_block, &profile, false)?);
        let attempts = journal.attempts_for_blocks(&[
            second_block.id,
            first_block.id.clone(),
            first_block.id,
        ])?;
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].id, first.id);
        assert_eq!(attempts[0].provider_cost.as_deref(), Some("0.001"));
        assert_eq!(attempts[1].id, second.id);
        assert!(journal.attempts_for_blocks(&["bad".into()]).is_err());
        drop(journal);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn stale_prepared_cannot_submit_across_connections() -> Result<()> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let (block, profile) = fixture();
        let first = GenerationJournal::open(&dir)?;
        let stale = ready(first.prepare(&block, &profile, false)?);
        first.conn.execute("INSERT INTO generation_attempts(id,block_id,profile_id,text_hash,state,prepared_at) VALUES (?1,?2,?3,?4,'prepared',?5)", params!["newer", block.id, profile.id(), text_hash(&block.text), now()])?;
        let second = GenerationJournal::open(&dir)?;
        let barrier = Arc::new(Barrier::new(2));
        let left_barrier = Arc::clone(&barrier);
        let stale_id = stale.id;
        let left = thread::spawn(move || {
            left_barrier.wait();
            first.mark_submitted(&stale_id).is_ok()
        });
        let right = thread::spawn(move || {
            barrier.wait();
            second.mark_submitted("newer").is_ok()
        });
        assert!(!left.join().unwrap());
        assert!(right.join().unwrap());
        fs::remove_dir_all(dir)?;
        Ok(())
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
        block.id = "b".repeat(64);
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
        let threads: [thread::JoinHandle<bool>; 2] = std::array::from_fn(|_| {
            let dir = dir.clone();
            let barrier = Arc::clone(&barrier);
            let id = attempt.id.clone();
            thread::spawn(move || {
                let journal = GenerationJournal::open(&dir).unwrap();
                barrier.wait();
                journal.mark_submitted(&id).is_ok()
            })
        });
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
