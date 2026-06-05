use anyhow::Result;
use flate2::Compression;
use flate2::write::GzEncoder;
use prost::Message;
use std::collections::HashMap;
use std::io::Write;
use std::time::SystemTime;

use crate::zend::Frame;

use super::{SampleMeta, Sink};

mod pprof {
    include!(concat!(env!("OUT_DIR"), "/perftools.profiles.rs"));
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Accumulates samples into a pprof v3 profile and encodes them as gzipped
/// protobuf. Shared by `PprofSink` (write once at shutdown) and the Pyroscope
/// sink (encode + reset on every push window).
pub struct PprofBuilder {
    strings: Vec<String>,
    string_idx: HashMap<String, i64>,
    functions: Vec<pprof::Function>,
    function_key: HashMap<(i64, i64), u64>,
    locations: Vec<pprof::Location>,
    location_key: HashMap<(u64, i64), u64>,
    samples: HashMap<Vec<u64>, i64>,
    /// Start of the current accumulation window, unix nanos.
    window_start_nanos: i64,
}

impl PprofBuilder {
    pub fn new() -> Self {
        let mut b = Self {
            strings: Vec::new(),
            string_idx: HashMap::new(),
            functions: Vec::new(),
            function_key: HashMap::new(),
            locations: Vec::new(),
            location_key: HashMap::new(),
            samples: HashMap::new(),
            window_start_nanos: now_nanos(),
        };
        // pprof requires string_table[0] == "".
        b.intern("");
        b
    }

    // Only the Pyroscope sink consults this before a push.
    #[cfg_attr(not(feature = "pyroscope"), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    fn intern(&mut self, s: &str) -> i64 {
        if let Some(&i) = self.string_idx.get(s) {
            return i;
        }
        let i = self.strings.len() as i64;
        self.strings.push(s.to_string());
        self.string_idx.insert(s.to_string(), i);
        i
    }

    fn function_id(&mut self, name: i64, filename: i64) -> u64 {
        if let Some(&id) = self.function_key.get(&(name, filename)) {
            return id;
        }
        let id = (self.functions.len() + 1) as u64;
        self.functions.push(pprof::Function {
            id,
            name,
            system_name: name,
            filename,
            start_line: 0,
        });
        self.function_key.insert((name, filename), id);
        id
    }

    fn location_id(&mut self, function_id: u64, line: i64) -> u64 {
        if let Some(&id) = self.location_key.get(&(function_id, line)) {
            return id;
        }
        let id = (self.locations.len() + 1) as u64;
        self.locations.push(pprof::Location {
            id,
            mapping_id: 0,
            address: 0,
            line: vec![pprof::Line { function_id, line }],
            is_folded: false,
        });
        self.location_key.insert((function_id, line), id);
        id
    }

    /// Fold one captured stack into the aggregate.
    pub fn add(&mut self, frames: &[Frame]) {
        let mut location_ids = Vec::with_capacity(frames.len());
        for f in frames {
            let name = match &f.class {
                Some(c) => format!("{c}::{}", f.function),
                None => f.function.to_string(),
            };
            let name_idx = self.intern(&name);
            let file_idx = self.intern(f.file.as_deref().unwrap_or(""));
            let fid = self.function_id(name_idx, file_idx);
            let lid = self.location_id(fid, f.line as i64);
            location_ids.push(lid);
        }
        *self.samples.entry(location_ids).or_insert(0) += 1;
    }

    /// Encode the current window to gzipped pprof and **reset** the builder for
    /// the next window. Returns `(gzipped_bytes, window_start_nanos, window_end_nanos)`.
    pub fn take_gzipped(&mut self) -> Result<(Vec<u8>, i64, i64)> {
        let samples_unit = self.intern("count");
        let samples_type = self.intern("samples");

        let sample = self
            .samples
            .drain()
            .map(|(loc_ids, count)| pprof::Sample {
                location_id: loc_ids,
                value: vec![count],
                label: vec![],
            })
            .collect();

        let start = self.window_start_nanos;
        let end = now_nanos();

        let profile = pprof::Profile {
            sample_type: vec![pprof::ValueType {
                r#type: samples_type,
                unit: samples_unit,
            }],
            sample,
            mapping: vec![],
            location: std::mem::take(&mut self.locations),
            function: std::mem::take(&mut self.functions),
            string_table: std::mem::take(&mut self.strings),
            drop_frames: 0,
            keep_frames: 0,
            time_nanos: start,
            duration_nanos: (end - start).max(0),
            period_type: None,
            period: 0,
            comment: vec![],
            default_sample_type: 0,
        };

        let mut buf = Vec::with_capacity(4096);
        profile.encode(&mut buf)?;

        let mut gz = GzEncoder::new(Vec::with_capacity(buf.len() / 2), Compression::default());
        gz.write_all(&buf)?;
        let gzipped = gz.finish()?;

        // Reset interning state for the next window.
        self.string_idx.clear();
        self.function_key.clear();
        self.location_key.clear();
        self.intern("");
        self.window_start_nanos = end;

        Ok((gzipped, start, end))
    }
}

pub struct PprofSink {
    w: Option<Box<dyn Write + Send>>,
    builder: PprofBuilder,
}

impl PprofSink {
    pub fn new(w: Box<dyn Write + Send>) -> Self {
        Self {
            w: Some(w),
            builder: PprofBuilder::new(),
        }
    }
}

impl Sink for PprofSink {
    fn write_sample(&mut self, frames: &[Frame], _meta: &SampleMeta) -> Result<()> {
        self.builder.add(frames);
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let (bytes, _, _) = self.builder.take_gzipped()?;
        let mut writer = self.w.take().expect("pprof finish called twice");
        writer.write_all(&bytes)?;
        writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn frame(function: &str) -> Frame {
        Frame {
            class: None,
            function: Arc::from(function),
            file: Some(Arc::from("/t.php")),
            line: 1,
        }
    }

    #[test]
    fn take_gzipped_emits_gzip_and_resets() {
        let mut b = PprofBuilder::new();
        assert!(b.is_empty());
        b.add(&[frame("a"), frame("b")]);
        assert!(!b.is_empty());

        let (bytes, start, end) = b.take_gzipped().unwrap();
        // gzip magic.
        assert_eq!(&bytes[..2], &[0x1f, 0x8b]);
        assert!(end >= start);
        // Window is reset: no samples carried over, and the window advances.
        assert!(b.is_empty());
        assert_eq!(b.window_start_nanos, end);

        // A second window still produces a valid, independent profile.
        b.add(&[frame("c")]);
        let (bytes2, _, _) = b.take_gzipped().unwrap();
        assert_eq!(&bytes2[..2], &[0x1f, 0x8b]);
    }
}
