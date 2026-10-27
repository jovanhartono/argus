//! Telescope's search engine: the file-name index, the fuzzy matcher and ranking.

mod ignore;
mod index;
mod pages;
mod scope;
mod search;
mod stand_in;
mod store;
mod update;
mod walk;

pub use ignore::Ignore;
pub use index::{EntryId, Index};
pub use pages::Pages;
pub use scope::{Scope, Watch};
pub use search::{Hit, Searcher, Ticket, Tickets};
pub use stand_in::stand_in;
pub use store::{OpenError, Opened, Saved, Settings, open_index, open_or_scan, save_index};
pub use update::Update;
pub use walk::{Access, Clutter, Scan, home, scan, scan_until};

#[cfg(test)]
#[global_allocator]
static PAGES: Pages = Pages;
