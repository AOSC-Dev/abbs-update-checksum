use abbs_meta_apml::ParseError;
use eyre::bail;
use eyre::Result;
use faster_hex::hex_string;
use futures::StreamExt;
use log::debug;
use log::warn;
use reqwest::header::HeaderValue;
use reqwest::header::CONTENT_LENGTH;
use reqwest::Client;
use reqwest::ClientBuilder;
use sha2::Digest;
use sha2::Sha256;
use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use tokio::task::spawn_blocking;

const VCS: &[&str] = &["git", "bzr", "svn", "hg", "bk"];
const UA: &str = "curl/8.10.0";

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
) -> Result<()>
where
    C: Fn(bool, usize, usize, u64) + Copy,
{
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

            let typ = split.first().unwrap_or(&"tbl");
            let src = split.last().unwrap_or(&"");

            if VCS.contains(&typ.trim().to_lowercase().as_str()) {
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

    for (k, v) in src_chksum_map {
        let type_arch = k.split_once("__");

        if let Some((_, arch)) = type_arch {
            let key = format!("CHKSUMS__{}", arch);
            context.insert(key.to_string(), v.join(" "));
        } else {
            context.insert("CHKSUMS".to_string(), v.join(" "));
        }
    }

    Ok(())
}

async fn get_sha256(
    client: &Client,
    src: &str,
    task_index: usize,
    cb: impl (Fn(bool, usize, usize, u64)),
    index: usize,
) -> Result<(String, usize)> {
    let mut sha256 = Sha256::new();
    let resp = client.get(src).send().await?;
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

pub async fn update_from_str<C>(
    s: &str,
    cb: C,
    threads: usize,
) -> Result<HashMap<String, Vec<String>>>
where
    C: Fn(bool, usize, usize, u64) + Copy,
{
    let mut context = parse_from_str(s, false)?;
    let client = ClientBuilder::new().user_agent(UA).referer(false).build()?;

    update_all_checksum(&client, &mut context, cb, threads).await?;
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

    Ok(new)
}

pub async fn get_new_spec<C>(spec_inner: &mut String, cb: C, threads: usize) -> Result<()>
where
    C: Fn(bool, usize, usize, u64) + Copy,
{
    let new = update_from_str(&*spec_inner, cb, threads).await?;

    debug!("{new:?}");

    for (k, v) in new {
        let start = spec_inner.find(&k).unwrap();
        let mut tmp_ref = spec_inner.as_str();
        tmp_ref = &tmp_ref[start..];
        let start_delimit = tmp_ref.find("\"").unwrap();
        tmp_ref = &tmp_ref[start_delimit + 1..];
        let end_delimit = tmp_ref.find("\"").unwrap();

        debug!("replace range: {}", &spec_inner[start..start + start_delimit + end_delimit + 2]);

        spec_inner.replace_range(
            start..start + start_delimit + end_delimit + 2,
            &format!("{k}=\"{}\"", &v.join(" \\\n         ")),
        );
    }

    Ok(())
}

#[tokio::test]
async fn test_update_all_checksum() {
    let mut context: HashMap<String, String> = HashMap::new();

    abbs_meta_apml::parse(
        r#"VER=5.115.0
SRCS="tbl::https://download.kde.org/stable/frameworks/${VER%.*}/kiconthemes-$VER.tar.xz"
CHKSUMS="sha256::abc \
         SKIP"
CHKUPDATE="anitya::id=8762""#,
        &mut context,
    )
    .unwrap();

    let client = ClientBuilder::new().user_agent("acbs").build().unwrap();

    update_all_checksum(
        &client,
        &mut context,
        |status, index, downloaded, total| {
            dbg!(status, index, downloaded, total);
        },
        4,
    )
    .await
    .unwrap();

    assert_eq!(
        context.get("CHKSUMS").unwrap(),
        "sha256::6925134bf76bb8bf6b3dabada008ded8f60fa196aa7a00c0c720c29008719d2f"
    );
}
