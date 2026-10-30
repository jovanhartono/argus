//! Argus's search engine: the file-name index, the fuzzy matcher, ranking and frecency.

mod apps;
mod frecency;
mod ignore;
mod index;
mod name_starts;
mod pages;
mod scope;
mod search;
mod siblings;
mod stand_in;
mod store;
mod update;
mod volume;
mod walk;
mod within;

pub use apps::{AppId, Apps};
pub use frecency::{Boosts, Frecency, FrecencyError, unix_now};
pub use ignore::Ignore;
pub use index::{EntryId, Index};
pub use pages::Pages;
pub use scope::{Scope, Watch};
pub use search::{Hit, Item, Searcher, Ticket, Tickets};
pub use stand_in::stand_in;
pub use store::{
    OpenError, Opened, Saved, Settings, open_index, open_or_scan, remove_old_temps, save_index,
};
pub use update::{Changes, Update};
pub use volume::{Refusal, check_volume};
pub use walk::{Access, Cloud, Clutter, Scan, home, reads_inside, scan, scan_until};
pub use within::Within;

#[cfg(test)]
#[global_allocator]
static PAGES: Pages = Pages;
