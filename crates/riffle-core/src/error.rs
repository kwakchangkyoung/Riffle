use thiserror::Error;

/// Riffle 코어 에러 타입
#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O 오류: {0}")]
    Io(#[from] std::io::Error),
    #[error("잘못된 검색어: {0}")]
    InvalidQuery(String),
}

/// Riffle 코어 Result 타입 별칭
pub type Result<T> = std::result::Result<T, Error>;
