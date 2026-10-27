//! Telescope's search engine: the file-name index, the fuzzy matcher and ranking.

mod ignore;
mod index;
mod search;
mod stand_in;
mod walk;

pub use ignore::Ignore;
pub use index::{EntryId, Index};
pub use search::{Hit, Searcher, Ticket};
pub use stand_in::stand_in;
pub use walk::{Access, Clutter, Scan, home, scan};
