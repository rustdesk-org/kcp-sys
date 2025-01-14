#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid state")]
    InvalidState,
    #[error("Invalid state need reset")]
    InvalidStateNeedRst,
    #[error("Connection reset")]
    ConnectioinReset,
    #[error("Create connection failed")]
    CreateConnectionFailed,
    #[error("Anyhow error")]
    AnyhowError(#[from] anyhow::Error),

    #[error("Connect timeout")]
    ConnectTimeout,

    #[error("Shutdown")]
    Shutdown,
}
