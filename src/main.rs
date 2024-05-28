use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use abbs_update_checksum_core::{get_new_spec, parse_from_str};
use clap::Parser;
use eyre::{bail, OptionExt, Result};
use walkdir::WalkDir;

#[derive(Debug, Parser)]
struct Args {
    #[clap(short, long)]
    dry_run: bool,
    #[clap(short, long, default_value_t = String::from("."))]
    tree: String,
    package: String,
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    let pkg = args.package;
    let tree = get_tree(Path::new(&args.tree))?;

    let mut spec = None;

    for i in WalkDir::new(tree).max_depth(3).min_depth(3) {
        let i = i?;
        if !i.file_type().is_dir() {
            continue;
        }

        let path = i.path().join("defines");

        if path.exists() {
            let f = fs::read_to_string(path)?;
            let defines = parse_from_str(&f)?;
            if defines
                .get("PKGNAME")
                .map(|x| x == &pkg.replace('"', ""))
                .unwrap_or(false)
            {
                spec = Some(()).and_then(|_| Some(i.path().parent()?.join("spec")));
            }
        }
    }

    let spec = spec.ok_or_eyre("Failed to get spec")?;

    let mut spec_inner = fs::read_to_string(&spec)?;

    let async_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .build()
        .unwrap();

    async_runtime.block_on(get_new_spec(&mut spec_inner))?;

    if args.dry_run {
        println!("{}", spec_inner);
    } else {
        let mut f = fs::File::create(&spec)?;
        f.write_all(spec_inner.as_bytes())?;
    }

    Ok(())
}

fn get_tree(directory: &Path) -> Result<PathBuf> {
    let mut tree = directory.canonicalize()?;
    let mut has_groups;

    loop {
        has_groups = tree.join("groups").is_dir();

        if !has_groups && tree.to_str() == Some("/") {
            bail!("Failed to get ABBS tree");
        }

        if has_groups {
            return Ok(tree);
        }

        tree.pop();
    }
}
