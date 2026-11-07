# Control-path integration record

The former unapplied patch proposal has been integrated. The current protocol
contract and reproducible commands are in [vita49.2-host-profile.md](vita49.2-host-profile.md).

Capture now owns the UDP control socket and its UHD adapter on one thread.
Common dispatch calls a backend and reports readback, not an assumed setter
result. The capture crate's default no-UHD build exercises the same socket and
dispatch with a mock backend, independent wire vectors and a two-subscriber
host UDP harness. The command preamble was corrected against an external
SoapyVITA49 layout (CAM, Message ID, optional identifiers, CIF0).

Observed local gates: `cargo test -p sigproc -p sigproc-common --quiet` passed
with 7 capture tests, 96 common tests passing and 1 pre-existing ignored test.
A fresh read-only reviewer found no source-level blocker but could not execute
tests because that reviewer had no command tool. The parent executed the gates.
These observations do not prove UHD-feature compilation, hardware tuning,
live deployment or Windows multicast delivery. Those require separate evidence.
