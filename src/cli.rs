use clap::Parser;

#[derive(Parser)]
#[command(about = "Step through git diffs against older commits with the arrow keys")]
pub(crate) struct Args {
  /// Commit to compare from (the static side). Defaults to the working tree.
  pub(crate) commit: Option<String>,

  /// Slide both sides back together: HEAD~0 vs HEAD~1, then HEAD~1 vs HEAD~2, ...
  #[arg(short, long)]
  pub(crate) follow: bool,

  /// Ignore your git pager / color / external-diff config and use the built-in colors
  #[arg(long)]
  pub(crate) no_pager: bool,
}
