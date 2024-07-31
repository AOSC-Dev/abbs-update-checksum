use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use abbs_update_checksum_core::get_new_spec;
use clap::Parser;
use dashmap::DashMap;
use eyre::{bail, OptionExt, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use walkdir::WalkDir;

#[derive(Debug, Parser)]
struct Args {
    #[clap(short, long)]
    dry_run: bool,
    #[clap(short, long, default_value_t = String::from("."))]
    tree: String,
    #[clap(short, long, default_value_t = 4)]
    thread: usize,
    package: String,
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    let pkg = args.package;
    let tree = get_tree(Path::new(&args.tree))?;

    let mut spec = None;

    for i in WalkDir::new(tree).max_depth(2).min_depth(2) {
        let i = i?;
        if !i.file_type().is_dir() {
            continue;
        }

        let path = i.path();

        if !path.file_name().map(|x| x == &*pkg).unwrap_or(false) {
            continue;
        }

        spec = Some(path.join("spec"));
    }

    let spec = spec.ok_or_eyre("Failed to get spec")?;

    let mut spec_inner = fs::read_to_string(&spec)?;

    let mb = MultiProgress::new();
    let map: DashMap<usize, ProgressBar> = DashMap::new();

    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()?
        .block_on(get_new_spec(
            &mut spec_inner,
            |status, index, inc, total| match map.get(&index) {
                Some(pb) => {
                    if !status {
                        pb.inc(inc as u64);
                    } else {
                        pb.finish_and_clear();
                    }
                }
                None => {
                    let pb = mb.add(ProgressBar::new(total));
                    pb.set_style(ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
                        .unwrap()
                        .progress_chars("#>-"));
                    pb.inc(inc as u64);
                    map.insert(index, pb);
                }
            },
            args.thread,
        ))?;

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
