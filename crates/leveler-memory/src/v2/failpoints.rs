//! Test-only crash injection for the cross-file commit protocol.
//!
//! Compiled only with the `crash-injection` feature. The point to abort at is
//! named by `LEVELER_MEMORY_CRASH_AT`; without the feature every call is a
//! no-op, so a production build has no way to be interrupted here.

#[cfg(feature = "crash-injection")]
pub(crate) fn hit(name: &str) {
    if std::env::var("LEVELER_MEMORY_CRASH_AT").is_ok_and(|value| value == name) {
        // Abort without unwinding: the durable state is exactly what a power
        // loss at this point would leave behind.
        std::process::abort();
    }
}

#[cfg(not(feature = "crash-injection"))]
#[inline(always)]
pub(crate) fn hit(_name: &str) {}
