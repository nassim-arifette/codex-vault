/// Best-effort process lifetime peak resident memory. Chain reports use this instead of a current
/// RSS sample so a short-lived spike during compression/rewrite cannot be hidden by measuring late.
#[cfg(windows)]
pub fn process_peak_rss_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `GetCurrentProcess` returns a valid pseudo-handle for this process, and
    // `counters` points to writable storage of the exact structure size passed to the API.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    (ok != 0).then_some(counters.PeakWorkingSetSize as u64)
}

#[cfg(target_os = "linux")]
pub fn process_peak_rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `usage` is valid writable storage for `rusage`; on success getrusage initializes it.
    let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if ok != 0 {
        return None;
    }
    // SAFETY: `getrusage` returned success above, so `usage` has been initialized.
    let usage = unsafe { usage.assume_init() };
    Some((usage.ru_maxrss as u64).saturating_mul(1024))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_peak_rss_bytes() -> Option<u64> {
    None
}
