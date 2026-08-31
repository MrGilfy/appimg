//! Generates the man page and the shell completions from the same clap
//! definition the binary uses, so they can never drift apart.

use std::env;
use std::fs;
use std::path::Path;

use clap::CommandFactory;

include!("src/cli.rs");

fn main() {
    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = std::path::PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is always set"));
    let repo_root =
        std::path::PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"))
            .join("..")
            .join("..");

    generate(&out_dir);

    // `OUT_DIR` carries a build hash in its path, which packaging cannot
    // rely on, so the same files also land in `man/` and `completions/` at
    // the root of the source tree. They are created when they are missing,
    // which is what a fresh checkout looks like. A source tree that cannot
    // be written to is not an error, `OUT_DIR` still has everything.
    let man_dir = repo_root.join("man");
    let completions_dir = repo_root.join("completions");
    if fs::create_dir_all(&man_dir).is_ok() && fs::create_dir_all(&completions_dir).is_ok() {
        copy(&out_dir.join("appimg.1"), &man_dir.join("appimg.1"));
        for name in ["appimg.fish", "appimg.bash", "_appimg", "appimg.elv"] {
            copy(&out_dir.join(name), &completions_dir.join(name));
        }
    }
}

fn generate(out_dir: &Path) {
    let mut command = Cli::command();

    let mut page = Vec::new();
    clap_mangen::Man::new(command.clone()).render(&mut page).expect("rendering the man page");
    fs::write(out_dir.join("appimg.1"), page).expect("writing the man page");

    for shell in [
        clap_complete::Shell::Fish,
        clap_complete::Shell::Bash,
        clap_complete::Shell::Zsh,
        clap_complete::Shell::Elvish,
    ] {
        clap_complete::generate_to(shell, &mut command, "appimg", out_dir)
            .expect("writing the completion script");
    }
}

fn copy(from: &Path, to: &Path) {
    let _ = fs::copy(from, to);
}
