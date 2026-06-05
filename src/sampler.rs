use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::Args;
use crate::discover::{self, ProcessFilter};
use crate::output::{self, SampleMeta, Sink};
use crate::target::{AttachOptions, Target};
use crate::zend::Frame;

/// Per-worker thread stack. Sample loop uses ~4 KB; 256 KB is plenty and
/// avoids the default 2 MB/thread that dominates RSS in multi-PID mode.
const WORKER_STACK_SIZE: usize = 256 * 1024;

pub struct Sampler {
    target: Target,
    sink: Box<dyn Sink>,
    capture_request_info: bool,
    max_depth: usize,
}

impl Sampler {
    pub fn attach(pid: i32, args: &Args) -> Result<Self> {
        let target = Target::attach(
            pid,
            AttachOptions {
                executor_globals: args.executor_globals,
                php_version_addr: None,
                php_version_string: args.php_version.as_deref(),
            },
        )?;
        tracing::info!(
            "attached pid={} php_version={} layout={} exe={}",
            pid,
            target.php_version,
            target.layout.label,
            target.exe
        );

        let sink = output::build_sink(args)?;

        Ok(Self {
            target,
            sink,
            capture_request_info: args.request_info,
            max_depth: args.max_depth,
        })
    }

    pub fn run(&mut self, interval: Duration, duration: Option<Duration>) -> Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = Arc::clone(&stop);
            ctrlc_like::install(move || stop.store(true, Ordering::SeqCst))?;
        }

        let start = Instant::now();
        let mut next = Instant::now();
        let mut taken: u64 = 0;
        let mut errors: u64 = 0;

        while !stop.load(Ordering::SeqCst) {
            if let Some(d) = duration
                && start.elapsed() >= d
            {
                break;
            }

            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            }
            next += interval;
            // If we fell badly behind, snap forward rather than sampling in a
            // tight catch-up loop.
            if Instant::now() > next + interval * 4 {
                next = Instant::now() + interval;
            }

            match self.sample_once() {
                Ok(_) => taken += 1,
                Err(e) => {
                    errors += 1;
                    tracing::debug!("sample failed: {e:#}");
                    // If the target is gone, stop.
                    if !std::path::Path::new(&format!("/proc/{}/status", self.target.pid)).exists()
                    {
                        tracing::info!("target pid {} exited", self.target.pid);
                        break;
                    }
                }
            }
        }

        self.sink.finish()?;
        tracing::info!("done: {taken} samples, {errors} errors");
        Ok(())
    }

    fn sample_once(&mut self) -> Result<()> {
        let frames = self.target.capture_stack(self.max_depth)?;
        if frames.is_empty() {
            return Ok(());
        }
        let meta = if self.capture_request_info {
            SampleMeta {
                request_uri: self.target.request_var("REQUEST_URI"),
                request_method: self.target.request_var("REQUEST_METHOD"),
            }
        } else {
            SampleMeta {
                request_uri: None,
                request_method: None,
            }
        };
        self.sink.write_sample(&frames, &meta)
    }
}

/// Live-TUI mode. Reuses the same PID-discovery + per-PID sampling threads
/// as `run_multi`, but routes frames into the TUI instead of an output sink.
#[cfg(feature = "tui")]
pub fn run_top(args: &Args, interval: Duration, duration: Option<Duration>) -> Result<()> {
    use crate::tui;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc_like::install(move || stop.store(true, Ordering::SeqCst))?;
    }

    let (frame_tx, frame_rx) = mpsc::channel::<Vec<Frame>>();

    // Spawn sampling thread(s). For single-PID we still go through a thread
    // so the TUI redraws on the main thread without blocking.
    let max_depth = args.max_depth;
    let executor_globals_override = args.executor_globals;
    let php_version_string = args.php_version.clone();

    let php_version_string = Arc::new(php_version_string);
    let spawn_sampler = |pid: i32, tx: mpsc::Sender<Vec<Frame>>, stop: Arc<AtomicBool>| {
        let php_version_string = Arc::clone(&php_version_string);
        thread::Builder::new()
            .name(format!("pfp-top-{pid}"))
            .stack_size(WORKER_STACK_SIZE)
            .spawn(move || {
                let opts = AttachOptions {
                    executor_globals: executor_globals_override,
                    php_version_addr: None,
                    php_version_string: php_version_string.as_deref(),
                };
                let mut target = match Target::attach(pid, opts) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("attach pid={pid} failed: {e:#}");
                        return;
                    }
                };
                let mut next = Instant::now();
                while !stop.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    if now < next {
                        thread::sleep(next - now);
                    }
                    next += interval;
                    if Instant::now() > next + interval * 4 {
                        next = Instant::now() + interval;
                    }
                    match target.capture_stack(max_depth) {
                        Ok(frames) if !frames.is_empty() => {
                            if tx.send(frames).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            if !std::path::Path::new(&format!("/proc/{pid}/status")).exists() {
                                break;
                            }
                        }
                    }
                }
            })
            .ok()
    };

    let _samplers: Vec<_> = if let Some(pid) = args.pid {
        vec![spawn_sampler(pid, frame_tx.clone(), Arc::clone(&stop))]
            .into_iter()
            .flatten()
            .collect()
    } else if args.pgrep.is_some() || args.cmdline.is_some() {
        let cgroup = if args.this_container {
            discover::self_cgroup_id()
        } else {
            None
        };
        let filter = ProcessFilter {
            name: args.pgrep.clone(),
            cmdline: args.cmdline.clone(),
            cgroup,
        };
        let pids = discover::discover(&filter)?;
        if pids.is_empty() {
            anyhow::bail!("no matching processes found");
        }
        pids.into_iter()
            .filter_map(|pid| spawn_sampler(pid, frame_tx.clone(), Arc::clone(&stop)))
            .collect()
    } else {
        anyhow::bail!("must specify -p PID, -P PGREP, or --cmdline");
    };
    drop(frame_tx);

    // Stop after duration if requested.
    if let Some(d) = duration {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            thread::sleep(d);
            stop.store(true, Ordering::SeqCst);
        });
    }

    tui::run(frame_rx, stop, Duration::from_millis(250))
}

struct SampleMsg {
    pid: i32,
    frames: Vec<Frame>,
    meta: SampleMeta,
}

/// Multi-PID runner. Spawns one thread per discovered PID; rediscovers on a
/// timer to attach to newly-spawned workers. All samples flow through a
/// single mpsc and are written by the main thread to keep output ordered.
pub fn run_multi(
    filter: ProcessFilter,
    args: &Args,
    interval: Duration,
    duration: Option<Duration>,
) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc_like::install(move || stop.store(true, Ordering::SeqCst))?;
    }

    let mut sink = output::build_sink(args)?;

    let (tx, rx) = mpsc::channel::<SampleMsg>();

    // Per-worker handle so we can clean up exited threads.
    struct WorkerHandle {
        stop: Arc<AtomicBool>,
        join: thread::JoinHandle<()>,
    }
    let mut workers: HashMap<i32, WorkerHandle> = HashMap::new();

    let max_depth = args.max_depth;
    let request_info = args.request_info;
    let executor_globals_override = args.executor_globals;
    let php_version_string = args.php_version.clone();

    let start = Instant::now();
    let rediscover_every = Duration::from_secs(args.rediscover_secs.max(1));
    let mut next_rediscover = Instant::now();

    let spawn_worker = |pid: i32, tx: mpsc::Sender<SampleMsg>| -> Option<WorkerHandle> {
        let opts = AttachOptions {
            executor_globals: executor_globals_override,
            php_version_addr: None,
            php_version_string: php_version_string.as_deref(),
        };
        let mut target = match Target::attach(pid, opts) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("attach pid={pid} failed: {e:#}");
                return None;
            }
        };
        tracing::info!(
            "attached pid={pid} php_version={} layout={} exe={}",
            target.php_version,
            target.layout.label,
            target.exe
        );

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name(format!("pfp-{pid}"))
            .stack_size(WORKER_STACK_SIZE)
            .spawn(move || {
                let mut next = Instant::now();
                while !stop_thread.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    if now < next {
                        thread::sleep(next - now);
                    }
                    next += interval;
                    if Instant::now() > next + interval * 4 {
                        next = Instant::now() + interval;
                    }

                    let frames = match target.capture_stack(max_depth) {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::debug!("pid={pid} sample error: {e:#}");
                            // Process probably exited.
                            if !std::path::Path::new(&format!("/proc/{pid}/status")).exists() {
                                tracing::info!("pid={pid} exited");
                                break;
                            }
                            continue;
                        }
                    };
                    if frames.is_empty() {
                        continue;
                    }
                    let meta = if request_info {
                        SampleMeta {
                            request_uri: target.request_var("REQUEST_URI"),
                            request_method: target.request_var("REQUEST_METHOD"),
                        }
                    } else {
                        SampleMeta {
                            request_uri: None,
                            request_method: None,
                        }
                    };
                    if tx.send(SampleMsg { pid, frames, meta }).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(WorkerHandle { stop, join })
    };

    // Initial discovery.
    let initial = discover::discover(&filter)?;
    if initial.is_empty() {
        tracing::warn!("no matching processes found yet — will keep retrying");
    }
    for pid in initial {
        if let Some(h) = spawn_worker(pid, tx.clone()) {
            workers.insert(pid, h);
        }
    }

    // Main event loop: drain samples, periodically rediscover, exit on stop.
    let mut taken: u64 = 0;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let Some(d) = duration
            && start.elapsed() >= d
        {
            break;
        }

        // Drain pending samples for up to ~50ms before checking timers.
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(msg) => {
                let mut meta = msg.meta;
                // Annotate request_uri with pid so multi-pid runs are
                // disambiguable when the same URI hits multiple workers.
                if let Some(u) = meta.request_uri.take() {
                    meta.request_uri = Some(format!("[pid {}] {u}", msg.pid));
                }
                if let Err(e) = sink.write_sample(&msg.frames, &meta) {
                    tracing::warn!("sink write failed: {e:#}");
                }
                taken += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Rediscover and reap dead workers.
        if Instant::now() >= next_rediscover {
            next_rediscover = Instant::now() + rediscover_every;
            // Reap workers whose threads have finished.
            workers.retain(|pid, h| {
                if h.join.is_finished() {
                    tracing::debug!("worker pid={pid} thread finished");
                    false
                } else {
                    true
                }
            });
            match discover::discover(&filter) {
                Ok(found) => {
                    for &pid in &found {
                        if !workers.contains_key(&pid)
                            && let Some(h) = spawn_worker(pid, tx.clone())
                        {
                            workers.insert(pid, h);
                        }
                    }
                }
                Err(e) => tracing::warn!("rediscover failed: {e:#}"),
            }
        }
    }

    // Tear down: signal workers, drop sender, drain remaining, join.
    for h in workers.values() {
        h.stop.store(true, Ordering::SeqCst);
    }
    drop(tx);
    while let Ok(msg) = rx.recv_timeout(Duration::from_millis(100)) {
        let _ = sink.write_sample(&msg.frames, &msg.meta);
        taken += 1;
    }
    for (_, h) in workers.drain() {
        let _ = h.join.join();
    }
    sink.finish()?;
    tracing::info!("done: {taken} samples across all workers");
    Ok(())
}

mod ctrlc_like {
    use anyhow::Result;
    use nix::sys::signal::{self, SigHandler, Signal};
    use std::sync::Mutex;

    static HANDLER: Mutex<Option<Box<dyn Fn() + Send + Sync>>> = Mutex::new(None);

    extern "C" fn on_signal(_: libc::c_int) {
        if let Ok(g) = HANDLER.lock()
            && let Some(cb) = g.as_ref()
        {
            cb();
        }
    }

    pub fn install<F>(cb: F) -> Result<()>
    where
        F: Fn() + Send + Sync + 'static,
    {
        *HANDLER.lock().unwrap() = Some(Box::new(cb));
        unsafe {
            signal::signal(Signal::SIGINT, SigHandler::Handler(on_signal))?;
            signal::signal(Signal::SIGTERM, SigHandler::Handler(on_signal))?;
        }
        Ok(())
    }
}
