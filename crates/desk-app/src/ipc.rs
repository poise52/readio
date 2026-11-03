use desk_core::{GenerationAttempt, GenerationState};
use serde::Serialize;
use std::collections::BTreeMap;

macro_rules! wire_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        #[cfg(test)]
        impl $name {
            const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

wire_enum!(IpcErrorCode {
    Cancelled,
    RetryRequired,
    Failed,
    StaleQuote,
    BudgetExceeded,
    InvalidRequest
});
wire_enum!(OfflineJobState {
    None,
    Paused,
    Running,
    Complete,
    Error
});

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcError {
    pub code: IpcErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<GenerationState>,
}

impl IpcError {
    pub(crate) fn cancelled() -> Self {
        Self {
            code: IpcErrorCode::Cancelled,
            message: "Запрос озвучивания больше не нужен".into(),
            retryable: false,
            details: None,
            attempt_id: None,
            state: None,
        }
    }
    pub(crate) fn retry_required(attempt: GenerationAttempt) -> Self {
        Self { code: IpcErrorCode::RetryRequired, message: "Предыдущая генерация могла быть оплачена, но сохранённого аудио нет. Для повтора требуется подтверждение.".into(), retryable: false, details: None, attempt_id: Some(attempt.id), state: Some(attempt.state) }
    }
    pub(crate) fn stale_quote(message: &str) -> Self {
        Self {
            code: IpcErrorCode::StaleQuote,
            message: message.into(),
            retryable: true,
            details: None,
            attempt_id: None,
            state: None,
        }
    }
}
impl From<String> for IpcError {
    fn from(message: String) -> Self {
        Self {
            code: IpcErrorCode::Failed,
            message,
            retryable: false,
            details: None,
            attempt_id: None,
            state: None,
        }
    }
}
impl From<&str> for IpcError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl From<IpcError> for String {
    fn from(error: IpcError) -> Self {
        error.message
    }
}
impl std::fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for IpcError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn union<T: Serialize>(values: &[T]) -> Result<String, serde_json::Error> {
        values
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map(|values| values.join(" | "))
    }

    fn contract() -> Result<String, serde_json::Error> {
        let states = [
            GenerationState::Prepared,
            GenerationState::Submitted,
            GenerationState::Completed,
            GenerationState::Uncertain,
        ];
        Ok(format!(
            "// Generated from crates/desk-app/src/ipc.rs; do not edit by hand.\n// prettier-ignore\nexport type IpcErrorCode = {};\n// prettier-ignore\nexport type OfflineJobState = {};\n// prettier-ignore\nexport type GenerationState = {};\nexport interface IpcError {{\n  code: IpcErrorCode;\n  message: string;\n  retryable: boolean;\n  details?: Record<string, string>;\n  attemptId?: string;\n  state?: GenerationState;\n}}\n",
            union(IpcErrorCode::ALL)?,
            union(OfflineJobState::ALL)?,
            union(&states)?,
        ))
    }

    #[test]
    fn typescript_contract_matches_rust_wire_values() -> Result<(), Box<dyn std::error::Error>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../frontend/app/types/ipc.ts");
        assert_eq!(
            std::fs::read_to_string(path)?.replace("\r\n", "\n"),
            contract()?
        );
        Ok(())
    }

    #[test]
    fn error_wire_shape_keeps_attempt_and_omits_empty_details() -> Result<(), serde_json::Error> {
        let error = IpcError {
            code: IpcErrorCode::RetryRequired,
            message: "confirm".into(),
            retryable: false,
            details: None,
            attempt_id: Some("attempt".into()),
            state: Some(GenerationState::Uncertain),
        };
        assert_eq!(
            serde_json::to_value(error)?,
            serde_json::json!({
                "code": "retry_required", "message": "confirm", "retryable": false,
                "attemptId": "attempt", "state": "uncertain"
            })
        );
        Ok(())
    }

    #[test]
    #[ignore = "writes the generated TypeScript contract"]
    fn regenerate_typescript_contract() -> Result<(), Box<dyn std::error::Error>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../frontend/app/types/ipc.ts");
        std::fs::write(path, contract()?)?;
        Ok(())
    }
}
