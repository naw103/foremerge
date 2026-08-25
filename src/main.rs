//! The `foremerge` command.
//!
//! The implementation lives in `foremerge::cli` so that this binary and the
//! short-named `fmg` binary share one compilation rather than building the
//! same source twice.

fn main() {
    foremerge::cli::run()
}
