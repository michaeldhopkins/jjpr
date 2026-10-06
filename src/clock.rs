//! Local wall-clock time, for what jjpr prints.

/// `(YYYY-MM-DD, HH:MM)` in local time for Unix seconds `secs`.
pub fn local(secs: u64) -> (String, String) {
    let t = libc::time_t::try_from(secs).unwrap_or(0);
    // SAFETY: `tm` is plain data, so all-zero is a valid value of it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals of the right types, and the
    // reentrant variants write only into `tm`.
    #[cfg(unix)]
    unsafe {
        libc::localtime_r(&t, &mut tm)
    };
    // SAFETY: as above; localtime_s writes only into `tm`.
    #[cfg(windows)]
    unsafe {
        libc::localtime_s(&mut tm, &t)
    };
    (
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        ),
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_formats_a_date_and_a_time() {
        let (date, time) = super::local(1_700_000_000);
        assert_eq!(date.len(), 10);
        assert!(date.starts_with("2023-11-1"), "{date}");
        assert_eq!(time.len(), 5);
        assert_eq!(&time[2..3], ":");
    }
}
