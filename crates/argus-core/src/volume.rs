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

/// Disk images open read-only as APFS or Mac OS Extended. A drive formatted for Windows that
/// macOS can only read, like NTFS, stays in. A disk image opened for writing, or an .iso,
/// looks like any drive and is indexed; a Mac drive locked read-only is taken for an image.
/// Telling them apart for sure takes asking Disk Arbitration for the device's model.
#[cfg_attr(
    not(any(target_os = "macos", test)),
    expect(dead_code, reason = "only a Mac lists its volumes so far")
)]
fn judge(mount: &Mount<'_>) -> Result<(), Refusal> {
    let mut parts = mount.at.components();
    let in_volumes = parts.next() == Some(Component::RootDir)
        && parts.next() == Some(Component::Normal("Volumes".as_ref()))
        && matches!(parts.next(), Some(Component::Normal(name)) if !name.as_encoded_bytes().starts_with(b"."))
        && parts.next().is_none();
    let backup = mount.from.contains("com.apple.TimeMachine")
        || mount.from.contains("com.apple.backupd")
        || mount.at.iter().any(|name| name == ".timemachine");
    let image = mount.quarantined || (mount.read_only && matches!(mount.kind, "apfs" | "hfs"));
    if mount.startup {
        Err(Refusal::StartupDisk)
    } else if backup {
        Err(Refusal::Backup)
    } else if !mount.local {
        Err(Refusal::Network)
    } else if !mount.browsable {
        Err(Refusal::Hidden)
    } else if image {
        Err(Refusal::DiskImage)
    } else if !in_volumes {
        Err(Refusal::NotAVolume)
    } else {
        Ok(())
    }
}

/// The real path of the volume mounted at `path`, when it belongs in the index. Reads only
/// the system's list of mounts, never the volume itself, so a drive spinning up or a file
/// server gone away never holds up the window's main thread. `path` is the mount point, as
/// macOS reports mounts; any other path is resolved on disk first.
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
    judge(&mac::facts(mount, &startup)).map(|()| mount.at.clone())
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
            assert_eq!(judge(&mount), expected, "{mount:?}");
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
            assert_eq!(judge(&mount), Err(reason));
            match reason {
                Refusal::StartupDisk => mount.startup = false,
                Refusal::Backup => mount.from = "/dev/disk4s1",
                Refusal::Network => mount.local = true,
                Refusal::Hidden => mount.browsable = true,
                Refusal::DiskImage => mount.quarantined = false,
                Refusal::NotAVolume => mount.at = Path::new("/Volumes/Photos"),
            }
        }
        assert_eq!(judge(&mount), Ok(()));
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

    #[test]
    fn finds_older_time_machine_disks_by_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!holds_backups(dir.path()));
        std::fs::create_dir(dir.path().join("Backups.backupdb")).unwrap();
        assert!(holds_backups(dir.path()));
        assert!(!holds_backups(&dir.path().join("gone")));
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
