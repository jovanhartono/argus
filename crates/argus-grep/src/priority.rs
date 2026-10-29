//! Text search runs below name search, so typing a name stays quick while files are read.

#[cfg(target_os = "macos")]
mod qos {
    use std::ffi::{c_int, c_uint};

    // From sys/qos.h.
    pub(super) const QOS_CLASS_BACKGROUND: c_uint = 0x09;

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

/// Lowers this thread's priority. At the utility class, a running text search still pushed
/// name searches over default folders from 2.5 to 10.8 ms at p95; at background it left them
/// at 2.5 ms. The text search took a quarter longer over the home folder, and about twice as
/// long over 50,000 tiny files on a busy Mac. Background also holds back disk reads while other
/// programs use the disk; that was measured only with files already cached.
pub(crate) fn yield_to_names() {
    #[cfg(target_os = "macos")]
    qos::pthread_set_qos_class_self_np(qos::QOS_CLASS_BACKGROUND, 0);
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn lowered() -> bool {
    qos::qos_class_self() == qos::QOS_CLASS_BACKGROUND
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn the_class_is_one_macos_accepts() {
        std::thread::spawn(|| {
            assert_eq!(
                qos::pthread_set_qos_class_self_np(qos::QOS_CLASS_BACKGROUND, 0),
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
