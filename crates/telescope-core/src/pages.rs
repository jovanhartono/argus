//! Big blocks of memory straight from the operating system, which takes them back the moment
//! they are freed.
//!
//! macOS's own allocator keeps freed big blocks for reuse, still counted against the app.
//! A scan of the whole home folder frees about 50 MB of outgrown lists and lookup tables on
//! the way, so without this the app would hold twice the index's memory long after the scan.

use std::alloc::{GlobalAlloc, Layout, System};

/// Install with `#[global_allocator]` in the program. Blocks of 1 MiB and up are mapped
/// pages of their own; smaller ones, and every block off macOS, go to the system allocator.
pub struct Pages;

/// Blocks this big and up get their own pages. Smaller ones come and go too often, as a text
/// search's line buffers do, for a system call each to pay off.
#[cfg(target_os = "macos")]
const OWN_PAGES: usize = 1 << 20;

#[cfg(target_os = "macos")]
fn own_pages(layout: Layout) -> bool {
    layout.size() >= OWN_PAGES && layout.align() <= page_size()
}

#[cfg(target_os = "macos")]
fn page_size() -> usize {
    // SAFETY: sysconf only reads a system value.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).unwrap_or(4096)
}

#[cfg(target_os = "macos")]
fn round_up(size: usize, page: usize) -> usize {
    size.div_ceil(page) * page
}

#[cfg(target_os = "macos")]
fn map(size: usize) -> *mut u8 {
    // SAFETY: a new anonymous private mapping, which no other memory overlaps.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        std::ptr::null_mut()
    } else {
        at.cast()
    }
}

/// # Safety
/// `at` and `size` are a whole mapping from `map`, or its end from a page boundary on.
#[cfg(target_os = "macos")]
unsafe fn unmap(at: *mut u8, size: usize) {
    // SAFETY: as the caller promises; the pages are no longer used.
    unsafe { libc::munmap(at.cast(), size) };
}

// SAFETY: every block is valid for its layout until freed: mapped pages are page-aligned,
// which `own_pages` checks covers the alignment, and hold at least the size asked for. Each
// block is freed by the side that made it, as `own_pages` gives the same answer for a
// block's layout every time.
#[cfg(target_os = "macos")]
unsafe impl GlobalAlloc for Pages {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if own_pages(layout) {
            map(layout.size())
        } else {
            // SAFETY: as the caller promises.
            unsafe { System.alloc(layout) }
        }
    }

    /// Mapped pages start out zero, and stay out of memory until written.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if own_pages(layout) {
            map(layout.size())
        } else {
            // SAFETY: as the caller promises.
            unsafe { System.alloc_zeroed(layout) }
        }
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        if own_pages(layout) {
            // SAFETY: a mapping of this size from `alloc`, or what `realloc` left of one.
            unsafe { unmap(at, layout.size()) };
        } else {
            // SAFETY: as the caller promises.
            unsafe { System.dealloc(at, layout) };
        }
    }

    unsafe fn realloc(&self, at: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller promises a size that rounded up to the alignment fits an isize.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        match (own_pages(layout), own_pages(new)) {
            // SAFETY: as the caller promises.
            (false, false) => unsafe { System.realloc(at, layout, new_size) },
            // A list trimmed to its length keeps its place and gives back the pages past it.
            (true, true) if new_size <= layout.size() => {
                let page = page_size();
                let (keep, had) = (round_up(new_size, page), round_up(layout.size(), page));
                if had > keep {
                    // SAFETY: the end of this block's mapping, from a page boundary on.
                    unsafe { unmap(at.add(keep), had - keep) };
                }
                at
            }
            // A grown list is copied a step at a time, each step's old pages given back at once,
            // so growing the 30 MB name text never holds two copies of it.
            (true, true) => {
                let moved = map(new_size);
                if !moved.is_null() {
                    let had = round_up(layout.size(), page_size());
                    for from in (0..had).step_by(OWN_PAGES) {
                        let to = had.min(from + OWN_PAGES);
                        // SAFETY: `from..to` lies within the old mapping, which nothing uses past
                        // this point, and the copy within both blocks; `from` is page-aligned.
                        unsafe {
                            let bytes = layout.size().min(to) - from;
                            std::ptr::copy_nonoverlapping(at.add(from), moved.add(from), bytes);
                            unmap(at.add(from), to - from);
                        }
                    }
                }
                moved
            }
            _ => {
                // SAFETY: `new` is a valid layout, as above.
                let moved = unsafe { self.alloc(new) };
                if !moved.is_null() {
                    // SAFETY: both blocks hold at least the smaller size and never overlap.
                    unsafe {
                        std::ptr::copy_nonoverlapping(at, moved, layout.size().min(new_size));
                        self.dealloc(at, layout);
                    }
                }
                moved
            }
        }
    }
}

// SAFETY: every call goes to the system allocator as it is.
#[cfg(not(target_os = "macos"))]
unsafe impl GlobalAlloc for Pages {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller promises.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller promises.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        // SAFETY: as the caller promises.
        unsafe { System.dealloc(at, layout) }
    }

    unsafe fn realloc(&self, at: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as the caller promises.
        unsafe { System.realloc(at, layout, new_size) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    /// Allocates, fills, grows or trims through every pair of sizes either side of the line,
    /// and checks that what was written survives each move.
    #[test]
    fn keeps_what_was_written_across_every_size() {
        let sizes = [
            0,
            1,
            4095,
            4096,
            100_000,
            MIB - 1,
            MIB,
            MIB + 1,
            3 * MIB + 77,
            9 * MIB,
        ];
        for align in [1, 8, 64, 4096] {
            for &from in &sizes {
                for &to in &sizes {
                    let (from, to) = (from.max(1), to.max(1));
                    let layout = Layout::from_size_align(from, align).unwrap();
                    // SAFETY: a non-zero size; every block is freed once with its layout.
                    unsafe {
                        let at = Pages.alloc(layout);
                        assert!(!at.is_null());
                        assert_eq!(at as usize % align, 0);
                        let written = (0..from).step_by(997).chain([from - 1]);
                        for i in written.clone() {
                            at.add(i).write(i as u8);
                        }
                        let moved = Pages.realloc(at, layout, to);
                        assert!(!moved.is_null());
                        assert_eq!(moved as usize % align, 0);
                        for i in written.filter(|&i| i < to) {
                            assert_eq!(moved.add(i).read(), i as u8, "{from} to {to}");
                        }
                        moved.add(to - 1).write(1);
                        Pages.dealloc(moved, Layout::from_size_align(to, align).unwrap());
                    }
                }
            }
        }
    }

    #[test]
    fn zeroed_blocks_read_zero() {
        for size in [64, MIB, 5 * MIB] {
            let layout = Layout::from_size_align(size, 8).unwrap();
            // SAFETY: a non-zero size, freed once with its layout.
            unsafe {
                let at = Pages.alloc_zeroed(layout);
                let bytes = std::slice::from_raw_parts(at, size);
                assert!(bytes.iter().all(|&byte| byte == 0));
                Pages.dealloc(at, layout);
            }
        }
    }

    /// The memory macOS counts against the process, as Activity Monitor shows it, and the most
    /// it has been.
    #[cfg(target_os = "macos")]
    fn footprints() -> (u64, u64) {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
        let pid = i32::try_from(std::process::id()).unwrap();
        // SAFETY: `info` is big enough for the version asked for.
        let read =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
        assert_eq!(read, 0);
        // SAFETY: filled in by the call above.
        let info = unsafe { info.assume_init() };
        (info.ri_phys_footprint, info.ri_lifetime_max_phys_footprint)
    }

    #[cfg(target_os = "macos")]
    fn footprint() -> u64 {
        footprints().0
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_grown_big_block_never_counts_twice() {
        const SIZE: usize = 64 * MIB;
        let layout = Layout::from_size_align(SIZE, 8).unwrap();
        // SAFETY: a non-zero size; the block is freed once with its layout.
        let (held, most_before, most_after) = unsafe {
            let at = Pages.alloc(layout);
            std::ptr::write_bytes(at, 1, SIZE);
            let (held, most_before) = footprints();
            let at = Pages.realloc(at, layout, SIZE + SIZE / 4);
            assert_eq!(at.add(SIZE - 1).read(), 1);
            let most_after = footprints().1;
            Pages.dealloc(at, Layout::from_size_align(SIZE + SIZE / 4, 8).unwrap());
            (held, most_before, most_after)
        };
        // Other tests allocate meanwhile, so allow them some.
        let allowed = most_before.max(held + (SIZE / 2) as u64);
        assert!(
            most_after <= allowed,
            "{held}, most {most_before} then {most_after}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_freed_big_block_stops_counting_at_once() {
        // The system allocator keeps a freed block this size counted.
        const SIZE: usize = 64 * MIB;
        let layout = Layout::from_size_align(SIZE, 8).unwrap();
        // SAFETY: a non-zero size; the block is freed once with its layout.
        let (held, after) = unsafe {
            let at = Pages.alloc(layout);
            std::ptr::write_bytes(at, 1, SIZE);
            let held = footprint();
            // Trimmed to a quarter, then freed.
            let at = Pages.realloc(at, layout, SIZE / 4);
            let trimmed = footprint();
            let gone = held.saturating_sub(trimmed);
            assert!(gone > (SIZE / 2) as u64, "{held} then {trimmed}");
            Pages.dealloc(at, Layout::from_size_align(SIZE / 4, 8).unwrap());
            (held, footprint())
        };
        // Other tests allocate meanwhile, so allow them some.
        let gone = held.saturating_sub(after);
        assert!(gone > (SIZE - 16 * MIB) as u64, "{held} then {after}");
    }
}
