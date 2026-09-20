//! Sequence-gap tracking for a VITA49 stream.
//!
//! Dropped packets are already counted at the transport (`Vita49Forwarder`
//! counts unresolved destinations and `WouldBlock` sends). That count alone is
//! not enough downstream: audio spliced across a lost region sounds continuous,
//! and a tone-frequency assertion could then pass *across* a corrupted stretch
//! and report success for the wrong reason.
//!
//! The VITA49 sample-count timestamp makes the loss precisely measurable — the
//! header carries integer seconds plus a sample-count fraction, so consecutive
//! packets reveal exactly how many samples went missing. This tracker turns
//! that into (a) counters for `/metrics` and (b) explicit [`GapMarker`]s that a
//! module writing an artifact records in its sidecar metadata.

/// A detected discontinuity in the sample stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapMarker {
    /// Sample counter at which the stream resumed.
    pub at_sample: u64,
    /// How many samples are missing before it.
    pub missing_samples: u64,
}

/// Tracks continuity of a sample-counting stream.
#[derive(Debug, Default, Clone, Copy)]
pub struct GapTracker {
    /// Sample counter expected next, once a packet has been seen.
    next_expected: Option<u64>,
    gaps: u64,
    gap_samples: u64,
    /// Times the counter went backwards (out-of-order or a restarted source).
    resets: u64,
    last_marker: Option<GapMarker>,
}

impl GapTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a packet: `counter` is its first sample's counter, `samples` how
    /// many samples it carries.
    ///
    /// Returns a [`GapMarker`] when this packet skipped samples, `None` when the
    /// stream is contiguous (or when it went backwards, which is counted
    /// separately as a reset rather than reported as a gap).
    pub fn observe(&mut self, counter: u64, samples: u64) -> Option<GapMarker> {
        let end = counter.saturating_add(samples);
        match self.next_expected {
            None => {
                self.next_expected = Some(end);
                None
            }
            Some(expected) if counter == expected => {
                self.next_expected = Some(end);
                None
            }
            Some(expected) if counter > expected => {
                let missing = counter - expected;
                self.gaps += 1;
                self.gap_samples += missing;
                let marker = GapMarker {
                    at_sample: counter,
                    missing_samples: missing,
                };
                self.last_marker = Some(marker);
                self.next_expected = Some(end);
                Some(marker)
            }
            Some(_) => {
                // Counter moved backwards: out-of-order delivery, or the
                // producer restarted. Not a gap — resynchronise and count it.
                self.resets += 1;
                self.next_expected = Some(end);
                None
            }
        }
    }

    /// Number of forward discontinuities seen.
    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    /// Total samples lost to those discontinuities.
    pub fn gap_samples(&self) -> u64 {
        self.gap_samples
    }

    /// Number of backwards counter movements (out-of-order or restart).
    pub fn resets(&self) -> u64 {
        self.resets
    }

    /// Most recent gap, if any.
    pub fn last_marker(&self) -> Option<GapMarker> {
        self.last_marker
    }

    /// True when no discontinuity has been observed.
    pub fn is_contiguous(&self) -> bool {
        self.gaps == 0 && self.resets == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contiguous_stream_reports_nothing() {
        let mut t = GapTracker::new();
        for i in 0..100 {
            assert_eq!(t.observe(i * 2048, 2048), None, "packet {i}");
        }
        assert_eq!((t.gaps(), t.gap_samples(), t.resets()), (0, 0, 0));
        assert!(t.is_contiguous());
        assert_eq!(t.last_marker(), None);
    }

    #[test]
    fn a_skipped_packet_is_measured_in_samples() {
        let mut t = GapTracker::new();
        t.observe(0, 2048);
        t.observe(2048, 2048);
        // Packet(s) missing: the stream resumes 3 packets later.
        let marker = t.observe(2048 * 5, 2048).expect("gap detected");
        assert_eq!(marker.at_sample, 2048 * 5);
        assert_eq!(marker.missing_samples, 2048 * 3);
        assert_eq!(t.gaps(), 1);
        assert_eq!(t.gap_samples(), 2048 * 3);
        assert!(!t.is_contiguous());
        assert_eq!(t.last_marker(), Some(marker));
    }

    #[test]
    fn several_gaps_accumulate_independently() {
        let mut t = GapTracker::new();
        t.observe(0, 100);
        t.observe(250, 100); // missing 150
        t.observe(350, 100);
        t.observe(1000, 100); // missing 550
        assert_eq!(t.gaps(), 2);
        assert_eq!(t.gap_samples(), 700);
    }

    /// A backwards counter is a restart or reordering, not loss: counting it as
    /// a gap would report samples as "missing" that were never sent.
    #[test]
    fn backwards_counters_are_resets_not_gaps() {
        let mut t = GapTracker::new();
        t.observe(10_000, 2048);
        assert_eq!(t.observe(0, 2048), None, "restart is not a gap");
        assert_eq!(t.gaps(), 0);
        assert_eq!(t.resets(), 1);
        assert!(!t.is_contiguous());

        // Resynchronised: the next contiguous packet is quiet again.
        assert_eq!(t.observe(2048, 2048), None);
        assert_eq!(t.resets(), 1);
    }

    #[test]
    fn a_single_sample_gap_is_still_a_gap() {
        let mut t = GapTracker::new();
        t.observe(0, 2048);
        let m = t.observe(2049, 2048).expect("one sample lost");
        assert_eq!(m.missing_samples, 1);
    }

    #[test]
    fn the_first_packet_establishes_the_reference_without_a_gap() {
        let mut t = GapTracker::new();
        assert_eq!(t.observe(987_654, 2048), None);
        assert_eq!(t.observe(987_654 + 2048, 2048), None);
        assert!(t.is_contiguous());
    }
}
