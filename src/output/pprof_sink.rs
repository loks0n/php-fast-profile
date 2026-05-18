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

pub struct PprofSink {
    w: Option<Box<dyn Write + Send>>,
    strings: Vec<String>,
    string_idx: HashMap<String, i64>,
    functions: Vec<pprof::Function>,
    function_key: HashMap<(i64, i64), u64>,
    locations: Vec<pprof::Location>,
    location_key: HashMap<(u64, i64), u64>,
    samples: HashMap<Vec<u64>, i64>,
    started_nanos: i64,
}

impl PprofSink {
    pub fn new(w: Box<dyn Write + Send>) -> Self {
        let mut s = Self {
            w: Some(w),
            strings: Vec::new(),
            string_idx: HashMap::new(),
            functions: Vec::new(),
            function_key: HashMap::new(),
            locations: Vec::new(),
            location_key: HashMap::new(),
            samples: HashMap::new(),
            started_nanos: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
        };
        // pprof requires string_table[0] == "".
        s.intern("");
        s
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
}

impl Sink for PprofSink {
    fn write_sample(&mut self, frames: &[Frame], _meta: &SampleMeta) -> Result<()> {
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
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let samples_unit = self.intern("count");
        let samples_type = self.intern("samples");

        let samples = self
            .samples
            .drain()
            .map(|(loc_ids, count)| pprof::Sample {
                location_id: loc_ids,
                value: vec![count],
                label: vec![],
            })
            .collect();

        let now_nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        let profile = pprof::Profile {
            sample_type: vec![pprof::ValueType {
                r#type: samples_type,
                unit: samples_unit,
            }],
            sample: samples,
            mapping: vec![],
            location: std::mem::take(&mut self.locations),
            function: std::mem::take(&mut self.functions),
            string_table: std::mem::take(&mut self.strings),
            drop_frames: 0,
            keep_frames: 0,
            time_nanos: self.started_nanos,
            duration_nanos: now_nanos - self.started_nanos,
            period_type: None,
            period: 0,
            comment: vec![],
            default_sample_type: 0,
        };

        let mut buf = Vec::with_capacity(4096);
        profile.encode(&mut buf)?;

        let writer = self.w.take().expect("pprof finish called twice");
        let mut gz = GzEncoder::new(writer, Compression::default());
        gz.write_all(&buf)?;
        gz.finish()?.flush()?;
        Ok(())
    }
}
