//! The seam the unit tests cannot cover: a node's probe output crossing the
//! ssh boundary as JSON and arriving at the cockpit's gate arithmetic.
//!
//! Everything on either side of that boundary is unit-tested. What is *not*
//! tested there is the boundary itself — that `ProbeReport` survives a
//! serialise/deserialise round trip with the arrival series intact, and that
//! the gates then decide the same way they would have decided on the node. A
//! silent loss here (a renamed field defaulting to empty, an `f64` series
//! truncated) would show up as "every gate is UNKNOWN" at the bench, with no
//! error anywhere.
//!
//! This test builds a synthetic session spool, probes it, ships the report
//! through JSON exactly as `csid fleet status` does, and adjudicates.

use std::path::{Path, PathBuf};

use csid::fleet::gates::{self, GateVerdict};
use csid::fleet::health::{Budgets, State};
use csid::fleet::probe::{self, ProbeOptions, ProbeReport};

/// One raw frame in the framing `DurableSink` writes.
fn frame(ftm: u32, unix_ts_ns: u64, ntone: u16, mac: [u8; 6]) -> Vec<u8> {
    let mut hdr = vec![0u8; 272];
    hdr[8..12].copy_from_slice(&ftm.to_le_bytes());
    hdr[46] = 1;
    hdr[47] = 1;
    hdr[52..54].copy_from_slice(&ntone.to_le_bytes());
    hdr[68..74].copy_from_slice(&mac);
    hdr[208..216].copy_from_slice(&unix_ts_ns.to_le_bytes());
    let csi = vec![0u8; ntone as usize * 2 * 2];

    let msg_len = (4 + hdr.len() + 4 + csi.len()) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&msg_len.to_be_bytes());
    out.extend_from_slice(&(hdr.len() as u32).to_be_bytes());
    out.extend_from_slice(&hdr);
    out.extend_from_slice(&(csi.len() as u32).to_be_bytes());
    out.extend_from_slice(&csi);
    out
}

/// A spool holding one capturing session at `rate_hz` for `seconds`, ending now.
fn spool(tag: &str, host: &str, rate_hz: f64, jitter: f64, seconds: f64) -> PathBuf {
    let root = std::env::temp_dir().join(format!("csid-seam-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join(format!("{host}_lab-anchor_20260810-101500"));
    std::fs::create_dir_all(&dir).unwrap();

    let now = csid::util::now_unix_ns();
    let n = (rate_hz * seconds) as usize;
    let mac = [0xef, 0xbe, 0xad, 0xde, 0xad, 0xde];
    let mut bytes = Vec::new();
    for i in 0..n {
        // Deterministic, bounded wobble so the CV is a chosen quantity.
        let wobble = jitter * (((i * 7919) % 101) as f64 / 100.0 - 0.5);
        let t = (i as f64 + wobble) / rate_hz;
        let ftm = (t * 320_000_000.0) as u32;
        let ts = now - ((seconds - t) * 1e9) as u64;
        bytes.extend_from_slice(&frame(ftm, ts, 52, mac));
    }
    std::fs::write(dir.join("capture.raw"), bytes).unwrap();

    let sidecar = serde_json::json!({
        "schema": "csid-session/1",
        "session_id": format!("{host}_lab-anchor_20260810-101500"),
        "experiment": "lab-anchor",
        "tag": "seam-test",
        "radio": {
            "interface": "wlp1s0", "monitor": "wlp1s0mon0", "band": "2.4",
            "channel": 11, "control_freq_mhz": 2462, "center_freq_mhz": null,
            "width": "HT20", "interval_us": 0, "mac_filter": []
        },
        "environment": { "hostname": host, "csid_version": "0.1.0" },
        "started_at": "2026-08-10T10:15:00Z",
        "ended_at": null,
        "status": "capturing",
        "summary": null
    });
    std::fs::write(
        dir.join("metadata.json"),
        serde_json::to_string_pretty(&sidecar).unwrap(),
    )
    .unwrap();
    root
}

/// Probe a spool and ship the report through JSON, exactly as the cockpit does.
fn probe_over_the_wire(root: &Path, window_s: f64) -> ProbeReport {
    let local = probe::probe(&ProbeOptions {
        spool: root.to_path_buf(),
        window_s,
        ..ProbeOptions::default()
    })
    .expect("the probe must not fail on a readable spool");

    // This is the ssh boundary: stdout on the node, stdin on the laptop.
    let wire = serde_json::to_string(&local).expect("the report must serialise");
    serde_json::from_str::<ProbeReport>(&wire).expect("the cockpit must parse it back")
}

#[test]
fn a_healthy_link_survives_the_wire_and_passes_both_registered_gates() {
    let root = spool("good", "monad04", 122.5, 0.3, 40.0);
    let report = probe_over_the_wire(&root, 30.0);

    assert_eq!(report.schema, "csid-probe/1");
    assert_eq!(report.health.host, "monad04");
    assert!(report.health.capture_alive, "a fresh capture is alive");

    let scope = report
        .health
        .scope
        .as_ref()
        .expect("a scope crossed the wire");
    assert_eq!(scope.src_mac, "ef:be:ad:de:ad:de");
    assert!(scope.scoped_records > 3000, "{scope:?}");

    // The arrival series is what the gates run on. If it were lost, both gates
    // would silently become UNKNOWN.
    assert!(
        report.arrival_s.len() > 3000,
        "the arrival series must cross the wire intact, got {}",
        report.arrival_s.len()
    );

    let g1 = gates::g1_delivered_rate("monad04", &report.arrival_s, gates::G1_FLOOR_HZ);
    assert_eq!(g1.verdict, GateVerdict::Pass, "{}", g1.render(1));
    let ci = g1.ci.unwrap();
    assert!(
        (ci.point - 122.5).abs() < 2.0,
        "the rate must survive the round trip: {ci:?}"
    );
    assert!(ci.lo >= gates::G1_FLOOR_HZ, "{ci:?}");

    let g2 = gates::g2_interarrival_cv("monad04", &report.arrival_s, gates::G2_CV_CEILING);
    assert_eq!(g2.verdict, GateVerdict::Pass, "{}", g2.render(3));
    assert!(g2.ci.unwrap().hi < gates::G2_CV_CEILING);

    std::fs::remove_dir_all(&root).ok();
}

/// The 2.4 GHz illuminated baseline from the readiness audit: 21.2 Hz, nowhere
/// near the floor. The gate must fail *and* carry the Day-0 abort text, which
/// is the thing the operator acts on.
#[test]
fn an_under_rate_link_fails_g1_across_the_wire_with_the_abort_text() {
    let root = spool("slow", "monad07", 21.2, 0.2, 40.0);
    let report = probe_over_the_wire(&root, 30.0);

    let g1 = gates::g1_delivered_rate("monad07", &report.arrival_s, gates::G1_FLOOR_HZ);
    assert_eq!(g1.verdict, GateVerdict::Fail);
    let text = g1.render(1);
    assert!(text.contains("STOP AND FIX"), "{text}");
    assert!(text.contains("95% CI lower bound ="), "{text}");

    // And the node grades as a hard failure at glance level too.
    let mut health = report.health.clone();
    assert_eq!(health.grade(&Budgets::default()), State::Fail);

    std::fs::remove_dir_all(&root).ok();
}

/// A node whose spool exists but holds nothing is UNKNOWN, not a healthy node
/// reporting 0 Hz. The distinction is the whole cockpit contract.
#[test]
fn an_empty_spool_crosses_the_wire_as_unknown_not_as_zero() {
    let root = std::env::temp_dir().join(format!("csid-seam-empty-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let report = probe_over_the_wire(&root, 30.0);

    assert!(report.health.session_id.is_none());
    assert!(report.arrival_s.is_empty());
    assert!(report.health.delivered_hz.is_none());

    let mut health = report.health.clone();
    assert_eq!(health.grade(&Budgets::default()), State::Unknown);

    let g1 = gates::g1_delivered_rate("monad09", &report.arrival_s, gates::G1_FLOOR_HZ);
    assert_eq!(
        g1.verdict,
        GateVerdict::Unknown,
        "no records must not read as a passed gate"
    );
    assert!(g1.ci.is_none());

    std::fs::remove_dir_all(&root).ok();
}

/// A marker written on a node must round-trip through the JSON the cockpit
/// parses off stdout, with the app-shared field names and the nanosecond stamp
/// intact — that stamp is the boundary of record.
#[test]
fn a_marker_survives_the_wire_with_its_nanosecond_stamp_and_app_field_names() {
    let root = spool("marker", "monad04", 122.5, 0.3, 20.0);
    let (dir, sidecar) = probe::newest_session(&root).expect("a session to mark");

    let m = csid::marker::Marker::now(
        sidecar.environment.hostname.clone().unwrap(),
        sidecar.session_id.clone(),
        "S1-ZA-CYC-03",
        "ZONE-A",
        "cycling",
        "start",
        Some(4),
        Some("walking".into()),
        None,
    );
    csid::marker::append(&dir, &m).expect("the marker must land in the session");

    // What the cockpit reads back off stdout.
    let back: csid::marker::Marker = serde_json::from_str(&m.to_line().unwrap()).unwrap();
    assert_eq!(back, m);
    assert!(
        back.unix_ts_ns > 1_700_000_000_000_000_000,
        "nanoseconds, not milliseconds: {}",
        back.unix_ts_ns
    );
    assert_eq!(back.source, "fleet");

    // And it is on disk, one line, in the session the capture is in.
    let log = std::fs::read_to_string(dir.join(csid::marker::MARKER_FILE)).unwrap();
    assert_eq!(log.lines().count(), 1);
    let on_disk: csid::marker::Marker = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(on_disk.block_id, "S1-ZA-CYC-03");
    assert_eq!(on_disk.session_id, sidecar.session_id);

    std::fs::remove_dir_all(&root).ok();
}
