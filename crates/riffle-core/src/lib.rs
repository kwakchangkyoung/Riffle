pub mod error;
pub mod index;
pub mod lines;
pub mod search;
pub mod source;

#[cfg(test)]
mod tests;

pub use error::{Error, Result};
pub use index::{CHECKPOINT_INTERVAL, IndexProgress, LineIndex};
pub use lines::{Line, LogFile, MAX_DISPLAY_BYTES};
pub use search::{
    MAX_RESULTS, Matcher, SearchOptions, SearchOutcome, SearchProgress, search, utf16_ranges,
};
pub use source::Source;
