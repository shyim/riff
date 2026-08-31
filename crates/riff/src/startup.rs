#[cfg(unix)]
const FALLBACK_NOFILE_LIMIT: libc::rlim_t = 10_240;

/// Raise the standalone CLI's file-descriptor limit before starting concurrent work.
///
/// This deliberately lives in the CLI startup path rather than the embedded API: a
/// library must not mutate its host process's resource limits. Failure is harmless;
/// Riff keeps the inherited limit and its existing concurrency bounds still apply.
#[cfg(unix)]
pub(crate) fn raise_nofile_limit() {
    // SAFETY: getrlimit and setrlimit synchronously read and update this process's
    // resource table. The pointers refer to a live, uniquely borrowed value and
    // every syscall failure is handled without relying on partially written data.
    unsafe {
        let mut limit = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            log::trace!("getrlimit(RLIMIT_NOFILE) failed; keeping inherited limit");
            return;
        }

        let before = limit.rlim_cur;
        let hard = limit.rlim_max;
        if before >= hard {
            log::trace!("RLIMIT_NOFILE soft={before} already at hard limit");
            return;
        }

        limit.rlim_cur = hard;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
            log::trace!("raised RLIMIT_NOFILE soft {before} -> {hard}");
            return;
        }

        limit.rlim_cur = fallback_nofile_limit(before, hard);
        if limit.rlim_cur > before && libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
            log::trace!(
                "raised RLIMIT_NOFILE soft {before} -> {} (hard={hard}, fallback cap)",
                limit.rlim_cur
            );
        } else {
            log::trace!("setrlimit(RLIMIT_NOFILE) failed; keeping soft={before}");
        }
    }
}

#[cfg(unix)]
fn fallback_nofile_limit(current: libc::rlim_t, hard: libc::rlim_t) -> libc::rlim_t {
    current.max(FALLBACK_NOFILE_LIMIT).min(hard)
}

#[cfg(not(unix))]
pub(crate) fn raise_nofile_limit() {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn fallback_raises_low_limits_to_ten_thousand() {
        assert_eq!(fallback_nofile_limit(256, 65_536), 10_240);
    }

    #[test]
    fn fallback_respects_the_hard_limit() {
        assert_eq!(fallback_nofile_limit(256, 4096), 4096);
    }

    #[test]
    fn fallback_never_lowers_an_existing_high_limit() {
        assert_eq!(fallback_nofile_limit(16_384, 65_536), 16_384);
    }
}
