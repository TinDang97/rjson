//! Machine-stack headroom checks for deeply nested documents.
//!
//! `loads` and `dumps` recurse once per nesting level (a few hundred bytes of
//! stack each). The depth limits (1024 / 254) keep that bounded, but not
//! below every thread's stack: threads with 64-128 KiB stacks (musl/Alpine
//! defaults, `threading.stack_size`, embedders) overflowed and the process
//! died with SIGSEGV. Every `CHECK_EVERY` levels the recursion now compares
//! the stack pointer with the thread's stack bounds and fails cleanly
//! (`RecursionError`) when less than `RESERVE` would be left.
//!
//! The bounds come from the OS once per thread (cached thread-locally). Where
//! they are unknown the check is a no-op, which is the old behaviour.

use std::cell::Cell;

/// Nesting depth from which the recursion checks the stack. Below it the hot
/// path keeps its single depth comparison (against this instead of the
/// nesting limit), so ordinary documents pay nothing; 16 unchecked levels
/// take ~4 KiB of stack, well inside `RESERVE`.
pub(crate) const CHECK_FROM: u32 = 16;

/// Nesting levels between two checks past `CHECK_FROM`.
pub(crate) const CHECK_EVERY: u32 = 8;

/// Stack that must stay free at a check: room for the next `CHECK_EVERY`
/// levels (well under 1 KiB each) plus the leaf work under them (allocation,
/// CPython helpers). Guard pages are excluded from the bounds.
pub(crate) const RESERVE: usize = 12 * 1024;

/// `RESERVE` for recursion that may call Python code below the check
/// (`dumps` in guarded mode: `default=`, dataclass/Enum attributes, a Python
/// `utcoffset()`). CPython 3.14 checks the C stack itself before running
/// Python code: with less than ~32 KiB left it raises RecursionError, with
/// less than ~16 KiB it aborts the process ("Unrecoverable stack overflow").
/// Keep its 32 KiB free on top of ours. Earlier versions only count calls.
pub(crate) const RESERVE_PYCALL: usize = if cfg!(Py_3_14) { RESERVE + 32 * 1024 } else { RESERVE };

/// Not yet computed for this thread.
const UNKNOWN: usize = usize::MAX;

thread_local! {
    /// Lowest usable address of this thread's stack, or 0 when the bounds
    /// are not available (the check then never fires).
    static LOW: Cell<usize> = const { Cell::new(UNKNOWN) };
}

/// Whether this thread has less than `reserve` bytes of stack left, i.e. is
/// too close to the end of its stack to go `CHECK_EVERY` levels deeper. Call
/// it only every `CHECK_EVERY` levels.
#[cold]
#[inline(never)]
pub(crate) fn exhausted(reserve: usize) -> bool {
    let marker = 0u8;
    // The address of a local is (close to) the stack pointer here.
    let here = std::hint::black_box(&marker) as *const u8 as usize;
    LOW.with(|l| {
        let mut low = l.get();
        if low == UNKNOWN {
            low = stack_low().unwrap_or(0);
            l.set(low);
        }
        // Stacks grow down on every platform rjson supports.
        low != 0 && here < low.saturating_add(reserve)
    })
}

/// Lowest usable address of the calling thread's stack (above any guard
/// area), or None if unknown.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn stack_low() -> Option<usize> {
    // SAFETY: plain libc calls on the current thread; `attr` is initialized
    // by pthread_getattr_np before use and destroyed after.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let mut guard: libc::size_t = 0;
        let ok = libc::pthread_attr_getstack(&attr, &mut addr, &mut size) == 0
            && libc::pthread_attr_getguardsize(&attr, &mut guard) == 0;
        libc::pthread_attr_destroy(&mut attr);
        if !ok || addr.is_null() || size == 0 {
            return None;
        }
        #[cfg(target_env = "musl")]
        if libc::syscall(libc::SYS_gettid) as libc::pid_t == libc::getpid() {
            return musl_main_thread_low(addr as usize + size);
        }
        // glibc and musl report the guard area inside [addr, addr + size)
        // for threads they created; skipping it is conservative either way.
        Some(addr as usize + guard)
    }
}

/// musl's `pthread_getattr_np` reports only the part of the main thread's
/// stack that is mapped so far (often ~100-200 KiB), not what it can grow
/// to, so a document a few hundred levels deep raised RecursionError on an
/// 8 MiB stack (Alpine). The main thread's stack grows down from `top` to
/// `RLIMIT_STACK`, as glibc computes it; `top` is musl's (the auxiliary
/// vector's page, a little below the real top: argv/env are above it, and
/// count against the limit), so keep 64 KiB of slack. No limit: unknown.
#[cfg(target_env = "musl")]
unsafe fn musl_main_thread_low(top: usize) -> Option<usize> {
    let mut lim: libc::rlimit = std::mem::zeroed();
    if libc::getrlimit(libc::RLIMIT_STACK, &mut lim) != 0 || lim.rlim_cur == libc::RLIM_INFINITY {
        return None;
    }
    let size = usize::try_from(lim.rlim_cur).ok()?;
    top.checked_sub(size)?.checked_add(64 * 1024)
}

#[cfg(target_os = "macos")]
fn stack_low() -> Option<usize> {
    // SAFETY: plain libc calls on the current thread.
    unsafe {
        let this = libc::pthread_self();
        let top = libc::pthread_get_stackaddr_np(this) as usize;
        let size = libc::pthread_get_stacksize_np(this);
        if top == 0 || size == 0 || size > top {
            return None;
        }
        // The main thread's size excludes its guard page; threads' guard
        // pages lie below `top - size`. One page of slack covers both.
        Some(top - size + 16 * 1024)
    }
}

#[cfg(windows)]
fn stack_low() -> Option<usize> {
    #[link(name = "kernel32")]
    extern "system" {
        // Windows 8+; the lowest address of the reserved stack region.
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: writes two usizes we own.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    if low == 0 || high <= low {
        return None;
    }
    // The bottom pages are the guard page(s) and the stack-overflow
    // handler's reserve: stay 64 KiB above them.
    Some(low + 64 * 1024)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn stack_low() -> Option<usize> {
    None
}

/// Free stack left on this thread (for the lenient fallback's decision), or
/// None if unknown.
pub(crate) fn remaining() -> Option<usize> {
    let marker = 0u8;
    let here = std::hint::black_box(&marker) as *const u8 as usize;
    stack_low().map(|low| here.saturating_sub(low))
}
