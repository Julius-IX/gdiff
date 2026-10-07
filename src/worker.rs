use std::sync::mpsc::{Receiver, Sender};

use crate::view::{Ctx, View};

pub(crate) struct Job {
  pub(crate) offset: isize,
  pub(crate) width: u16,
  pub(crate) urgent: bool, // the user is looking at this one (vs. speculative prefetch)
}

pub(crate) struct Reply {
  pub(crate) offset: isize,
  pub(crate) width: u16,
  pub(crate) view: Option<View>, // None = job was skipped/dropped
}

/// Single worker thread. Rules:
///  - newest urgent job wins, older urgent ones are dropped (key-repeat coalescing for free)
///  - prefetch jobs run only when nothing urgent is waiting, and only near the cursor
pub(crate) fn worker(ctx: Ctx, jobs: Receiver<Job>, replies: Sender<Reply>) {
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
