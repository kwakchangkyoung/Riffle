//! Riffle 원격 파일 소스 (SFTP, S3). Tauri 를 모르는 순수 라이브러리.

mod agent;
mod block_cache;
mod conn;
mod error;
mod pool;
mod remote_file;
mod s3;
mod ssh_config;
mod uri;

pub use agent::{AGENT_VERSION, AgentEvent, AgentSession, QueryJson};
pub use conn::Transport;
pub use error::RemoteError;
pub use pool::{RemoteEntry, SftpPool};
pub use riffle_core::RandomRead;
pub use s3::{
    S3Auth, S3Entry, S3Pool, S3StaticCreds, S3Uri, list_aws_profiles, list_aws_profiles_from,
};
pub use ssh_config::{SshHost, list_ssh_hosts, list_ssh_hosts_from};
pub use uri::RemoteUri;
