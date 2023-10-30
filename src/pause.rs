/// This strategy cooperatively gives up a timeslice to the OS scheduler.
/// Requires that `std` feature is enabled and therefore it is not suitable
/// for "no_std" environments as it links to the `std` library.
#[cfg(feature = "std")]
#[inline(always)]
pub(crate) fn pause() {
    std::thread::yield_now();
}

/// This strategy emits machine instruction to signal the processor that it is
/// running in a busy-wait spin-loop. Does not require linking to the `std`
/// library, so it is suitable for `no_std` environments.
#[cfg(not(feature = "std"))]
#[inline(always)]
pub(crate) fn pause() {
    core::hint::spin_loop();
}
