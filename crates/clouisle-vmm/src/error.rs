use clouisle_core::ClouisleError;

/// VMM 层专用错误。
pub type VmmResult<T> = std::result::Result<T, VmmError>;

#[derive(Debug, thiserror::Error)]
pub enum VmmError {
    #[error("Firecracker API error: {0}")]
    Api(String),
    #[error("Socket not ready (timeout)")]
    SocketNotReady,
    #[error("Firecracker process exited unexpectedly: {0}")]
    ProcessDied(String),
    #[error("Jailer error: {0}")]
    Jailer(String),
    #[error("Check config failed: {0}")]
    Config(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<VmmError> for ClouisleError {
    fn from(e: VmmError) -> Self {
        use clouisle_core::ErrorKind;
        ClouisleError::with_source(ErrorKind::Vmm, e.to_string(), e)
    }
}