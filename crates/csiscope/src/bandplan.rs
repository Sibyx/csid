//! Where BLE lands inside this capture's tone grid.
//!
//! ## Why a live console carries a design-time check
//!
//! EXP-010 tests whether BLE activity is attributable inside a Wi-Fi capture by
//! moving the receiver's passband to include or exclude BLE advertising while
//! holding the room and the emitter fixed. Whether that is even possible is
//! decided by arithmetic on two channel plans, and it was got wrong once: the
//! inclusion arm was written as ch13 with advertising channel 39, which places
//! the treatment at array index 50.6 of 51 — four tenths of a tone from the
//! band edge, inside the roll-off region that produced 55% of the ABBA probe's
//! spurious events. Treatment and dominant artefact would have occupied the
//! same subcarriers and the arm could not have produced a falsifiable result.
//!
//! The check is cheap and the cost of skipping it is a booked room and a wasted
//! session, so it belongs where the operator already is: on the console, over
//! the capture's own measured spectrum, before the session starts.
//!
//! ## The arithmetic
//!
//! A 2.4 GHz Wi-Fi channel *c* is centred at `2407 + 5c` MHz. A BLE RF channel
//! *k* sits at `2402 + 2k` MHz, and the three **advertising** channels are 37,
//! 38 and 39 at 2402, 2426 and 2480 MHz — deliberately placed in the gaps
//! between the popular Wi-Fi channels 1, 6 and 11, which is exactly why they
//! are hard to capture.
//!
//! Everything else follows from [`crate::tones`]: the offset in MHz becomes a
//! subcarrier index, and the subcarrier index becomes an array position that
//! can be compared against the measured artefact zones.
//!
//! ## What it does not claim
//!
//! Landing inside the passband is necessary, not sufficient. An LE 1M burst is
//! GFSK at 1 Msym/s and occupies about 1.06 MHz — some 3.4 subcarriers of a
//! 52-tone grid, not the 6.4 the 2 MHz channel allocation suggests. That
//! concentration buys about 11.9 dB against a wideband comparison, and a 3 dB
//! per-record excursion still needs the burst within roughly 19.6 dB of the
//! Wi-Fi frame. The panel reports geometry. It does not promise detection.

use crate::tones;

/// Centre frequency of a 2.4 GHz Wi-Fi channel, in MHz.
pub fn wifi_centre_mhz(channel: u32) -> Option<f64> {
    // Channel 14 is 2484 MHz and Japan-only; it is not a `2407 + 5c` channel
    // and the fleet's regdomain (SK) cannot use it.
    match channel {
        1..=13 => Some(2407.0 + 5.0 * channel as f64),
        _ => None,
    }
}

/// Centre frequency of a BLE RF channel index (0..=39), in MHz.
///
/// The *index* is not the advertising-channel number: indices 0..=39 map to
/// 2402..2480 MHz in 2 MHz steps, and the advertising channels 37, 38, 39 are
/// the ones at 2402, 2426 and 2480.
pub fn ble_channel_mhz(index: u32) -> f64 {
    2402.0 + 2.0 * index as f64
}

/// Is this BLE index one of the three primary advertising channels?
///
/// Returns the advertising-channel number as the literature names it.
pub fn advertising_number(index: u32) -> Option<u32> {
    match index {
        0 => Some(37),
        12 => Some(38),
        39 => Some(39),
        _ => None,
    }
}

/// One BLE channel's position inside a capture's tone grid.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Landing {
    /// BLE RF channel index, 0..=39.
    pub index: u32,
    /// 37, 38 or 39 when this is a primary advertising channel.
    pub advertising: Option<u32>,
    pub freq_mhz: f64,
    /// Subcarrier index, fractional. Negative is below the band centre.
    pub subcarrier: f64,
    /// Position on the delivered tone array, fractional.
    pub array_index: f64,
    /// Tones to the nulled DC centre — the card's "From DC" column.
    pub tones_from_dc: f64,
    /// Tones to the nearer band edge — the card's "From edge" column.
    pub tones_from_edge: f64,
    /// The nearer of the two — the card's "Min hazard" column, and the number
    /// the verdict is read off.
    pub artefact_distance: f64,
}

/// Everything the bandplan panel needs for one capture.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Bandplan {
    /// False on 5 and 6 GHz, where no BLE channel can exist and the panel says
    /// so instead of drawing an empty axis.
    pub applicable: bool,
    pub wifi_channel: u32,
    pub centre_mhz: f64,
    pub ntone: usize,
    pub spacing_khz: f64,
    /// Occupied half-width in MHz, from the outermost delivered tone.
    pub half_span_mhz: f64,
    /// Every BLE channel whose centre falls inside the occupied band.
    pub inside: Vec<Landing>,
    /// The advertising channels that fall inside, if any.
    pub advertising_inside: Vec<u32>,
    /// Array-index bounds of the measured artefact regions, for the drawing.
    pub dc_zone: (usize, usize),
    pub low_edge_zone: (usize, usize),
    pub high_edge_zone: (usize, usize),
    /// One sentence an operator can act on.
    pub verdict: String,
}

/// Tones each side of DC, and at each band edge, treated as instrument.
///
/// Three is what the ch6 capture and the ABBA probe both show: a deep notch
/// over the DC region and 15 dB of roll-off in the outer few tones.
pub const ARTEFACT_TONES: usize = 3;

/// The plan for a capture whose channel is in dispute: there isn't one.
///
/// The record says one channel and the daemon says another (see
/// `wire::RadioInfo::channel_mismatch`). Every number below the channel is a
/// function of it, so the honest output is the disagreement itself. Drawing the
/// plan for either candidate would state a finding — "BLE cannot appear here",
/// or a list of advertisers that do — from a premise nobody has established.
///
/// The artefact zones are still filled: they are functions of the tone grid
/// alone, which is not in dispute.
pub fn disputed(
    record_channel: u32,
    tuned_channel: Option<u32>,
    ntone: usize,
    spacing_hz: f64,
) -> Bandplan {
    let zones = tones::artefact_zones(ntone, ARTEFACT_TONES);
    Bandplan {
        applicable: false,
        wifi_channel: record_channel,
        ntone,
        spacing_khz: spacing_hz / 1e3,
        dc_zone: zones.dc,
        low_edge_zone: zones.low_edge,
        high_edge_zone: zones.high_edge,
        verdict: match tuned_channel {
            Some(t) => format!(
                "No band plan: the records carry channel {record_channel} and csid \
                 says the radio is tuned to channel {t}. Everything here is a \
                 function of the channel, so nothing is drawn until those agree. \
                 Check that the tune took effect before trusting this capture."
            ),
            None => "No band plan: the capture's channel could not be established."
                .to_string(),
        },
        ..Default::default()
    }
}

/// Build the bandplan for a capture on `channel` with `ntone` tones at
/// `spacing_hz`.
pub fn compute(channel: u32, ntone: usize, spacing_hz: f64) -> Bandplan {
    let zones = tones::artefact_zones(ntone, ARTEFACT_TONES);
    let mut plan = Bandplan {
        applicable: false,
        wifi_channel: channel,
        ntone,
        spacing_khz: spacing_hz / 1e3,
        dc_zone: zones.dc,
        low_edge_zone: zones.low_edge,
        high_edge_zone: zones.high_edge,
        ..Default::default()
    };

    let Some(centre) = wifi_centre_mhz(channel) else {
        plan.verdict = "Not a 2.4 GHz channel — BLE cannot appear in this capture \
                        at all, which makes it a clean negative control."
            .to_string();
        return plan;
    };
    plan.applicable = true;
    plan.centre_mhz = centre;

    // The outermost delivered tone, not `ntone/2 · spacing`: the delivered
    // tones do not include DC, so the occupied band is wider than they are.
    let half_span_hz = tones::occupied_span_hz(ntone, spacing_hz) / 2.0;
    plan.half_span_mhz = half_span_hz / 1e6;

    for index in 0..40u32 {
        let f = ble_channel_mhz(index);
        let offset_hz = (f - centre) * 1e6;
        if offset_hz.abs() > half_span_hz {
            continue;
        }
        let k = offset_hz / spacing_hz;
        let i = tones::array_index(k, ntone);
        plan.inside.push(Landing {
            index,
            advertising: advertising_number(index),
            freq_mhz: f,
            subcarrier: k,
            array_index: i,
            tones_from_dc: tones::tones_from_dc(i, ntone),
            tones_from_edge: tones::tones_from_edge(i, ntone),
            artefact_distance: tones::artefact_distance(i, ntone),
        });
    }
    plan.advertising_inside = plan
        .inside
        .iter()
        .filter_map(|l| l.advertising)
        .collect();

    plan.verdict = verdict(&plan);
    plan
}

fn verdict(plan: &Bandplan) -> String {
    let Some(adv) = plan
        .inside
        .iter()
        .find(|l| l.advertising.is_some())
    else {
        return format!(
            "No BLE advertising channel is inside this passband — only data \
             channels can ever appear ({} of 37 in band). An exclusion arm here \
             is a ZERO-advertising condition, not a low one.",
            plan.inside.len()
        );
    };
    let n = adv.advertising.unwrap_or(0);
    if adv.artefact_distance <= ARTEFACT_TONES as f64 {
        format!(
            "Advertising channel {n} lands at array index {:.1}, INSIDE a measured \
             artefact region. Treatment and artefact would occupy the same \
             subcarriers, so this arm cannot be falsified. Move the channel.",
            adv.array_index
        )
    } else if adv.artefact_distance < 6.0 {
        format!(
            "Advertising channel {n} lands at array index {:.1}, only {:.1} tones \
             from a measured artefact region. Usable, but the margin is thin.",
            adv.array_index, adv.artefact_distance
        )
    } else {
        format!(
            "Advertising channel {n} lands at array index {:.1}, {:.1} tones clear \
             of both artefact regions. This is a usable inclusion arm.",
            adv.array_index, adv.artefact_distance
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: f64 = 312_500.0;

    /// The two rows of the EXP-010 table that decided the arm.
    #[test]
    fn ch13_is_refused_and_ch3_is_recommended() {
        let bad = compute(13, 52, LEGACY);
        let adv = bad.inside.iter().find(|l| l.advertising == Some(39)).unwrap();
        assert!((adv.array_index - 50.6).abs() < 0.05, "{}", adv.array_index);
        assert!((adv.artefact_distance - 0.4).abs() < 0.05);
        assert!(bad.verdict.contains("cannot be falsified"), "{}", bad.verdict);

        let good = compute(3, 52, LEGACY);
        let adv = good.inside.iter().find(|l| l.advertising == Some(38)).unwrap();
        assert!((adv.array_index - 37.8).abs() < 0.05, "{}", adv.array_index);
        assert!((adv.artefact_distance - 12.3).abs() < 0.05);
        assert!(good.verdict.contains("usable inclusion arm"), "{}", good.verdict);
    }

    /// The card's central claim about the exclusion arm: ch11 is not a
    /// low-advertising condition, it is a zero-advertising one.
    #[test]
    fn ch11_contains_no_advertising_channel() {
        let plan = compute(11, 52, LEGACY);
        assert!(plan.applicable);
        assert!(plan.advertising_inside.is_empty());
        assert!(!plan.inside.is_empty(), "data channels are still in band");
        assert!(plan.verdict.contains("ZERO-advertising"), "{}", plan.verdict);
    }

    /// The ch6 capture measured on 2026-08-17: eight BLE channels in band and
    /// no advertising channel among them.
    #[test]
    fn the_measured_ch6_capture_has_eight_ble_channels_in_band() {
        let plan = compute(6, 52, LEGACY);
        assert_eq!(plan.inside.len(), 8, "{:?}", plan.inside);
        assert!(plan.advertising_inside.is_empty());
    }

    #[test]
    fn five_gigahertz_is_not_applicable() {
        let plan = compute(36, 52, LEGACY);
        assert!(!plan.applicable);
        assert!(plan.inside.is_empty());
        assert!(plan.verdict.contains("negative control"), "{}", plan.verdict);
    }

    /// Advertising channel 37 is unreachable on every legal European channel:
    /// it sits at the very bottom of the band, below ch1's OFDM tones.
    #[test]
    fn advertising_channel_37_is_never_in_an_ofdm_passband() {
        for c in 1..=13u32 {
            let plan = compute(c, 52, LEGACY);
            assert!(
                !plan.advertising_inside.contains(&37),
                "channel {c} claims to see advertising 37"
            );
        }
    }

    /// The ranking in the card, reproduced from the geometry alone.
    #[test]
    fn ch3_is_the_best_of_the_inclusion_candidates() {
        let mut ranked: Vec<(u32, f64)> = (1..=13u32)
            .filter_map(|c| {
                let p = compute(c, 52, LEGACY);
                p.inside
                    .iter()
                    .find(|l| l.advertising.is_some())
                    .map(|l| (c, l.artefact_distance))
            })
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        assert_eq!(ranked.first().map(|r| r.0), Some(3), "{ranked:?}");
    }
}
