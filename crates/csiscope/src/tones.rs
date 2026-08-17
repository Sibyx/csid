//! Where a delivered tone actually sits in frequency.
//!
//! ## The bug this module exists to remove
//!
//! The console used to map array position to frequency as
//! `(i − n/2 + 0.5) · spacing`, which assumes the delivered tones are
//! contiguous. They are not. 802.11 never transmits on DC and nulls the tones
//! immediately around it on the wider PHYs, so a capture's used tones are **two
//! runs with a hole between them**, and the driver hands over only the used
//! ones.
//!
//! On the 52-tone legacy grid the error is half a subcarrier: the outermost
//! tone read +7.97 MHz where it is physically +8.125 MHz. Half a tone sounds
//! like nothing until it decides an experiment. The vault's BLE-overlap
//! derivation places advertising channel 39, on Wi-Fi ch13, at subcarrier
//! +25.6 — array index **50.6 of 51**, four tenths of a tone from the band
//! edge, inside the roll-off region that produced 55% of the ABBA probe's
//! spurious events. That is the number that moved EXP-010's inclusion arm from
//! ch13 to ch3, and it is only reachable with the right grid.
//!
//! This is invariant 7 of the `csi-visualization` skill, and the Python service
//! fixed the same bug on 2026-08-17. The two instruments now agree.
//!
//! ## What is deliberately still wrong
//!
//! Two transforms keep treating the tones as contiguous, for the same reason
//! the Python service left them alone — fixing them would move numbers that
//! have been quoted, and that needs its own measurement rather than a quiet
//! correction:
//!
//! - [`crate::dsp::cir`] IFFTs the used tones without zero-padding the DC hole
//!   onto its true FFT bin.
//! - [`crate::dsp::detrend`] fits over array index rather than subcarrier `k`,
//!   so a 52-tone `tau_ns` is about 2% off.
//!
//! Both are bounded, neither changes a shape, and both are stated in the
//! panels' own readouts. Do not quote either as an absolute delay.

/// How a source's tone axis relates to frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grid {
    /// An 802.11 used-tone set: the indices below, with a hole at DC.
    Dot11,
    /// Contiguous FFT bins with no hole. Nothing on this fleet delivers one;
    /// the ray-traced simulator does, which is why the case is named.
    Uniform,
}

/// The used-tone index range for a delivered tone count, as `(lo, hi)` where
/// the set is `±{lo..=hi}` and the count is `2 · (hi − lo + 1)`.
///
/// From 802.11-2020 §17.3.5 (legacy OFDM), §19.3.7 (HT) and §27.3.10 (HE).
/// Every entry is the data-plus-pilot set the driver reports, not the FFT size:
///
/// | tones | `k` | PHY |
/// |---|---|---|
/// | 52 | ±1…±26 | legacy OFDM 20 MHz, non-HT duplicate |
/// | 56 | ±1…±28 | HT20, VHT20 |
/// | 114 | ±2…±58 | HT40, VHT40 |
/// | 242 | ±2…±122 | HE20 (RU242); VHT80 at the wide spacing |
/// | 484 | ±3…±244 | HE40 (RU484) |
/// | 996 | ±3…±500 | HE80 (RU996) |
fn used_range(ntone: usize) -> Option<(i32, i32)> {
    match ntone {
        52 => Some((1, 26)),
        56 => Some((1, 28)),
        114 => Some((2, 58)),
        242 => Some((2, 122)),
        484 => Some((3, 244)),
        996 => Some((3, 500)),
        _ => None,
    }
}

/// The grid a tone count implies. An unrecognised count falls back to
/// [`Grid::Uniform`] — an honest "I do not know where the hole is" rather than
/// a guessed hole in the wrong place.
pub fn grid(ntone: usize) -> Grid {
    if used_range(ntone).is_some() {
        Grid::Dot11
    } else {
        Grid::Uniform
    }
}

/// Subcarrier index `k` for array position `i`.
///
/// Returns a float because callers ask the inverse question too — "which array
/// position is this frequency?" — and the answer there is genuinely fractional.
/// For an integer `i` on a `Dot11` grid the result is always a whole number.
pub fn tone_index(i: usize, ntone: usize) -> f64 {
    match used_range(ntone) {
        Some((lo, hi)) => {
            let half = (hi - lo + 1) as usize;
            if i < half {
                // Lower run, descending away from DC: index 0 is the most
                // negative tone.
                -(hi as f64) + i as f64
            } else {
                lo as f64 + (i - half) as f64
            }
        }
        // No hole to place, so the tones are the bins. Centred so that the
        // midpoint of the array is the band centre.
        None => i as f64 - (ntone as f64 - 1.0) / 2.0,
    }
}

/// Array position for subcarrier index `k`, the inverse of [`tone_index`].
///
/// Fractional and unclamped: a BLE channel lands between tones far more often
/// than on one, and rounding that to the nearest integer here would throw away
/// exactly the precision the band-edge check needs.
pub fn array_index(k: f64, ntone: usize) -> f64 {
    match used_range(ntone) {
        Some((lo, hi)) => {
            let half = (hi - lo + 1) as f64;
            if k < 0.0 {
                k + hi as f64
            } else {
                half + k - lo as f64
            }
        }
        None => k + (ntone as f64 - 1.0) / 2.0,
    }
}

/// Frequency offset from the band centre, in Hz, for array position `i`.
pub fn offset_hz(i: usize, ntone: usize, spacing_hz: f64) -> f64 {
    tone_index(i, ntone) * spacing_hz
}

/// Fill `out` with every delivered tone's offset from the band centre, in Hz.
pub fn offsets_hz_into(ntone: usize, spacing_hz: f64, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(ntone);
    for i in 0..ntone {
        out.push(offset_hz(i, ntone, spacing_hz) as f32);
    }
}

/// Width of the *occupied* band in Hz — outermost tone to outermost tone, plus
/// one tone of skirt.
///
/// Distinct from `ntone · spacing`, which counts only the delivered tones and
/// so under-reports by exactly the width of the DC hole.
pub fn occupied_span_hz(ntone: usize, spacing_hz: f64) -> f64 {
    match used_range(ntone) {
        Some((_, hi)) => (2 * hi + 1) as f64 * spacing_hz,
        None => ntone as f64 * spacing_hz,
    }
}

// -- the two measured artefact regions ----------------------------------------
//
// Both are instrument, not channel, and both were measured rather than derived:
//
// - **DC** — the array positions flanking the nulled centre. The ABBA probe and
//   the 2026-08-17 ch6 capture both show a deep notch there.
// - **Band edge** — the outermost tones, where that same capture rolls off some
//   15 dB and where the probe found 55% of its spurious events.
//
// The distances below are the columns of EXP-010's own ranking table, and they
// are defined against the *features* — the DC null and the last tone — rather
// than against a zone of arbitrary width. That matters for reproducibility:
// widening the zone must not silently move a published number.

/// Distance in tones from array position `i` to the nulled DC centre.
///
/// The DC null sits between the two runs, at `(ntone − 1) / 2` on the array —
/// a half-integer position on every even tone count, because there is no array
/// slot for a tone that is never transmitted.
pub fn tones_from_dc(i: f64, ntone: usize) -> f64 {
    (i - (ntone as f64 - 1.0) / 2.0).abs()
}

/// Distance in tones from array position `i` to the nearer band edge.
pub fn tones_from_edge(i: f64, ntone: usize) -> f64 {
    let last = ntone as f64 - 1.0;
    i.min(last - i)
}

/// Distance to the nearer of the two artefact features — the card's
/// "min hazard" column.
pub fn artefact_distance(i: f64, ntone: usize) -> f64 {
    tones_from_dc(i, ntone).min(tones_from_edge(i, ntone))
}

/// Array positions the drawing should shade as instrument: within `edge` tones
/// of DC, or of either end.
pub fn artefact_zones(ntone: usize, edge: usize) -> ArtefactZones {
    let half = ntone / 2;
    ArtefactZones {
        dc: (half.saturating_sub(edge), (half + edge).min(ntone)),
        low_edge: (0, edge.min(ntone)),
        high_edge: (ntone.saturating_sub(edge), ntone),
    }
}

/// Half-open array-index ranges the analysis should treat as instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtefactZones {
    pub dc: (usize, usize),
    pub low_edge: (usize, usize),
    pub high_edge: (usize, usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every recognised count must be exactly `2 · (hi − lo + 1)` tones, or the
    /// table and the tone count disagree and every index built from it is off.
    #[test]
    fn the_used_sets_have_the_tone_counts_they_claim() {
        for n in [52usize, 56, 114, 242, 484, 996] {
            let (lo, hi) = used_range(n).unwrap();
            assert_eq!(2 * (hi - lo + 1) as usize, n, "{n} tones");
        }
    }

    #[test]
    fn the_grid_is_two_runs_with_a_hole() {
        // 52 tones: −26…−1 then +1…+26. Nothing at 0.
        assert_eq!(tone_index(0, 52), -26.0);
        assert_eq!(tone_index(25, 52), -1.0);
        assert_eq!(tone_index(26, 52), 1.0);
        assert_eq!(tone_index(51, 52), 26.0);
        for i in 0..52 {
            assert_ne!(tone_index(i, 52), 0.0, "DC must not be delivered");
        }
    }

    /// The number the old formula got wrong, stated as a test so it cannot
    /// come back: the outermost legacy tone is at 8.125 MHz, not 7.97.
    #[test]
    fn the_outermost_legacy_tone_is_at_8125_khz() {
        let f = offset_hz(51, 52, 312_500.0);
        assert!((f - 8_125_000.0).abs() < 1.0, "got {f} Hz");

        let old: f64 = (51.0 - 52.0 / 2.0 + 0.5) * 312_500.0;
        assert!((old - 7_968_750.0).abs() < 1.0, "the old formula, for the record");
    }

    /// Reproduces the vault's own derivation (`diary/2026-08-17`): ch13 is
    /// centred 2472 MHz, BLE advertising channel 39 is at 2480 MHz, and the
    /// resulting array index decided that EXP-010's inclusion arm had to move.
    #[test]
    fn ble_adv39_on_ch13_lands_at_array_index_50_6() {
        let offset_hz: f64 = (2480.0 - 2472.0) * 1e6;
        let k = offset_hz / 312_500.0;
        assert!((k - 25.6).abs() < 1e-9, "subcarrier {k}");

        let i = array_index(k, 52);
        assert!((i - 50.6).abs() < 1e-9, "array index {i}");

        // The card's row for ch13: from DC 25.1, from edge 0.4, min hazard 0.4.
        assert!((tones_from_dc(i, 52) - 25.1).abs() < 0.05);
        assert!((tones_from_edge(i, 52) - 0.4).abs() < 0.05);
        assert!((artefact_distance(i, 52) - 0.4).abs() < 0.05);
    }

    /// The ch3 replacement, from the same table in the card:
    /// from DC 12.3, from edge 13.2, min hazard 12.3.
    #[test]
    fn ble_adv38_on_ch3_is_clear_of_both_hazards() {
        let k: f64 = ((2426.0 - 2422.0) * 1e6) / 312_500.0;
        let i = array_index(k, 52);
        assert!((i - 37.8).abs() < 1e-9, "array index {i}");
        assert!((tones_from_dc(i, 52) - 12.3).abs() < 0.05);
        assert!((tones_from_edge(i, 52) - 13.2).abs() < 0.05);
        assert!((artefact_distance(i, 52) - 12.3).abs() < 0.05);
    }

    /// The rest of the card's ranking table, so a change to the grid that
    /// preserved ch3 and ch13 but broke the middle would still be caught.
    #[test]
    fn the_cards_ranking_table_reproduces() {
        // (wifi channel, advertising channel, array index, min hazard)
        let rows = [
            (3u32, 2426.0, 37.8, 12.3),
            (5u32, 2426.0, 6.8, 6.8),
            (4u32, 2426.0, 22.8, 2.7),
            (13u32, 2480.0, 50.6, 0.4),
        ];
        for (ch, adv_mhz, want_i, want_d) in rows {
            let centre = 2407.0 + 5.0 * ch as f64;
            let k = ((adv_mhz - centre) * 1e6) / 312_500.0;
            let i = array_index(k, 52);
            assert!((i - want_i).abs() < 0.05, "ch{ch}: index {i}");
            let d = artefact_distance(i, 52);
            assert!((d - want_d).abs() < 0.05, "ch{ch}: hazard {d}");
        }
    }

    #[test]
    fn array_index_inverts_tone_index() {
        for n in [52usize, 56, 114, 242, 484, 996, 31] {
            for i in 0..n {
                let back = array_index(tone_index(i, n), n);
                assert!((back - i as f64).abs() < 1e-9, "{n} tones, index {i}");
            }
        }
    }

    #[test]
    fn an_unknown_tone_count_falls_back_to_contiguous_bins() {
        assert_eq!(grid(31), Grid::Uniform);
        assert_eq!(tone_index(0, 31), -15.0);
        assert_eq!(tone_index(15, 31), 0.0);
        assert_eq!(tone_index(30, 31), 15.0);
    }

    /// The occupied band is wider than the delivered tones, by the hole.
    #[test]
    fn occupied_span_counts_the_hole() {
        let delivered = 52.0 * 312_500.0;
        let occupied = occupied_span_hz(52, 312_500.0);
        assert!(occupied > delivered);
        assert!((occupied - 16_562_500.0).abs() < 1.0, "got {occupied}");
    }
}
