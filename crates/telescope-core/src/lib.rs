//! Telescope's search engine: the file-name index, the fuzzy matcher and ranking.

mod index;
mod search;
mod walk;

pub use index::{EntryId, Index};
pub use search::{Hit, Searcher, Ticket};
pub use walk::{Scan, scan};
