//! Files kept only in iCloud Drive, Dropbox or OneDrive: reading one would download it.

use std::fs::Metadata;
use std::path::Path;

/// Asked before a file is opened.
pub(crate) fn is_cloud_only(_path: &Path, metadata: &Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        is_dataless(metadata.st_flags())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        is_recalled(metadata.file_attributes())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = metadata;
        false
    }
}

/// `SF_DATALESS` from sys/stat.h: the file's contents live in the cloud.
#[cfg(target_os = "macos")]
fn is_dataless(flags: u32) -> bool {
    const SF_DATALESS: u32 = 0x4000_0000;
    flags & SF_DATALESS != 0
}

/// OneDrive's placeholders: offline, or fetched when opened or read.
#[cfg(windows)]
fn is_recalled(attributes: u32) -> bool {
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    attributes & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(target_os = "macos")]
mod policy {
    use std::ffi::c_int;

    // From sys/resource.h.
    pub(super) const MATERIALIZE_DATALESS_FILES: c_int = 3;
    pub(super) const SCOPE_THREAD: c_int = 1;
    pub(super) const MATERIALIZE_OFF: c_int = 1;

    // SAFETY: the signatures match sys/resource.h; both take and return plain integers
    // and touch no memory of ours, so any arguments are safe.
    unsafe extern "C" {
        pub(super) safe fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
        #[cfg(test)]
        pub(super) safe fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
    }
}

/// A file that turns cloud-only between the check and the read fails on this thread
/// instead of downloading.
pub(crate) fn never_download() {
    #[cfg(target_os = "macos")]
    policy::setiopolicy_np(
        policy::MATERIALIZE_DATALESS_FILES,
        policy::SCOPE_THREAD,
        policy::MATERIALIZE_OFF,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_file_is_not_cloud_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hi").unwrap();
        let metadata = std::fs::symlink_metadata(&file).unwrap();
        assert!(!is_cloud_only(&file, &metadata));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_the_dataless_flag() {
        assert!(is_dataless(0x4000_0000));
        assert!(is_dataless(0x4000_0020));
        assert!(!is_dataless(0));
        // UF_HIDDEN and UF_COMPRESSED, set on ordinary local files.
        assert!(!is_dataless(0x8000 | 0x20));
    }

    #[cfg(windows)]
    #[test]
    fn reads_the_recall_attributes() {
        assert!(is_recalled(0x40_0000));
        assert!(is_recalled(0x4_0000 | 0x20));
        assert!(is_recalled(0x1000));
        // ARCHIVE and PINNED: a OneDrive file kept on this PC.
        assert!(!is_recalled(0x20 | 0x8_0000));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn turns_off_downloads_for_this_thread_only() {
        use policy::*;
        std::thread::spawn(|| {
            never_download();
            assert_eq!(
                getiopolicy_np(MATERIALIZE_DATALESS_FILES, SCOPE_THREAD),
                MATERIALIZE_OFF
            );
        })
        .join()
        .unwrap();
        assert_ne!(
            getiopolicy_np(MATERIALIZE_DATALESS_FILES, SCOPE_THREAD),
            MATERIALIZE_OFF
        );
    }
}
