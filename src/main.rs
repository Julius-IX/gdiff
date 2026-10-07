use std::{
  io::{self, Write},
  process::{Command, Stdio},
};

use clap::Parser;
use ratatui::{
  layout::{Constraint, Layout},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, Borders, Paragraph},
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

struct App {
  base: String,      // full hash of the starting commit
  base_name: String, // what to call it in the UI ("HEAD" or whatever the user passed)
  working_tree: bool,
  follow: bool,
  total: usize,
  offset: usize,
  max_offset: usize,

  raw: bool,             // --no-pager: ignore user config entirely
  pager: Option<String>, // resolved from git config (None when raw)
  color: bool,           // does the user's color.diff want color?

  lines: Vec<Line<'static>>,
  title: String,
  old_label: String,
  new_label: String,
  scroll: usize,
  page: usize,
  exit: bool,
}

impl App {
  fn rev(&self, n: usize) -> String {
    if n == 0 {
      self.base.clone()
    } else {
      format!("{}~{}", self.base, n)
    }
  }

  fn name(&self, n: usize) -> String {
    if n == 0 {
      self.base_name.clone()
    } else {
      format!("{}~{}", self.base_name, n)
    }
  }

  fn side(&self, n: usize) -> String {
    let short = git(&["rev-parse", "--short", &self.rev(n)])
      .map(|s| s.trim().to_string())
      .unwrap_or_else(|_| "???".into());
    format!("{short} ({})", self.name(n))
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

  /// Re-run git diff (and the pager) for the current offset.
  fn refresh(&mut self) {
    todo!()
  }

  fn step(&mut self, delta: isize) {
    let new = (self.offset as isize + delta).clamp(0, self.max_offset as isize) as usize;
    if new != self.offset {
      self.offset = new;
      self.refresh();
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
      .title(self.title.clone());
    let inner_h = block.inner(main).height as usize;
    self.page = inner_h.max(1);
    self.scroll = self.scroll.min(self.lines.len().saturating_sub(inner_h));
    let end = (self.scroll + inner_h).min(self.lines.len());
    let visible = self.lines[self.scroll..end].to_vec();
    f.render_widget(Paragraph::new(visible).block(block), main);

    // Indicator line
    let bold = |c| Style::new().fg(c).add_modifier(Modifier::BOLD);
    let indicator = Line::from(vec![
      Span::raw("   new: "),
      Span::styled(self.new_label.clone(), bold(Color::Green)),
      Span::raw(" old: "),
      Span::styled(self.old_label.clone(), bold(Color::Red)),
      Span::raw(format!(
        "   │ step {}/{} · {} commits · via {}",
        self.offset,
        self.max_offset,
        self.total,
        self.mode_label()
      )),
    ]);
    f.render_widget(Paragraph::new(indicator), info);

    // Controls bar
    let bar = Paragraph::new(
      " q quit │ ↑/↓ scroll diff │ PgUp/PgDn page │ ←/→ older/newer commit to compare against ",
    )
    .style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_widget(bar, keys);
  }

  fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
    todo!()
  }
}

fn main() {}
