pub mod error;

pub use error::{Error, Result};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn test_io_error_conversion() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "파일을 찾을 수 없음");
        let err: Error = io_err.into();
        match err {
            Error::Io(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
        }
    }
}
