//! Text search runs below name search, so typing a name stays quick while files are read.

#[cfg(target_os = "macos")]
mod qos {
    use std::ffi::{c_int, c_uint};

    // From sys/qos.h.
    pub(super) const QOS_CLASS_UTILITY: c_uint = 0x11;

    // SAFETY: the signatures match pthread/qos.h and sys/qos.h; both take and return plain
    // integers and touch no memory of ours, so any arguments are safe.
    unsafe extern "C" {
        pub(super) safe fn pthread_set_qos_class_self_np(
            class: c_uint,
            relative_priority: c_int,
        ) -> c_int;
        #[cfg(test)]
        pub(super) safe fn qos_class_self() -> c_uint;
    }
}

/// Lowers this thread's priority to utility. At background, the lowest, macOS held back its
/// disk reads while Spotlight took in an npm install's new files, and ⌘T took up to 2 s to
/// its first line instead of 35 ms. Background with a normal disk policy can't be had: setting
/// one drops the thread to the window's own priority. Utility slowed name search no more than
/// background did (1 Nov, docs/m4-text-search.html decision 4).
pub(crate) fn yield_to_names() {
    #[cfg(target_os = "macos")]
    qos::pthread_set_qos_class_self_np(qos::QOS_CLASS_UTILITY, 0);
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn lowered() -> bool {
    qos::qos_class_self() == qos::QOS_CLASS_UTILITY
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn the_class_is_one_macos_accepts() {
        std::thread::spawn(|| {
            assert_eq!(
                qos::pthread_set_qos_class_self_np(qos::QOS_CLASS_UTILITY, 0),
                0
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn lowers_this_thread_only() {
        std::thread::spawn(|| {
            yield_to_names();
            assert!(lowered());
        })
        .join()
        .unwrap();
        assert!(!lowered());
    }
}
