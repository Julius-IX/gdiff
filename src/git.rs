use std::process::Command;

/// Run git, return stdout on success or stderr on failure.
pub(crate) fn git(args: &[&str]) -> Result<String, String> {
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
