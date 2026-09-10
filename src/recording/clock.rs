//! Core Media clock correlation, never calibrated using callback arrival time.

use objc2_core_media::{CMClock, CMSyncConvertTime, CMSyncGetRelativeRate, CMTime, CMTimeFlags};

pub fn host_seconds() -> f64 {
    // SAFETY: Read-only singleton clock; retained locally, never invalidated.
    unsafe { seconds(CMClock::host_time_clock().time()).expect("Host clock is numeric") }
}

pub fn to_host(timestamp: CMTime, clock: &CMClock) -> Result<f64, String> {
    seconds(timestamp)?;
    // SAFETY: Both CFType arguments are live CMClock objects. Core Media tracks
    // their relative drift; conversion keeps their common origin, not PTS zero.
    unsafe {
        let host = CMClock::host_time_clock();
        let rate = CMSyncGetRelativeRate(clock.as_ref(), host.as_ref());
        if !rate.is_finite() || (rate - 1.0).abs() > 0.001 {
            return Err(format!(
                "Device clock drift is unsupported ({rate:.8} relative rate)"
            ));
        }
        seconds(CMSyncConvertTime(timestamp, clock.as_ref(), host.as_ref()))
    }
}

fn seconds(time: CMTime) -> Result<f64, String> {
    if time.timescale <= 0
        || time.epoch != 0
        || time.value < 0
        || !time.flags.contains(CMTimeFlags::Valid)
        || time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
    {
        return Err("Capture clock returned a nonnumeric or unsupported timestamp".into());
    }
    Ok(time.value as f64 / f64::from(time.timescale))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_correlation_preserves_origin_and_progression() {
        unsafe {
            let clock = CMClock::host_time_clock();
            let time = clock.time();
            assert_eq!(to_host(time, &clock).unwrap(), seconds(time).unwrap());
            let before = host_seconds();
            std::thread::sleep(std::time::Duration::from_millis(2));
            assert!(host_seconds() > before);
            let mut invalid = time;
            invalid.flags = CMTimeFlags::Indefinite;
            assert!(to_host(invalid, &clock).is_err());
            invalid = time;
            invalid.epoch = 1;
            assert!(to_host(invalid, &clock).is_err());
        }
    }
}
