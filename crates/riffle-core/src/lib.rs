pub mod error;
pub mod filter;
pub mod gz;
pub mod index;
pub mod lines;
pub mod lineset;
pub mod search;
pub mod source;

#[cfg(test)]
mod tests;

pub use error::{Error, Result};
pub use filter::{Combine, FilterRule, FilterSet, FilterSpec, Level, RuleKind, line_level};
pub use gz::{DecompressProgress, GzWriter, cleanup_temp_dir, is_gzip, open_gzip};
pub use index::{CHECKPOINT_INTERVAL, IndexProgress, LineIndex};
pub use lines::{Line, LogFile, MAX_DISPLAY_BYTES, RefreshOutcome};
pub use lineset::{LineSet, MAX_FILTER_RANGES};
pub use search::{
    MAX_RESULTS, Matcher, SearchOptions, SearchOutcome, SearchProgress, filter_lines_range,
    filter_scan, search, search_filtered, utf16_ranges,
};
pub use source::Source;
