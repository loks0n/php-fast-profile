use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use crate::zend::Frame;

use super::pprof_sink::PprofBuilder;
use super::{SampleMeta, Sink};

pub struct PyroscopeConfig {
    /// Base server URL, e.g. `http://pyroscope:4040` (no trailing slash).
    pub url: String,
    /// Application name including any `{label=value}` selector.
    pub name: String,
    pub auth_token: Option<String>,
    pub tenant_id: Option<String>,
    /// Extra request headers, applied after the built-in auth headers.
    pub headers: Vec<(String, String)>,
    pub push_interval: Duration,
}

pub struct PyroscopeSink {
    cfg: PyroscopeConfig,
    agent: ureq::Agent,
    builder: PprofBuilder,
    last_push: Instant,
}

impl PyroscopeSink {
    pub fn new(cfg: PyroscopeConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build();
        tracing::info!(
            "pyroscope export enabled: url={} name={} interval={:?}",
            cfg.url,
            cfg.name,
            cfg.push_interval
        );
        Self {
            cfg,
            agent,
            builder: PprofBuilder::new(),
            last_push: Instant::now(),
        }
    }

    /// Encode the current window and POST it to Pyroscope's `/ingest` endpoint.
    /// Transient failures are logged and swallowed — a server hiccup must never
    /// stop the profiler.
    fn push(&mut self) {
        if self.builder.is_empty() {
            self.last_push = Instant::now();
            return;
        }
        match self.try_push() {
            Ok(()) => tracing::debug!("pushed profile to pyroscope"),
            Err(e) => tracing::warn!("pyroscope push failed: {e:#}"),
        }
        self.last_push = Instant::now();
    }

    fn try_push(&mut self) -> Result<()> {
        // Grafana Pyroscope's pprof ingest reads the gzipped profile as the raw
        // request body (not multipart/form-data); the leading `--boundary` of a
        // multipart body parses as protobuf field 5 and is rejected.
        let (body, from_nanos, until_nanos) = self.builder.take_gzipped()?;

        let mut req = self
            .agent
            .post(&format!("{}/ingest", self.cfg.url))
            .query("name", &self.cfg.name)
            .query("from", &(from_nanos / 1_000_000_000).to_string())
            .query("until", &(until_nanos / 1_000_000_000).to_string())
            .query("format", "pprof")
            .query("spyName", "pfp")
            .set("Content-Type", "application/octet-stream");
        if let Some(token) = &self.cfg.auth_token {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        if let Some(tenant) = &self.cfg.tenant_id {
            req = req.set("X-Scope-OrgID", tenant);
        }
        // Applied last so an explicit --pyroscope-header can override the above.
        for (name, value) in &self.cfg.headers {
            req = req.set(name, value);
        }

        req.send_bytes(&body)
            .context("POST /ingest")
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e))
    }
}

impl Sink for PyroscopeSink {
    fn write_sample(&mut self, frames: &[Frame], _meta: &SampleMeta) -> Result<()> {
        self.builder.add(frames);
        if self.last_push.elapsed() >= self.cfg.push_interval {
            self.push();
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.push();
        Ok(())
    }
}
