use std::{
  collections::{HashMap, HashSet},
  io,
  sync::mpsc::{Receiver, Sender},
  time::Duration,
};

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

use crate::{
  view::{Ctx, View},
  worker::{Job, Reply},
};

/// How many entries to keep around before evicting the ones farthest from the cursor.
pub(crate) const CACHE_MAX: usize = 24;

pub(crate) fn term_width() -> u16 {
  terminal::size()
    .map(|(c, _)| c.saturating_sub(2))
    .unwrap_or(80)
}

pub(crate) struct App {
  pub(crate) ctx: Ctx,
  pub(crate) total: usize,
  pub(crate) offset: isize,
  pub(crate) min_offset: isize,
  pub(crate) max_offset: isize,
  pub(crate) width: u16,

  pub(crate) view: View, // what's on screen right now
  pub(crate) loading: bool,
  pub(crate) cache: HashMap<isize, View>,
  pub(crate) requested: HashSet<isize>, // jobs in flight to prevent duplicates
  pub(crate) jobs: Sender<Job>,
  pub(crate) replies: Receiver<Reply>,
  pub(crate) pending_scroll: Option<usize>, // restore scroll after a resize re-render

  pub(crate) scroll: usize,
  pub(crate) page: usize,
  pub(crate) exit: bool,
  pub(crate) show_msg: bool, // commit message popup
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

  pub(crate) fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
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
pub(crate) fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
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
