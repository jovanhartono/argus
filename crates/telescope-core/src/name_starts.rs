//! Where each distinct name starts in the name text, in 2 bytes a name instead of 4.

/// Starts in a group, which share one full number.
pub(crate) const GROUP: usize = 16;

/// Each name's start, then one final end, kept as the first start of each group and each
/// start's distance from it. The first name, the whole path scanned, may be long, so its
/// start has a group of its own. Any other name is a file name of at most 255 characters,
/// 765 bytes, so a group spans well under 64 KB.
#[derive(Clone, Default)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct NameStarts {
    pub(crate) groups: Vec<u32>,
    pub(crate) offsets: Vec<u16>,
}

impl NameStarts {
    /// The starts as plain numbers, as a saved index keeps them; `None` when one is too far
    /// from its group's first.
    pub(crate) fn from_starts(starts: &[u32]) -> Option<Self> {
        let mut kept = Self::default();
        kept.reserve_exact(starts.len());
        for &start in starts {
            if !kept.push(start) {
                return None;
            }
        }
        Some(kept)
    }

    fn group(at: usize) -> usize {
        at.div_ceil(GROUP)
    }

    /// Adds the next start. False, and nothing added, when it is too far from its group's
    /// first.
    #[must_use]
    pub(crate) fn push(&mut self, start: u32) -> bool {
        let group = Self::group(self.offsets.len());
        if group == self.groups.len() {
            self.groups.push(start);
        }
        let Some(offset) = start
            .checked_sub(self.groups[group])
            .and_then(|offset| u16::try_from(offset).ok())
        else {
            return false;
        };
        self.offsets.push(offset);
        true
    }

    pub(crate) fn len(&self) -> usize {
        self.offsets.len()
    }

    pub(crate) fn get(&self, at: usize) -> usize {
        self.groups[Self::group(at)] as usize + usize::from(self.offsets[at])
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len()).map(|at| self.get(at) as u32)
    }

    /// The groups `more` further starts add.
    pub(crate) fn more_groups(&self, more: usize) -> usize {
        let last = (self.len() + more).checked_sub(1);
        let groups = last.map_or(0, |last| Self::group(last) + 1);
        groups.saturating_sub(self.groups.len())
    }

    pub(crate) fn reserve_exact(&mut self, more: usize) {
        self.groups.reserve_exact(self.more_groups(more));
        self.offsets.reserve_exact(more);
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.groups.shrink_to_fit();
        self.offsets.shrink_to_fit();
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.groups.capacity() * size_of::<u32>() + self.offsets.capacity() * size_of::<u16>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_back_every_start_on_both_sides_of_each_group_edge() {
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = |below: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % u64::from(below)) as u32
        };
        // A long first name, then names up to 765 bytes, some empty.
        let mut starts = vec![0, 100_000];
        for _ in 0..1000 {
            let last = *starts.last().unwrap();
            starts.push(last + next(766) * next(2));
        }
        let kept = NameStarts::from_starts(&starts).unwrap();
        assert_eq!(kept.len(), starts.len());
        assert_eq!(kept.iter().collect::<Vec<_>>(), starts);
        assert_eq!(kept.groups.len(), 1 + (starts.len() - 1).div_ceil(GROUP));
        assert_eq!(kept.groups.capacity(), kept.groups.len());
        assert_eq!(kept.offsets.capacity(), kept.offsets.len());
    }

    #[test]
    fn room_reserved_after_some_starts_holds_that_many_more() {
        for have in 0..40 {
            for more in 0..40 {
                let mut kept = NameStarts::default();
                for start in 0..have {
                    assert!(kept.push(start));
                }
                kept.reserve_exact(more as usize);
                let room = (kept.groups.capacity(), kept.offsets.capacity());
                for start in have..have + more {
                    assert!(kept.push(start));
                }
                let after = (kept.groups.capacity(), kept.offsets.capacity());
                assert_eq!(after, room, "{have} then {more}");
            }
        }
    }

    #[test]
    fn refuses_a_start_too_far_from_its_group_s_first() {
        let mut kept = NameStarts::default();
        for start in [0, 5, 5 + u32::from(u16::MAX)] {
            assert!(kept.push(start));
        }
        assert!(!kept.push(6 + u32::from(u16::MAX)));
        assert!(!kept.push(4));
        assert_eq!(kept.len(), 3);
        assert!(NameStarts::from_starts(&[0, 1, 70_000]).is_none());
        assert!(NameStarts::from_starts(&[0, 70_000, 70_001]).is_some());
    }
}
