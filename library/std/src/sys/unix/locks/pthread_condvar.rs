use crate::cell::UnsafeCell;
use crate::ptr;
use crate::sync::atomic::{AtomicPtr, Ordering::Relaxed};
use crate::sys::locks::{pthread_mutex, Mutex};
use crate::sys_common::lazy_box::{LazyBox, LazyInit};
use crate::time::Duration;

struct AllocatedCondvar(UnsafeCell<libc::pthread_cond_t>);

pub struct Condvar {
    inner: LazyBox<AllocatedCondvar>,
    mutex: AtomicPtr<libc::pthread_mutex_t>,
}

const TIMESPEC_MAX: libc::timespec =
    libc::timespec { tv_sec: <libc::time_t>::MAX, tv_nsec: 1_000_000_000 - 1 };

fn saturating_cast_to_time_t(value: u64) -> libc::time_t {
    if value > <libc::time_t>::MAX as u64 { <libc::time_t>::MAX } else { value as libc::time_t }
}

#[inline]
fn raw(c: &Condvar) -> *mut libc::pthread_cond_t {
    c.inner.0.get()
}

unsafe impl Send for AllocatedCondvar {}
unsafe impl Sync for AllocatedCondvar {}

impl LazyInit for AllocatedCondvar {
    fn init() -> Box<Self> {
        let condvar = Box::new(AllocatedCondvar(UnsafeCell::new(libc::PTHREAD_COND_INITIALIZER)));

        cfg_if::cfg_if! {
            if #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "watchos",
                target_os = "l4re",
                target_os = "android",
                target_os = "redox"
            ))] {
                // `pthread_condattr_setclock` is unfortunately not supported on these platforms.
            } else if #[cfg(any(target_os = "espidf", target_os = "horizon"))] {
                // NOTE: ESP-IDF's PTHREAD_COND_INITIALIZER support is not released yet
                // So on that platform, init() should always be called
                // Moreover, that platform does not have pthread_condattr_setclock support,
                // hence that initialization should be skipped as well
                //
                // Similar story for the 3DS (horizon).
                let r = unsafe { libc::pthread_cond_init(condvar.0.get(), crate::ptr::null()) };
                assert_eq!(r, 0);
            } else {
                use crate::mem::MaybeUninit;
                let mut attr = MaybeUninit::<libc::pthread_condattr_t>::uninit();
                let r = unsafe { libc::pthread_condattr_init(attr.as_mut_ptr()) };
                assert_eq!(r, 0);
                let r = unsafe { libc::pthread_condattr_setclock(attr.as_mut_ptr(), libc::CLOCK_MONOTONIC) };
                assert_eq!(r, 0);
                let r = unsafe { libc::pthread_cond_init(condvar.0.get(), attr.as_ptr()) };
                assert_eq!(r, 0);
                let r = unsafe { libc::pthread_condattr_destroy(attr.as_mut_ptr()) };
                assert_eq!(r, 0);
            }
        }

        condvar
    }
}

impl Drop for AllocatedCondvar {
    #[inline]
    fn drop(&mut self) {
        let r = unsafe { libc::pthread_cond_destroy(self.0.get()) };
        if cfg!(target_os = "dragonfly") {
            // On DragonFly pthread_cond_destroy() returns EINVAL if called on
            // a condvar that was just initialized with
            // libc::PTHREAD_COND_INITIALIZER. Once it is used or
            // pthread_cond_init() is called, this behaviour no longer occurs.
            debug_assert!(r == 0 || r == libc::EINVAL);
        } else {
            debug_assert_eq!(r, 0);
        }
    }
}

impl Condvar {
    #[rustc_const_stable(feature = "const_locks", since = "1.63.0")]
    pub const fn new() -> Condvar {
        Condvar { inner: LazyBox::new(), mutex: AtomicPtr::new(ptr::null_mut()) }
    }

    #[inline]
    fn verify(&self, mutex: *mut libc::pthread_mutex_t) {
        // Relaxed is okay here because we never read through `self.addr`, and only use it to
        // compare addresses.
        match self.mutex.compare_exchange(ptr::null_mut(), mutex, Relaxed, Relaxed) {
            Ok(_) => {}                // Stored the address
            Err(n) if n == mutex => {} // Lost a race to store the same address
            _ => panic!("attempted to use a condition variable with two mutexes"),
        }
    }

    #[inline]
    pub fn notify_one(&self) {
        let r = unsafe { libc::pthread_cond_signal(raw(self)) };
        debug_assert_eq!(r, 0);
    }

    #[inline]
    pub fn notify_all(&self) {
        let r = unsafe { libc::pthread_cond_broadcast(raw(self)) };
        debug_assert_eq!(r, 0);
    }

    #[inline]
    pub unsafe fn wait(&self, mutex: &Mutex) {
        let mutex = pthread_mutex::raw(mutex);
        self.verify(mutex);
        let r = libc::pthread_cond_wait(raw(self), mutex);
        debug_assert_eq!(r, 0);
    }

    // This implementation is used on systems that support pthread_condattr_setclock
    // where we configure condition variable to use monotonic clock (instead of
    // default system clock). This approach avoids all problems that result
    // from changes made to the system time.
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "watchos",
        target_os = "android",
        target_os = "espidf",
        target_os = "horizon"
    )))]
    pub unsafe fn wait_timeout(&self, mutex: &Mutex, dur: Duration) -> bool {
        use crate::mem;

        let mutex = pthread_mutex::raw(mutex);
        self.verify(mutex);

        let mut now: libc::timespec = mem::zeroed();
        let r = libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
        assert_eq!(r, 0);

        // Nanosecond calculations can't overflow because both values are below 1e9.
        let nsec = dur.subsec_nanos() + now.tv_nsec as u32;

        let sec = saturating_cast_to_time_t(dur.as_secs())
            .checked_add((nsec / 1_000_000_000) as libc::time_t)
            .and_then(|s| s.checked_add(now.tv_sec));
        let nsec = nsec % 1_000_000_000;

        let timeout =
            sec.map(|s| libc::timespec { tv_sec: s, tv_nsec: nsec as _ }).unwrap_or(TIMESPEC_MAX);

        let r = libc::pthread_cond_timedwait(raw(self), mutex, &timeout);
        assert!(r == libc::ETIMEDOUT || r == 0);
        r == 0
    }

    // This implementation is modeled after libcxx's condition_variable
    // https://github.com/llvm-mirror/libcxx/blob/release_35/src/condition_variable.cpp#L46
    // https://github.com/llvm-mirror/libcxx/blob/release_35/include/__mutex_base#L367
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "watchos",
        target_os = "android",
        target_os = "espidf",
        target_os = "horizon"
    ))]
    pub unsafe fn wait_timeout(&self, mutex: &Mutex, mut dur: Duration) -> bool {
        use crate::time::Instant;

        let mutex = pthread_mutex::raw(mutex);
        self.verify(mutex);

        // 1000 years
        let max_dur = Duration::from_secs(1000 * 365 * 86400);

        if dur > max_dur {
            // OSX implementation of `pthread_cond_timedwait` is buggy
            // with super long durations. When duration is greater than
            // 0x100_0000_0000_0000 seconds, `pthread_cond_timedwait`
            // in macOS Sierra return error 316.
            //
            // This program demonstrates the issue:
            // https://gist.github.com/stepancheg/198db4623a20aad2ad7cddb8fda4a63c
            //
            // To work around this issue, and possible bugs of other OSes, timeout
            // is clamped to 1000 years, which is allowable per the API of `wait_timeout`
            // because of spurious wakeups.

            dur = max_dur;
        }

        // First, figure out what time it currently is, in both system and
        // stable time.  pthread_cond_timedwait uses system time, but we want to
        // report timeout based on stable time.
        let mut sys_now = libc::timeval { tv_sec: 0, tv_usec: 0 };
        let stable_now = Instant::now();
        let r = libc::gettimeofday(&mut sys_now, ptr::null_mut());
        assert_eq!(r, 0, "unexpected error: {:?}", crate::io::Error::last_os_error());

        let nsec = dur.subsec_nanos() as libc::c_long + (sys_now.tv_usec * 1000) as libc::c_long;
        let extra = (nsec / 1_000_000_000) as libc::time_t;
        let nsec = nsec % 1_000_000_000;
        let seconds = saturating_cast_to_time_t(dur.as_secs());

        let timeout = sys_now
            .tv_sec
            .checked_add(extra)
            .and_then(|s| s.checked_add(seconds))
            .map(|s| libc::timespec { tv_sec: s, tv_nsec: nsec })
            .unwrap_or(TIMESPEC_MAX);

        // And wait!
        let r = libc::pthread_cond_timedwait(raw(self), mutex, &timeout);
        debug_assert!(r == libc::ETIMEDOUT || r == 0);

        // ETIMEDOUT is not a totally reliable method of determining timeout due
        // to clock shifts, so do the check ourselves
        stable_now.elapsed() < dur
    }
}
