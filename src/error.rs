use thiserror::Error;

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("mupdf error: {0}")]
    Mupdf(#[from] mupdf::error::Error),
    #[error("document has no pages")]
    EmptyDocument,
    #[error("conversion error: {0}")]
    Converting(String),
}
