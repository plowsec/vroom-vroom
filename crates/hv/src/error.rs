#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("hypervisor call {call} failed: {code:#x}")]
    Backend { call: &'static str, code: i64 },
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("guest fault: {0}")]
    GuestFault(String),
    #[error("memory: {0}")]
    Memory(#[from] bulwark_memory::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
