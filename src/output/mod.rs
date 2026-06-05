use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};

use crate::cli::{Args, Format};
use crate::zend::Frame;

#[cfg(feature = "pprof")]
mod pprof_sink;
#[cfg(feature = "pyroscope")]
mod pyroscope_sink;

pub struct SampleMeta {
    pub request_uri: Option<String>,
    pub request_method: Option<String>,
}

pub trait Sink: Send {
    fn write_sample(&mut self, frames: &[Frame], meta: &SampleMeta) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}

/// Build the output sink from parsed args. `--pyroscope-url` selects continuous
/// push export (sidecar mode); otherwise output is a file/stdout writer in the
/// chosen `--format`.
pub fn build_sink(args: &Args) -> Result<Box<dyn Sink>> {
    if args.pyroscope_url.is_some() {
        #[cfg(feature = "pyroscope")]
        return Ok(Box::new(pyroscope_sink::PyroscopeSink::new(
            pyroscope_config(args),
        )));
        #[cfg(not(feature = "pyroscope"))]
        anyhow::bail!("--pyroscope-url requires the `pyroscope` feature (default-on)");
    }

    let writer: Box<dyn Write + Send> = match &args.output {
        Some(path) => Box::new(BufWriter::new(
            File::create(path).with_context(|| format!("creating {}", path.display()))?,
        )),
        None => Box::new(BufWriter::new(io::stdout())),
    };
    Ok(make_sink(args.format, writer))
}

#[cfg(feature = "pyroscope")]
fn pyroscope_config(args: &Args) -> pyroscope_sink::PyroscopeConfig {
    let app = args.pyroscope_app.clone();
    let name = if args.pyroscope_label.is_empty() {
        app
    } else {
        format!("{app}{{{}}}", args.pyroscope_label.join(","))
    };
    pyroscope_sink::PyroscopeConfig {
        url: args
            .pyroscope_url
            .clone()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string(),
        name,
        auth_token: args.pyroscope_auth_token.clone(),
        tenant_id: args.pyroscope_tenant_id.clone(),
        push_interval: std::time::Duration::from_secs(args.push_interval_secs.max(1)),
    }
}

pub fn make_sink(format: Format, w: Box<dyn Write + Send>) -> Box<dyn Sink> {
    match format {
        Format::Stacks => Box::new(StacksSink { w }),
        Format::Folded => Box::new(FoldedSink {
            w,
            counts: HashMap::new(),
        }),
        #[cfg(feature = "pprof")]
        Format::Pprof => Box::new(pprof_sink::PprofSink::new(w)),
        #[cfg(not(feature = "pprof"))]
        Format::Pprof => {
            panic!("pprof output not compiled in; rebuild with `--features pprof` (default)")
        }
        #[cfg(feature = "tui")]
        Format::Top => unreachable!("Format::Top is dispatched before make_sink"),
        #[cfg(not(feature = "tui"))]
        Format::Top => {
            panic!("tui output not compiled in; rebuild with `--features tui` (default)")
        }
    }
}

fn render_frame(f: &Frame) -> String {
    let func = match &f.class {
        Some(c) => format!("{c}::{}", f.function),
        None => f.function.to_string(),
    };
    let file: &str = f.file.as_deref().unwrap_or("<unknown>");
    format!("{func} {file}:{}", f.line)
}

struct StacksSink {
    w: Box<dyn Write + Send>,
}

impl Sink for StacksSink {
    fn write_sample(&mut self, frames: &[Frame], meta: &SampleMeta) -> Result<()> {
        if let Some(uri) = &meta.request_uri {
            writeln!(self.w, "# request_uri = {uri}")?;
        }
        if let Some(m) = &meta.request_method {
            writeln!(self.w, "# request_method = {m}")?;
        }
        for (i, f) in frames.iter().enumerate() {
            writeln!(self.w, "{i} {}", render_frame(f))?;
        }
        writeln!(self.w)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.w.flush()?;
        Ok(())
    }
}

struct FoldedSink {
    w: Box<dyn Write + Send>,
    counts: HashMap<String, u64>,
}

impl Sink for FoldedSink {
    fn write_sample(&mut self, frames: &[Frame], _meta: &SampleMeta) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        // Folded stacks are root-first.
        let key = frames
            .iter()
            .rev()
            .map(|f| match &f.class {
                Some(c) => format!("{c}::{}", f.function),
                None => f.function.to_string(),
            })
            .collect::<Vec<_>>()
            .join(";");
        *self.counts.entry(key).or_insert(0) += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let mut entries: Vec<_> = self.counts.drain().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in entries {
            writeln!(self.w, "{k} {v}")?;
        }
        self.w.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(class: Option<&str>, function: &str, file: &str, line: u32) -> Frame {
        Frame {
            class: class.map(std::sync::Arc::from),
            function: std::sync::Arc::from(function),
            file: Some(std::sync::Arc::from(file)),
            line,
        }
    }

    fn empty_meta() -> SampleMeta {
        SampleMeta {
            request_uri: None,
            request_method: None,
        }
    }

    /// Drive a sink and return the bytes it wrote.
    fn drain<F>(format: Format, mut feed: F) -> Vec<u8>
    where
        F: FnMut(&mut dyn Sink) -> Result<()>,
    {
        // Buffer the output through an Arc<Mutex<Vec<u8>>> so we can read it
        // back after `finish` consumes the writer.
        struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer: Box<dyn Write + Send> = Box::new(SharedBuf(buf.clone()));
        let mut sink = make_sink(format, writer);
        feed(&mut *sink).unwrap();
        sink.finish().unwrap();
        drop(sink);
        buf.lock().unwrap().clone()
    }

    #[test]
    fn folded_sink_aggregates_and_orders_root_first() {
        let bytes = drain(Format::Folded, |sink| {
            // Two samples of the same stack, one different sample.
            let stack_a = [
                frame(Some("Worker"), "level3", "/t.php", 10),
                frame(Some("Worker"), "level2", "/t.php", 9),
                frame(Some("Worker"), "level1", "/t.php", 8),
            ];
            let stack_b = [
                frame(None, "usleep", "<internal>", 0),
                frame(Some("Worker"), "level3", "/t.php", 10),
            ];
            sink.write_sample(&stack_a, &empty_meta())?;
            sink.write_sample(&stack_a, &empty_meta())?;
            sink.write_sample(&stack_b, &empty_meta())?;
            Ok(())
        });
        let out = String::from_utf8(bytes).unwrap();

        // Folded format is root-first (reversed from leaf-first input) and
        // semicolon-joined, with the count after a space.
        assert!(out.contains("Worker::level1;Worker::level2;Worker::level3 2"));
        assert!(out.contains("Worker::level3;usleep 1"));
        // Sorted lexicographically: level1-rooted line comes first.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("Worker::level1"));
    }

    #[test]
    fn folded_sink_handles_empty_stacks() {
        let bytes = drain(Format::Folded, |sink| {
            sink.write_sample(&[], &empty_meta())?;
            Ok(())
        });
        assert!(bytes.is_empty());
    }
}
