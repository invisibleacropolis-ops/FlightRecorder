use anyhow::{Context, Result, bail};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

pub fn qpc_frequency() -> Result<i64> {
    let mut value = 0i64;
    unsafe { QueryPerformanceFrequency(&mut value) }.context("QueryPerformanceFrequency failed")?;
    if value <= 0 {
        bail!("QueryPerformanceFrequency returned an invalid frequency");
    }
    Ok(value)
}

pub fn qpc_ticks() -> Result<i64> {
    let mut value = 0i64;
    unsafe { QueryPerformanceCounter(&mut value) }.context("QueryPerformanceCounter failed")?;
    Ok(value)
}

pub fn ticks_to_100ns(ticks: i64, frequency: i64) -> i64 {
    ((ticks as i128 * 10_000_000i128) / frequency as i128) as i64
}

pub fn qpc_now_100ns() -> Result<i64> {
    Ok(ticks_to_100ns(qpc_ticks()?, qpc_frequency()?))
}

pub fn offset_100ns(now_100ns: i64, origin_100ns: i64) -> i64 {
    now_100ns.saturating_sub(origin_100ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_ticks_without_floating_point_drift() {
        assert_eq!(ticks_to_100ns(10_000_000, 10_000_000), 10_000_000);
        assert_eq!(ticks_to_100ns(5, 2), 25_000_000);
    }

    #[test]
    fn qpc_is_monotonic_for_real_calls() {
        let first = qpc_now_100ns().unwrap();
        let second = qpc_now_100ns().unwrap();
        assert!(second >= first);
    }
}
