/// Errors returned by [`crate::RegtestNode`].
#[derive(Debug, thiserror::Error)]
pub enum RegtestError {
    /// A submitted transaction blob could not be deserialized.
    #[error("bad tx blob: {0}")]
    BadTxBlob(String),
}
