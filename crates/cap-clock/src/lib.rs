//! One monotonic clock per platform, shared by frames and inputs.
//!
//! - Windows: QueryPerformanceCounter (same base as WGC `SystemRelativeTime`
//!   and Raw Input message times after conversion).
//! - Linux: CLOCK_MONOTONIC (PipeWire PTS; evdev after `EVIOCSCLOCKID`).
//! - macOS: mach_absolute_time (CMTime host time, IOHID timestamps).

use cap_types::Nanos;

/// Current time in nanoseconds on the platform monotonic clock.
pub fn now_ns() -> Nanos {
    imp::now_ns()
}

/// Convert a raw platform tick value (QPC ticks / mach ticks) to nanoseconds.
/// On Linux raw values are already nanoseconds.
pub fn ticks_to_ns(ticks: i64) -> Nanos {
    imp::ticks_to_ns(ticks)
}

/// Sleep until the monotonic clock reaches `deadline_ns` (coarse sleep, then spin
/// for the final stretch so a 20 Hz ticker stays within ~0.1 ms).
pub fn sleep_until(deadline_ns: Nanos) {
    const SPIN_NS: Nanos = 1_000_000;
    loop {
        let now = now_ns();
        let remaining = deadline_ns - now;
        if remaining <= 0 {
            return;
        }
        if remaining > SPIN_NS {
            std::thread::sleep(std::time::Duration::from_nanos((remaining - SPIN_NS) as u64));
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Nanos;
    pub fn now_ns() -> Nanos {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid pointer to a timespec.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as Nanos * 1_000_000_000 + ts.tv_nsec as Nanos
    }
    pub fn ticks_to_ns(ticks: i64) -> Nanos {
        ticks
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Nanos;
    use std::sync::OnceLock;

    #[repr(C)]
    struct TimebaseInfo {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut TimebaseInfo) -> i32;
    }

    fn timebase() -> (i128, i128) {
        static TB: OnceLock<(i128, i128)> = OnceLock::new();
        *TB.get_or_init(|| {
            let mut info = TimebaseInfo { numer: 0, denom: 0 };
            // SAFETY: valid out-pointer.
            unsafe { mach_timebase_info(&mut info) };
            (info.numer as i128, info.denom as i128)
        })
    }
    pub fn now_ns() -> Nanos {
        // SAFETY: no preconditions.
        ticks_to_ns(unsafe { mach_absolute_time() } as i64)
    }
    pub fn ticks_to_ns(ticks: i64) -> Nanos {
        let (n, d) = timebase();
        (ticks as i128 * n / d) as Nanos
    }
}

#[cfg(windows)]
mod imp {
    use super::Nanos;
    use std::sync::OnceLock;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    fn freq() -> i128 {
        static F: OnceLock<i128> = OnceLock::new();
        *F.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: valid out-pointer; cannot fail on XP+.
            unsafe { QueryPerformanceFrequency(&mut f).ok() };
            f as i128
        })
    }
    pub fn now_ns() -> Nanos {
        let mut c = 0i64;
        // SAFETY: valid out-pointer.
        unsafe { QueryPerformanceCounter(&mut c).ok() };
        ticks_to_ns(c)
    }
    pub fn ticks_to_ns(ticks: i64) -> Nanos {
        (ticks as i128 * 1_000_000_000 / freq()) as Nanos
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn monotonic_and_sleep() {
        let a = super::now_ns();
        super::sleep_until(a + 5_000_000);
        let b = super::now_ns();
        assert!(b - a >= 5_000_000 && b - a < 50_000_000, "{}", b - a);
    }
}
