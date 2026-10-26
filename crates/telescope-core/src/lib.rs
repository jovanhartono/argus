//! Telescope's search engine: the file-name index, the fuzzy matcher and ranking.

mod index;
mod walk;

pub use index::{EntryId, Index};
pub use walk::{Scan, scan};
