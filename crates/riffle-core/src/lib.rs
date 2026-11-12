pub mod error;
pub mod gz;
pub mod index;
pub mod lines;
pub mod search;
pub mod source;

#[cfg(test)]
mod tests;

pub use error::{Error, Result};
pub use gz::{DecompressProgress, GzWriter, cleanup_temp_dir, is_gzip, open_gzip};
pub use index::{CHECKPOINT_INTERVAL, IndexProgress, LineIndex};
pub use lines::{Line, LogFile, MAX_DISPLAY_BYTES, RefreshOutcome};
pub use search::{
    MAX_RESULTS, Matcher, SearchOptions, SearchOutcome, SearchProgress, search, utf16_ranges,
};
pub use source::Source;
