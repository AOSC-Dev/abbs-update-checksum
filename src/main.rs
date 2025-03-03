use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::exit,
};

use abbs_update_checksum_core::get_new_spec;
use clap::Parser;
use dashmap::DashMap;
use eyre::{bail, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{info, LevelFilter};
use simplelog::{ColorChoice, ConfigBuilder, TermLogger, TerminalMode};
use walkdir::WalkDir;

#[derive(Debug, Parser)]
struct Args {
    #[clap(short, long)]
    dry_run: bool,
    #[clap(short, long, default_value_t = String::from("."))]
    tree: String,
    #[clap(long, default_value_t = 4)]
    threads: usize,
    packages: Vec<String>,
}

fn main() -> Result<()> {
    TermLogger::init(
        LevelFilter::Info,
        ConfigBuilder::default().build(),
        TerminalMode::Stderr,
        ColorChoice::Auto,
    )?;

    let args = Args::parse();

    let pkgs = args.packages;
    let tree = get_tree(Path::new(&args.tree))?;

    let mut specs = vec![];

    for i in WalkDir::new(tree).max_depth(2).min_depth(2) {
        let i = i?;
        if !i.file_type().is_dir() {
            continue;
        }

        let path = i.path();

        if !path
            .file_name()
            .is_some_and(|pkg| pkgs.contains(&pkg.to_string_lossy().to_string()))
        {
            continue;
        }

        specs.push(path.join("spec"));

        if pkgs.len() == specs.len() {
            break;
        }
    }

    let mut changed = false;

    for spec in specs {
        let mut spec_file = fs::read_to_string(&spec)?;

        let mb = MultiProgress::new();
        let map: DashMap<usize, ProgressBar> = DashMap::new();

        let is_changed = tokio::runtime::Builder::new_multi_thread()
            .enable_io()
            .enable_time()
            .build()?
            .block_on(get_new_spec(
                &mut spec_file,
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
                args.threads,
            ))?;

        info!("{} is changed: {}", spec.display(), is_changed);

        if !changed {
            changed = is_changed;
        }

        if args.dry_run {
            println!("{}", spec_file);
        } else {
            let mut f = fs::File::create(&spec)?;
            f.write_all(spec_file.as_bytes())?;
        }
    }

    if changed && args.dry_run {
        exit(1)
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
