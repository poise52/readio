pub use crate::ipc::{IpcError as BlockError, IpcErrorCode as BlockErrorCode};
pub type BlockResult<T> = Result<T, BlockError>;
