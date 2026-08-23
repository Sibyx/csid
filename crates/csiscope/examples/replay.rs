//! Replay a real capture into the console's live socket.
//!
//! This exists so the console can be exercised against **measurements**, on a
//! machine with no radio. It reads a `capture.raw` or `capture.csiq` out of the
//! archive and re-emits every record over UDP, in order, at the intervals the
//! hardware actually delivered them — the FTM stamps in the file, not a clock
//! of its own.
//!
//! Nothing here generates a channel. There is no model of a room, no synthetic
//! reflector and no chosen null rate: a replayed record is byte-for-byte the
//! one the AX210 handed to `csid`, empty payloads and burst gaps included. A
//! console that renders a replay correctly renders the capture correctly,
//! which is the only claim worth being able to make.
//!
//! ```console
//! $ cargo run -p csiscope --example replay -- \
//!       --path /path/to/monad01_.../capture.raw --target 127.0.0.1:5599
//! $ csiscope --udp-bind 127.0.0.1:5599
//! ```
//!
//! The 320 MHz FTM counter wraps about every 13.4 seconds, so gaps are taken
//! from [`csiq::FtmUnwrapper`] rather than from raw differences — the same
//! unwrapper `csid summarize` uses, for the same reason.

use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::Parser;
use csiq::CsiRecord;

#[derive(Parser)]
#[command(name = "replay", about = "Replay an archived capture into csiscope")]
struct Cli {
    /// `capture.raw` or `capture.csiq` from a session directory.
    #[arg(long)]
    path: PathBuf,

    /// Where csiscope is listening (`csiscope --udp-bind`).
    #[arg(long, default_value = "127.0.0.1:5599")]
    target: String,

    /// Channel width to parse `capture.raw` with. The raw framing does not
    /// carry it; the session's `metadata.json` states it under `radio.width`.
    /// Ignored for `.csiq`, which carries its own.
    #[arg(long, default_value = "HT20")]
    width: String,

    /// Replay faster or slower than real time. 1.0 preserves the recorded
    /// intervals exactly, which is what makes the timing panels meaningful.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,

    /// Start again from the beginning when the file runs out.
    #[arg(long)]
    loop_forever: bool,

    /// Also publish a `status.json` beside the replay, built from the session's
    /// own `metadata.json`.
    ///
    /// Half of what the console shows cannot be derived from records —
    /// `frames_seen`, the commanded interval, the channel the radio was tuned
    /// to. During a live capture `csid` publishes those; during a replay they
    /// come from the sidecar the same session wrote, which is the durable
    /// record of exactly that. Point `csiscope --status` at the same path.
    #[arg(long, value_name = "PATH")]
    status: Option<PathBuf>,
}

/// Rebuild the status document this session published while it ran.
///
/// Every field is read out of the session's own `metadata.json`; nothing is
/// invented. `records` and `uptime_s` advance as the replay does, because those
/// are the two the console watches move.
fn publish_status(path: &PathBuf, meta: &serde_json::Value, records: u64, empty: u64, uptime_s: u64) {
    let radio = &meta["radio"];
    let summary = &meta["summary"];
    let doc = serde_json::json!({
        "schema": "csid-status/1",
        "session_id": meta["session_id"],
        "run_id": meta["run_id"],
        "run_id_generated": meta["run_id_generated"],
        "experiment": meta["experiment"],
        "host": meta["environment"]["hostname"],
        "state": "capturing",
        "started_unix_ns": 0,
        "uptime_s": uptime_s,
        "channel": radio["channel"],
        "width": radio["width"],
        "band": radio["band"],
        "control_freq_mhz": radio["control_freq_mhz"],
        "center_freq_mhz": radio["center_freq_mhz"],
        "interval_us": radio["interval_us"],
        "records": records,
        "empty_records": empty,
        // The sidecar has no frames_seen — the time-transfer receiver counts it
        // and only a live session has one. Reporting the record count means the
        // yield reads 100%, which is honest for a replay: every record in the
        // file did become a record.
        "frames_seen": records,
        "rate_hz": summary["mean_rate_hz"],
        "capture_bytes": summary["capture_bytes"],
        "live_sent": records,
        "live_dropped": 0,
    });
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, doc.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn width_from(token: &str) -> Result<csiq::Width> {
    Ok(match token.to_ascii_lowercase().as_str() {
        "noht" | "20mhz" | "ht20" => csiq::Width::Ht20,
        "ht40-" | "ht40minus" => csiq::Width::Ht40Minus,
        "ht40+" | "ht40plus" | "40mhz" => csiq::Width::Ht40Plus,
        "80mhz" | "w80" => csiq::Width::W80,
        "160mhz" | "w160" => csiq::Width::W160,
        other => bail!("unknown width {other:?}"),
    })
}

/// Read every record out of a session artefact, whichever of the two it is.
fn load(path: &PathBuf, width: csiq::Width) -> Result<Vec<CsiRecord>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut out = Vec::new();

    if path.extension().is_some_and(|e| e == "csiq") {
        let mut c = csiq::container::Reader::new(reader).context("reading the csiq container")?;
        while let Some(r) = c.next_record().context("reading a csiq record")? {
            out.push(r);
        }
    } else {
        let mut rr = csiq::raw::RawReader::new(reader, width);
        while let Some(r) = rr.next_record().context("reading a raw record")? {
            out.push(r);
        }
    }
    Ok(out)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let records = load(&cli.path, width_from(&cli.width)?)?;
    if records.is_empty() {
        bail!("{} holds no records", cli.path.display());
    }

    // Recorded gaps, unwrapped across the FTM counter's ~13.4 s wrap.
    let mut unwrapper = csiq::FtmUnwrapper::new();
    let ticks: Vec<u64> = records.iter().map(|r| unwrapper.push(r.ftm)).collect();
    let gaps: Vec<Duration> = ticks
        .windows(2)
        .map(|w| {
            let s = csiq::ftm_to_seconds(w[1].saturating_sub(w[0])) / cli.speed.max(1e-6);
            // A negative or absurd gap means the counter did something the
            // unwrapper could not follow; pass it through as immediate rather
            // than sleeping for a day.
            Duration::from_secs_f64(s.clamp(0.0, 5.0))
        })
        .collect();

    let span: f64 = gaps.iter().map(Duration::as_secs_f64).sum();
    let empties = records
        .iter()
        .filter(|r| r.iq.is_empty() || r.iq.iter().all(|&v| v == 0))
        .count();
    eprintln!(
        "replaying {} — {} records over {:.1} s at {}x, {} ({:.1}%) with an all-zero payload",
        cli.path.display(),
        records.len(),
        span,
        cli.speed,
        empties,
        100.0 * empties as f64 / records.len() as f64,
    );

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(&cli.target)
        .with_context(|| format!("connecting to {}", cli.target))?;

    // The session's own sidecar, when a status document was asked for.
    let meta = match &cli.status {
        Some(_) => {
            let side = cli
                .path
                .parent()
                .map(|d| d.join("metadata.json"))
                .filter(|p| p.exists())
                .context("--status needs metadata.json beside the capture")?;
            let text = std::fs::read_to_string(&side)
                .with_context(|| format!("reading {}", side.display()))?;
            Some(serde_json::from_str::<serde_json::Value>(&text).context("parsing the sidecar")?)
        }
        None => None,
    };

    let mut seq: u32 = 0;
    let started = std::time::Instant::now();
    loop {
        // A fresh session uid per pass, so the console treats a loop as csid
        // having restarted rather than as one impossibly long session.
        let session_uid = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(1);

        for (i, rec) in records.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(gaps[i - 1]);
            }
            let dg = csiq::live::encode(session_uid, seq, rec);
            // A dropped datagram is the transport being honest, not an error:
            // the live path is best-effort by construction on the node too.
            let _ = sock.send(&dg);
            seq = seq.wrapping_add(1);

            // Once a second, as csid does.
            if let (Some(p), Some(m)) = (&cli.status, &meta) {
                if i % 32 == 0 {
                    let sent = i as u64 + 1;
                    let empty_so_far = records[..=i]
                        .iter()
                        .filter(|r| r.iq.is_empty() || r.iq.iter().all(|&v| v == 0))
                        .count() as u64;
                    publish_status(p, m, sent, empty_so_far, started.elapsed().as_secs());
                }
            }
        }

        if !cli.loop_forever {
            return Ok(());
        }
    }
}
