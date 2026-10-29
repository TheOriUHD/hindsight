//! One clock for everything: QPC in 100ns units. WGC frame times, DXGI present
//! times and WASAPI packet positions are all on this clock.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

fn freq() -> i64 {
    static F: OnceLock<i64> = OnceLock::new();
    *F.get_or_init(|| {
        let mut f = 0;
        unsafe { QueryPerformanceFrequency(&mut f).ok() };
        f
    })
}

pub fn now_100ns() -> i64 {
    let mut c = 0;
    unsafe { QueryPerformanceCounter(&mut c).ok() };
    (c as i128 * 10_000_000 / freq() as i128) as i64
}

pub const SAMPLE_RATE: i64 = hindsight_encode::SAMPLE_RATE as i64;

/// Sample index at 48 kHz for a time relative to `origin`.
pub fn sample_at(t_100ns: i64, origin: i64) -> i64 {
    ((t_100ns - origin) as i128 * SAMPLE_RATE as i128 / 10_000_000) as i64
}
