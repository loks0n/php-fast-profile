//! Live `top`-style display. Aggregates sampled stacks into a sorted table
//! showing per-function exclusive/inclusive sample counts.
//!
//! Keys: `q`/Esc to quit, `i` toggle inclusive/exclusive sort, `r` reset.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::zend::Frame;

#[derive(Default)]
pub struct Aggregator {
    rows: HashMap<String, Counts>,
    samples: u64,
    started: Option<Instant>,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    exclusive: u64,
    inclusive: u64,
}

impl Aggregator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, frames: &[Frame]) {
        if self.started.is_none() {
            self.started = Some(Instant::now());
        }
        if frames.is_empty() {
            return;
        }
        self.samples += 1;
        // Frames are leaf-first: frames[0] is the currently-executing call.
        let leaf_key = qualified_name(&frames[0]);
        self.rows.entry(leaf_key).or_default().exclusive += 1;

        // Inclusive: every distinct function in this stack picks up one.
        let mut seen: HashMap<String, ()> = HashMap::new();
        for f in frames {
            let k = qualified_name(f);
            if seen.insert(k.clone(), ()).is_none() {
                self.rows.entry(k).or_default().inclusive += 1;
            }
        }
    }

    pub fn reset(&mut self) {
        self.rows.clear();
        self.samples = 0;
        self.started = Some(Instant::now());
    }

    fn top_rows(&self, by_inclusive: bool, limit: usize) -> Vec<(String, Counts)> {
        let mut v: Vec<_> = self.rows.iter().map(|(k, v)| (k.clone(), *v)).collect();
        v.sort_by(|a, b| {
            let key_a = if by_inclusive {
                a.1.inclusive
            } else {
                a.1.exclusive
            };
            let key_b = if by_inclusive {
                b.1.inclusive
            } else {
                b.1.exclusive
            };
            key_b.cmp(&key_a)
        });
        v.truncate(limit);
        v
    }
}

fn qualified_name(f: &Frame) -> String {
    match &f.class {
        Some(c) => format!("{c}::{}", f.function),
        None => f.function.to_string(),
    }
}

/// Run the live TUI. Pulls frames from `rx` and refreshes every `tick`.
/// Returns when the user presses q/Esc, the channel disconnects, or `stop`
/// is set.
pub fn run(rx: Receiver<Vec<Frame>>, stop: Arc<AtomicBool>, tick: Duration) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_loop(&mut terminal, rx, &stop, tick);

    // Always restore the terminal, even on error.
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    res
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rx: Receiver<Vec<Frame>>,
    stop: &AtomicBool,
    tick: Duration,
) -> Result<()> {
    let mut agg = Aggregator::new();
    let mut by_inclusive = true;
    let mut last_draw = Instant::now() - tick;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        // Drain everything currently in the channel.
        loop {
            match rx.try_recv() {
                Ok(frames) => agg.record(&frames),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        if last_draw.elapsed() >= tick {
            draw(terminal, &agg, by_inclusive)?;
            last_draw = Instant::now();
        }

        // Cap event polling so we redraw on time.
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(k) = event::read()?
        {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('i') => by_inclusive = !by_inclusive,
                KeyCode::Char('r') => agg.reset(),
                _ => {}
            }
        }
    }
    Ok(())
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    agg: &Aggregator,
    by_inclusive: bool,
) -> Result<()> {
    terminal.draw(|f| {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(f.area());

        let total = agg.samples.max(1);
        let elapsed = agg
            .started
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let rate = if elapsed > 0.0 {
            total as f64 / elapsed
        } else {
            0.0
        };
        let header = format!(
            " pfp top — {} samples in {:.1}s ({:.0} Hz) — sort: {}",
            total,
            elapsed,
            rate,
            if by_inclusive {
                "inclusive"
            } else {
                "exclusive"
            },
        );
        f.render_widget(
            Paragraph::new(header).block(Block::default().borders(Borders::ALL).title("pfp")),
            layout[0],
        );

        let rows: Vec<Row> = agg
            .top_rows(by_inclusive, layout[1].height.saturating_sub(2) as usize)
            .into_iter()
            .map(|(name, c)| {
                let inc_pct = (c.inclusive as f64 / total as f64) * 100.0;
                let exc_pct = (c.exclusive as f64 / total as f64) * 100.0;
                Row::new(vec![
                    Cell::from(format!("{:>5.1}%", inc_pct)),
                    Cell::from(format!("{:>5.1}%", exc_pct)),
                    Cell::from(format!("{:>8}", c.inclusive)),
                    Cell::from(format!("{:>8}", c.exclusive)),
                    Cell::from(name),
                ])
            })
            .collect();

        let header_row = Row::new(vec!["INCL%", "EXCL%", "INCL", "EXCL", "FUNCTION"])
            .style(Style::default().add_modifier(Modifier::BOLD));
        let widths = [
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(20),
        ];
        let table = Table::new(rows, widths)
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title("functions"));
        f.render_widget(table, layout[1]);

        f.render_widget(
            Paragraph::new(" q/Esc quit · i toggle sort · r reset"),
            layout[2],
        );
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(name: &str) -> Frame {
        Frame {
            function: name.into(),
            class: None,
            file: None,
            line: 0,
        }
    }

    #[test]
    fn aggregator_counts_exclusive_at_leaf() {
        let mut a = Aggregator::new();
        a.record(&[frame("usleep"), frame("level3"), frame("level2")]);
        a.record(&[frame("usleep"), frame("level3"), frame("level2")]);
        a.record(&[frame("level3"), frame("level2")]);
        let by_excl = a.top_rows(false, 10);
        let usleep = by_excl.iter().find(|(n, _)| n == "usleep").unwrap();
        assert_eq!(usleep.1.exclusive, 2);
        let level3 = by_excl.iter().find(|(n, _)| n == "level3").unwrap();
        assert_eq!(level3.1.exclusive, 1);
    }

    #[test]
    fn aggregator_counts_inclusive_once_per_sample() {
        let mut a = Aggregator::new();
        // level2 calls level2 recursively — inclusive should still be 1, not 2.
        a.record(&[frame("level2"), frame("level2"), frame("entry")]);
        let by_inc = a.top_rows(true, 10);
        let l2 = by_inc.iter().find(|(n, _)| n == "level2").unwrap();
        assert_eq!(l2.1.inclusive, 1);
        let entry = by_inc.iter().find(|(n, _)| n == "entry").unwrap();
        assert_eq!(entry.1.inclusive, 1);
    }

    #[test]
    fn reset_clears_counts() {
        let mut a = Aggregator::new();
        a.record(&[frame("foo")]);
        a.reset();
        assert!(a.top_rows(false, 10).is_empty());
        assert_eq!(a.samples, 0);
    }
}
