use std::thread;

use ratatui::{
  style::{Color, Modifier, Style},
  text::{Line, Span},
};

use crate::{
  git::git,
  pager::through_pager,
  style::{err_lines, parse_ansi, style_line},
};

/// Everything needed to build a view for any offset. Cloneable so the worker thread owns a copy.
#[derive(Clone)]
pub(crate) struct Ctx {
  pub(crate) base: String,      // full hash of the starting commit
  pub(crate) base_name: String, // what to call it in the UI
  pub(crate) newer: Vec<String>, // commits above `base` on HEAD's first-parent chain; newer[k-1] is k steps up
  pub(crate) working_tree: bool,
  pub(crate) follow: bool,
  pub(crate) raw: bool,
  pub(crate) pager: Option<String>,
  pub(crate) color: bool,
}

/// One fully-rendered screen's worth of data, for caching
#[derive(Clone, Default)]
pub(crate) struct View {
  pub(crate) lines: Vec<Line<'static>>,
  pub(crate) title: String,
  pub(crate) old_label: String,
  pub(crate) new_label: String,
  pub(crate) old_msg: Vec<Line<'static>>,
  pub(crate) new_msg: Vec<Line<'static>>,
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

  pub(crate) fn mode_label(&self) -> String {
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
  pub(crate) fn build(&self, offset: isize, width: u16) -> View {
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
