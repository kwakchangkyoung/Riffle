//! Riffle 원격 파일 소스 (SFTP, S3). Tauri 를 모르는 순수 라이브러리.

/// 스트리밍 목록(`list_prefix_stream`/`list_dir_stream`)의 누적 항목 상한.
/// 넘으면 그 지점까지만 보내고 `ListSummary::truncated` 를 세운다.
pub const LIST_STREAM_MAX: usize = 200_000;

/// 스트리밍 목록이 한 번에 on_page 로 넘기는 항목 수 (S3 응답 페이지와 같은 크기).
pub(crate) const LIST_PAGE_SIZE: usize = 1_000;

/// 스트리밍 목록 호출의 종료 요약.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListSummary {
    /// on_page 로 전달된 항목 수.
    pub total: usize,
    /// 상한 도달로 나머지가 잘렸다.
    pub truncated: bool,
    /// 취소 플래그로 중간에 멈췄다.
    pub cancelled: bool,
}

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
