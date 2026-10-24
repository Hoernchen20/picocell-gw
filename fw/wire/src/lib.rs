//! Wire formats used by the firmware, free of I/O so they can be tested on the host.

#![no_std]

pub mod dns;
pub mod sntp;
pub mod time;
