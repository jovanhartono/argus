//! Telescope's search engine: the file-name index, the fuzzy matcher and ranking.

mod ignore;
mod index;
mod search;
mod walk;

pub use ignore::Ignore;
pub use index::{EntryId, Index};
pub use search::{Hit, Searcher, Ticket};
pub use walk::{Clutter, Scan, home, scan};
