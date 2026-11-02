mod audio;
mod generation;
mod importer;
mod model;
mod store;

pub use audio::{
    AUDIO_BLOCK_MAX_ESCAPED_BYTES, AUDIO_BLOCK_MAX_PASSAGES, AudioBlock, AudioCache, AudioPart,
    AudioProfile, CachedAudio, OfflineManifest, plan_book, plan_chapter,
};
pub use generation::{GenerationAttempt, GenerationJournal, GenerationPrepare, GenerationState};
pub use importer::import_book;
pub use model::{
    BillingUnit, Book, BookSummary, BookmarkEntry, Chapter, Mark, Passage, Preferences,
    split_passages,
};
pub use store::Store;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),
    #[error("Ошибка базы данных: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Ошибка JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Некорректная книга: {0}")]
    Format(String),
    #[error("Книга не найдена")]
    NotFound,
}
pub type Result<T> = std::result::Result<T, Error>;
