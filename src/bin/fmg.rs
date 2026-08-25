//! The `fmg` command: the same program as `foremerge` under a shorter name.
//!
//! Both binaries call the same entry point, so they cannot drift apart. Clap
//! takes the usage line from how the program was invoked, so help and errors
//! here name `fmg`.

fn main() {
    foremerge::cli::run()
}
