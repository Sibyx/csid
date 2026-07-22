//! The CSIQ file container: a fixed header carrying an embedded session block
//! (the capture metadata / sidecar), followed by length-framed records.
//!
//! ```text
//! FileHeader
//!   magic       "CSIQ"            4 bytes
//!   version     u16 LE            = 1
//!   flags       u16 LE            bit0 = session block present
//!   session_len u32 LE
//!   session     UTF-8 JSON        session_len bytes  (opaque to this crate)
//! Record*  (until EOF)
//!   tag         u8                = 0xA1
//!   len         u32 LE            payload length
//!   payload     TLV bytes         (see `tlv`)
//! ```

use std::io::{self, Read, Write};

use serde_json::Value;

use crate::error::{CsiqError, Result};
use crate::record::CsiRecord;
use crate::tlv;
use crate::{FORMAT_VERSION, MAGIC, RECORD_TAG};

const FLAG_SESSION: u16 = 0x0001;

/// Streaming writer for a `.csiq` file.
pub struct Writer<W: Write> {
    inner: W,
    records: u64,
}

impl<W: Write> Writer<W> {
    /// Write the file header (with an optional session-metadata JSON value) and
    /// return a writer ready to append records.
    pub fn new(mut inner: W, session: Option<&Value>) -> Result<Self> {
        inner.write_all(&MAGIC)?;
        inner.write_all(&FORMAT_VERSION.to_le_bytes())?;
        let (flags, body) = match session {
            Some(v) => (FLAG_SESSION, serde_json::to_vec(v)?),
            None => (0u16, Vec::new()),
        };
        inner.write_all(&flags.to_le_bytes())?;
        inner.write_all(&(body.len() as u32).to_le_bytes())?;
        inner.write_all(&body)?;
        Ok(Writer { inner, records: 0 })
    }

    /// Append one record.
    pub fn write_record(&mut self, r: &CsiRecord) -> Result<()> {
        let payload = tlv::encode_payload(r);
        self.inner.write_all(&[RECORD_TAG])?;
        self.inner
            .write_all(&(payload.len() as u32).to_le_bytes())?;
        self.inner.write_all(&payload)?;
        self.records += 1;
        Ok(())
    }

    /// Number of records written so far.
    pub fn record_count(&self) -> u64 {
        self.records
    }

    /// Flush and return the underlying writer.
    pub fn finish(mut self) -> Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// Streaming reader for a `.csiq` file.
pub struct Reader<R: Read> {
    inner: R,
    session: Option<Value>,
}

impl<R: Read> Reader<R> {
    /// Read and validate the file header, decoding the embedded session block.
    pub fn new(mut inner: R) -> Result<Self> {
        let mut magic = [0u8; 4];
        inner.read_exact(&mut magic)?;
        if magic != MAGIC {
            return Err(CsiqError::BadMagic {
                expected: MAGIC,
                found: magic,
            });
        }
        let version = read_u16(&mut inner)?;
        if version != FORMAT_VERSION {
            return Err(CsiqError::UnsupportedVersion(version));
        }
        let flags = read_u16(&mut inner)?;
        let session_len = read_u32(&mut inner)? as usize;
        let session = if flags & FLAG_SESSION != 0 {
            let mut body = vec![0u8; session_len];
            inner.read_exact(&mut body)?;
            Some(serde_json::from_slice(&body)?)
        } else {
            // Skip any bytes even if the flag is unset but a length was given.
            if session_len > 0 {
                io::copy(&mut (&mut inner).take(session_len as u64), &mut io::sink())?;
            }
            None
        };
        Ok(Reader { inner, session })
    }

    /// The embedded session metadata, if present.
    pub fn session(&self) -> Option<&Value> {
        self.session.as_ref()
    }

    /// Read the next record, or `Ok(None)` at clean end of file.
    pub fn next_record(&mut self) -> Result<Option<CsiRecord>> {
        let mut tag = [0u8; 1];
        match self.inner.read_exact(&mut tag) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        if tag[0] != RECORD_TAG {
            return Err(CsiqError::Malformed("bad record tag (stream desync)"));
        }
        let len = read_u32(&mut self.inner)? as usize;
        let mut payload = vec![0u8; len];
        self.inner.read_exact(&mut payload)?;
        Ok(Some(tlv::decode_payload(&payload)?))
    }
}

impl<R: Read> Iterator for Reader<R> {
    type Item = Result<CsiRecord>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

fn read_u16<R: Read>(r: &mut R) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
