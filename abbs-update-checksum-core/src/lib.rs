use eyre::Result;
use faster_hex::hex_string;
use log::warn;
use reqwest::Client;
use reqwest::ClientBuilder;
use sha2::Digest;
use sha2::Sha256;
use std::borrow::Cow;
use std::collections::HashMap;
use tokio::task::spawn_blocking;

const VCS: &[&str] = &["git", "bzr", "svn", "hg", "bk"];

pub fn parse_from_str(s: &str) -> Result<HashMap<String, String>> {
    let mut context = HashMap::new();

    match abbs_meta_apml::parse(s, &mut context) {
        Ok(()) => (),
        Err(e) => {
            warn!("{e:?}, buildit will use fallback method to parse file");
            for line in s.split("\n") {
                let stmt = line.split_once("=");
                if let Some((name, value)) = stmt {
                    context.insert(name.to_string(), value.replace("\"", ""));
                }
            }
        }
    };

    Ok(context)
}

async fn update_all_checksum(client: &Client, context: &mut HashMap<String, String>) -> Result<()> {
    let mut src_chksum_map = HashMap::new();

    for (k, v) in context.clone() {
        if !k.starts_with("SRCS") && !k.starts_with("SRCTBL") {
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
            context.insert(
                key.to_string(),
                v.join(&format!(" \\\n{}", " ".repeat(key.len() + 2))),
            );
        } else {
            context.insert(
                "CHKSUMS".to_string(),
                v.join(&format!(" \\\n{}", " ".repeat(9))),
            );
        }
    }

    context.remove("CHKSUM");

    Ok(())
}

async fn get_sha256(client: &Client, src: &str) -> Result<String, eyre::Error> {
    let mut sha256 = Sha256::new();
    let resp = client.get(src).send().await?;
    let mut resp = resp.error_for_status()?;

    while let Some(chunk) = resp.chunk().await? {
        sha256.update(&chunk);
    }

    let s = spawn_blocking(move || format!("sha256::{}", hex_string(&sha256.finalize()))).await?;

    Ok(s)
}

pub async fn update_from_str(s: &str) -> Result<Vec<(String, String)>> {
    let mut context = parse_from_str(&s)?;
    let client = ClientBuilder::new().user_agent("acbs").build()?;
    update_all_checksum(&client, &mut context).await?;
    let mut res = vec![];

    for (k, v) in context {
        if k.starts_with("CHKSUM") {
            res.push((k.to_string(), format!("{k}=\"{v}\"")));
        }
    }

    Ok(res)
}

pub async fn get_new_spec(spec_inner: &mut String) -> Result<()> {
    let checksums = update_from_str(&*spec_inner).await?;

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
            if checksums.iter().all(|x| x.0 != k) && !new_line_value {
                continue;
            }
            // checksum 多行的情况
            if v.chars().filter(|x| x == &'"').count() % 2 != 0 {
                new_line_value = true;
                start_line = i;
            } else {
                // checksum 只有一行
                let find = checksums.iter().find(|x| x.0 == k);
                if let Some((_, v)) = find {
                    *c = v.clone();
                }
            }
        }
    }

    if start_line != 0 || end_line != 0 {
        let mut new_split = vec![];

        if let Some(v) = split.get(..start_line) {
            new_split.extend_from_slice(v);
        }

        if let Some(v) = split.get(end_line + 1..) {
            new_split.extend_from_slice(v);
        }

        *spec_inner = new_split.join("\n");
        for (_, i) in checksums {
            spec_inner.push_str(&format!("\n{i}"));
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_update_all_checksum() {
    let mut context = HashMap::new();

    abbs_meta_apml::parse(
        r#"VER=5.115.0
SRCS="tbl::https://download.kde.org/stable/frameworks/${VER%.*}/kiconthemes-$VER.tar.xz"
CHKSUMS="sha256::6925134bf76bb8bf6b3dabada008ded8f60fa196aa7a00c0c720c29008719d2f"
CHKUPDATE="anitya::id=8762""#,
        &mut context,
    )
    .unwrap();

    let client = ClientBuilder::new().user_agent("acbs").build().unwrap();

    update_all_checksum(&client, &mut context).await.unwrap();

    assert_eq!(
        context.get("SRCS").unwrap(),
        "sha256::6925134bf76bb8bf6b3dabada008ded8f60fa196aa7a00c0c720c29008719d2f"
    );
}
