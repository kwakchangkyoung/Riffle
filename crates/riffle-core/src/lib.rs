pub mod error;
pub mod filter;
pub mod fingerprint;
pub mod gz;
pub mod index;
pub mod lines;
pub mod lineset;
pub mod merge;
pub mod remote;
pub mod search;
pub mod source;
pub mod structured;
pub mod timestamp;

#[cfg(test)]
mod tests;

pub use error::{Error, Result};
pub use filter::{Combine, FilterRule, FilterSet, FilterSpec, Level, RuleKind, line_level};
pub use fingerprint::{HEAD_FINGERPRINT_BYTES, head_fingerprint};
pub use gz::{
    DecompressProgress, GzWriter, cleanup_temp_dir, is_gzip, is_gzip_reader, open_gzip,
    open_gzip_remote,
};
pub use index::{CHECKPOINT_INTERVAL, IndexProgress, LineIndex};
pub use lines::{Line, LogFile, MAX_DISPLAY_BYTES, RefreshOutcome};
pub use lineset::{LineSet, MAX_FILTER_RANGES};
pub use merge::{MergeIndex, MergeProgress, MergedLine};
pub use remote::RandomRead;
pub use search::{
    MAX_RESULTS, Matcher, SearchOptions, SearchOutcome, SearchProgress, filter_lines_range,
    filter_scan, search, search_filtered, utf16_ranges,
};
pub use source::Source;
pub use structured::{Columns, EXTRA_COLUMN, LogFormat, detect_columns, split_fields};
pub use timestamp::{NEG_INF_TS, TsContext, parse_timestamp, year_of_unix_secs};
