//! `sigproc-common` — shared contracts and DSP primitives for the sigproc
//! microservice chain.
//!
//! Every module in this workspace depends on this crate for the things that
//! must not diverge between services:
//!
//! * [`vita49`] — the VITA 49.0 IF Data Packet wire format. It is the contract
//!   between `sigproc` (capture), `sigproc-channelizer` and any downstream
//!   consumer, so it is defined in exactly one place.
//! * [`config`] — `config.toml` parsing and validation shared by the services.
//! * [`dsp`] — signal processing primitives. Today the fused multiply-add
//!   kernels; the channelizer building blocks (NCO mixer, FIR low-pass,
//!   integer decimator, power estimation, sequence-gap tracking) land in M2.
//! * [`vectors`] — deterministic AM/DSB test vectors and the VITA49 packet-stream
//!   writer, shared by the fixture binary, the gates and the tests.
//! * [`metrics`] — counters, `/metrics` exposition and a dependency-free HTTP
//!   surface, so packet loss and channel level are assertable rather than
//!   merely logged.
//!
//! This crate deliberately has **no UHD dependency**: everything here builds and
//! tests on any host, including CI without libuhd installed. That is what makes
//! the wire format and the DSP verifiable without an SDR attached.

pub mod config;
pub mod dsp;
pub mod metrics;
pub mod vectors;
pub mod vita49;
