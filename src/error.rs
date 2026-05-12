use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Docker API error: {0}")]
    Docker(#[from] bollard::errors::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Registry authentication failed for {registry}: {reason}")]
    Auth { registry: String, reason: String },

    #[error("Missing 'Docker-Content-Digest' header for {image}")]
    MissingDigest { image: String },

    #[error("Failed to parse WWW-Authenticate header: {0}")]
    AuthHeader(String),

    #[error("No manifest found for {image}")]
    NoManifest { image: String },
}

pub type Result<T> = std::result::Result<T, Error>;
