use thiserror::Error;

pub type ResourceResult<T> = Result<T, ResourceError>;

#[derive(Debug, Error)]
pub enum ResourceError {
    #[error("invalid resource uri: {0}")]
    InvalidUri(String),
    #[error("catalog error: {0}")]
    Catalog(String),
    #[error("provider error: {0}")]
    Provider(String),
}
