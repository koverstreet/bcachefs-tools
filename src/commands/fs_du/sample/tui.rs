//! fs du --sample --interactive: the estimate refined live, browsed by
//! directory, by device, or by background target.
//!
//! The sampler threads draw until told to stop, each sending a tally of a
//! few hundred samples over a channel. Everything else is the UI thread's:
//! it merges the tallies, attributes each new file's samples, resolves paths
//! in what's left of a frame, and renders. The threads share only the
//! sampler, read only, and two flags: stop, and paused, which parks them
//! between samples so a paused display does no IO.
//!
//! Estimates are counts until rendered, scaled by the samples so far: each
//! frame shows the estimate of that many samples, and refines with more. A
//! file whose path isn't known yet is pending, shown in the header and in no
//! directory, so the tree only ever holds resolved data. Reflink pointers
//! are read the first time a sample lands on reflinked data, which stalls
//! the display for as long as that walk of the extents btree takes.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::util::Sectors;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use owo_colors::OwoColorize;

use super::{dev_usage, estimate, kinds_to_text, owner_files, sample_one, target_label, Sampler, Tally,
            Tree};
use crate::commands::fs_du::reflink::ReflinkRefs;
use crate::commands::fs_du::resolve::{fs_path, Resolver};
use crate::commands::fs_du::{Cli, Mounted};
use crate::util::{run_tui, terminal_size};
use crate::wrappers::handle::DevUsage;

/// Samples a thread sends at a time
const BATCH: u64 = 256;
/// Of each frame, the time path resolution may take
const RESOLVE_BUDGET: Duration = Duration::from_millis(50);
const FRAME: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq)]
enum View {
    Paths,
    Devices,
    Targets,
}

/// A file's attribution, once resolved.
struct File {
    path:   Option<Rc<Path>>,
    target: String,
}

struct App<'a> {
    m:            &'a Mounted,
    sampler:      &'a Sampler<'a>,
    seed:         u64,
    threads:      u64,
    started:      Instant,
    tally:        Tally,
    resolver:     Resolver<'a>,
    refs:         Option<ReflinkRefs>,
    usage:        Vec<DevUsage>,
    files:        HashMap<(u64, u32), File>,
    /// Samples of files not resolved yet
    pending:      HashMap<(u64, u32), f64>,
    tree:         Tree,
    targets:      BTreeMap<String, f64>,
    unresolved:   f64,
    unreferenced: f64,
    view:         View,
    cwd:          PathBuf,
    cursor:       usize,
}

impl App<'_> {
    fn add(&mut self, chunk: Tally) -> Result<()> {
        for (owner, &n) in &chunk.owners {
            if matches!(owner, super::Owner::Indirect(_)) && self.refs.is_none() {
                self.refs = Some(ReflinkRefs::load(&self.m.handle).context("reading reflink pointers")?);
            }
            let by = owner_files(owner, self.refs.as_ref());
            if by.is_empty() {
                self.unreferenced += n as f64;
            }
            for file in &by {
                let share = n as f64 / by.len() as f64;
                match self.files.get(file) {
                    Some(f) => attribute(f, share, &mut self.tree, &mut self.targets, &mut self.unresolved),
                    None => *self.pending.entry(*file).or_default() += share,
                }
            }
        }
        self.tally.merge(chunk);
        Ok(())
    }

    /// Resolves pending files until @deadline.
    fn resolve(&mut self, deadline: Instant) -> Result<()> {
        while Instant::now() < deadline {
            let Some(&file) = self.pending.keys().next() else { break };
            let share = self.pending.remove(&file).unwrap_or_default();
            let (inum, snapshot) = file;

            let f = File {
                path:   self.resolver.path(inum, snapshot)
                    .with_context(|| format!("resolving inode {inum} in snapshot {snapshot}"))?,
                target: target_label(&self.m.fs, &mut self.resolver, inum, snapshot)?,
            };
            attribute(&f, share, &mut self.tree, &mut self.targets, &mut self.unresolved);
            self.files.insert(file, f);
        }
        Ok(())
    }

    fn est(&self, out: &mut Printbuf, samples: f64) {
        let (est, margin) = estimate(&self.tally, self.sampler.space.total, samples);
        write!(out, "{}\r{}\r  ", Sectors(est), Sectors(margin));
    }

    /// The entries below the current directory, largest first.
    fn entries(&self) -> Vec<(&std::ffi::OsString, &Tree)> {
        let mut v: Vec<_> = self.tree.get(&self.cwd)
            .map(|t| t.children.iter().collect())
            .unwrap_or_default();
        v.sort_by(|a, b| b.1.samples.total_cmp(&a.1.samples));
        v
    }

    /// The frame's lines, and which is the cursor's.
    fn frame(&self, paused: bool) -> (Vec<String>, Option<usize>) {
        let elapsed = self.started.elapsed().as_secs_f64();
        let mut out = Printbuf::new();
        out.set_human_readable(true);

        writeln!(out, "{} samples of {} on {} devices, {:.0}/s, seed {}, {} threads{}",
                 self.tally.n, Sectors(self.sampler.space.total), self.sampler.space.devs.len(),
                 self.tally.n as f64 / elapsed.max(0.001), self.seed, self.threads,
                 if paused { ", paused" } else { "" });
        let pending: f64 = self.pending.values().sum();
        let mut header = Printbuf::new();
        header.set_human_readable(true);
        header.aligned(|sub| {
            for (n, what) in [(pending, "pending resolution"), (self.unresolved, "unresolved"),
                              (self.unreferenced, "reflinked, unreferenced")] {
                if n != 0.0 {
                    self.est(sub, n);
                    writeln!(sub, "{what}");
                }
            }
        });
        write!(out, "{header}");

        let mut cursor_line = None;
        let mut body = Printbuf::new();
        body.set_human_readable(true);
        match self.view {
            View::Paths => {
                let here = self.tree.get(&self.cwd).map_or(0.0, |t| t.samples);
                body.aligned(|sub| {
                    write!(sub, "disk\r±95%\r  {}\n", self.cwd.display());
                    self.est(sub, here);
                    writeln!(sub, ".");
                    for (name, t) in self.entries() {
                        self.est(sub, t.samples);
                        let dir = if t.children.is_empty() { "" } else { "/" };
                        writeln!(sub, "{}{dir}", Path::new(name).display());
                    }
                });
                cursor_line = Some(2 + self.cursor);
            }
            View::Devices => {
                for (i, d) in self.sampler.space.devs.iter().enumerate() {
                    writeln!(body, "{} (device {}):", d.name, d.idx);
                    body.aligned(|sub| kinds_to_text(sub, &self.tally, self.sampler.space.total, &[i],
                                                     &self.usage));
                    writeln!(body);
                }
            }
            View::Targets => body.aligned(|sub| {
                write!(sub, "estimate\r±95%\r  background target\n");
                for (label, &n) in &self.targets {
                    self.est(sub, n);
                    writeln!(sub, "{label}");
                }
            }),
        }

        let mut lines: Vec<String> = out.to_string().lines().map(String::from).collect();
        let top = lines.len() + 1;
        lines.push(String::new());
        lines.extend(body.to_string().lines().map(String::from));
        (lines, cursor_line.map(|l| top + l))
    }

    fn key(&mut self, code: KeyCode, paused: &AtomicBool) -> bool {
        let entries = self.entries().len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(entries.saturating_sub(1)),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') if self.view == View::Paths => {
                let into = self.entries().get(self.cursor)
                    .filter(|(_, t)| !t.children.is_empty())
                    .map(|(name, _)| self.cwd.join(name));
                if let Some(into) = into {
                    self.cwd = into;
                    self.cursor = 0;
                }
            }
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') if self.view == View::Paths => {
                let up = self.cwd.parent().map(Path::to_path_buf);
                if let Some(up) = up {
                    let from = self.cwd.file_name().map(|n| n.to_os_string());
                    self.cwd = up;
                    self.cursor = self.entries().iter()
                        .position(|(name, _)| Some(*name) == from.as_ref())
                        .unwrap_or(0);
                }
            }
            KeyCode::Char('v') | KeyCode::Tab => self.view = match self.view {
                View::Paths   => View::Devices,
                View::Devices => View::Targets,
                View::Targets => View::Paths,
            },
            KeyCode::Char('p') | KeyCode::Char(' ') => { paused.fetch_xor(true, Ordering::Relaxed); }
            _ => {}
        }
        false
    }
}

fn attribute(f: &File, share: f64, tree: &mut Tree, targets: &mut BTreeMap<String, f64>,
             unresolved: &mut f64) {
    match &f.path {
        Some(path) => tree.add(path, share),
        None => *unresolved += share,
    }
    *targets.entry(f.target.clone()).or_default() += share;
}

/// Draws until @stop, @paused parking it, sending tallies to @tx.
fn sampler_thread(sampler: &Sampler<'_>, seed: u64, stop: &AtomicBool, paused: &AtomicBool,
                  tx: mpsc::Sender<Result<Tally>>) {
    let mut rng = fastrand::Rng::with_seed(seed);

    while !stop.load(Ordering::Relaxed) {
        if paused.load(Ordering::Relaxed) {
            std::thread::park_timeout(FRAME);
            continue;
        }
        let chunk = (0..BATCH).try_fold(Tally::default(), |mut chunk, _| {
            chunk.add(sample_one(sampler, &mut rng)?);
            Ok(chunk)
        });
        let failed = chunk.is_err();
        if tx.send(chunk).is_err() || failed {
            return;
        }
    }
}

pub(super) fn run(m: &Mounted, cli: &Cli, sampler: &Sampler<'_>, seed: u64, threads: u64) -> Result<()> {
    let mut resolver = Resolver::new(&m.handle, &m.fs)?;
    let cwd = fs_path(&mut resolver, &cli.paths[0])
        .with_context(|| format!("{}", cli.paths[0].display()))?;
    let mut app = App {
        m, sampler, seed, threads,
        started:      Instant::now(),
        tally:        Tally::default(),
        resolver,
        refs:         None,
        usage:        dev_usage(&m.handle, sampler.space)?,
        files:        HashMap::new(),
        pending:      HashMap::new(),
        tree:         Tree::default(),
        targets:      BTreeMap::new(),
        unresolved:   0.0,
        unreferenced: 0.0,
        view:         View::Paths,
        cwd,
        cursor:       0,
    };
    let stop = AtomicBool::new(false);
    let paused = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel();

    std::thread::scope(|s| {
        for i in 0..threads {
            let tx = tx.clone();
            let (stop, paused) = (&stop, &paused);
            s.spawn(move || sampler_thread(sampler, seed.wrapping_add(i), stop, paused, tx));
        }
        drop(tx);

        let ret = run_tui(|stdout| loop {
            let frame_end = Instant::now() + FRAME;
            for chunk in rx.try_iter() {
                app.add(chunk?)?;
            }
            app.resolve(Instant::now() + RESOLVE_BUDGET)?;
            draw(stdout, &app, paused.load(Ordering::Relaxed))?;

            if event::poll(frame_end.saturating_duration_since(Instant::now()))? {
                if let Event::Key(key) = event::read()? {
                    let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl_c || app.key(key.code, &paused) {
                        return Ok(());
                    }
                }
            }
        });
        stop.store(true, Ordering::Relaxed);
        ret
    })
}

fn draw(stdout: &mut io::Stdout, app: &App<'_>, paused: bool) -> Result<()> {
    let (width, height) = terminal_size(io::stdout()).unwrap_or((120, 40));
    let (lines, cursor_line) = app.frame(paused);
    let footer = "q quit  arrows/hjkl move  enter open  backspace up  v view  p pause";
    let rows = usize::from(height).saturating_sub(1).max(1);
    let skip = cursor_line.map_or(0, |c| (c + 1).saturating_sub(rows));

    execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(terminal::ClearType::All))?;
    for (i, line) in lines.iter().enumerate().skip(skip).take(rows) {
        let line: String = line.chars().take(width.into()).collect();
        if Some(i) == cursor_line {
            write!(stdout, "{}\r\n", line.reversed())?;
        } else {
            write!(stdout, "{line}\r\n")?;
        }
    }
    write!(stdout, "{}", footer.dimmed())?;
    stdout.flush()?;
    Ok(())
}
