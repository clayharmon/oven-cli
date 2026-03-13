#[derive(thiserror::Error, Debug)]
pub enum OvenError {
    #[error("config error: {0}")]
    Config(String),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("git error: {0}")]
    Git(String),
    #[error("pipeline error: {0}")]
    Pipeline(String),
    #[error("agent {agent} failed: {message}")]
    Agent { agent: String, message: String },
    #[error("github error: {0}")]
    GitHub(String),
    #[error("process error: {0}")]
    Process(String),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
