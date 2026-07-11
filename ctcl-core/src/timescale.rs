//! Timescale offsets. Mirrors the Worker's LEAP constant: a flat, honest
//! approximation, not a live leap-second table. See CTCL's own /v1/version
//! `leap_table` field for the same caveat on the hosted API.

use crate::encoding::NS_PER_S;

pub const TAI_MINUS_UTC_S: i128 = 37;
pub const GPS_MINUS_UTC_S: i128 = 18;
pub const LEAP_TABLE_AS_OF: &str = "2017-01-01";

pub fn tai_approx_ns(utc_ns: i128) -> i128 {
    utc_ns + TAI_MINUS_UTC_S * NS_PER_S
}

pub fn gps_approx_ns(utc_ns: i128) -> i128 {
    utc_ns + GPS_MINUS_UTC_S * NS_PER_S
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tai_and_gps_offsets() {
        let utc_ns = 1_700_000_000 * NS_PER_S;
        assert_eq!(tai_approx_ns(utc_ns), utc_ns + 37 * NS_PER_S);
        assert_eq!(gps_approx_ns(utc_ns), utc_ns + 18 * NS_PER_S);
    }
}
