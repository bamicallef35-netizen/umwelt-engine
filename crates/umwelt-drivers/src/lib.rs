//! Sensor drivers — trait-abstracted so `umwelt-core` never depends on specific hardware.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
#![warn(missing_docs)]

use umwelt_core::TimestampUs;

/// A sensor that emits typed samples at a known cadence.
pub trait Sensor {
    /// The sample type this sensor emits.
    type Sample;
    /// Read the latest sample. Non-blocking; may return `None` if no fresh data.
    fn read(&mut self, now: TimestampUs) -> Option<Self::Sample>;
}
