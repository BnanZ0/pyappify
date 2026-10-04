//! Wrap an existing onedir using the same packaging implementation as the launcher.
#[path = "../src/mirror_package.rs"]
#[allow(dead_code)]
mod mirror_package;
#[path = "../src/mirror_zip.rs"]
#[allow(dead_code)]
mod mirror_zip;
#[path = "../src/program_files.rs"]
#[allow(dead_code)]
mod program_files;

fn main() -> anyhow::Result<()> {
    mirror_package::package(&std::env::args().skip(1).collect::<Vec<_>>())
}
