use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use abbs_update_checksum_core::{parse_from_str, update_from_str};
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
                .map(|x| x == &pkg.replace("\"", ""))
                .unwrap_or(false)
            {
                spec = Some(()).and_then(|_| Some(i.path().parent()?.join("spec")));
            }
        }
    }

    let spec = spec.ok_or_eyre("Failed to get spec")?;

    let mut spec_inner = fs::read_to_string(&spec)?;
    let (nk, nv) = update_from_str(&spec_inner)?;

    let mut split = spec_inner
        .trim()
        .split('\n')
        .map(|x| x.to_string())
        .collect::<Vec<_>>();

    let mut new_line_value = false;
    let mut start_line = 0;
    let mut end_line = 0;
    for (i, c) in split.iter_mut().enumerate() {
        let a = c.split_once('=');

        if new_line_value {
            if c.contains('"') && c.chars().filter(|x| x == &'"').count() % 2 != 0 {
                new_line_value = false;
                end_line = i;
            }
        }

        if let Some((k, v)) = a {
            if k != nk && !new_line_value {
                continue;
            }

            if v.chars().filter(|x| x == &'"').count() % 2 != 0 {
                new_line_value = true;
                start_line = i;
            } else {
                *c = nv.clone();
            }
        }
    }

    if start_line != 0 {
        let mut new_split = vec![];
        new_split.extend_from_slice(&split[..start_line]);

        if let Some(v) = split.get(end_line + 1..) {
            new_split.extend_from_slice(v);
        }

        spec_inner = new_split.join("\n");
        spec_inner.push_str(&format!("\n{nv}"));
    }

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
