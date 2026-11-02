use desk_core::{GenerationAttempt, GenerationState};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockErrorCode {
    Cancelled,
    RetryRequired,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockError {
    pub code: BlockErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<GenerationState>,
}

impl BlockError {
    pub(crate) fn cancelled() -> Self {
        Self {
            code: BlockErrorCode::Cancelled,
            message: "Запрос озвучивания больше не нужен".into(),
            attempt_id: None,
            state: None,
        }
    }
    pub(crate) fn retry_required(attempt: GenerationAttempt) -> Self {
        Self {
            code: BlockErrorCode::RetryRequired,
            message: "Предыдущая генерация могла быть оплачена, но сохранённого аудио нет. Для повтора требуется подтверждение.".into(),
            attempt_id: Some(attempt.id), state: Some(attempt.state),
        }
    }
}
impl From<String> for BlockError {
    fn from(message: String) -> Self {
        Self {
            code: BlockErrorCode::Failed,
            message,
            attempt_id: None,
            state: None,
        }
    }
}
impl From<&str> for BlockError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl From<BlockError> for String {
    fn from(error: BlockError) -> Self {
        error.message
    }
}
impl std::fmt::Display for BlockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}
impl std::error::Error for BlockError {}

pub type BlockResult<T> = Result<T, BlockError>;
