use std::{
  io::Write,
  process::{Command, Stdio},
};

use crate::git::git;

/// The pager git would use for `git diff` (pager.diff > core.pager > $GIT_PAGER),
/// minus the ones that are just passthroughs when piped.
pub(crate) fn find_pager() -> Option<String> {
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
pub(crate) fn through_pager(pager: &str, input: &str, width: u16) -> Result<String, String> {
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
