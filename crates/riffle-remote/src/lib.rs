//! Riffle 원격 파일 소스 (SFTP). Tauri 를 모르는 순수 라이브러리.

mod conn;
mod error;
mod pool;
mod remote_file;
mod ssh_config;
mod uri;

pub use conn::Transport;
pub use error::RemoteError;
pub use pool::{RemoteEntry, SftpPool};
pub use riffle_core::RandomRead;
pub use ssh_config::{SshHost, list_ssh_hosts, list_ssh_hosts_from};
pub use uri::RemoteUri;
