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

/// The exported container's file name.
///
/// **The file is compressed; the format is not** (IP-139 Phase 6). CSIQ's own
/// `FORMAT_VERSION` stays 1 and the byte stream inside is exactly what a v1
/// reader expects, so nothing about the container changed — only the envelope
/// it is stored in, and the extension says which envelope that is.
///
/// This buys the `VENDOR_HDR` blob. The 272-byte driver header is 203 constant
/// bytes out of 272 on a real capture, which is the most compressible thing in
/// the record, while the CSI matrix barely compresses at all. Keeping the header
/// verbatim is what lets a later reader recover a field this build cannot name,
/// and compression is what makes that affordable.
///
/// `csid-sync` copies the whole directory and `csid-prune`'s payload list is a
/// deny-list of `metadata.json` and `.synced`, so neither needed changing.
pub const CSIQ_NAME: &str = "capture.csiq.zst";

/// zstd level. 3 is the crate default and the speed/ratio knee on a Pi 5 —
/// the sealer already competes with the capture for CPU, so a higher level
/// would trade a running measurement for disk that is not scarce at the point
/// the seal happens.
const ZSTD_LEVEL: i32 = 3;

/// True when `path` should be written (or read) through zstd.
///
/// Keyed on the extension rather than a flag, so the file's own name is the
/// single statement of how it is stored and a directory listing cannot lie.
pub fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zst"))
}

/// Convert a raw capture into a `.csiq`, embedding `session` as the session
/// block. Returns the number of records written.
///
/// The session block is taken **by value, never by path**. A live capture
/// already holds its sidecar in memory, and the on-disk copy is deliberately
/// stale while a segment seals — it says `capturing` until the export lands, so
/// that a crash mid-export leaves a directory `csid-sync` skips. Re-reading it
/// here is what made every segmented capture's `.csiq` claim it was never
/// finished. Passing the value makes the write order a caller's choice and
/// removes the trap from the format.
pub fn raw_to_csiq_with_session(
    raw: &Path,
    out: &Path,
    width: csiq::Width,
    session: Option<&Value>,
) -> Result<u64> {
    convert(raw, out, width, session, false)
}

/// [`raw_to_csiq_with_session`], keeping the driver header on every record.
///
/// The blob is what makes the export lossless: a field this build cannot name
/// survives at its documented offset, so the next recovery costs a decoder
/// rather than a re-capture. It is affordable because [`CSIQ_NAME`] compresses.
pub fn raw_to_csiq_lossless(
    raw: &Path,
    out: &Path,
    width: csiq::Width,
    session: Option<&Value>,
    keep_vendor_hdr: bool,
) -> Result<u64> {
    convert(raw, out, width, session, keep_vendor_hdr)
}

/// Open an exported container for reading, compressed or not.
///
/// The extension decides, so a caller never has to know which envelope a given
/// file uses — which matters because the archive holds both: every file written
/// before IP-139 Phase 6 is a plain `capture.csiq`, and every file after is a
/// `capture.csiq.zst`. Both hold the same CSIQ v1 byte stream.
pub fn open_csiq(path: &Path) -> Result<csiq::Reader<Box<dyn std::io::Read>>> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let inner: Box<dyn std::io::Read> = if is_compressed(path) {
        Box::new(zstd::Decoder::new(BufReader::new(f)).with_context(|| {
            format!("{} is named .zst but is not a zstd frame", path.display())
        })?)
    } else {
        Box::new(BufReader::new(f))
    };
    csiq::Reader::new(inner).with_context(|| format!("reading {}", path.display()))
}

/// The exported container in a session directory, whichever envelope it uses.
///
/// Prefers the compressed name, so a directory that holds both after a
/// re-export reads the current one.
pub fn find_csiq(dir: &Path) -> Option<PathBuf> {
    [CSIQ_NAME, "capture.csiq"]
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
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

    let out = out.unwrap_or_else(|| dir.join(CSIQ_NAME));
    let n = convert(&raw, &out, width, session.as_ref(), true)?;
    Ok((out, n))
}

fn convert(
    raw: &Path,
    out: &Path,
    width: csiq::Width,
    session: Option<&Value>,
    keep_vendor_hdr: bool,
) -> Result<u64> {
    let input = File::open(raw).with_context(|| format!("opening {}", raw.display()))?;
    let output = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut reader =
        csiq::raw::RawReader::new(BufReader::new(input), width).keeping_vendor_hdr(keep_vendor_hdr);

    if is_compressed(out) {
        let enc = zstd::Encoder::new(BufWriter::new(output), ZSTD_LEVEL)
            .with_context(|| format!("starting zstd for {}", out.display()))?;
        let writer = csiq::Writer::new(enc, session).context("writing CSIQ header")?;
        let (n, enc) = drain(&mut reader, writer)?;
        // `finish` writes zstd's frame epilogue. Without it the file decodes as
        // a truncated frame, which every reader reports as corruption rather
        // than as a missing flush.
        enc.finish().context("finishing the zstd frame")?;
        return Ok(n);
    }

    let writer = csiq::Writer::new(BufWriter::new(output), session).context("writing CSIQ header")?;
    let (n, _) = drain(&mut reader, writer)?;
    Ok(n)
}

/// Copy every readable record from the raw stream into `writer`.
///
/// Returns the record count and the underlying sink, so a compressed export can
/// finish its frame. Split out of [`convert`] so the compressed and plain paths
/// share one loop — the truncated-tail rule below is the kind of thing that
/// drifts the moment it exists twice.
fn drain<R: std::io::Read, W: std::io::Write>(
    reader: &mut csiq::raw::RawReader<R>,
    mut writer: csiq::Writer<W>,
) -> Result<(u64, W)> {

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
    let sink = writer.finish()?;
    if skipped > 0 {
        tracing::warn!(skipped, "export finished with a truncated tail");
    }
    Ok((n, sink))
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
