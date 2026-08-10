//! Raw → CSIQ export.
//!
//! The raw capture is the lossless source of truth and is never rewritten. The
//! `.csiq` container is a *derived*, self-describing artifact: it embeds the
//! session sidecar and re-encodes each record as TLVs so a consumer needs no
//! knowledge of the driver ABI.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::ExperimentConfig;

/// Convert a raw capture into a `.csiq`, embedding the sidecar as the session
/// block. Returns the number of records written.
pub fn raw_to_csiq(
    raw: &Path,
    out: &Path,
    cfg: &ExperimentConfig,
    sidecar_path: &Path,
) -> Result<u64> {
    let session = read_sidecar(sidecar_path);
    convert(raw, out, cfg.radio.width.to_csiq(), session.as_ref())
}

/// Export a session directory (`capture.raw` + `metadata.json`) to `.csiq`,
/// taking the monitor width from the sidecar. Returns `(path, records)`.
pub fn export_session(dir: &Path, out: Option<PathBuf>) -> Result<(PathBuf, u64)> {
    let raw = dir.join("capture.raw");
    if !raw.is_file() {
        anyhow::bail!("{} not found — not a session directory?", raw.display());
    }
    let sidecar_path = dir.join("metadata.json");
    let session = read_sidecar(&sidecar_path);
    let width = session
        .as_ref()
        .and_then(width_from_sidecar)
        .unwrap_or(csiq::Width::Unknown(0));

    let out = out.unwrap_or_else(|| dir.join("capture.csiq"));
    let n = convert(&raw, &out, width, session.as_ref())?;
    Ok((out, n))
}

fn convert(raw: &Path, out: &Path, width: csiq::Width, session: Option<&Value>) -> Result<u64> {
    let input = File::open(raw).with_context(|| format!("opening {}", raw.display()))?;
    let output = File::create(out).with_context(|| format!("creating {}", out.display()))?;

    let mut reader = csiq::raw::RawReader::new(BufReader::new(input), width);
    let mut writer =
        csiq::Writer::new(BufWriter::new(output), session).context("writing CSIQ header")?;

    let mut n = 0u64;
    let mut skipped = 0u64;
    loop {
        match reader.next_record() {
            Ok(Some(rec)) => {
                writer.write_record(&rec)?;
                n += 1;
            }
            Ok(None) => break,
            Err(e) => {
                // A truncated tail is normal if a session was killed mid-write;
                // stop cleanly rather than discarding everything before it.
                tracing::warn!(error = %e, records = n, "stopping export at unreadable record");
                skipped += 1;
                break;
            }
        }
    }
    writer.finish()?;
    if skipped > 0 {
        tracing::warn!(skipped, "export finished with a truncated tail");
    }
    Ok(n)
}

fn read_sidecar(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Recover the monitor width recorded in a sidecar.
/// Monitor width as recorded in a session sidecar.
///
/// Public because the time-transfer recovery export (`csid timesync export`)
/// walks the same `capture.raw` and must read it the same way — a second copy
/// of this table is a second thing to forget when a width is added.
pub fn width_from_sidecar(v: &Value) -> Option<csiq::Width> {
    let w = v.get("radio")?.get("width")?.as_str()?;
    Some(match w {
        "NOHT" => csiq::Width::Noht,
        "HT20" => csiq::Width::Ht20,
        "HT40-" => csiq::Width::Ht40Minus,
        "HT40+" => csiq::Width::Ht40Plus,
        "80MHz" => csiq::Width::W80,
        "160MHz" => csiq::Width::W160,
        "320MHz" => csiq::Width::W320,
        _ => csiq::Width::Unknown(0),
    })
}
