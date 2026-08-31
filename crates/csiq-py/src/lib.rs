//! PyO3 bindings for the CSIQ reader — the optional `csiq[fast]` accelerator.
//!
//! # What this is, and what it is not
//!
//! This is a **third** implementation of one format's reader, behind an API the
//! pure-Python package already defines. The spec's own rule is that when two
//! implementations disagree, the document is authoritative and both are bugs.
//! A third raises that cost, so two things hold it honest:
//!
//! * `tests/test_backend_parity.py` decodes every fixture with both backends and
//!   requires byte-identical output. It is not advisory.
//! * The pure-Python path is never removed. A platform with no wheel and no Rust
//!   toolchain reads every file, more slowly.
//!
//! # It is not justified by lake throughput
//!
//! Measured 2026-08-31 before this crate existed: the Python parser runs at
//! 64,104 rec/s (34.3 MB/s), so a 48 GB pass costs 0.39 h of decode against a
//! 7 h ingest budget, and 39 % of the per-record cost is NumPy rather than the
//! TLV walk. This crate exists for interactive latency and for the published
//! package, not to rescue the nightly job.
//!
//! # Parity is on the data, not on the object
//!
//! Every record attribute carries the same value and the same Python type as the
//! pure reader produces, including `iq` as a list of `int`. That list is the
//! expensive part, so `iq_bytes` is offered beside it as a zero-copy
//! `bytes` a caller can hand straight to `numpy.frombuffer`. The pure reader
//! exposes the same property, so using it does not fork the API.

use pyo3::exceptions::{PyOSError, PyStopIteration, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyModule};

use csiq::record::{Bandwidth, Modulation, Width};
use csiq::{CsiRecord, Reader};

use std::fs::File;
use std::io::{BufReader, Read};

/// Width code -> the label the Python reader uses. Kept as an explicit table so
/// the two implementations cannot drift through a `Display` impl changing.
fn width_label(w: Width) -> String {
    match w {
        Width::Noht => "NOHT".into(),
        Width::Ht20 => "HT20".into(),
        Width::Ht40Minus => "HT40-".into(),
        Width::Ht40Plus => "HT40+".into(),
        Width::W80 => "80MHz".into(),
        Width::W160 => "160MHz".into(),
        Width::W320 => "320MHz".into(),
        // `Unknown(0)` is not a decoded code — code 0 IS NOHT. It is the Rust
        // reader's "the WIDTH field was absent" placeholder (`tlv.rs`), where
        // the Python reader uses its dataclass default of "NOHT". Same fact,
        // two renderings, and the parity test caught the difference on its
        // first run. Rendering it the reference reader's way here keeps one
        // API; which sentinel is RIGHT is a spec question, and the honest
        // answer is probably neither — absence should be absence.
        Width::Unknown(0) => "NOHT".into(),
        Width::Unknown(code) => format!("unknown({code})"),
    }
}

fn modulation_label(m: Modulation) -> String {
    match m {
        Modulation::Cck => "cck".into(),
        Modulation::LegacyOfdm => "legacy_ofdm".into(),
        Modulation::Ht => "ht".into(),
        Modulation::Vht => "vht".into(),
        Modulation::He => "he".into(),
        Modulation::Eht => "eht".into(),
        Modulation::Unknown(code) => format!("unknown({code})"),
    }
}

/// Bandwidth in MHz, or `None` for a code this build does not know.
///
/// An unrecognised code is carried verbatim and **must not** be coerced to
/// 20 MHz, which is why `Unknown` maps to `None` rather than to a default.
fn bandwidth_mhz(b: Bandwidth) -> Option<u16> {
    match b {
        Bandwidth::W20 => Some(20),
        Bandwidth::W40 => Some(40),
        Bandwidth::W80 => Some(80),
        Bandwidth::W160 => Some(160),
        Bandwidth::W320 => Some(320),
        Bandwidth::Unknown(_) => None,
    }
}

/// One decoded record, with the pure reader's attribute names and types.
#[pyclass(name = "FastRecord", module = "csiq_fast")]
pub struct FastRecord {
    #[pyo3(get)]
    ftm: u32,
    #[pyo3(get)]
    us: u32,
    #[pyo3(get)]
    unix_ts_ns: u64,
    #[pyo3(get)]
    rnf: u32,
    /// `(modulation, mcs, nss)` or `None`. The Python layer rebuilds `PhyLabel`.
    #[pyo3(get)]
    phy: Option<(String, u8, u8)>,
    #[pyo3(get)]
    seq: u8,
    #[pyo3(get)]
    nrx: u8,
    #[pyo3(get)]
    ntx: u8,
    #[pyo3(get)]
    ntone: u16,
    #[pyo3(get)]
    rssi: Vec<i16>,
    #[pyo3(get)]
    channel: u32,
    #[pyo3(get)]
    width: String,
    /// `(bandwidth_mhz, antenna_sel)` or `None`. **Absent is not 20 MHz.**
    #[pyo3(get)]
    bw_antsel: Option<(Option<u16>, u8)>,
    /// `None` marks the node's own transmission looped back, not a missing clock.
    #[pyo3(get)]
    mono_us: Option<u64>,
    src_mac: [u8; 6],
    vendor_hdr: Option<Vec<u8>>,
    iq: Vec<i16>,
    node: Vec<(String, i64)>,
}

#[pymethods]
impl FastRecord {
    #[getter]
    fn src_mac<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.src_mac)
    }

    #[getter]
    fn vendor_hdr<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.vendor_hdr.as_ref().map(|b| PyBytes::new(py, b))
    }

    /// Sparse node/host state. An absent key is absence, never a zero.
    #[getter]
    fn node<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (key, value) in &self.node {
            dict.set_item(key, value)?;
        }
        Ok(dict)
    }

    /// Interleaved I/Q as a list of `int`, exactly as the pure reader returns.
    #[getter]
    fn iq<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(py, self.iq.iter().copied())
    }

    /// The same coefficients as little-endian `i16` bytes, zero-copy.
    ///
    /// Hand this to `numpy.frombuffer(buf, dtype="<i2")` and no Python integer
    /// is ever built. That list is the expensive half of decoding a record.
    #[getter]
    fn iq_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let mut out = Vec::with_capacity(self.iq.len() * 2);
        for value in &self.iq {
            out.extend_from_slice(&value.to_le_bytes());
        }
        PyBytes::new(py, &out)
    }

    fn __repr__(&self) -> String {
        format!(
            "<FastRecord ftm={} ntone={} nrx={} ntx={}>",
            self.ftm, self.ntone, self.nrx, self.ntx
        )
    }
}

impl FastRecord {
    fn from_record(r: CsiRecord) -> Self {
        let mut node: Vec<(String, i64)> = Vec::new();
        if let Some(v) = r.node.temp_mc {
            node.push(("temp_mc".into(), v as i64));
        }
        if let Some(v) = r.node.throttle_flags {
            node.push(("throttle_flags".into(), v as i64));
        }
        if let Some(v) = r.node.spool_free_bytes {
            node.push(("spool_free_bytes".into(), v as i64));
        }
        if let Some(v) = r.node.load_m {
            node.push(("load_m".into(), v as i64));
        }
        if let Some(v) = r.node.nic_temp_c {
            node.push(("nic_temp_c".into(), v as i64));
        }
        Self {
            ftm: r.ftm,
            us: r.us,
            unix_ts_ns: r.unix_ts_ns,
            rnf: r.rnf,
            phy: r
                .phy
                .map(|p| (modulation_label(p.modulation), p.mcs, p.nss)),
            seq: r.seq,
            nrx: r.nrx,
            ntx: r.ntx,
            ntone: r.ntone,
            rssi: r.rssi,
            channel: r.channel,
            width: width_label(r.width),
            bw_antsel: r
                .bw_antsel
                .map(|b| (bandwidth_mhz(b.bandwidth), b.antenna_sel)),
            mono_us: r.mono_us,
            src_mac: r.src_mac,
            vendor_hdr: r.vendor_hdr,
            iq: r.iq,
            node,
        }
    }
}

/// Lazy record iterator over one container.
///
/// `unsendable`: it owns an open file handle, so it is bound to the thread that
/// created it. Sharing a half-consumed reader across threads would interleave
/// reads and desynchronise the stream, which the `0xA1` tag would then catch as
/// corruption — a confusing way to learn about a threading mistake.
#[pyclass(name = "FastRecords", module = "csiq_fast", unsendable)]
pub struct FastRecords {
    reader: Option<Reader<BufReader<Box<dyn Read + Send>>>>,
}

#[pymethods]
impl FastRecords {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> PyResult<FastRecord> {
        let reader = match self.reader.as_mut() {
            Some(r) => r,
            None => return Err(PyStopIteration::new_err(())),
        };
        match reader.next_record() {
            Ok(Some(record)) => Ok(FastRecord::from_record(record)),
            Ok(None) => {
                self.reader = None;
                Err(PyStopIteration::new_err(()))
            }
            // A desync or a truncation is a real error and must stop the
            // iteration loudly. The Python layer maps it onto the typed
            // hierarchy so both backends raise the same class.
            Err(err) => Err(PyValueError::new_err(err.to_string())),
        }
    }
}

/// Open a `.csiq` container. Returns `(session_json_or_None, records)`.
///
/// The session block comes back as a JSON **string** rather than a dict: parsing
/// it with Python's own `json` is what guarantees both backends produce the same
/// object for the same bytes, down to integer and float handling.
#[pyfunction]
fn read_csiq(path: &str) -> PyResult<(Option<String>, FastRecords)> {
    let file = File::open(path).map_err(|e| PyOSError::new_err(e.to_string()))?;
    let stream: Box<dyn Read + Send> = Box::new(file);
    let reader =
        Reader::new(BufReader::new(stream)).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let session = reader
        .session()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    Ok((
        session,
        FastRecords {
            reader: Some(reader),
        },
    ))
}

/// The container format version this build reads. Always 1.
#[pyfunction]
fn format_version() -> u16 {
    csiq::FORMAT_VERSION
}

#[pymodule]
fn csiq_fast(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<FastRecord>()?;
    m.add_class::<FastRecords>()?;
    m.add_function(wrap_pyfunction!(read_csiq, m)?)?;
    m.add_function(wrap_pyfunction!(format_version, m)?)?;
    m.add("__doc__", "PyO3 accelerator for the CSIQ reader (csiq[fast]).")?;
    Ok(())
}
