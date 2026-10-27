//! Telescope's search engine: the file-name index, the fuzzy matcher and ranking.

mod ignore;
mod index;
mod search;
mod stand_in;
mod store;
mod walk;

pub use ignore::Ignore;
pub use index::{EntryId, Index};
pub use search::{Hit, Searcher, Ticket, Tickets};
pub use stand_in::stand_in;
pub use store::{OpenError, Opened, Saved, Settings, open_index, open_or_scan, save_index};
pub use walk::{Access, Clutter, Scan, home, scan};
