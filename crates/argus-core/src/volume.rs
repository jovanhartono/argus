//! Which mounted volumes join the index: drives plugged into this Mac, not file servers,
//! backups, opened disk images or the startup disk under another name.

use std::path::{Component, Path, PathBuf};

/// Why a mounted volume is left out of the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not mounted, or not a volume of its own straight inside /Volumes.
    NotAVolume,
    /// The startup disk under another name, like /Volumes/Macintosh HD.
    StartupDisk,
    /// A Time Machine backup, which holds the same files many times over.
    Backup,
    /// A file server, which may be slow or gone at any moment.
    Network,
    /// Mounted to stay out of Finder's sidebar, as macOS does with volumes of its own.
    Hidden,
    /// An opened disk image, like an installer's .dmg.
    DiskImage,
}

/// What the system's list of mounts tells about a mounted volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(any(target_os = "macos", test)),
    expect(dead_code, reason = "only a Mac lists its volumes so far")
)]
struct Mount<'a> {
    at: &'a Path,
    /// The device or server it comes from, like /dev/disk4s1.
    from: &'a str,
    /// The file system, like apfs or exfat.
    kind: &'a str,
    local: bool,
    browsable: bool,
    read_only: bool,
    /// Set by macOS on a disk image downloaded from the internet.
    quarantined: bool,
    /// The same device as / or the startup disk's data volume.
    startup: bool,
}

/// `disk_image` asks Disk Arbitration whether the device is an opened disk image, .sparsebundle
/// or .iso, and is only called once every cheaper check has passed. A quarantined mount is
/// always an image. When Disk Arbitration can't say, a read-only APFS or Mac OS Extended mount
/// is taken for one, as disk images open that way; a drive formatted for Windows stays in.
#[cfg_attr(
    not(any(target_os = "macos", test)),
    expect(dead_code, reason = "only a Mac lists its volumes so far")
)]
fn judge(mount: &Mount<'_>, disk_image: impl FnOnce() -> Option<bool>) -> Result<(), Refusal> {
    let mut parts = mount.at.components();
    let in_volumes = parts.next() == Some(Component::RootDir)
        && parts.next() == Some(Component::Normal("Volumes".as_ref()))
        && matches!(parts.next(), Some(Component::Normal(name)) if !name.as_encoded_bytes().starts_with(b"."))
        && parts.next().is_none();
    let backup = mount.from.contains("com.apple.TimeMachine")
        || mount.from.contains("com.apple.backupd")
        || mount.at.iter().any(|name| name == ".timemachine");
    let locked_mac = mount.read_only && matches!(mount.kind, "apfs" | "hfs");
    if mount.startup {
        Err(Refusal::StartupDisk)
    } else if backup {
        Err(Refusal::Backup)
    } else if !mount.local {
        Err(Refusal::Network)
    } else if !mount.browsable {
        Err(Refusal::Hidden)
    } else if mount.quarantined {
        Err(Refusal::DiskImage)
    } else if !in_volumes {
        Err(Refusal::NotAVolume)
    } else if disk_image().unwrap_or(locked_mac) {
        Err(Refusal::DiskImage)
    } else {
        Ok(())
    }
}

/// The real path of the volume mounted at `path`, when it belongs in the index. Reads only
/// the system's list of mounts and, for a volume that could be a drive, what Disk Arbitration
/// knows of the device, never the volume itself, so a drive spinning up or a file server gone
/// away never holds up the window's main thread. Asking Disk Arbitration is a call to a system
/// service and makes it wait 0.1-0.4 ms, up to 2 ms the first time; the rest takes about
/// 15 µs. `path` is the mount point, as macOS reports mounts; any other path is resolved on
/// disk first.
#[cfg(target_os = "macos")]
pub fn check_volume(path: &Path) -> Result<PathBuf, Refusal> {
    let mounts = mac::mounts();
    let find = |at: &Path| mounts.iter().find(|mount| mount.at == at);
    let mount = match find(path) {
        Some(mount) => mount,
        None => {
            let real = std::fs::canonicalize(path).map_err(|_| Refusal::NotAVolume)?;
            find(&real).ok_or(Refusal::NotAVolume)?
        }
    };
    let startup: Vec<&str> = mounts
        .iter()
        .filter(|mount| mount.at == Path::new("/") || mount.at == Path::new("/System/Volumes/Data"))
        .map(|mount| mount.from.as_str())
        .collect();
    // The opt-in DriveImageTests in apps/macos stand a writable disk image in for a USB drive.
    let disk_image = || match std::env::var_os("ARGUS_DRIVE_IMAGE") {
        Some(_) => None,
        None => mac::disk_image(&mount.from),
    };
    judge(&mac::facts(mount, &startup), disk_image).map(|()| mount.at.clone())
}

/// Volumes are only indexed on a Mac so far.
#[cfg(not(target_os = "macos"))]
pub fn check_volume(_: &Path) -> Result<PathBuf, Refusal> {
    Err(Refusal::NotAVolume)
}

/// The folder older Time Machine disks, formatted Mac OS Extended, keep their backups in.
/// Read on the volume itself, so only off the window's main thread.
pub(crate) fn holds_backups(volume: &Path) -> bool {
    std::fs::symlink_metadata(volume.join("Backups.backupdb")).is_ok()
}

#[cfg(target_os = "macos")]
mod mac {
    use super::Mount;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    pub(super) const LOCAL: u32 = libc::MNT_LOCAL as u32;
    pub(super) const DONT_BROWSE: u32 = libc::MNT_DONTBROWSE as u32;
    pub(super) const READ_ONLY: u32 = libc::MNT_RDONLY as u32;
    pub(super) const QUARANTINE: u32 = libc::MNT_QUARANTINE as u32;

    pub(super) struct Mounted {
        pub(super) at: PathBuf,
        pub(super) from: String,
        pub(super) kind: String,
        pub(super) flags: u32,
    }

    /// `startup` holds the devices of / and the startup disk's data volume.
    pub(super) fn facts<'a>(mount: &'a Mounted, startup: &[&str]) -> Mount<'a> {
        Mount {
            at: &mount.at,
            from: &mount.from,
            kind: &mount.kind,
            local: mount.flags & LOCAL != 0,
            browsable: mount.flags & DONT_BROWSE == 0,
            read_only: mount.flags & READ_ONLY != 0,
            quarantined: mount.flags & QUARANTINE != 0,
            startup: startup.contains(&mount.from.as_str()),
        }
    }

    fn text(field: &[libc::c_char]) -> Vec<u8> {
        field
            .iter()
            .map(|&byte| byte.cast_unsigned())
            .take_while(|&byte| byte != 0)
            .collect()
    }

    /// Every mounted file system, as the system last noted them. `MNT_NOWAIT` reads that
    /// note instead of asking each file system, which could wait on a file server gone away.
    pub(super) fn mounts() -> Vec<Mounted> {
        // SAFETY: a null buffer of no bytes asks only how many file systems are mounted.
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        let Ok(count) = usize::try_from(count) else {
            return Vec::new();
        };
        // Room for a few more mounted in between.
        let mut found: Vec<libc::statfs> = Vec::with_capacity(count + 4);
        let room = found.capacity() * size_of::<libc::statfs>();
        let room = libc::c_int::try_from(room).unwrap_or(libc::c_int::MAX);
        // SAFETY: the buffer has room for `room` bytes of `statfs` records, and getfsstat
        // writes whole records within it, returning how many.
        let written = unsafe { libc::getfsstat(found.as_mut_ptr(), room, libc::MNT_NOWAIT) };
        let written = usize::try_from(written).unwrap_or(0);
        // SAFETY: getfsstat wrote the first `written` records, which fit in the capacity, and
        // `statfs` is plain numbers and byte arrays, valid as written.
        unsafe { found.set_len(written.min(found.capacity())) };
        found
            .iter()
            .map(|mount| Mounted {
                at: PathBuf::from(OsString::from_vec(text(&mount.f_mntonname))),
                from: String::from_utf8_lossy(&text(&mount.f_mntfromname)).into_owned(),
                kind: String::from_utf8_lossy(&text(&mount.f_fstypename)).into_owned(),
                flags: mount.f_flags,
            })
            .collect()
    }

    type CFTypeRef = *const std::ffi::c_void;
    const UTF8: u32 = 0x0800_0100;

    // SAFETY: the signatures match CoreFoundation's CFBase.h, CFString.h and CFDictionary.h,
    // and DiskArbitration's DASession.h and DADisk.h.
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFEqual(one: CFTypeRef, other: CFTypeRef) -> u8;
        fn CFStringCreateWithBytes(
            allocator: CFTypeRef,
            bytes: *const u8,
            len: isize,
            encoding: u32,
            external: u8,
        ) -> CFTypeRef;
        fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    }

    #[link(name = "DiskArbitration", kind = "framework")]
    unsafe extern "C" {
        static kDADiskDescriptionDeviceModelKey: CFTypeRef;
        fn DASessionCreate(allocator: CFTypeRef) -> CFTypeRef;
        fn DADiskCreateFromBSDName(
            allocator: CFTypeRef,
            session: CFTypeRef,
            name: *const libc::c_char,
        ) -> CFTypeRef;
        fn DADiskCopyDescription(disk: CFTypeRef) -> CFTypeRef;
    }

    /// A CoreFoundation object this code created, released once.
    struct Owned(CFTypeRef);

    impl Owned {
        fn new(made: CFTypeRef) -> Option<Self> {
            // Made only when not null, as dropping one releases it.
            (!made.is_null()).then(|| Self(made))
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: made by a Create or Copy call, so this code holds one reference to it.
            unsafe { CFRelease(self.0) };
        }
    }

    /// Whether Disk Arbitration knows the device `from`, like /dev/disk4s1, as an opened disk
    /// image, by its model. `None` for a file server, or a device it can't tell about.
    pub(super) fn disk_image(from: &str) -> Option<bool> {
        const IMAGE: &str = "Disk Image";
        let name = std::ffi::CString::new(from.strip_prefix("/dev/")?).ok()?;
        let null = std::ptr::null();
        // SAFETY: each call is given live objects, valid bytes for their length, or null where
        // the headers allow one, and returns null when it has nothing.
        let session = Owned::new(unsafe { DASessionCreate(null) })?;
        let disk = Owned::new(unsafe { DADiskCreateFromBSDName(null, session.0, name.as_ptr()) })?;
        let description = Owned::new(unsafe { DADiskCopyDescription(disk.0) })?;
        let image = Owned::new(unsafe {
            CFStringCreateWithBytes(null, IMAGE.as_ptr(), IMAGE.len() as isize, UTF8, 0)
        })?;
        // SAFETY: the key is DiskArbitration's own, and the dictionary keeps the value alive.
        let model =
            unsafe { CFDictionaryGetValue(description.0, kDADiskDescriptionDeviceModelKey) };
        // SAFETY: both are live objects; CFEqual tells objects of different kinds apart.
        (!model.is_null()).then(|| unsafe { CFEqual(model, image.0) } != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An external drive formatted for Macs, plugged in.
    fn drive(at: &str) -> Mount<'_> {
        Mount {
            at: Path::new(at),
            from: "/dev/disk4s1",
            kind: "apfs",
            local: true,
            browsable: true,
            read_only: false,
            quarantined: false,
            startup: false,
        }
    }

    #[test]
    fn keeps_drives_and_refuses_the_rest_for_its_reason() {
        let cases: [(Mount<'_>, Result<(), Refusal>); 25] = [
            (drive("/Volumes/Photos"), Ok(())),
            (drive("/Volumes/Photos/"), Ok(())),
            (drive("/Volumes/My Passport 2TB"), Ok(())),
            (
                Mount {
                    kind: "exfat",
                    ..drive("/Volumes/CARD")
                },
                Ok(()),
            ),
            // Windows drives macOS can only read stay in.
            (
                Mount {
                    kind: "ntfs",
                    read_only: true,
                    ..drive("/Volumes/Win")
                },
                Ok(()),
            ),
            (
                Mount {
                    kind: "msdos",
                    ..drive("/Volumes/NO NAME")
                },
                Ok(()),
            ),
            // A memory card with its lock switch on.
            (
                Mount {
                    kind: "msdos",
                    read_only: true,
                    ..drive("/Volumes/LOCKED")
                },
                Ok(()),
            ),
            (
                Mount {
                    kind: "exfat",
                    read_only: true,
                    ..drive("/Volumes/LOCKED")
                },
                Ok(()),
            ),
            (drive("/"), Err(Refusal::NotAVolume)),
            (drive("/Volumes"), Err(Refusal::NotAVolume)),
            (drive("/Volumes/Photos/inside"), Err(Refusal::NotAVolume)),
            (drive("/Volumes/.hidden"), Err(Refusal::NotAVolume)),
            (drive("/System/Volumes/Data"), Err(Refusal::NotAVolume)),
            (drive("Volumes/Photos"), Err(Refusal::NotAVolume)),
            (
                Mount {
                    startup: true,
                    ..drive("/Volumes/Macintosh HD")
                },
                Err(Refusal::StartupDisk),
            ),
            (
                Mount {
                    from: "//ana@nas/share",
                    kind: "smbfs",
                    local: false,
                    ..drive("/Volumes/share")
                },
                Err(Refusal::Network),
            ),
            (
                Mount {
                    kind: "nfs",
                    local: false,
                    ..drive("/Volumes/nfs")
                },
                Err(Refusal::Network),
            ),
            (
                Mount {
                    browsable: false,
                    ..drive("/Volumes/Recovery")
                },
                Err(Refusal::Hidden),
            ),
            (
                Mount {
                    quarantined: true,
                    read_only: true,
                    ..drive("/Volumes/Argus")
                },
                Err(Refusal::DiskImage),
            ),
            (
                Mount {
                    kind: "hfs",
                    read_only: true,
                    ..drive("/Volumes/Installer")
                },
                Err(Refusal::DiskImage),
            ),
            (
                Mount {
                    read_only: true,
                    ..drive("/Volumes/Installer")
                },
                Err(Refusal::DiskImage),
            ),
            (
                Mount {
                    quarantined: true,
                    kind: "exfat",
                    ..drive("/Volumes/Download")
                },
                Err(Refusal::DiskImage),
            ),
            (
                Mount {
                    from: "com.apple.backupd.Snapshot@/dev/disk5s2",
                    ..drive("/Volumes/Backup")
                },
                Err(Refusal::Backup),
            ),
            (
                Mount {
                    from: "com.apple.TimeMachine.2026-10-28-101500.backup@/dev/disk5s2",
                    ..drive("/Volumes/.timemachine/ABCD/2026-10-28-101500.backup")
                },
                Err(Refusal::Backup),
            ),
            (
                Mount {
                    from: "/dev/disk5s2",
                    ..drive("/Volumes/.timemachine/ABCD")
                },
                Err(Refusal::Backup),
            ),
        ];
        for (mount, expected) in cases {
            assert_eq!(judge(&mount, || None), expected, "{mount:?}");
        }
    }

    #[test]
    fn disk_arbitration_tells_images_from_drives() {
        let locked = Mount {
            read_only: true,
            ..drive("/Volumes/Locked")
        };
        let cases = [
            // A Mac drive locked read-only.
            (locked, false, Ok(())),
            (
                Mount {
                    kind: "hfs",
                    ..locked
                },
                false,
                Ok(()),
            ),
            (drive("/Volumes/Photos"), false, Ok(())),
            // A disk image you made and opened for writing, or a .sparsebundle.
            (drive("/Volumes/T"), true, Err(Refusal::DiskImage)),
            (
                Mount {
                    kind: "cd9660",
                    read_only: true,
                    ..drive("/Volumes/ISO")
                },
                true,
                Err(Refusal::DiskImage),
            ),
            (
                Mount {
                    kind: "exfat",
                    ..drive("/Volumes/EX")
                },
                true,
                Err(Refusal::DiskImage),
            ),
            (locked, true, Err(Refusal::DiskImage)),
            // A downloaded installer stays out whatever Disk Arbitration says.
            (
                Mount {
                    quarantined: true,
                    ..drive("/Volumes/Installer")
                },
                false,
                Err(Refusal::DiskImage),
            ),
        ];
        for (mount, image, expected) in cases {
            assert_eq!(judge(&mount, || Some(image)), expected, "{mount:?} {image}");
        }
    }

    #[test]
    fn the_first_reason_wins() {
        let everything = Mount {
            startup: true,
            from: "com.apple.TimeMachine.2026-10-28-101500.backup@/dev/disk5s2",
            local: false,
            browsable: false,
            quarantined: true,
            ..drive("/elsewhere")
        };
        let mut mount = everything;
        for reason in [
            Refusal::StartupDisk,
            Refusal::Backup,
            Refusal::Network,
            Refusal::Hidden,
            Refusal::DiskImage,
            Refusal::NotAVolume,
        ] {
            // Disk Arbitration is asked only once every cheaper check has passed.
            let unasked = || panic!("asked Disk Arbitration before {reason:?}");
            assert_eq!(judge(&mount, unasked), Err(reason));
            match reason {
                Refusal::StartupDisk => mount.startup = false,
                Refusal::Backup => mount.from = "/dev/disk4s1",
                Refusal::Network => mount.local = true,
                Refusal::Hidden => mount.browsable = true,
                Refusal::DiskImage => mount.quarantined = false,
                Refusal::NotAVolume => mount.at = Path::new("/Volumes/Photos"),
            }
        }
        assert_eq!(judge(&mount, || None), Ok(()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_macs_own_disks_and_folders_stay_out() {
        assert_eq!(check_volume(Path::new("/")), Err(Refusal::StartupDisk));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(check_volume(dir.path()), Err(Refusal::NotAVolume));
        assert_eq!(
            check_volume(&dir.path().join("gone")),
            Err(Refusal::NotAVolume)
        );
        assert_eq!(check_volume(Path::new("/dev")), Err(Refusal::Hidden));
        assert_eq!(
            check_volume(Path::new("/System/Volumes/Data")),
            Err(Refusal::StartupDisk)
        );
        // The startup disk's name in /Volumes, where a Mac has one, is a link to /.
        for entry in std::fs::read_dir("/Volumes").unwrap() {
            let path = entry.unwrap().path();
            if std::fs::canonicalize(&path).is_ok_and(|real| real == Path::new("/")) {
                assert_eq!(check_volume(&path), Err(Refusal::StartupDisk), "{path:?}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mount_flags_and_devices_become_facts() {
        let mounted = |from: &str, flags: u32| mac::Mounted {
            at: "/Volumes/Photos".into(),
            from: from.into(),
            kind: "apfs".into(),
            flags,
        };
        let startup = ["/dev/disk3s1s1", "/dev/disk3s5"];
        let plain = Mount {
            at: Path::new("/Volumes/Photos"),
            from: "/dev/disk4s1",
            kind: "apfs",
            local: true,
            browsable: true,
            read_only: false,
            quarantined: false,
            startup: false,
        };
        let drive = mounted("/dev/disk4s1", mac::LOCAL);
        assert_eq!(mac::facts(&drive, &startup), plain);
        let cases = [
            (
                mac::LOCAL | mac::READ_ONLY,
                Mount {
                    read_only: true,
                    ..plain
                },
            ),
            (
                mac::LOCAL | mac::QUARANTINE,
                Mount {
                    quarantined: true,
                    ..plain
                },
            ),
            (
                mac::LOCAL | mac::DONT_BROWSE,
                Mount {
                    browsable: false,
                    ..plain
                },
            ),
            (
                0,
                Mount {
                    local: false,
                    ..plain
                },
            ),
        ];
        for (flags, expected) in cases {
            let drive = mounted("/dev/disk4s1", flags);
            assert_eq!(mac::facts(&drive, &startup), expected, "{flags:#x}");
        }
        for from in startup {
            let data = mounted(from, mac::LOCAL);
            let facts = mac::facts(&data, &startup);
            assert_eq!(
                facts,
                Mount {
                    from,
                    startup: true,
                    ..plain
                }
            );
        }
        // A file server's name is never taken for the startup disk.
        let share = mounted("//ana@nas/share", 0);
        assert!(!mac::facts(&share, &startup).startup);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn disk_arbitration_knows_the_startup_disk_is_no_image() {
        let mounts = mac::mounts();
        let root = mounts
            .iter()
            .find(|mount| mount.at == Path::new("/"))
            .unwrap();
        // A virtual Mac, like a CI runner, gives its startup disk no model at all.
        assert_ne!(mac::disk_image(&root.from), Some(true), "{}", root.from);
        assert_eq!(mac::disk_image("//ana@nas/share"), None);
        assert_eq!(mac::disk_image("/dev/disk999s9"), None);
    }

    #[test]
    fn finds_older_time_machine_disks_by_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!holds_backups(dir.path()));
        std::fs::create_dir(dir.path().join("Backups.backupdb")).unwrap();
        assert!(holds_backups(dir.path()));
        assert!(!holds_backups(&dir.path().join("gone")));
    }

    /// For checking by hand with a drive or disk image attached:
    /// `cargo test -p argus-core judge_what_is_mounted -- --ignored --nocapture`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "prints how each volume mounted right now is judged"]
    fn judge_what_is_mounted() {
        for entry in std::fs::read_dir("/Volumes").unwrap() {
            let path = entry.unwrap().path();
            let start = std::time::Instant::now();
            let judged = check_volume(&path);
            println!("{path:?}: {judged:?} in {:?}", start.elapsed());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn every_mount_has_a_place_and_a_kind() {
        let mounts = mac::mounts();
        assert!(mounts.iter().any(|mount| mount.at == Path::new("/")));
        for mount in &mounts {
            assert!(mount.at.is_absolute() || mount.at.as_os_str().is_empty());
            assert!(!mount.kind.is_empty(), "{:?}", mount.at);
        }
    }
}
