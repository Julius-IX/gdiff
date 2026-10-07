use ratatui::{
  style::{Color, Modifier, Style},
  text::{Line, Span},
};

/// Fallback colors for --no-pager (plain `git diff --no-color` output).
pub(crate) fn style_line(raw: &str) -> Line<'static> {
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

pub(crate) fn err_lines(e: &str) -> Vec<Line<'static>> {
  e.lines()
    .map(|l| Line::styled(l.to_string(), Style::new().fg(Color::Red)))
    .collect()
}

/// Apply one SGR escape (`ESC [ ... m`) to a style.
pub(crate) fn apply_sgr(mut s: Style, params: &str) -> Style {
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
pub(crate) fn parse_ansi(text: &str) -> Vec<Line<'static>> {
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
