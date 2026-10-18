//! A live dashboard for a campaign, in the spirit of AFL's status screen:
//! only what tells you whether the fuzzer is healthy and still finding
//! things. It reads a `Hub` (a local run or a master) and quits on `q`.

use crate::hub::{now, Hub, LOST_AFTER_SECS};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, event, execute, queue, terminal};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const REFRESH: Duration = Duration::from_millis(250);

/// Draw `hub` until `done` is set. `q`, Esc or Ctrl-C set `stop`: the owner
/// finishes the campaign, then sets `done`.
pub fn spawn(hub: Arc<Mutex<Hub>>, stop: Arc<AtomicBool>, done: Arc<AtomicBool>) -> JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || {
        let mut out = std::io::stdout();
        terminal::enable_raw_mode()?;
        execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        let result = (|| -> std::io::Result<()> {
            while !done.load(Ordering::Relaxed) {
                if event::poll(REFRESH)? {
                    if let event::Event::Key(key) = event::read()? {
                        let quit = matches!(key.code, event::KeyCode::Char('q') | event::KeyCode::Esc)
                            || (key.code == event::KeyCode::Char('c')
                                && key.modifiers.contains(event::KeyModifiers::CONTROL));
                        if quit && key.kind == event::KeyEventKind::Press {
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                }
                let (width, height) = terminal::size()?;
                let screen = {
                    let hub = hub.lock().unwrap_or_else(|e| e.into_inner());
                    render(&hub, width as usize, height as usize, stop.load(Ordering::Relaxed))
                };
                draw(&mut out, &screen)?;
            }
            Ok(())
        })();
        restore();
        result
    })
}

fn restore() {
    let mut out = std::io::stdout();
    let _ = execute!(out, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

/// A run of text in one style.
#[derive(Debug, Clone)]
struct Span {
    text: String,
    color: Option<Color>,
    bold: bool,
}

type Line = Vec<Span>;

fn plain(text: impl Into<String>) -> Span {
    Span {
        text: text.into(),
        color: None,
        bold: false,
    }
}

fn label(text: impl Into<String>) -> Span {
    Span {
        text: text.into(),
        color: Some(Color::DarkGrey),
        bold: false,
    }
}

fn value(text: impl Into<String>) -> Span {
    Span {
        text: text.into(),
        color: None,
        bold: true,
    }
}

fn colored(text: impl Into<String>, color: Color) -> Span {
    Span {
        text: text.into(),
        color: Some(color),
        bold: true,
    }
}

fn width_of(line: &[Span]) -> usize {
    line.iter().map(|span| span.text.chars().count()).sum()
}

/// Cut or pad `line` to exactly `width` columns.
fn fit(line: &[Span], width: usize) -> Line {
    let mut out = Vec::new();
    let mut left = width;
    for span in line {
        if left == 0 {
            break;
        }
        let text: String = span.text.chars().take(left).collect();
        left -= text.chars().count();
        out.push(Span {
            text,
            ..span.clone()
        });
    }
    if left > 0 {
        out.push(plain(" ".repeat(left)));
    }
    out
}

/// A box `width` columns wide around `lines`.
fn boxed(title: &str, lines: &[Line], width: usize) -> Vec<Line> {
    let inner = width.saturating_sub(4);
    let mut top = format!("┌ {title} ");
    let used = top.chars().count();
    top.push_str(&"─".repeat(width.saturating_sub(used + 1)));
    top.push('┐');
    let mut out = vec![vec![label(top)]];
    for line in lines {
        let mut row = vec![label("│ ")];
        row.extend(fit(line, inner));
        row.push(label(" │"));
        out.push(row);
    }
    out.push(vec![label(format!(
        "└{}┘",
        "─".repeat(width.saturating_sub(2))
    ))]);
    out
}

/// Two boxes side by side.
fn beside(left: Vec<Line>, right: Vec<Line>, left_width: usize) -> Vec<Line> {
    let rows = left.len().max(right.len());
    (0..rows)
        .map(|i| {
            let mut row = left
                .get(i)
                .map(|line| fit(line, left_width))
                .unwrap_or_else(|| vec![plain(" ".repeat(left_width))]);
            row.extend(right.get(i).cloned().unwrap_or_default());
            row
        })
        .collect()
}

pub fn duration(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {:02}h", s / 86_400, s % 86_400 / 3600),
    }
}

fn ago(at: Option<f64>, started: f64) -> Span {
    match at {
        Some(at) => value(format!("{} ago", duration(now() - at))),
        None if now() - started > 60.0 => colored("none yet", Color::Yellow),
        None => label("none yet"),
    }
}

fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn rate(per_sec: f64) -> String {
    if per_sec >= 100.0 {
        format!("{per_sec:.0}/s")
    } else {
        format!("{per_sec:.1}/s")
    }
}

fn millis(us: u64) -> String {
    if us >= 10_000 {
        format!("{:.0} ms", us as f64 / 1000.0)
    } else {
        format!("{:.1} ms", us as f64 / 1000.0)
    }
}

fn bar(fraction: f64, width: usize) -> Span {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    colored(
        format!("{}{}", "█".repeat(filled), "░".repeat(width - filled)),
        Color::Green,
    )
}

fn sparkline(samples: &[u64], width: usize) -> (Span, u64) {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let shown = &samples[samples.len().saturating_sub(width)..];
    let peak = shown.iter().copied().max().unwrap_or(0).max(1);
    let line: String = shown
        .iter()
        .map(|sample| LEVELS[((*sample * 7 + peak / 2) / peak).min(7) as usize])
        .collect();
    (colored(line, Color::Cyan), peak)
}

fn render(hub: &Hub, width: usize, height: usize, stopping: bool) -> Vec<Line> {
    let width = width.max(60);
    let at = now();
    let instances: Vec<_> = hub.instances().collect();
    let live = hub.live().count();
    let mut screen = Vec::new();

    let mode = if hub.master {
        format!("master · {live}/{} workers", instances.len())
    } else {
        "local".to_string()
    };
    let status = if stopping {
        colored(" stopping… ", Color::Yellow)
    } else {
        label(" [q] quit ")
    };
    let head = vec![value(" vroom-fuzz "), label("· "), plain(hub.title.clone()), label(format!(" · {mode}"))];
    let pad = width.saturating_sub(width_of(&head) + width_of(std::slice::from_ref(&status)));
    let mut head = head;
    head.push(plain(" ".repeat(pad)));
    head.push(status);
    screen.push(head);

    let execs = hub.execs();
    let elapsed = at - hub.started;
    let left_width = width / 2;
    let progress = vec![
        vec![label("run time       "), value(duration(elapsed))],
        vec![label("last new cov   "), ago(hub.last_new_cov, hub.started)],
        vec![label("last finding   "), ago(hub.last_finding, hub.started)],
        vec![
            label("corpus         "),
            value(count(hub.corpus_size() as u64)),
            label(if hub.master {
                format!("  ({} redundant offers)", count(hub.redundant))
            } else {
                String::new()
            }),
        ],
    ];
    let total = hub.blocks.len().max(1);
    let covered = hub.covered_blocks();
    let mut functions = BTreeMap::<&str, (usize, usize)>::new();
    for block in &hub.blocks {
        let row = functions
            .entry(if block.function.is_empty() {
                "<unknown>"
            } else {
                block.function.as_str()
            })
            .or_default();
        row.0 += 1;
        if hub.is_covered(block.id) {
            row.1 += 1;
        }
    }
    let touched = functions.values().filter(|(_, hit)| *hit > 0).count();
    let coverage = vec![
        vec![
            label("blocks     "),
            value(format!("{covered} / {}", hub.blocks.len())),
            label(format!("  {:.1}%", covered as f64 * 100.0 / total as f64)),
        ],
        vec![
            label("           "),
            bar(covered as f64 / total as f64, (width - left_width).saturating_sub(17).min(40)),
        ],
        vec![label("edges      "), value(count(hub.edge_count() as u64))],
        vec![
            label("functions  "),
            value(format!("{touched} / {}", functions.len())),
            label(" reached"),
        ],
    ];
    screen.extend(beside(
        boxed("progress", &progress, left_width),
        boxed("coverage", &coverage, width - left_width),
        left_width,
    ));

    let rows = |f: fn(&crate::hub::InstanceStats) -> u64| -> u64 {
        instances.iter().map(|row| f(&row.stats)).sum()
    };
    let weighted = |f: fn(&crate::hub::InstanceStats) -> u64| -> u64 {
        let n: u64 = rows(|s| s.execs).max(1);
        instances
            .iter()
            .map(|row| f(&row.stats) * row.stats.execs)
            .sum::<u64>()
            / n
    };
    let samples: Vec<u64> = hub.history.iter().copied().collect();
    let (spark, peak) = sparkline(&samples, width.saturating_sub(16));
    let restores = rows(|s| s.restores);
    let throughput = vec![
        vec![
            label("execs "),
            value(count(execs)),
            label("   now "),
            value(rate(hub.execs_per_sec())),
            label("   avg "),
            value(rate(execs as f64 / elapsed.max(1e-3))),
            label("   exec "),
            value(millis(weighted(|s| s.exec_us))),
            label("   reset "),
            value(millis(weighted(|s| s.reset_us))),
        ],
        vec![
            label("disk restores "),
            if restores > 0 {
                colored(count(restores), Color::Yellow)
            } else {
                value("0")
            },
            label("   timeouts "),
            value(count(rows(|s| s.timeouts))),
            label("   foreign threads stepped "),
            value(count(rows(|s| s.foreign_threads))),
        ],
        vec![spark, label(format!("  peak {}", rate(peak as f64)))],
    ];
    screen.extend(boxed("throughput", &throughput, width));

    let findings: Vec<_> = hub.findings().collect();
    let crashes = rows(|s| s.crashes);
    let mut finding_lines: Vec<Line> = findings
        .iter()
        .rev()
        .take(5)
        .map(|row| {
            vec![
                colored(format!("{:<16}", format!("{:?}", row.finding.class)), Color::Red),
                label(format!(
                    "{:<20}",
                    row.finding.pc.map_or_else(|| "-".to_string(), |pc| format!("{pc:#x}"))
                )),
                label(format!("{:<10}", format!("{} ago", duration(at - row.first_seen)))),
                plain(row.finding.detail.clone()),
            ]
        })
        .collect();
    if finding_lines.is_empty() {
        finding_lines.push(vec![label("nothing yet")]);
    } else if findings.len() > 5 {
        finding_lines.push(vec![label(format!("… {} more in crashes/", findings.len() - 5))]);
    }
    screen.extend(boxed(
        &format!(
            "findings · {} unique · {} crashing inputs",
            findings.len(),
            count(crashes)
        ),
        &finding_lines,
        width,
    ));

    if hub.master || instances.len() > 1 {
        let name_width = instances
            .iter()
            .map(|row| row.stats.name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(8, 32)
            + 2;
        let mut lines = vec![vec![label(format!(
            "{:<name_width$}{:>6}{:>10}{:>13}{:>8}{:>8}{:>9}{:>9}{:>8}",
            "instance", "state", "exec/s", "execs", "corpus", "blocks", "crashes", "restores", "sync"
        ))]];
        for row in &instances {
            let s = &row.stats;
            let silent = at - row.last_seen;
            let state = if s.done {
                label(format!("{:>6}", "done"))
            } else if silent >= LOST_AFTER_SECS {
                colored(format!("{:>6}", "lost"), Color::Red)
            } else {
                colored(format!("{:>6}", "run"), Color::Green)
            };
            lines.push(vec![
                plain(format!("{:<name_width$}", truncate(&s.name, name_width - 2))),
                state,
                plain(format!(
                    "{:>10}{:>13}{:>8}{:>8}{:>9}{:>9}{:>8}",
                    rate(if s.done { 0.0 } else { s.execs_per_sec }),
                    count(s.execs),
                    s.corpus,
                    s.blocks,
                    count(s.crashes),
                    s.restores,
                    duration(silent),
                )),
            ]);
        }
        screen.extend(boxed("instances", &lines, width));
    }

    let room = height.saturating_sub(screen.len() + 2);
    if room > 0 {
        let mut reached: Vec<_> = functions.iter().filter(|(_, (_, hit))| *hit > 0).collect();
        reached.sort_by(|a, b| (b.1 .1 * a.1 .0).cmp(&(a.1 .1 * b.1 .0)).then(a.0.cmp(b.0)));
        let unreached = functions.len() - reached.len();
        let bar_width = 20;
        let name_width = width.saturating_sub(bar_width + 20).min(48);
        let mut lines: Vec<Line> = reached
            .iter()
            .take(room.saturating_sub(usize::from(unreached > 0)).max(1))
            .map(|(name, (blocks, hit))| {
                vec![
                    plain(format!("{:<name_width$} ", truncate(name, name_width))),
                    value(format!("{hit:>4}/{blocks:<4} ")),
                    bar(*hit as f64 / *blocks as f64, bar_width),
                ]
            })
            .collect();
        if reached.len() > lines.len() {
            lines.push(vec![label(format!("… {} more reached", reached.len() - lines.len()))]);
        }
        if unreached > 0 {
            lines.push(vec![label(format!("{unreached} functions not reached yet"))]);
        }
        lines.truncate(room);
        screen.extend(boxed("functions · blocks reached", &lines, width));
    }
    screen.truncate(height);
    screen
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

fn draw(out: &mut impl Write, screen: &[Line]) -> std::io::Result<()> {
    queue!(out, cursor::MoveTo(0, 0))?;
    for (row, line) in screen.iter().enumerate() {
        queue!(out, cursor::MoveTo(0, row as u16))?;
        for span in line {
            if let Some(color) = span.color {
                queue!(out, SetForegroundColor(color))?;
            }
            if span.bold {
                queue!(out, SetAttribute(Attribute::Bold))?;
            }
            queue!(out, Print(&span.text), SetAttribute(Attribute::Reset), ResetColor)?;
        }
        queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    }
    queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
    out.flush()
}

/// The dashboard as plain text, for a terminal without raw mode or a test.
pub fn snapshot(hub: &Hub, width: usize, height: usize) -> String {
    render(hub, width, height, false)
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{Entry, FindingReport, InstanceStats};
    use crate::{BugClass, CorpusStore, CoverageBlock, Finding};

    #[test]
    fn dashboard_shows_the_campaign() {
        let block = |id: u64, function: &str| CoverageBlock {
            id,
            gpa: None,
            va: Some(id * 4),
            size: 4,
            successors: Vec::new(),
            module: String::new(),
            function: function.into(),
            source: None,
        };
        let mut hub = Hub::new(
            "hevd",
            vec![block(1, "Dispatch"), block(2, "Dispatch"), block(3, "Overflow")],
            None,
            None,
        );
        hub.master = true;
        let finding = Finding {
            class: BugClass::BugCheck,
            detail: "bugcheck 0x1e".into(),
            pc: Some(0x1234),
            input: String::new(),
        };
        hub.submit(
            InstanceStats {
                name: "mac-0".into(),
                execs: 1234,
                execs_per_sec: 101.0,
                crashes: 7,
                ..Default::default()
            },
            vec![Entry {
                hash: CorpusStore::hash(b"x"),
                data: b"x".to_vec(),
                blocks: Some(vec![1]),
                edges: vec![],
                findings: vec![],
            }],
            vec![FindingReport {
                signature: crate::signature(&finding),
                finding,
                data: b"x".to_vec(),
            }],
        )
        .unwrap();
        let text = snapshot(&hub, 100, 40);
        for expected in [
            "master · 1/1 workers",
            "1 / 3",
            "1,234",
            "101/s",
            "1 unique · 7 crashing inputs",
            "bugcheck 0x1e",
            "mac-0",
            "Dispatch",
            "1 functions not reached yet",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(text.lines().all(|line| line.chars().count() <= 100));
    }
}
