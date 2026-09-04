//! Capability envelope: band/channel/width validation and the ETSI centre-
//! frequency tables needed to tune 80/160 MHz.
//!
//! This module is the reason `csid validate` can reject an impossible capture
//! before a radio is touched. The measured envelope constants come from the
//! IP-120 on-hardware benchmark (AX210 + iax, 2026-07-21).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::RadioConfig;

/// Operating band. Required explicitly for 6 GHz because its channel numbering
/// overlaps 2.4 GHz (channel 1 exists in both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Band {
    #[serde(rename = "2.4", alias = "2.4GHz", alias = "24")]
    Ghz24,
    #[serde(rename = "5", alias = "5GHz")]
    Ghz5,
    #[serde(rename = "6", alias = "6GHz")]
    Ghz6,
}

/// Monitor width, spelled as `iw` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum WidthCfg {
    #[serde(rename = "NOHT", alias = "noht")]
    Noht,
    #[serde(rename = "HT20", alias = "ht20")]
    Ht20,
    #[serde(rename = "HT40-", alias = "ht40-")]
    Ht40Minus,
    #[serde(rename = "HT40+", alias = "ht40+")]
    Ht40Plus,
    #[serde(rename = "80MHz", alias = "80", alias = "80mhz")]
    W80,
    #[serde(rename = "160MHz", alias = "160", alias = "160mhz")]
    W160,
    /// 802.11be — accepted by config, rejected by validation on AX210 hardware.
    #[serde(rename = "320MHz", alias = "320", alias = "320mhz")]
    W320,
}

impl WidthCfg {
    /// The literal `iw dev … set freq` width token.
    pub fn iw_token(self) -> &'static str {
        match self {
            WidthCfg::Noht => "NOHT",
            WidthCfg::Ht20 => "HT20",
            WidthCfg::Ht40Minus => "HT40-",
            WidthCfg::Ht40Plus => "HT40+",
            WidthCfg::W80 => "80MHz",
            WidthCfg::W160 => "160MHz",
            WidthCfg::W320 => "320MHz",
        }
    }

    /// Map to the CSIQ on-wire width code.
    pub fn to_csiq(self) -> csiq::Width {
        match self {
            WidthCfg::Noht => csiq::Width::Noht,
            WidthCfg::Ht20 => csiq::Width::Ht20,
            WidthCfg::Ht40Minus => csiq::Width::Ht40Minus,
            WidthCfg::Ht40Plus => csiq::Width::Ht40Plus,
            WidthCfg::W80 => csiq::Width::W80,
            WidthCfg::W160 => csiq::Width::W160,
            WidthCfg::W320 => csiq::Width::W320,
        }
    }

    /// Widths above 40 MHz need an explicit centre frequency when tuning.
    pub fn needs_center(self) -> bool {
        matches!(self, WidthCfg::W80 | WidthCfg::W160 | WidthCfg::W320)
    }

    /// The numeric width for iw's `set freq <control> <width> <center1>` form.
    ///
    /// iw has two mutually exclusive syntaxes: the keyword tokens (`HT20`,
    /// `80MHz`, …) belong to the centre-less form, and passing a centre
    /// frequency after them makes iw print usage and exit 1. Whenever a
    /// centre is supplied the width must be numeric.
    pub fn iw_numeric(self) -> &'static str {
        match self {
            WidthCfg::Noht | WidthCfg::Ht20 => "20",
            WidthCfg::Ht40Minus | WidthCfg::Ht40Plus => "40",
            WidthCfg::W80 => "80",
            WidthCfg::W160 => "160",
            WidthCfg::W320 => "320",
        }
    }
}

/// 5 GHz 80 MHz groups → centre channel.
const G5_80: &[(&[u32], u32)] = &[
    (&[36, 40, 44, 48], 42),
    (&[52, 56, 60, 64], 58),
    (&[100, 104, 108, 112], 106),
    (&[116, 120, 124, 128], 122),
    (&[132, 136, 140, 144], 138),
    (&[149, 153, 157, 161], 155),
    (&[165, 169, 173, 177], 171),
];

/// 5 GHz 160 MHz groups → centre channel.
const G5_160: &[(&[u32], u32)] = &[
    (&[36, 40, 44, 48, 52, 56, 60, 64], 50),
    (&[100, 104, 108, 112, 116, 120, 124, 128], 114),
    (&[149, 153, 157, 161, 165, 169, 173, 177], 163),
];

/// The 20 MHz channel set this build accepts on 5 GHz.
const CH5: &[u32] = &[
    36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144,
    149, 153, 157, 161, 165, 169, 173, 177,
];

/// Infer the band from a channel number when not stated. 6 GHz can never be
/// inferred (it overlaps 2.4 GHz), so it must be given explicitly.
pub fn infer_band(channel: u32) -> Option<Band> {
    match channel {
        1..=14 => Some(Band::Ghz24),
        c if CH5.contains(&c) => Some(Band::Ghz5),
        _ => None,
    }
}

/// Resolve the band for a radio config (explicit wins over inference).
pub fn resolve_band(radio: &RadioConfig) -> Result<Band> {
    if let Some(b) = radio.band {
        return Ok(b);
    }
    infer_band(radio.channel).ok_or_else(|| {
        anyhow::anyhow!(
            "cannot infer band for channel {} — set radio.band explicitly (\"2.4\", \"5\", or \"6\")",
            radio.channel
        )
    })
}

/// Control-channel centre frequency in MHz.
pub fn channel_to_freq(band: Band, channel: u32) -> Option<u32> {
    match band {
        Band::Ghz24 => match channel {
            14 => Some(2484),
            1..=13 => Some(2407 + 5 * channel),
            _ => None,
        },
        Band::Ghz5 => CH5.contains(&channel).then(|| 5000 + 5 * channel),
        Band::Ghz6 => {
            // 6 GHz 20 MHz channels are 1, 5, 9 … 233.
            ((1..=233).contains(&channel) && (channel - 1) % 4 == 0).then(|| 5950 + 5 * channel)
        }
    }
}

/// The inverse of [`channel_to_freq`]: band and channel for a control
/// frequency the radio or a scan reported. `None` for a frequency no band
/// this build knows carries.
///
/// 6 GHz starts at 5955 MHz (channel 1). 5945 MHz is not a channel centre on
/// any band and reads as `None`, which is right: a survey line that says 5945
/// is a parse error, not a channel.
pub fn freq_to_channel(freq_mhz: u32) -> Option<(Band, u32)> {
    match freq_mhz {
        2484 => Some((Band::Ghz24, 14)),
        2412..=2472 if (freq_mhz - 2407) % 5 == 0 => Some((Band::Ghz24, (freq_mhz - 2407) / 5)),
        5180..=5885 if (freq_mhz - 5000) % 5 == 0 => {
            let ch = (freq_mhz - 5000) / 5;
            CH5.contains(&ch).then_some((Band::Ghz5, ch))
        }
        5955..=7115 if (freq_mhz - 5950) % 20 == 5 => Some((Band::Ghz6, (freq_mhz - 5950) / 5)),
        _ => None,
    }
}

impl WidthCfg {
    /// The width an associated link reports, as a monitor-width token.
    ///
    /// `iw dev … info` prints `width: 40 MHz` without saying which side the
    /// secondary sits on; the centre frequency does. Used by STA-mode capture,
    /// where the width is observed off the link rather than commanded.
    pub fn from_observed(
        width_mhz: u32,
        control_mhz: u32,
        center_mhz: Option<u32>,
    ) -> Option<Self> {
        match width_mhz {
            20 => Some(WidthCfg::Ht20),
            40 => match center_mhz {
                Some(c) if c < control_mhz => Some(WidthCfg::Ht40Minus),
                Some(_) => Some(WidthCfg::Ht40Plus),
                None => None,
            },
            80 => Some(WidthCfg::W80),
            160 => Some(WidthCfg::W160),
            320 => Some(WidthCfg::W320),
            _ => None,
        }
    }
}

/// The centre frequency (MHz) a wide capture must be tuned to, or `None` for
/// widths that do not need one.
pub fn center_freq(band: Band, channel: u32, width: WidthCfg) -> Result<Option<u32>> {
    if !width.needs_center() {
        return Ok(None);
    }
    let center_ch = match (band, width) {
        (Band::Ghz5, WidthCfg::W80) => group_center(G5_80, channel),
        (Band::Ghz5, WidthCfg::W160) => group_center(G5_160, channel),
        // 6 GHz is regular: 80 MHz centres every 16 channels from 7,
        // 160 MHz centres every 32 from 15.
        (Band::Ghz6, WidthCfg::W80) => Some(((channel - 1) / 16) * 16 + 7),
        (Band::Ghz6, WidthCfg::W160) => Some(((channel - 1) / 32) * 32 + 15),
        (Band::Ghz24, _) => bail!("width {} is not valid on 2.4 GHz", width.iw_token()),
        (_, WidthCfg::W320) => bail!("320 MHz (802.11be) is not supported by AX210 hardware"),
        _ => None,
    };
    let center_ch = center_ch.ok_or_else(|| {
        anyhow::anyhow!(
            "channel {channel} is not the member of any {} group",
            width.iw_token()
        )
    })?;
    let freq = match band {
        Band::Ghz5 => 5000 + 5 * center_ch,
        Band::Ghz6 => 5950 + 5 * center_ch,
        Band::Ghz24 => bail!("no wide centre on 2.4 GHz"),
    };
    Ok(Some(freq))
}

fn group_center(groups: &[(&[u32], u32)], channel: u32) -> Option<u32> {
    groups
        .iter()
        .find(|(members, _)| members.contains(&channel))
        .map(|(_, c)| *c)
}

/// Full radio-config validation: band, channel, width legality, and the
/// existence of a centre frequency for wide captures.
pub fn validate_radio(radio: &RadioConfig) -> Result<()> {
    let band = resolve_band(radio)?;

    if channel_to_freq(band, radio.channel).is_none() {
        bail!(
            "channel {} is not valid on the {} band",
            radio.channel,
            band_name(band)
        );
    }

    // Width legality per band.
    match (band, radio.width) {
        (
            Band::Ghz24,
            WidthCfg::Noht | WidthCfg::Ht20 | WidthCfg::Ht40Minus | WidthCfg::Ht40Plus,
        ) => {}
        (Band::Ghz24, w) => bail!("width {} is not valid on 2.4 GHz", w.iw_token()),
        (_, WidthCfg::W320) => {
            bail!("320 MHz (802.11be) is beyond the AX210 envelope (max 160 MHz)")
        }
        _ => {}
    }

    // Wide captures must resolve to a real centre.
    center_freq(band, radio.channel, radio.width)?;

    if radio.interface.is_empty() {
        bail!("radio.interface must be set");
    }
    if radio.monitor.is_empty() {
        bail!("radio.monitor must be set");
    }
    Ok(())
}

fn band_name(b: Band) -> &'static str {
    match b {
        Band::Ghz24 => "2.4 GHz",
        Band::Ghz5 => "5 GHz",
        Band::Ghz6 => "6 GHz",
    }
}

/// The measured capability envelope, reported by `csid caps`.
///
/// Values are the IP-120 on-hardware benchmark results (node `pi5-csi-iax`,
/// AX210, fw 78.3bfdc55f.0, regdomain SK DFS-ETSI, 2026-07-21).
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub tone_counts: &'static [u16],
    pub max_mimo: &'static str,
    pub sustained_rate_hz: u32,
    pub sustained_bytes_per_sec: u32,
    pub ftm_clock_hz: u64,
    pub ftm_resolution_ns: f64,
    pub ftm_wrap_seconds: f64,
    pub notes: &'static [&'static str],
}

impl Default for Envelope {
    fn default() -> Self {
        Envelope {
            tone_counts: &[52, 56, 242, 484, 996, 1992],
            max_mimo: "2x2 (242-tone x 4 chains) when the source transmits MIMO",
            sustained_rate_hz: 608,
            sustained_bytes_per_sec: 440 * 1024,
            ftm_clock_hz: csiq::FTM_HZ,
            ftm_resolution_ns: 3.125,
            ftm_wrap_seconds: (1u64 << 32) as f64 / csiq::FTM_HZ as f64,
            notes: &[
                "CSI type follows the received frame; monitor width only bounds decodability.",
                "996-tone (HE80) requires monitor width >= the PPDU width.",
                "Wider monitor width lowers total frame rate (~371 Hz at 20/40 vs ~215-225 Hz at 80/160).",
                "Amplitude is AGC-RELATIVE: |H| carries the receiver's own gain setting as well as \
                 the channel, so read shape from it and never level; the absolute anchor is RSSI.",
                "interval_us is a clean rate cap: 10 ms -> ~88 Hz, 100 ms -> ~9.8 Hz.",
                "First 6 GHz tune after a 5 GHz retune can return -EIO; csid retries once.",
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequencies_round_trip_to_channels() {
        assert_eq!(freq_to_channel(2437), Some((Band::Ghz24, 6)));
        assert_eq!(freq_to_channel(2484), Some((Band::Ghz24, 14)));
        assert_eq!(freq_to_channel(5220), Some((Band::Ghz5, 44)));
        assert_eq!(freq_to_channel(5975), Some((Band::Ghz6, 5)));
        assert_eq!(freq_to_channel(6375), Some((Band::Ghz6, 85)));
        // Not a channel centre on any band.
        assert_eq!(freq_to_channel(5945), None);
        assert_eq!(freq_to_channel(5210), None);
        for ch in CH5 {
            let f = channel_to_freq(Band::Ghz5, *ch).unwrap();
            assert_eq!(freq_to_channel(f), Some((Band::Ghz5, *ch)));
        }
    }

    #[test]
    fn observed_widths_map_to_tokens() {
        assert_eq!(
            WidthCfg::from_observed(20, 5220, None),
            Some(WidthCfg::Ht20)
        );
        assert_eq!(
            WidthCfg::from_observed(40, 5220, Some(5230)),
            Some(WidthCfg::Ht40Plus)
        );
        assert_eq!(
            WidthCfg::from_observed(40, 5240, Some(5230)),
            Some(WidthCfg::Ht40Minus)
        );
        assert_eq!(WidthCfg::from_observed(40, 5220, None), None);
        assert_eq!(
            WidthCfg::from_observed(80, 5220, Some(5210)),
            Some(WidthCfg::W80)
        );
        assert_eq!(WidthCfg::from_observed(30, 5220, None), None);
    }

    #[test]
    fn five_ghz_centres() {
        assert_eq!(
            center_freq(Band::Ghz5, 36, WidthCfg::W80).unwrap(),
            Some(5210)
        );
        assert_eq!(
            center_freq(Band::Ghz5, 48, WidthCfg::W80).unwrap(),
            Some(5210)
        );
        assert_eq!(
            center_freq(Band::Ghz5, 60, WidthCfg::W80).unwrap(),
            Some(5290)
        );
        assert_eq!(
            center_freq(Band::Ghz5, 36, WidthCfg::W160).unwrap(),
            Some(5250)
        );
        assert_eq!(
            center_freq(Band::Ghz5, 149, WidthCfg::W160).unwrap(),
            Some(5815)
        );
    }

    #[test]
    fn six_ghz_centres_are_regular() {
        // Channels 1,5,9,13 share the 80 MHz centre 7 -> 5985 MHz.
        for ch in [1, 5, 9, 13] {
            assert_eq!(
                center_freq(Band::Ghz6, ch, WidthCfg::W80).unwrap(),
                Some(5985)
            );
        }
        assert_eq!(
            center_freq(Band::Ghz6, 17, WidthCfg::W80).unwrap(),
            Some(6065)
        );
        assert_eq!(
            center_freq(Band::Ghz6, 1, WidthCfg::W160).unwrap(),
            Some(6025)
        );
    }

    #[test]
    fn narrow_widths_need_no_centre() {
        assert_eq!(center_freq(Band::Ghz24, 6, WidthCfg::Ht20).unwrap(), None);
    }

    #[test]
    fn frequencies() {
        assert_eq!(channel_to_freq(Band::Ghz24, 1), Some(2412));
        assert_eq!(channel_to_freq(Band::Ghz24, 14), Some(2484));
        assert_eq!(channel_to_freq(Band::Ghz5, 36), Some(5180));
        assert_eq!(channel_to_freq(Band::Ghz6, 5), Some(5975));
        assert_eq!(channel_to_freq(Band::Ghz5, 37), None);
    }

    #[test]
    fn band_inference_refuses_ambiguous_six_ghz() {
        assert_eq!(infer_band(6), Some(Band::Ghz24));
        assert_eq!(infer_band(36), Some(Band::Ghz5));
        assert_eq!(infer_band(200), None);
    }
}
