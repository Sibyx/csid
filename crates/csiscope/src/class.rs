//! The identity of a **record class**: tone count and modulation family.
//!
//! This is the load-bearing abstraction for real captures. `csid caps` puts it
//! plainly — *CSI type follows the received frame* — so an ambient stream on a
//! busy channel is not one signal but several interleaved ones: legacy 52-tone
//! beacons, HT 56-tone data, HE 242-tone bursts, all arriving in the same
//! second from different transmitters.
//!
//! A console that renders "the newest record" therefore flickers between
//! incompatible geometries: the PHY label blinks, the spectrum changes width,
//! and the waterfall has no stable number of columns. Worse, a time series
//! built across the mix is meaningless, because consecutive samples describe
//! different measurements.
//!
//! So every view is scoped to exactly one class. The operator picks it; the
//! default is whichever class dominates the window. The full mix stays visible
//! in its own panel, because *what else is on the channel* is real information.
//!
//! ## Why this is a type and not a `String`
//!
//! The class of a record was previously computed as
//! `format!("{}:{}", ntone, format!("{modulation:?}").to_lowercase())` — two
//! heap allocations, a `Debug` format and an ASCII case conversion, per record.
//! It was computed three times per record per frame (the census, the window
//! filter, the fresh-record filter), and the results were keys in a
//! `HashMap<String, _>`, so every one of them was also SipHashed.
//!
//! On a 256-record window that is ~1,500 allocations and ~1,500 hashes per
//! frame to distinguish between two or three values. Together it was about 9%
//! of the process. A `Copy` key of four bytes compares in one instruction, and
//! the census below is a linear scan over a handful of entries — which beats
//! any hash map at this cardinality, and allocates nothing at all.
//!
//! The wire form is unchanged: `"<ntone>:<modulation>"`, e.g. `"56:ht"`.

use std::fmt;

use csiq::{CsiRecord, Modulation};

/// Modulation family as the console keys on it.
///
/// Mirrors [`csiq::Modulation`] with an explicit "the record carried no
/// `rate_n_flags`" case, and with the `Hash` the key needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Phy {
    /// No PHY label — `rate_n_flags` was unavailable for this record.
    #[default]
    Unlabelled,
    Cck,
    LegacyOfdm,
    Ht,
    Vht,
    He,
    Eht,
    /// A modulation nibble this build does not know, carried verbatim.
    Unknown(u8),
}

impl Phy {
    pub fn of(modulation: Option<Modulation>) -> Self {
        match modulation {
            None => Phy::Unlabelled,
            Some(Modulation::Cck) => Phy::Cck,
            Some(Modulation::LegacyOfdm) => Phy::LegacyOfdm,
            Some(Modulation::Ht) => Phy::Ht,
            Some(Modulation::Vht) => Phy::Vht,
            Some(Modulation::He) => Phy::He,
            Some(Modulation::Eht) => Phy::Eht,
            Some(Modulation::Unknown(v)) => Phy::Unknown(v),
        }
    }

    /// The stable lowercase name, where there is one.
    fn as_str(&self) -> Option<&'static str> {
        Some(match self {
            Phy::Unlabelled => "unlabelled",
            Phy::Cck => "cck",
            Phy::LegacyOfdm => "legacyofdm",
            Phy::Ht => "ht",
            Phy::Vht => "vht",
            Phy::He => "he",
            Phy::Eht => "eht",
            Phy::Unknown(_) => return None,
        })
    }
}

impl fmt::Display for Phy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_str() {
            Some(s) => f.write_str(s),
            // Matches what `format!("{:?}").to_lowercase()` produced, so a
            // pinned class survives an upgrade.
            None => match self {
                Phy::Unknown(v) => write!(f, "unknown({v})"),
                _ => unreachable!(),
            },
        }
    }
}

impl std::str::FromStr for Phy {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "unlabelled" => Phy::Unlabelled,
            "cck" => Phy::Cck,
            "legacyofdm" => Phy::LegacyOfdm,
            "ht" => Phy::Ht,
            "vht" => Phy::Vht,
            "he" => Phy::He,
            "eht" => Phy::Eht,
            other => {
                let v = other
                    .strip_prefix("unknown(")
                    .and_then(|r| r.strip_suffix(')'))
                    .and_then(|r| r.parse::<u8>().ok())
                    .ok_or(())?;
                Phy::Unknown(v)
            }
        })
    }
}

/// A record class: the tone grid and the modulation that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ClassKey {
    pub ntone: u16,
    pub phy: Phy,
}

impl ClassKey {
    /// The class a record belongs to. Allocation-free and branch-cheap; this
    /// runs once per record per pass.
    #[inline]
    pub fn of(rec: &CsiRecord) -> Self {
        ClassKey {
            ntone: rec.ntone,
            phy: Phy::of(rec.phy.map(|p| p.modulation)),
        }
    }

    /// Human label, e.g. `56-tone ht`.
    pub fn label(&self) -> String {
        format!("{}-tone {}", self.ntone, self.phy)
    }
}

impl fmt::Display for ClassKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ntone, self.phy)
    }
}

impl std::str::FromStr for ClassKey {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        let (tones, phy) = s.split_once(':').ok_or(())?;
        Ok(ClassKey {
            ntone: tones.parse().map_err(|_| ())?,
            phy: phy.parse()?,
        })
    }
}

impl serde::Serialize for ClassKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// How many records of each class are in the window.
///
/// A linear scan, deliberately. Real captures carry two to four classes at
/// once; at that cardinality a `Vec` probe is a couple of compares against
/// values already in a register, while a `HashMap<String, _>` was an
/// allocation plus a SipHash round per record.
#[derive(Debug, Default, Clone)]
pub struct Census {
    counts: Vec<(ClassKey, u64)>,
    total: u64,
}

impl Census {
    pub fn clear(&mut self) {
        self.counts.clear();
        self.total = 0;
    }

    #[inline]
    pub fn add(&mut self, key: ClassKey) {
        self.total += 1;
        for entry in self.counts.iter_mut() {
            if entry.0 == key {
                entry.1 += 1;
                return;
            }
        }
        self.counts.push((key, 1));
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    pub fn count(&self, key: ClassKey) -> u64 {
        self.counts
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
            .unwrap_or(0)
    }

    pub fn contains(&self, key: ClassKey) -> bool {
        self.counts.iter().any(|(k, _)| *k == key)
    }

    /// The most common class.
    ///
    /// Ties break on the key so the default class cannot oscillate between two
    /// equally common PHY types frame to frame. The tie-break reproduces the
    /// original string ordering (`b.0.cmp(a.0)` over `"<ntone>:<phy>"`), which
    /// is lexical over the rendered form rather than numeric over `ntone` —
    /// preserved deliberately so a pinned default does not move under an
    /// operator who upgrades mid-experiment.
    pub fn dominant(&self) -> Option<ClassKey> {
        self.counts
            .iter()
            .max_by(|a, b| {
                a.1.cmp(&b.1)
                    .then_with(|| b.0.to_string().cmp(&a.0.to_string()))
            })
            .map(|(k, _)| *k)
    }

    /// Every class present, ranked by share of the window, then by key.
    pub fn ranked(&self) -> Vec<(ClassKey, u64)> {
        let mut out = self.counts.clone();
        out.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// The wire form is a compatibility surface: the browser pins a class by
    /// string and gets it back in the header. Every variant must survive the
    /// round trip, and must render exactly what the old
    /// `format!("{:?}").to_lowercase()` produced.
    #[test]
    fn the_wire_form_round_trips_and_matches_the_old_rendering() {
        let cases = [
            (
                ClassKey {
                    ntone: 52,
                    phy: Phy::LegacyOfdm,
                },
                "52:legacyofdm",
            ),
            (
                ClassKey {
                    ntone: 56,
                    phy: Phy::Ht,
                },
                "56:ht",
            ),
            (
                ClassKey {
                    ntone: 996,
                    phy: Phy::He,
                },
                "996:he",
            ),
            (
                ClassKey {
                    ntone: 242,
                    phy: Phy::Unlabelled,
                },
                "242:unlabelled",
            ),
            (
                ClassKey {
                    ntone: 64,
                    phy: Phy::Unknown(7),
                },
                "64:unknown(7)",
            ),
        ];
        for (key, text) in cases {
            assert_eq!(key.to_string(), text);
            assert_eq!(ClassKey::from_str(text), Ok(key));
        }

        // And the rendering agrees with the formatting it replaced.
        for m in [
            Modulation::Cck,
            Modulation::LegacyOfdm,
            Modulation::Ht,
            Modulation::Vht,
            Modulation::He,
            Modulation::Eht,
            Modulation::Unknown(3),
        ] {
            let old = format!("{m:?}").to_lowercase();
            assert_eq!(Phy::of(Some(m)).to_string(), old, "{m:?}");
        }
    }

    #[test]
    fn nonsense_class_strings_are_rejected_not_guessed() {
        for s in [
            "",
            "ht",
            "52",
            "52:",
            ":ht",
            "x:ht",
            "52:nope",
            "52:unknown()",
        ] {
            assert!(ClassKey::from_str(s).is_err(), "{s:?} must not parse");
        }
    }

    #[test]
    fn the_census_ranks_by_share_and_breaks_ties_stably() {
        let legacy = ClassKey {
            ntone: 52,
            phy: Phy::LegacyOfdm,
        };
        let ht = ClassKey {
            ntone: 56,
            phy: Phy::Ht,
        };
        let mut c = Census::default();
        for i in 0..600 {
            c.add(if i % 6 == 0 { ht } else { legacy });
        }
        assert_eq!(c.total(), 600);
        assert_eq!(c.count(ht), 100);
        assert_eq!(c.count(legacy), 500);
        assert_eq!(c.dominant(), Some(legacy));
        assert!(c.contains(ht));
        let ranked = c.ranked();
        assert_eq!(ranked[0].0, legacy);
        assert_eq!(ranked[1].0, ht);

        // A dead heat must resolve the same way whichever order they arrived.
        let mut a = Census::default();
        let mut b = Census::default();
        for _ in 0..10 {
            a.add(legacy);
            a.add(ht);
            b.add(ht);
            b.add(legacy);
        }
        assert_eq!(a.dominant(), b.dominant());
    }
}
