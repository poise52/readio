//! Error Type
use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("tungstenite error: {0}")]
    TungsteniteError(#[from] tungstenite::Error),
    #[error("serde json error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
    #[error("unexpected message: {0}")]
    UnexpectedMessage(String),
}
