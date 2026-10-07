use std::{
  collections::{HashMap, HashSet},
  io::{self, Write},
  process::{Command, Stdio},
  sync::mpsc::{self, Receiver, Sender},
  thread,
  time::Duration,
};

use clap::Parser;
use ratatui::{
  crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
  },
  layout::{Constraint, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, Borders, Clear, Paragraph, Wrap},
  DefaultTerminal, Frame,
};

#[derive(Parser)]
#[command(about = "Step through git diffs against older commits with the arrow keys")]
struct Args {
  /// Commit to compare from (the static side). Defaults to the working tree.
  commit: Option<String>,

  /// Slide both sides back together: HEAD~0 vs HEAD~1, then HEAD~1 vs HEAD~2, ...
  #[arg(short, long)]
  follow: bool,

  /// Ignore your git pager / color / external-diff config and use the built-in colors
  #[arg(long)]
  no_pager: bool,
}

/// How many entries to keep around before evicting the ones farthest from the cursor.
const CACHE_MAX: usize = 24;

/// Run git, return stdout on success or stderr on failure.
fn git(args: &[&str]) -> Result<String, String> {
  let out = Command::new("git")
    .args(args)
    .output()
    .map_err(|e| format!("failed to run git: {e}"))?;
  if out.status.success() {
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
  } else {
    Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
  }
}

/// The pager git would use for `git diff` (pager.diff > core.pager > $GIT_PAGER),
/// minus the ones that are just passthroughs when piped.
fn find_pager() -> Option<String> {
  let mut cmd = None;
  if let Ok(v) = git(&["config", "--get", "pager.diff"]) {
    match v.trim() {
      "false" | "no" | "off" | "0" => return None,
      "true" | "yes" | "on" | "1" | "" => {}
      c => cmd = Some(c.to_string()),
    }
  }
  let cmd = match cmd {
    Some(c) => c,
    None => git(&["var", "GIT_PAGER"]).ok()?.trim().to_string(),
  };
  let prog = cmd.split_whitespace().next().unwrap_or("");
  let prog = prog.rsplit('/').next().unwrap_or("");
  if matches!(prog, "" | "cat" | "less" | "more") {
    None
  } else {
    Some(cmd)
  }
}

/// Feed `input` to the pager and capture what it prints. The pager's stdout is a pipe,
/// so tools like delta/bat won't try to page interactively.
fn through_pager(pager: &str, input: &str, width: u16) -> Result<String, String> {
  let bat_opts = match std::env::var("BAT_OPTS") {
    Ok(v) => format!("{v} --color=always --paging=never"),
    Err(_) => "--color=always --paging=never".into(),
  };
  let mut child = Command::new("sh")
    .args(["-c", pager])
    .env("COLUMNS", width.to_string())
    .env("BAT_OPTS", bat_opts)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("couldn't start '{pager}': {e}"))?;

  // write on a thread so a big diff can't deadlock against a full stdout pipe
  let mut stdin = child.stdin.take().unwrap();
  let data = input.as_bytes().to_vec();
  let writer = std::thread::spawn(move || {
    let _ = stdin.write_all(&data);
  });
  let out = child.wait_with_output().map_err(|e| e.to_string())?;
  let _ = writer.join();

  if out.status.success() {
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
  } else {
    Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
  }
}

/// Fallback colors for --no-pager (plain `git diff --no-color` output).
fn style_line(raw: &str) -> Line<'static> {
  let l = raw.replace('\t', "    ").replace('\r', "");
  let style = if l.starts_with("diff ") || l.starts_with("+++") || l.starts_with("---") {
    Style::new().add_modifier(Modifier::BOLD)
  } else if l.starts_with('+') {
    Style::new().fg(Color::Green)
  } else if l.starts_with('-') {
    Style::new().fg(Color::Red)
  } else if l.starts_with("@@") {
    Style::new().fg(Color::Cyan)
  } else {
    Style::new()
  };
  Line::styled(l, style)
}

fn err_lines(e: &str) -> Vec<Line<'static>> {
  e.lines()
    .map(|l| Line::styled(l.to_string(), Style::new().fg(Color::Red)))
    .collect()
}

/// Apply one SGR escape (`ESC [ ... m`) to a style.
fn apply_sgr(mut s: Style, params: &str) -> Style {
  let nums: Vec<u16> = if params.is_empty() {
    vec![0]
  } else {
    params
      .split([';', ':'])
      .map(|p| p.parse().unwrap_or(0))
      .collect()
  };
  let mut i = 0;
  while i < nums.len() {
    match nums[i] {
      0 => s = Style::new(),
      1 => s = s.add_modifier(Modifier::BOLD),
      2 => s = s.add_modifier(Modifier::DIM),
      3 => s = s.add_modifier(Modifier::ITALIC),
      4 => s = s.add_modifier(Modifier::UNDERLINED),
      7 => s = s.add_modifier(Modifier::REVERSED),
      22 => s = s.remove_modifier(Modifier::BOLD | Modifier::DIM),
      23 => s = s.remove_modifier(Modifier::ITALIC),
      24 => s = s.remove_modifier(Modifier::UNDERLINED),
      27 => s = s.remove_modifier(Modifier::REVERSED),
      n @ 30..=37 => s = s.fg(Color::Indexed((n - 30) as u8)),
      n @ 40..=47 => s = s.bg(Color::Indexed((n - 40) as u8)),
      n @ 90..=97 => s = s.fg(Color::Indexed((n - 90 + 8) as u8)),
      n @ 100..=107 => s = s.bg(Color::Indexed((n - 100 + 8) as u8)),
      39 => s.fg = None,
      49 => s.bg = None,
      38 | 48 => {
        let is_fg = nums[i] == 38;
        let color = match nums.get(i + 1) {
          Some(5) => {
            let c = nums.get(i + 2).map(|&v| Color::Indexed(v as u8));
            i += 2;
            c
          }
          Some(2) => {
            let c = match (nums.get(i + 2), nums.get(i + 3), nums.get(i + 4)) {
              (Some(&r), Some(&g), Some(&b)) => Some(Color::Rgb(r as u8, g as u8, b as u8)),
              _ => None,
            };
            i += 4;
            c
          }
          _ => None,
        };
        if let Some(c) = color {
          s = if is_fg { s.fg(c) } else { s.bg(c) };
        }
      }
      _ => {}
    }
    i += 1;
  }
  s
}

/// Turn ANSI-colored text (git's colors, delta, bat, ...) into ratatui lines.
fn parse_ansi(text: &str) -> Vec<Line<'static>> {
  let mut lines = Vec::new();
  let mut style = Style::new(); // SGR state carries across lines, like a real terminal

  for raw in text.lines() {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut line_bg = None; // `ESC[K` with a bg set = "paint the rest of the line"
    let mut it = raw.chars().peekable();

    while let Some(c) = it.next() {
      match c {
        '\x1b' => {
          let next = it.peek().copied();
          match next {
            Some('[') => {
              it.next();
              let mut params = String::new();
              let mut fin = ' ';
              for ch in it.by_ref() {
                if ('\x40'..='\x7e').contains(&ch) {
                  fin = ch;
                  break;
                }
                params.push(ch);
              }
              match fin {
                'm' => {
                  if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), style));
                  }
                  style = apply_sgr(style, &params);
                }
                'K' => line_bg = style.bg,
                _ => {}
              }
            }
            Some(']') => {
              // OSC (hyperlinks etc.): skip until BEL or ESC \
              it.next();
              while let Some(ch) = it.next() {
                if ch == '\x07' {
                  break;
                }
                if ch == '\x1b' {
                  it.next();
                  break;
                }
              }
            }
            _ => {
              it.next();
            }
          }
        }
        '\t' => buf.push_str("    "),
        '\r' => {}
        c => buf.push(c),
      }
    }
    if !buf.is_empty() {
      spans.push(Span::styled(buf, style));
    }
    let mut line = Line::from(spans);
    if let Some(bg) = line_bg {
      line = line.style(Style::new().bg(bg));
    }
    lines.push(line);
  }
  lines
}

fn term_width() -> u16 {
  terminal::size()
    .map(|(c, _)| c.saturating_sub(2))
    .unwrap_or(80)
}

/// Everything needed to build a view for any offset. Cloneable so the worker thread owns a copy.
#[derive(Clone)]
struct Ctx {
  base: String,      // full hash of the starting commit
  base_name: String, // what to call it in the UI
  newer: Vec<String>, // commits above `base` on HEAD's first-parent chain; newer[k-1] is k steps up
  working_tree: bool,
  follow: bool,
  raw: bool,
  pager: Option<String>,
  color: bool,
}

/// One fully-rendered screen's worth of data, for caching
#[derive(Clone, Default)]
struct View {
  lines: Vec<Line<'static>>,
  title: String,
  old_label: String,
  new_label: String,
  old_msg: Vec<Line<'static>>,
  new_msg: Vec<Line<'static>>,
}

impl Ctx {
  /// `n > 0` walks back from the base (`base~n`), `n < 0` walks forward towards HEAD.
  fn rev(&self, n: isize) -> String {
    match n {
      0 => self.base.clone(),
      n if n > 0 => format!("{}~{}", self.base, n),
      n => self.newer[(-n) as usize - 1].clone(),
    }
  }

  fn name(&self, n: isize) -> String {
    match n {
      0 => self.base_name.clone(),
      n if n > 0 => format!("{}~{}", self.base_name, n),
      n => format!("{}+{}", self.base_name, -n),
    }
  }

  /// Short hash + header (hash, author, date) + full message of `base~n`
  fn commit_info(&self, n: isize) -> (String, Vec<Line<'static>>) {
    let dim = Style::new().fg(Color::DarkGray);
    let fmt = "--format=%h%x00%an%x00%ad%x00%B";
    let date = "--date=format:%Y-%m-%d %H:%M";
    let out = match git(&["log", "-1", fmt, date, &self.rev(n)]) {
      Ok(o) => o,
      Err(e) => return ("???".into(), err_lines(&e)),
    };
    let mut parts = out.splitn(4, '\0');
    let (hash, author, when, body) = (
      parts.next().unwrap_or("").trim(),
      parts.next().unwrap_or("").trim(),
      parts.next().unwrap_or("").trim(),
      parts.next().unwrap_or("").trim(),
    );

    let mut lines = vec![
      Line::from(vec![
        Span::styled(
          hash.to_string(),
          Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {author} · {when}"), dim),
      ]),
      Line::raw(""),
    ];
    for (i, l) in body.lines().enumerate() {
      let style = if i == 0 {
        Style::new().add_modifier(Modifier::BOLD) // subject line
      } else {
        Style::new()
      };
      lines.push(Line::styled(l.replace('\t', "    "), style));
    }
    (hash.to_string(), lines)
  }

  fn mode_label(&self) -> String {
    if self.raw {
      "built-in colors".into()
    } else if let Some(p) = &self.pager {
      p.split_whitespace()
        .next()
        .unwrap_or("pager")
        .rsplit('/')
        .next()
        .unwrap_or("pager")
        .to_string()
    } else {
      "git colors".into()
    }
  }

  /// Build the whole view for `offset`. Pure function of (ctx, offset, width),
  /// so it can run on any thread. git diff + both git logs run in parallel.
  fn build(&self, offset: isize, width: u16) -> View {
    // old = the side that moves back in time, new = the static side
    let (old_n, new_n) = if self.follow {
      (offset + 1, Some(offset))
    } else if self.working_tree {
      (offset, None)
    } else if offset < 0 {
      // stepping towards newer commits: base is the old side, the newer commit the new side
      (0, Some(offset))
    } else {
      (offset, Some(0))
    };

    let mut args: Vec<String> = vec!["diff".into()];
    if self.raw {
      args.extend(["--no-color".into(), "--no-ext-diff".into()]);
    } else if self.color {
      args.push("--color=always".into());
    }
    args.push(self.rev(old_n));
    if let Some(n) = new_n {
      args.push(self.rev(n));
    }
    args.push("--".into());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    let (diff, old_info, new_info) = thread::scope(|s| {
      let d = s.spawn(|| git(&argv));
      let o = s.spawn(|| self.commit_info(old_n));
      let n = new_n.map(|n| s.spawn(move || self.commit_info(n)));
      (
        d.join().unwrap(),
        o.join().unwrap(),
        n.map(|h| h.join().unwrap()),
      )
    });

    let (old_hash, old_msg) = old_info;
    let old_label = format!("{old_hash} ({})", self.name(old_n));
    let (new_label, new_msg) = match (new_n, new_info) {
      (Some(n), Some((h, m))) => (format!("{h} ({})", self.name(n)), m),
      _ => (
        "working tree".to_string(),
        vec![Line::styled(
          "Working tree: uncommitted changes",
          Style::new().fg(Color::DarkGray),
        )],
      ),
    };
    let title = format!(
      " git diff {}{} ",
      self.name(old_n),
      new_n.map_or(String::new(), |n| format!(" {}", self.name(n)))
    );

    // slow ass pagers
    let lines = match diff {
      Err(e) => err_lines(&e),
      Ok(d) if d.trim().is_empty() => {
        vec![Line::styled(
          "(no changes)",
          Style::new().fg(Color::DarkGray),
        )]
      }
      Ok(d) if self.raw => d.lines().map(style_line).collect(),
      Ok(d) => {
        let text = match &self.pager {
          Some(p) => through_pager(p, &d, width),
          None => Ok(d),
        };
        match text {
          Ok(t) => parse_ansi(&t),
          Err(e) => err_lines(&format!("pager failed: {e}\n(try --no-pager)")),
        }
      }
    };

    View {
      lines,
      title,
      old_label,
      new_label,
      old_msg,
      new_msg,
    }
  }
}

struct Job {
  offset: isize,
  width: u16,
  urgent: bool, // the user is looking at this one (vs. speculative prefetch)
}

struct Reply {
  offset: isize,
  width: u16,
  view: Option<View>, // None = job was skipped/dropped
}

/// Single worker thread. Rules:
///  - newest urgent job wins, older urgent ones are dropped (key-repeat coalescing for free)
///  - prefetch jobs run only when nothing urgent is waiting, and only near the cursor
fn worker(ctx: Ctx, jobs: Receiver<Job>, replies: Sender<Reply>) {
  let mut queue: Vec<Job> = Vec::new();
  let mut focus = 0isize;

  loop {
    if queue.is_empty() {
      match jobs.recv() {
        Ok(j) => queue.push(j),
        Err(_) => return,
      }
    }
    while let Ok(j) = jobs.try_recv() {
      queue.push(j);
    }

    let idx = queue.iter().rposition(|j| j.urgent).unwrap_or(0);
    let job = queue.remove(idx);
    if job.urgent {
      focus = job.offset;
    }

    let (dead, live): (Vec<Job>, Vec<Job>) = queue
      .drain(..)
      .partition(|j| (job.urgent && j.urgent) || (!j.urgent && j.offset.abs_diff(focus) > 2));
    queue = live;
    for j in dead {
      let r = Reply {
        offset: j.offset,
        width: j.width,
        view: None,
      };
      if replies.send(r).is_err() {
        return;
      }
    }

    if !job.urgent && job.offset.abs_diff(focus) > 2 {
      let r = Reply {
        offset: job.offset,
        width: job.width,
        view: None,
      };
      if replies.send(r).is_err() {
        return;
      }
      continue;
    }

    let view = ctx.build(job.offset, job.width);
    let r = Reply {
      offset: job.offset,
      width: job.width,
      view: Some(view),
    };
    if replies.send(r).is_err() {
      return;
    }
  }
}

struct App {
  ctx: Ctx,
  total: usize,
  offset: isize,
  min_offset: isize,
  max_offset: isize,
  width: u16,

  view: View, // what's on screen right now
  loading: bool,
  cache: HashMap<isize, View>,
  requested: HashSet<isize>, // jobs in flight to prevent duplicates
  jobs: Sender<Job>,
  replies: Receiver<Reply>,
  pending_scroll: Option<usize>, // restore scroll after a resize re-render

  scroll: usize,
  page: usize,
  exit: bool,
  show_msg: bool, // commit message popup
}

impl App {
  fn request(&mut self, offset: isize, urgent: bool) {
    if self.cache.contains_key(&offset) || self.requested.contains(&offset) {
      return;
    }
    self.requested.insert(offset);
    let _ = self.jobs.send(Job {
      offset,
      width: self.width,
      urgent,
    });
  }

  fn prefetch(&mut self) {
    for d in [1isize, -1, 2, -2] {
      let o = self.offset + d;
      if o >= self.min_offset && o <= self.max_offset {
        self.request(o, false);
      }
    }
  }

  /// Point the screen at `self.offset`: instant if cached, otherwise ask the worker.
  fn goto(&mut self) {
    self.pending_scroll = None;
    if let Some(v) = self.cache.get(&self.offset) {
      self.view = v.clone();
      self.scroll = 0;
      self.loading = false;
      self.prefetch();
    } else {
      self.loading = true;
      let o = self.offset;
      self.request(o, true);
    }
  }

  fn evict(&mut self) {
    while self.cache.len() > CACHE_MAX {
      let cur = self.offset;
      let far = self.cache.keys().copied().max_by_key(|k| k.abs_diff(cur));
      match far {
        Some(k) => self.cache.remove(&k),
        None => break,
      };
    }
  }

  fn step(&mut self, delta: isize) {
    let new = (self.offset + delta).clamp(self.min_offset, self.max_offset);
    if new != self.offset {
      self.offset = new;
      self.goto();
    }
  }

  /// Collect whatever the worker has finished.
  fn pump(&mut self) {
    let mut changed = false;
    while let Ok(r) = self.replies.try_recv() {
      if r.width != self.width {
        continue; // rendered for an old terminal size
      }
      self.requested.remove(&r.offset);
      if let Some(v) = r.view {
        if r.offset == self.offset {
          self.view = v.clone();
          self.scroll = self.pending_scroll.take().unwrap_or(0);
          self.loading = false;
        }
        self.cache.insert(r.offset, v);
        changed = true;
      }
    }
    if changed {
      self.evict();
      if !self.loading {
        self.prefetch();
      }
    }
    // the job for the visible commit may have been dropped by the worker
    // (superseded, or prefetch that fell out of range): ask again
    if self.loading && !self.requested.contains(&self.offset) {
      let o = self.offset;
      self.request(o, true);
    }
  }

  fn on_resize(&mut self, cols: u16) {
    // only pager output depends on width
    if self.ctx.pager.is_none() {
      return;
    }
    self.width = cols.saturating_sub(2);
    self.cache.clear();
    self.requested.clear();
    let scroll = self.scroll;
    self.loading = true;
    let o = self.offset;
    self.request(o, true);
    self.pending_scroll = Some(scroll);
  }

  fn on_event(&mut self, ev: Event) {
    match ev {
      Event::Resize(c, _) => self.on_resize(c),
      Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
        KeyCode::Char('q') => self.exit = true,
        // Esc closes the popup first, quits only when nothing is open
        KeyCode::Esc if self.show_msg => self.show_msg = false,
        KeyCode::Esc => self.exit = true,
        KeyCode::Char('m') => self.show_msg = !self.show_msg,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.exit = true,
        KeyCode::Right | KeyCode::Char('l') => self.step(1),
        KeyCode::Left | KeyCode::Char('h') => self.step(-1),
        KeyCode::Down | KeyCode::Char('j') => self.scroll += 1,
        KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
        KeyCode::PageDown => self.scroll += self.page,
        KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page),
        _ => {}
      },
      _ => {}
    }
  }

  fn draw(&mut self, f: &mut Frame) {
    let [main, info, keys] = Layout::vertical([
      Constraint::Min(1),
      Constraint::Length(1),
      Constraint::Length(1),
    ])
    .areas(f.area());

    // Diff pane: only hand ratatui the lines that are actually visible
    let block = Block::default()
      .borders(Borders::ALL)
      .title(self.view.title.clone());
    let inner_h = block.inner(main).height as usize;
    self.page = inner_h.max(1);
    self.scroll = self
      .scroll
      .min(self.view.lines.len().saturating_sub(inner_h));
    let end = (self.scroll + inner_h).min(self.view.lines.len());
    let visible = self.view.lines[self.scroll..end].to_vec();
    f.render_widget(Paragraph::new(visible).block(block), main);

    // Indicator line
    let bold = |c| Style::new().fg(c).add_modifier(Modifier::BOLD);
    let mut spans = vec![
      Span::raw(" new: "),
      Span::styled(self.view.new_label.clone(), bold(Color::Green)),
      Span::raw("   old: "),
      Span::styled(self.view.old_label.clone(), bold(Color::Red)),
      Span::raw(format!(
        "   │ step {} [{}..{}] · {} commits · via {}",
        self.offset,
        self.min_offset,
        self.max_offset,
        self.total,
        self.ctx.mode_label()
      )),
    ];
    if self.loading {
      spans.push(Span::styled(
        "  ⧖ loading…",
        Style::new().fg(Color::Yellow),
      ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), info);

    // Controls bar
    let bar = Paragraph::new(
      " q quit │ ↑/↓ j/k scroll │ PgUp/PgDn page │ ←/→ h/l newer/older commit │ m messages ",
    )
    .style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_widget(bar, keys);

    // Commit message popup (drawn last so it sits on top)
    if self.show_msg {
      let area = centered(f.area(), 70, 60);
      f.render_widget(Clear, area);
      let [top, bottom] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

      let pane = |title: String, color: Color, lines: &Vec<Line<'static>>| {
        Paragraph::new(lines.clone())
          .block(
            Block::bordered()
              .border_style(Style::new().fg(color))
              .title(title),
          )
          .wrap(Wrap { trim: false })
      };
      f.render_widget(
        pane(
          format!(" new: {} ", self.view.new_label),
          Color::Green,
          &self.view.new_msg,
        ),
        top,
      );
      f.render_widget(
        pane(
          format!(" old: {} (m to close) ", self.view.old_label),
          Color::Red,
          &self.view.old_msg,
        ),
        bottom,
      );
    }
  }

  fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
    self.goto();
    while !self.exit {
      terminal.draw(|f| self.draw(f))?;

      // Wake up every 30ms even with no input so finished background work shows up.
      if event::poll(Duration::from_millis(30))? {
        self.on_event(event::read()?);
        // drain anything else already queued (held arrow key) before redrawing;
        // stepping is O(1) when cached and the worker drops stale requests
        while !self.exit && event::poll(Duration::ZERO)? {
          self.on_event(event::read()?);
        }
      }
      self.pump();
    }
    Ok(())
  }
}

/// A rect centered in `area`, `pct_x` wide and `pct_y` tall (in percent).
fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
  let [_, v, _] = Layout::vertical([
    Constraint::Percentage((100 - pct_y) / 2),
    Constraint::Percentage(pct_y),
    Constraint::Percentage((100 - pct_y) / 2),
  ])
  .areas(area);
  let [_, h, _] = Layout::horizontal([
    Constraint::Percentage((100 - pct_x) / 2),
    Constraint::Percentage(pct_x),
    Constraint::Percentage((100 - pct_x) / 2),
  ])
  .areas(v);
  h
}

fn die(msg: &str) -> ! {
  eprintln!("error: {msg}");
  std::process::exit(1);
}

fn main() {
  let args = Args::parse();

  match git(&["rev-parse", "--is-inside-work-tree"]) {
    Ok(s) if s.trim() == "true" => {}
    _ => die("not inside a git repository"),
  }

  let mut base_name = args.commit.clone().unwrap_or_else(|| "HEAD".into());
  let base = git(&[
    "rev-parse",
    "--verify",
    "--quiet",
    &format!("{base_name}^{{commit}}"),
  ])
  .map(|s| s.trim().to_string())
  .unwrap_or_else(|_| {
    die(&format!(
      "can't resolve '{base_name}' to a commit (any commits yet?)"
    ))
  });

  // a hash typed in full (or abbreviated) is just noise in the labels: use the short form
  if base_name.len() >= 7
    && base_name.chars().all(|c| c.is_ascii_hexdigit())
    && base.starts_with(&base_name.to_lowercase())
  {
    base_name = base[..7].to_string();
  }

  let total: usize = git(&["rev-list", "--count", &base])
    .ok()
    .and_then(|s| s.trim().parse().ok())
    .unwrap_or(1);

  // Commits newer than `base` along HEAD's first-parent chain (the same chain `~` walks),
  // so the user can step forward past the starting commit. Empty if `base` isn't on it
  // (or when no commit was given, where base is HEAD itself).
  let newer: Vec<String> = git(&["rev-list", "--first-parent", "HEAD"])
    .ok()
    .and_then(|s| {
      let chain: Vec<&str> = s.lines().collect();
      let pos = chain.iter().position(|h| *h == base)?;
      // chain[0] = HEAD ... chain[pos] = base; newer[k-1] = chain[pos-k]
      Some(chain[..pos].iter().rev().map(|h| h.to_string()).collect())
    })
    .unwrap_or_default();
  let min_offset = -(newer.len() as isize);

  // Non-follow: base~X needs X <= total-1. Follow: also needs base~(X+1), so one less.
  let max_offset = if args.follow {
    total.saturating_sub(2)
  } else {
    total.saturating_sub(1)
  } as isize;

  let ctx = Ctx {
    base,
    base_name,
    newer,
    working_tree: args.commit.is_none() && !args.follow,
    follow: args.follow,
    raw: args.no_pager,
    pager: if args.no_pager { None } else { find_pager() },
    // exit code 0 = "yes, color" (we pretend stdout is a tty, since the TUI is one)
    color: git(&["config", "--get-colorbool", "color.diff", "true"]).is_ok(),
  };

  let (job_tx, job_rx) = mpsc::channel::<Job>();
  let (reply_tx, reply_rx) = mpsc::channel::<Reply>();
  {
    let ctx = ctx.clone();
    thread::spawn(move || worker(ctx, job_rx, reply_tx));
  }

  let mut app = App {
    ctx,
    total,
    offset: 0,
    min_offset,
    max_offset,
    width: term_width(),
    view: View::default(),
    loading: false,
    cache: HashMap::new(),
    requested: HashSet::new(),
    jobs: job_tx,
    replies: reply_rx,
    pending_scroll: None,
    scroll: 0,
    page: 10,
    exit: false,
    show_msg: false,
  };

  let mut terminal = ratatui::init();
  let result = app.run(&mut terminal);
  ratatui::restore();

  if let Err(e) = result {
    die(&e.to_string());
  }
}
