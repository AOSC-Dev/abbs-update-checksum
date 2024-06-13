use abbs_meta_apml::ParseError;
use eyre::Result;
use faster_hex::hex_string;
use log::debug;
use log::warn;
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

async fn update_all_checksum(client: &Client, context: &mut HashMap<String, String>) -> Result<()> {
    let mut src_chksum_map = HashMap::new();

    for (k, v) in context.clone() {
        if k != "SRCS" && !k.starts_with("SRCS__") {
            continue;
        }

        let mut res = vec![];

        let split = v.split_whitespace().collect::<Vec<_>>();

        let mut tasks = vec![];

        for i in split {
            let split = i.trim().split("::").collect::<Vec<_>>();

            let typ = split.first().unwrap_or(&"tbl");
            let src = split.last().unwrap_or(&"");

            if VCS.contains(&typ.trim().to_lowercase().as_str()) {
                res.push(Cow::Borrowed("SKIP"));
            } else {
                res.push(Cow::Borrowed(""));
                let task = get_sha256(client, src);
                tasks.push(task);
            }
        }

        let tasks_res = futures::future::join_all(tasks).await;

        for c in tasks_res {
            let c = c?;
            let pos = res.iter().position(|x| x.is_empty()).unwrap();
            res[pos] = Cow::Owned(c);
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

async fn get_sha256(client: &Client, src: &str) -> Result<String> {
    let mut sha256 = Sha256::new();
    let resp = client.get(src).send().await?;
    let mut resp = resp.error_for_status()?;

    while let Some(chunk) = resp.chunk().await? {
        sha256.update(&chunk);
    }

    let s = spawn_blocking(move || format!("sha256::{}", hex_string(&sha256.finalize()))).await?;

    Ok(s)
}

pub async fn update_from_str(s: &str) -> Result<(Vec<Vec<String>>, Vec<Vec<String>>)> {
    let mut context = parse_from_str(s, false)?;
    let client = ClientBuilder::new().user_agent("acbs").build()?;

    let mut old = vec![];
    for (k, v) in &context {
        if k == "CHKSUMS" || k.starts_with("CHKSUMS__") {
            let v = v
                .split_whitespace()
                .map(|x| x.trim().to_string())
                .collect::<Vec<_>>();

            old.push(v);
        }
    }

    update_all_checksum(&client, &mut context).await?;
    let mut new = vec![];

    for (k, v) in context {
        if k == "CHKSUMS" || k.starts_with("CHKSUMS__") {
            let v = v
                .split_whitespace()
                .map(|x| x.to_string())
                .collect::<Vec<_>>();

            new.push(v);
        }
    }

    Ok((old, new))
}

pub async fn get_new_spec(spec_inner: &mut String) -> Result<()> {
    let (old, new) = update_from_str(&*spec_inner).await?;

    debug!("{old:?}");
    debug!("{new:?}");

    for (i, c) in old.iter().enumerate() {
        for (j, d) in c.iter().enumerate() {
            *spec_inner = spec_inner.replace(d, &new[i][j]);
        }
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

    update_all_checksum(&client, &mut context).await.unwrap();

    assert_eq!(
        context.get("CHKSUMS").unwrap(),
        "sha256::6925134bf76bb8bf6b3dabada008ded8f60fa196aa7a00c0c720c29008719d2f"
    );
}
