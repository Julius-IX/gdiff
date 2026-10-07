mod app;
mod cli;
mod git;
mod pager;
mod style;
mod view;
mod worker;

use std::{
  collections::{HashMap, HashSet},
  sync::mpsc,
  thread,
};

use clap::Parser;

use crate::{
  app::{term_width, App},
  cli::Args,
  git::git,
  pager::find_pager,
  view::{Ctx, View},
  worker::{worker, Job, Reply},
};

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
