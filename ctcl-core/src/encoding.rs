//! Value <-> canonical-nanosecond conversion. Mirrors the CTCL Worker's toNs()/
//! fromNs()/rfc3339() functions (src/worker.js in the CTCL repo) so the desktop
//! app agrees with the hosted API on every conversion, not just approximately.

use crate::error::CtclError;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;

pub const NS_PER_S: i128 = 1_000_000_000;
pub const NS_PER_MS: i128 = 1_000_000;
pub const NS_PER_US: i128 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    UnixS,
    UnixMs,
    UnixUs,
    UnixNs,
    Rfc3339,
}

impl Encoding {
    pub fn parse(s: &str) -> Result<Self, CtclError> {
        match s.to_lowercase().as_str() {
            "unix_s" => Ok(Encoding::UnixS),
            "unix_ms" => Ok(Encoding::UnixMs),
            "unix_us" => Ok(Encoding::UnixUs),
            "unix_ns" => Ok(Encoding::UnixNs),
            "rfc3339" | "iso8601" => Ok(Encoding::Rfc3339),
            other => Err(CtclError::UnknownEncoding(other.to_string())),
        }
    }

    fn unit_ns(&self) -> Option<i128> {
        match self {
            Encoding::UnixS => Some(NS_PER_S),
            Encoding::UnixMs => Some(NS_PER_MS),
            Encoding::UnixUs => Some(NS_PER_US),
            Encoding::UnixNs => Some(1),
            Encoding::Rfc3339 => None,
        }
    }
}

/// Parse a value string under the given encoding into canonical nanoseconds since
/// the Unix epoch (UTC). i128 covers roughly +-1.7e11 years at nanosecond resolution
/// - no BigInt equivalent needed for any date this side of the heat death of the universe.
pub fn to_ns(value: &str, encoding: &str) -> Result<i128, CtclError> {
    match Encoding::parse(encoding)? {
        Encoding::Rfc3339 => parse_rfc3339_ns(value),
        enc => parse_numeric_ns(value, enc.unit_ns().unwrap()),
    }
}

fn parse_numeric_ns(value: &str, unit_ns: i128) -> Result<i128, CtclError> {
    let s = value.trim();
    let neg = s.starts_with('-');
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let int_val: i128 = int_part
        .parse()
        .map_err(|_| CtclError::InvalidTimeValue(value.to_string()))?;
    let mut ns = int_val * unit_ns;
    if !frac_part.is_empty() {
        let frac_val: f64 = format!("0.{frac_part}")
            .parse()
            .map_err(|_| CtclError::InvalidTimeValue(value.to_string()))?;
        let frac_ns = (frac_val * unit_ns as f64).round() as i128;
        ns += if neg { -frac_ns } else { frac_ns };
    }
    Ok(ns)
}

fn parse_rfc3339_ns(value: &str) -> Result<i128, CtclError> {
    let dt = DateTime::parse_from_rfc3339(value.trim())
        .map_err(|_| CtclError::InvalidTimeValue(format!("unparseable rfc3339: {value}")))?;
    let secs = dt.timestamp() as i128;
    let nanos = dt.timestamp_subsec_nanos() as i128;
    Ok(secs * NS_PER_S + nanos)
}

/// Encode canonical ns to a target encoding string. `tz` (IANA name) only affects rfc3339.
pub fn from_ns(ns: i128, encoding: &str, tz: Option<&str>) -> Result<String, CtclError> {
    match Encoding::parse(encoding)? {
        Encoding::Rfc3339 => rfc3339(ns, tz),
        enc => {
            let unit = enc.unit_ns().unwrap();
            let whole = ns.div_euclid(unit);
            let rem = ns.rem_euclid(unit);
            if rem == 0 {
                return Ok(whole.to_string());
            }
            let frac_digits = unit.to_string().len() - 1;
            let frac_str = format!("{rem:0frac_digits$}");
            let frac_str = frac_str.trim_end_matches('0');
            Ok(format!("{whole}.{frac_str}"))
        }
    }
}

/// Build an RFC3339 string for canonical ns, optionally projected into an IANA tz.
pub fn rfc3339(ns: i128, tz: Option<&str>) -> Result<String, CtclError> {
    let secs = ns.div_euclid(NS_PER_S) as i64;
    let subsec_ns = ns.rem_euclid(NS_PER_S) as u32;
    let utc_dt = Utc
        .timestamp_opt(secs, subsec_ns)
        .single()
        .ok_or_else(|| CtclError::InvalidTimeValue("instant out of representable range".into()))?;

    match tz {
        None | Some("UTC") | Some("Z") | Some("utc") => {
            Ok(utc_dt.to_rfc3339_opts(SecondsFormat::AutoSi, true))
        }
        Some(tzname) => {
            let zone: Tz = tzname
                .parse()
                .map_err(|_| CtclError::InvalidTimezone(tzname.to_string()))?;
            Ok(utc_dt
                .with_timezone(&zone)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_unix_s_fraction() {
        let ns = to_ns("1234567890.123456789", "unix_s").unwrap();
        assert_eq!(ns, 1_234_567_890_123_456_789);
        let back = from_ns(ns, "unix_s", None).unwrap();
        assert_eq!(back, "1234567890.123456789");
    }

    #[test]
    fn round_trip_unix_ns_integer() {
        let ns = to_ns("1700000000000000000", "unix_ns").unwrap();
        assert_eq!(ns, 1_700_000_000_000_000_000);
        assert_eq!(from_ns(ns, "unix_ns", None).unwrap(), "1700000000000000000");
    }

    #[test]
    fn ns_through_rfc3339_round_trip() {
        // 2024-01-01T00:00:00.123456789Z
        let original_ns: i128 = 1_704_067_200_123_456_789;
        let s = rfc3339(original_ns, None).unwrap();
        let back = to_ns(&s, "rfc3339").unwrap();
        assert_eq!(back, original_ns);
    }

    #[test]
    fn taipei_offset_is_plus_eight() {
        // 2026-07-11T15:00:00Z -> Asia/Taipei is UTC+8, no DST
        let ns = to_ns("2026-07-11T15:00:00Z", "rfc3339").unwrap();
        let taipei = rfc3339(ns, Some("Asia/Taipei")).unwrap();
        assert!(taipei.starts_with("2026-07-11T23:00:00"));
        assert!(taipei.ends_with("+08:00"));
    }

    #[test]
    fn unknown_encoding_errors() {
        let err = to_ns("1", "unix_fortnights").unwrap_err();
        assert_eq!(err.code(), "UNKNOWN_ENCODING");
    }

    #[test]
    fn invalid_timezone_errors() {
        let err = rfc3339(0, Some("Not/AZone")).unwrap_err();
        assert_eq!(err.code(), "INVALID_TIMEZONE");
    }
}
