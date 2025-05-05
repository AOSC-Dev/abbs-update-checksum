use abbs_meta_apml::ParseError;
use core::str;
use eyre::ContextCompat;
use eyre::Result;
use faster_hex::hex_string;
use futures::StreamExt;
use log::debug;
use log::warn;
use reqwest::header::HeaderValue;
use reqwest::header::CONTENT_LENGTH;
use reqwest::Client;
use reqwest::ClientBuilder;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error;
use std::fmt::Display;
use tokio::task::spawn_blocking;

const VCS: &[&str] = &["git", "bzr", "svn", "hg", "bk"];
const UA: &str = "curl/8.10.0";

#[derive(Debug, Deserialize)]
struct PyPi {
    urls: Vec<PypiUri>,
}

#[derive(Debug, Deserialize)]
struct PypiUri {
    packagetype: String,
    url: String,
}

#[derive(Debug)]
pub struct ParseErrors(Vec<ParseError>);

impl Display for ParseErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, c) in self.0.iter().enumerate() {
            writeln!(f, "{i}. {c}")?;
        }

        Ok(())
    }
}

impl Error for ParseErrors {}

fn parse_from_str(
    s: &str,
    allow_fallback_method: bool,
) -> Result<HashMap<String, String>, ParseErrors> {
    let mut context = HashMap::new();

    if let Err(e) = abbs_meta_apml::parse(s, &mut context) {
        if !allow_fallback_method {
            return Err(ParseErrors(e));
        } else {
            warn!("{e:?}, buildit will use fallback method to parse file");
            for line in s.split('\n') {
                let stmt = line.split_once('=');
                if let Some((name, value)) = stmt {
                    context.insert(name.to_string(), value.replace('"', ""));
                }
            }
        }
    }

    Ok(context)
}

async fn update_all_checksum<C>(
    client: &Client,
    context: &mut HashMap<String, String>,
    cb: C,
    threads: usize,
) -> Result<bool>
where
    C: Fn(bool, usize, usize, u64) + Copy,
{
    let mut is_changed = false;
    let mut src_chksum_map = HashMap::new();

    let mut task_index = 0;
    for (k, v) in context.clone() {
        if k != "SRCS" && !k.starts_with("SRCS__") {
            continue;
        }

        let mut res = vec![];

        let split = v.split_whitespace().collect::<Vec<_>>();

        let mut tasks = vec![];

        for (i, c) in split.iter().enumerate() {
            let split = c.trim().split("::").collect::<Vec<_>>();

            let src_type = split.first().unwrap_or(&"tbl");
            let mut src: Cow<str> = Cow::Borrowed(*split.last().unwrap_or(&""));

            if src_type.trim().to_lowercase() == "pypi" {
                let ver = split
                    .iter()
                    .find_map(|x| x.strip_prefix("version="))
                    .context("pypi stmt is illegal")?;

                let url = get_pypi_download_url(client, &src, ver).await?;
                src = Cow::Owned(url.to_string());
            }

            if VCS.contains(&src_type.trim().to_lowercase().as_str()) {
                res.push(Cow::Borrowed("SKIP"));
            } else {
                res.push(Cow::Borrowed(""));
                let task = get_sha256(client, src, task_index, cb, i);
                task_index += 1;
                tasks.push(task);
            }
        }

        let tasks_res = futures::stream::iter(tasks)
            .buffer_unordered(threads)
            .collect::<Vec<_>>()
            .await;

        for c in tasks_res {
            let (checksum, index) = c?;
            res[index] = Cow::Owned(checksum);
        }

        src_chksum_map.insert(k, res);
    }

    for (src_type, checksum) in src_chksum_map {
        let type_arch = src_type.split_once("__");
        let new = checksum.join(" ");

        let src_type = if let Some((_, arch)) = type_arch {
            format!("CHKSUMS__{}", arch)
        } else {
            "CHKSUMS".to_string()
        };

        if context.get(&src_type).is_none_or(|old_checksum| {
            old_checksum
                .trim()
                .split_ascii_whitespace()
                .map(Cow::Borrowed)
                .collect::<HashSet<_>>()
                != checksum.into_iter().collect::<HashSet<_>>()
        }) {
            is_changed = true;
        }

        context.insert(src_type, new);
    }

    Ok(is_changed)
}

async fn get_pypi_download_url(client: &Client, pkg: &str, ver: &str) -> Result<String> {
    let url = format!("https://pypi.org/pypi/{}/{}/json", pkg, ver);
    let resp = client.get(url).send().await?;
    let resp = resp.error_for_status()?;
    let json: PyPi = resp.json().await?;

    let mut file_url = None;

    for url in json.urls {
        if &*url.packagetype == "sdist" {
            file_url = Some(url.url);
            break;
        }
    }
    Ok(file_url.context("Failed to get pypi src url")?)
}

async fn get_sha256(
    client: &Client,
    src: Cow<'_, str>,
    task_index: usize,
    cb: impl (Fn(bool, usize, usize, u64)),
    index: usize,
) -> Result<(String, usize)> {
    let mut sha256 = Sha256::new();
    let resp = client.get(&*src).send().await?;
    let mut resp = resp.error_for_status()?;

    let total_size = resp
        .headers()
        .get(CONTENT_LENGTH)
        .map(|x| x.to_owned())
        .unwrap_or(HeaderValue::from(0));

    let total_size = total_size
        .to_str()
        .ok()
        .and_then(|x| x.parse::<u64>().ok())
        .unwrap_or_default();

    while let Some(chunk) = resp.chunk().await? {
        sha256.update(&chunk);
        cb(false, task_index, chunk.len(), total_size);
    }

    let s = spawn_blocking(move || format!("sha256::{}", hex_string(&sha256.finalize()))).await?;

    cb(true, task_index, total_size as usize, total_size);

    Ok((s, index))
}

#[derive(Debug)]
pub struct UpdateChecksumResult {
    pub changed: bool,
    pub result: HashMap<String, Vec<String>>,
}

pub async fn update_from_str<C>(s: &str, cb: C, threads: usize) -> Result<UpdateChecksumResult>
where
    C: Fn(bool, usize, usize, u64) + Copy,
{
    let mut context = parse_from_str(s, false)?;
    let client = ClientBuilder::new().user_agent(UA).referer(false).build()?;

    let is_changed = update_all_checksum(&client, &mut context, cb, threads).await?;

    let mut new = HashMap::new();

    for (k, v) in context {
        if k == "CHKSUMS" || k.starts_with("CHKSUMS__") {
            let v = v
                .split_whitespace()
                .map(|x| x.to_string())
                .collect::<Vec<_>>();

            new.insert(k, v);
        }
    }

    Ok(UpdateChecksumResult {
        changed: is_changed,
        result: new,
    })
}

pub async fn get_new_spec<C>(spec_inner: &mut String, cb: C, threads: usize) -> Result<bool>
where
    C: Fn(bool, usize, usize, u64) + Copy,
{
    let update_chksum_res = update_from_str(&*spec_inner, cb, threads).await?;

    debug!("{update_chksum_res:?}");

    update_spec_inner(update_chksum_res.result, spec_inner);

    Ok(update_chksum_res.changed)
}

fn update_spec_inner(new: HashMap<String, Vec<String>>, spec_inner: &mut String) {
    for (k, v) in new {
        let start = spec_inner.find(&k).unwrap();
        let mut tmp_ref = spec_inner.as_str();
        tmp_ref = &tmp_ref[start..];
        let start_delimit = tmp_ref.find("\"").unwrap();
        tmp_ref = &tmp_ref[start_delimit + 1..];
        let end_delimit = tmp_ref.find("\"").unwrap();

        debug!(
            "replace range: {}",
            &spec_inner[start..start + start_delimit + end_delimit + 2]
        );

        spec_inner.replace_range(
            start..start + start_delimit + end_delimit + 2,
            &format!("{k}=\"{}\"", &v.join(" \\\n         ")),
        );
    }
}

#[test]
fn test_update_spec() {
    let map1 = [("CHKSUMS".to_string(), vec!["sha256::xyz".to_string()])]
        .into_iter()
        .collect::<HashMap<_, _>>();

    let mut spec = r#"VER=5.115.0
SRCS="tbl::https://download.kde.org/stable/frameworks/${VER%.*}/kiconthemes-$VER.tar.xz"
CHKSUMS="sha256::abc \
         SKIP"
CHKUPDATE="anitya::id=8762""#
        .to_string();

    update_spec_inner(map1, &mut spec);

    assert_eq!(
        spec,
        r#"VER=5.115.0
SRCS="tbl::https://download.kde.org/stable/frameworks/${VER%.*}/kiconthemes-$VER.tar.xz"
CHKSUMS="sha256::xyz"
CHKUPDATE="anitya::id=8762""#
            .to_string()
    );

    let map1 = [("CHKSUMS".to_string(), vec!["sha256::xyz".to_string()])]
        .into_iter()
        .collect::<HashMap<_, _>>();

    let mut spec = r#"VER=5.115.0
SRCS="tbl::https://download.kde.org/stable/frameworks/${VER%.*}/kiconthemes-$VER.tar.xz"
CHKSUMS="sha256::abc"
CHKUPDATE="anitya::id=8762""#
        .to_string();

    update_spec_inner(map1, &mut spec);

    assert_eq!(
        spec,
        r#"VER=5.115.0
SRCS="tbl::https://download.kde.org/stable/frameworks/${VER%.*}/kiconthemes-$VER.tar.xz"
CHKSUMS="sha256::xyz"
CHKUPDATE="anitya::id=8762""#
            .to_string()
    );

    let map2 = [(
        "CHKSUMS".to_string(),
        vec![
            "SKIP".to_string(),
            "sha256::95ecac13c3f8c69e3e80fa73864102f13730b5fd87e7438c23b96766ef458e41".to_string(),
            "sha256::b04eec580794279f6178644f6d7af090bd9bcbd3fb3b6873f3c714e21fa514fb".to_string(),
        ],
    )]
    .into_iter()
    .collect::<HashMap<_, _>>();

    let mut spec = r#"VER=3.113
SRCS="git::commit=tags/v$VER::https://github.com/lxgw/kose-font \
      file::rename=XiaolaiMonoSC-Regular.ttf::https://github.com/lxgw/kose-font/releases/download/v$VER/XiaolaiMonoSC-Regular.ttf \
      file::rename=XiaolaiSC-Regular.ttf::https://github.com/lxgw/kose-font/releases/download/v$VER/XiaolaiSC-Regular.ttf"
CHKSUMS="SKIP \
         sha256::95ecac13c3f8csha256::b04eec580794279f6178644f6d7af090bd9bcbd3fb3b6873f3c714e21fa514fb73864102f13730b5fd87e7438c23b96766ef458e41 \
         sha256::x"
CHKUPDATE="anitya::id=374941""#.to_string();

    update_spec_inner(map2, &mut spec);

    assert_eq!(
        spec,
r#"VER=3.113
SRCS="git::commit=tags/v$VER::https://github.com/lxgw/kose-font \
      file::rename=XiaolaiMonoSC-Regular.ttf::https://github.com/lxgw/kose-font/releases/download/v$VER/XiaolaiMonoSC-Regular.ttf \
      file::rename=XiaolaiSC-Regular.ttf::https://github.com/lxgw/kose-font/releases/download/v$VER/XiaolaiSC-Regular.ttf"
CHKSUMS="SKIP \
         sha256::95ecac13c3f8c69e3e80fa73864102f13730b5fd87e7438c23b96766ef458e41 \
         sha256::b04eec580794279f6178644f6d7af090bd9bcbd3fb3b6873f3c714e21fa514fb"
CHKUPDATE="anitya::id=374941""#.to_string()
    );
}
