//! ctcl-core: the reference-instant + heterogeneous-time-transformation logic
//! shared by the CTCL Temporal Port desktop app and its CLI. Same formulas as
//! the hosted Worker at commoninstant.org (src/worker.js in the CTCL repo) -
//! ported deliberately for behavioral parity, not just "close enough."
//!
//! Core formula: tau_i = Phi_i(I*). A shared reference instant I*, transformed
//! by each system's own rule Phi_i into its own local time tau_i.

pub mod encoding;
pub mod error;
pub mod instant;
pub mod system;
pub mod timescale;

pub use encoding::{from_ns, rfc3339, to_ns};
pub use error::CtclError;
pub use instant::{instant_view, now_ns, now_view, Encodings, InstantView, Timescales};
pub use system::{LocalTimeExtra, Pause, Rate, Segment, TablePoint, TemporalSystem};
pub use timescale::{gps_approx_ns, tai_approx_ns};
