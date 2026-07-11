//! The reference instant I* and its multi-encoding/multi-timescale view.
//! Mirrors instantViews()/nowEnvelope() in the CTCL Worker (src/worker.js).

use crate::encoding::{from_ns, NS_PER_MS};
use crate::error::CtclError;
use crate::timescale::{gps_approx_ns, tai_approx_ns};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Encodings {
    pub unix_s: String,
    pub unix_ms: String,
    pub unix_us: String,
    pub unix_ns: String,
    pub rfc3339: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timescales {
    pub utc: String,
    pub posix: String,
    pub tai_approx: String,
    pub gps_approx: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstantView {
    pub unix_ns: i128,
    pub encodings: Encodings,
    pub timescales: Timescales,
}

/// Project canonical UTC nanoseconds into the full encodings+timescales view.
/// This is the local, offline equivalent of the hosted API's `instantViews()` -
/// same math, so a desktop-computed instant and a commoninstant.org instant
/// agree bit-for-bit given the same nanosecond value.
pub fn instant_view(ns: i128) -> Result<InstantView, CtclError> {
    let iso = from_ns(ns, "rfc3339", None)?;
    Ok(InstantView {
        unix_ns: ns,
        encodings: Encodings {
            unix_s: from_ns(ns, "unix_s", None)?,
            unix_ms: from_ns(ns, "unix_ms", None)?,
            unix_us: from_ns(ns, "unix_us", None)?,
            unix_ns: from_ns(ns, "unix_ns", None)?,
            rfc3339: iso.clone(),
        },
        timescales: Timescales {
            utc: iso,
            posix: from_ns(ns, "unix_s", None)?,
            tai_approx: from_ns(tai_approx_ns(ns), "unix_s", None)?,
            gps_approx: from_ns(gps_approx_ns(ns), "unix_s", None)?,
        },
    })
}

/// The device wall clock right now, as canonical UTC nanoseconds. NOTE: this is
/// the LOCAL device clock, not a verified/synchronized source - callers that need
/// the hosted, honesty-labelled reference instant should call commoninstant.org's
/// GET /v1/now instead. See corpus/engineering-notes.md in the CTCL repo for why
/// that distinction matters (precision != accuracy, §16).
pub fn now_ns() -> i128 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch");
    d.as_nanos() as i128
}

pub fn now_view() -> Result<InstantView, CtclError> {
    // The device clock is millisecond-granular in practice on most platforms;
    // truncate to ms then re-expand, matching the Worker's own honesty framing
    // (ns/us fields are format-padding, not a real-precision claim).
    let ms = now_ns() / NS_PER_MS;
    instant_view(ms * NS_PER_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_view_agrees_across_fields() {
        let ns = 1_700_000_000_500_000_000i128;
        let v = instant_view(ns).unwrap();
        assert_eq!(v.unix_ns, ns);
        assert_eq!(v.encodings.unix_s, "1700000000.5");
        assert_eq!(v.timescales.posix, v.encodings.unix_s);
    }

    #[test]
    fn now_view_is_recent() {
        let v = now_view().unwrap();
        // sanity: should be after 2026-01-01 (1767225600) and before 2100-01-01
        assert!(v.unix_ns > 1_767_225_600 * crate::encoding::NS_PER_S);
        assert!(v.unix_ns < 4_102_444_800 * crate::encoding::NS_PER_S);
    }
}
