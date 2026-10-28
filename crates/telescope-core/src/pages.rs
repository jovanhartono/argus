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

/// New pages, at `near` when nothing is mapped there, else wherever there is room.
#[cfg(target_os = "macos")]
fn map_near(near: *mut u8, size: usize) -> *mut u8 {
    // SAFETY: a new anonymous private mapping. Without MAP_FIXED, `near` is only a hint, so
    // the new pages never replace any already mapped.
    let at = unsafe {
        libc::mmap(
            near.cast(),
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

#[cfg(target_os = "macos")]
fn map(size: usize) -> *mut u8 {
    map_near(std::ptr::null_mut(), size)
}

/// Maps `size` bytes from `end` on, and only there.
#[cfg(target_os = "macos")]
fn extend(end: *mut u8, size: usize) -> bool {
    let at = map_near(end, size);
    if at == end {
        return true;
    }
    if !at.is_null() {
        // SAFETY: the whole mapping just made, which nothing else has seen.
        unsafe { unmap(at, size) };
    }
    false
}

/// # Safety
/// `at..at + size` are pages from `map_near`, from a page boundary on, perhaps from more than
/// one call when a block grew in place.
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
            // A grown list keeps its place when the pages after it are free. Otherwise it is
            // copied a step at a time, each step's old pages given back at once, so growing the
            // 30 MB name text never holds two copies of it.
            (true, true) => {
                let page = page_size();
                let (had, wants) = (round_up(layout.size(), page), round_up(new_size, page));
                // SAFETY: one past the end of this block's mapping.
                if wants <= had || extend(unsafe { at.add(had) }, wants - had) {
                    return at;
                }
                let moved = map(new_size);
                if !moved.is_null() {
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

    /// Sizes either side of the line, and of a page past it.
    const SIZES: [usize; 10] = [
        1,
        4095,
        4096,
        100_000,
        MIB - 1,
        MIB,
        MIB + 1,
        MIB + 4000,
        3 * MIB + 77,
        9 * MIB,
    ];

    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 8).unwrap()
    }

    /// A byte for each place that differs from the bytes a page or a MiB away, so a step
    /// copied to the wrong place shows.
    fn pattern(i: usize, salt: usize) -> u8 {
        let i = i.wrapping_add(salt);
        (i ^ i >> 8 ^ i >> 16 ^ i >> 24) as u8
    }

    /// Places worth checking in a block of `size`: a spread, both ends, and both sides of
    /// every MiB step.
    fn places(size: usize) -> impl Iterator<Item = usize> {
        let steps = (1..=size / MIB).flat_map(|k| [k * MIB - 1, k * MIB]);
        (0..size)
            .step_by(997)
            .chain(steps)
            .chain([size - 1])
            .filter(move |&i| i < size)
    }

    /// # Safety
    /// `at` holds `size` bytes.
    unsafe fn fill(at: *mut u8, size: usize, salt: usize) {
        for i in places(size) {
            // SAFETY: as the caller promises.
            unsafe { at.add(i).write(pattern(i, salt)) };
        }
    }

    /// Checks what `fill` wrote over `written` bytes, up to `size`.
    ///
    /// # Safety
    /// `at` holds `size` bytes.
    unsafe fn check(at: *const u8, written: usize, size: usize, salt: usize) {
        for i in places(written).filter(|&i| i < size) {
            // SAFETY: as the caller promises.
            let read = unsafe { at.add(i).read() };
            assert_eq!(read, pattern(i, salt), "{written} then {size}, at {i}");
        }
    }

    /// Allocates, fills, grows or trims through every pair of sizes, and checks that what was
    /// written survives each move, for alignments up to past a page.
    #[test]
    fn keeps_what_was_written_across_every_size() {
        for align in [1, 8, 64, 4096, 1 << 15, 1 << 16] {
            for from in SIZES {
                for to in SIZES {
                    let layout = Layout::from_size_align(from, align).unwrap();
                    let salt = from ^ to;
                    // SAFETY: a non-zero size; every block is freed once with its layout.
                    unsafe {
                        let at = Pages.alloc(layout);
                        assert!(!at.is_null());
                        assert_eq!(at as usize % align, 0);
                        fill(at, from, salt);
                        let moved = Pages.realloc(at, layout, to);
                        assert!(!moved.is_null());
                        assert_eq!(moved as usize % align, 0);
                        check(moved, from, to, salt);
                        fill(moved, to, salt);
                        check(moved, to, to, salt);
                        Pages.dealloc(moved, Layout::from_size_align(to, align).unwrap());
                    }
                }
            }
        }
    }

    #[test]
    fn zeroed_blocks_read_zero_where_a_freed_block_was() {
        for size in [64, MIB, 5 * MIB] {
            // SAFETY: a non-zero size; each block is freed once with its layout.
            unsafe {
                let at = Pages.alloc(layout(size));
                assert!(!at.is_null());
                std::ptr::write_bytes(at, 0xAA, size);
                Pages.dealloc(at, layout(size));
                let at = Pages.alloc_zeroed(layout(size));
                assert!(!at.is_null());
                let bytes = std::slice::from_raw_parts(at, size);
                assert!(bytes.iter().all(|&byte| byte == 0), "{size}");
                Pages.dealloc(at, layout(size));
            }
        }
    }

    #[test]
    fn a_block_too_big_to_map_is_null_and_the_old_one_stays() {
        let huge = 1 << 62;
        // SAFETY: non-zero sizes; each block is freed once with its layout.
        unsafe {
            assert!(Pages.alloc(layout(huge)).is_null());
            assert!(Pages.alloc_zeroed(layout(huge)).is_null());
            for size in [64 * 1024, 2 * MIB] {
                let at = Pages.alloc(layout(size));
                assert!(!at.is_null());
                fill(at, size, 3);
                assert!(Pages.realloc(at, layout(size), huge).is_null());
                check(at, size, size, 3);
                Pages.dealloc(at, layout(size));
            }
        }
    }

    /// Keeps up to 8 blocks at once through a seeded run of allocations, grows, trims and
    /// frees, checking every block after each step. Alone on macOS, where the memory counted
    /// at the end must be back where it started.
    #[test]
    fn chains_of_grows_trims_and_frees_keep_every_block() {
        #[cfg(target_os = "macos")]
        if !alone("chains_of_grows_trims_and_frees_keep_every_block") {
            return;
        }
        #[cfg(target_os = "macos")]
        let start = footprint();
        // SAFETY: non-zero sizes; each block is freed once with its layout.
        unsafe {
            let mut at = Pages.alloc(layout(9 * MIB));
            fill(at, 9 * MIB, 7);
            let mut size = 9 * MIB;
            for to in [MIB + 1, 3 * MIB + 77, MIB] {
                at = Pages.realloc(at, layout(size), to);
                assert!(!at.is_null());
                check(at, size, to, 7);
                fill(at, to, 7);
                size = to;
            }
            Pages.dealloc(at, layout(size));
        }
        let page = 16 * 1024;
        let sizes = [64 * 1024, MIB - page, MIB + page, 2 * MIB - 1].into_iter();
        let sizes: Vec<usize> = sizes.chain(SIZES).collect();
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        let mut blocks: [Option<(*mut u8, usize, usize)>; 8] = [None; 8];
        for salt in 0..600 {
            let slot = next() % blocks.len();
            let size = sizes[next() % sizes.len()];
            // SAFETY: as above; each block's size is what it was last given.
            unsafe {
                blocks[slot] = match blocks[slot] {
                    None if next() % 4 == 0 => {
                        let at = Pages.alloc_zeroed(layout(size));
                        assert!(!at.is_null());
                        assert!(places(size).all(|i| at.add(i).read() == 0));
                        fill(at, size, salt);
                        Some((at, size, salt))
                    }
                    None => {
                        let at = Pages.alloc(layout(size));
                        assert!(!at.is_null());
                        fill(at, size, salt);
                        Some((at, size, salt))
                    }
                    Some((at, had, _)) if next() % 3 == 0 => {
                        Pages.dealloc(at, layout(had));
                        None
                    }
                    Some((at, had, salt)) => {
                        let moved = Pages.realloc(at, layout(had), size);
                        assert!(!moved.is_null());
                        check(moved, had, size, salt);
                        fill(moved, size, salt);
                        Some((moved, size, salt))
                    }
                };
                for &(at, size, salt) in blocks.iter().flatten() {
                    check(at, size, size, salt);
                }
            }
        }
        for (at, size, _) in blocks.into_iter().flatten() {
            // SAFETY: as above.
            unsafe { Pages.dealloc(at, layout(size)) };
        }
        // The system allocator keeps up to about 9 MB of the small blocks freed here. Pages
        // left mapped by any one kind of step would add up to far more over the run.
        #[cfg(target_os = "macos")]
        {
            let end = footprint();
            assert!(end < start + 32 * MIB as u64, "{start} then {end}");
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

    /// Runs the test `name` again in a process of its own, since the footprint counts every
    /// test running meanwhile. True inside that process, where the test does its work.
    #[cfg(target_os = "macos")]
    fn alone(name: &str) -> bool {
        const ALONE: &str = "TELESCOPE_PAGES_TEST_ALONE";
        if std::env::var_os(ALONE).is_some() {
            return true;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                &format!("pages::tests::{name}"),
                "--exact",
                "--test-threads=1",
            ])
            .env(ALONE, "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && said.contains("1 passed"),
            "{said}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        false
    }

    /// One page mapped right where a block would grow in place, so growing it must move it.
    #[cfg(target_os = "macos")]
    struct Blocker(*mut u8);

    #[cfg(target_os = "macos")]
    impl Blocker {
        fn after(block: *mut u8, size: usize) -> Self {
            let end = block.wrapping_add(round_up(size, page_size()));
            let at = map_near(end, page_size());
            assert_eq!(at, end);
            // SAFETY: a page just mapped.
            unsafe { at.write(0x5A) };
            Self(at)
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for Blocker {
        fn drop(&mut self) {
            // SAFETY: the page `after` mapped, which only this holds.
            unsafe {
                assert_eq!(self.0.read(), 0x5A, "the block grew over another mapping");
                unmap(self.0, page_size());
            }
        }
    }

    /// Slack for the test harness's own memory.
    #[cfg(target_os = "macos")]
    const SLACK: u64 = 4 * MIB as u64;

    /// A block the footprint shows plainly, and its size grown by a quarter.
    #[cfg(target_os = "macos")]
    const SIZE: usize = 64 * MIB;
    #[cfg(target_os = "macos")]
    const GROWN: usize = SIZE + SIZE / 4;

    #[cfg(target_os = "macos")]
    #[test]
    fn a_big_block_with_no_room_after_it_moves_and_keeps_what_was_written() {
        // Alone, so no other test maps or frees the pages after a block meanwhile.
        if !alone("a_big_block_with_no_room_after_it_moves_and_keeps_what_was_written") {
            return;
        }
        for from in SIZES.into_iter().filter(|&size| size >= MIB) {
            for to in SIZES.into_iter().filter(|&size| size > from) {
                // SAFETY: non-zero sizes; each block is freed once with its layout.
                unsafe {
                    let at = Pages.alloc(layout(from));
                    fill(at, from, from);
                    let blocker = Blocker::after(at, from);
                    let moved = Pages.realloc(at, layout(from), to);
                    assert!(!moved.is_null());
                    if round_up(to, page_size()) > round_up(from, page_size()) {
                        assert_ne!(moved, at, "{from} to {to}");
                    }
                    drop(blocker);
                    check(moved, from, to, from);
                    fill(moved, to, from);
                    Pages.dealloc(moved, layout(to));
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_grown_big_block_keeps_its_place_when_the_pages_after_it_are_free() {
        if !alone("a_grown_big_block_keeps_its_place_when_the_pages_after_it_are_free") {
            return;
        }
        // SAFETY: non-zero sizes; the block is freed once with its layout.
        unsafe {
            let at = Pages.alloc(layout(GROWN));
            std::ptr::write_bytes(at, 1, GROWN);
            // Trimming gives back the pages after it, so they are free to grow into.
            assert_eq!(Pages.realloc(at, layout(GROWN), SIZE), at);
            let held = footprint();
            assert_eq!(Pages.realloc(at, layout(SIZE), GROWN), at);
            assert_eq!(at.add(SIZE - 1).read(), 1);
            assert_eq!(at.add(SIZE).read(), 0);
            let now = footprint();
            assert!(now <= held + SLACK, "{held} then {now}");
            Pages.dealloc(at, layout(GROWN));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_moved_big_block_never_counts_twice() {
        if !alone("a_moved_big_block_never_counts_twice") {
            return;
        }
        // SAFETY: non-zero sizes; the block is freed once with its layout.
        unsafe {
            let at = Pages.alloc(layout(SIZE));
            std::ptr::write_bytes(at, 1, SIZE);
            let blocker = Blocker::after(at, SIZE);
            let (held, most_before) = footprints();
            let moved = Pages.realloc(at, layout(SIZE), GROWN);
            assert_ne!(moved, at);
            drop(blocker);
            assert!(places(SIZE).all(|i| moved.add(i).read() == 1));
            let (now, most_after) = footprints();
            // The old copy is given back, and was never held whole beside the new one.
            assert!(now <= held + SLACK, "{held} then {now}");
            let allowed = most_before.max(held + SLACK);
            assert!(
                most_after <= allowed,
                "{held}, most {most_before} then {most_after}"
            );
            Pages.dealloc(moved, layout(GROWN));
            let gone = held.saturating_sub(footprint());
            assert!(gone > SIZE as u64 - SLACK, "{held}, {gone} given back");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_freed_big_block_stops_counting_at_once() {
        if !alone("a_freed_big_block_stops_counting_at_once") {
            return;
        }
        // The system allocator would keep a freed block this size counted.
        // SAFETY: non-zero sizes; the block is freed once with its layout.
        unsafe {
            let at = Pages.alloc(layout(SIZE));
            std::ptr::write_bytes(at, 1, SIZE);
            let held = footprint();
            // Trimmed to a quarter, then freed.
            let at = Pages.realloc(at, layout(SIZE), SIZE / 4);
            let trimmed = footprint();
            let gone = held.saturating_sub(trimmed);
            assert!(
                gone > (SIZE * 3 / 4) as u64 - SLACK,
                "{held} then {trimmed}"
            );
            Pages.dealloc(at, layout(SIZE / 4));
            let after = footprint();
            let gone = held.saturating_sub(after);
            assert!(gone > SIZE as u64 - SLACK, "{held} then {after}");
        }
    }
}
