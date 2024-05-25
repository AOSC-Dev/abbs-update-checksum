use eyre::bail;
use eyre::Result;
use faster_hex::hex_string;
use log::warn;
use reqwest::blocking::Client;
use reqwest::blocking::ClientBuilder;
use sha2::Digest;
use sha2::Sha256;
use std::{
    collections::HashMap,
    io::{self, Cursor, Read},
};

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

fn update_all_checksum(client: &Client, context: &mut HashMap<String, String>) -> Result<()> {
    let mut not_only = false;
    for (k, v) in context.iter_mut() {
        if !k.starts_with("SRCS") && !k.starts_with("SRCTBL") {
            continue;
        }

        let mut res = String::new();
        *v = v.replace("\\", "");
        let split = v.split('\n').collect::<Vec<_>>();
        for (i, c) in split.iter().enumerate() {
            let split = c.split("::").collect::<Vec<_>>();

            let typ = split.first().unwrap_or(&"tbl");
            let src = split.last().unwrap_or(&"");

            if not_only && i != split.len() - 1 {
                res.push_str(" \\\n");
            }

            if VCS.contains(&typ.trim().to_lowercase().as_str()) {
                res.push_str("SKIP");
                not_only = true;
            } else {
                let mut sha256 = Sha256::new();
                let resp = client.get(src.to_owned()).send()?;
                let mut resp = resp.error_for_status()?;
                let mut buf = vec![];
                resp.read_to_end(&mut buf)?;
                let mut cursor = Cursor::new(buf);
                io::copy(&mut cursor, &mut sha256)?;

                let s = format!("sha256::{}", hex_string(&sha256.finalize()));
                res.push_str(&s);
            }

            if i == split.len() - 1 {
                res.push_str("\"");
            }
        }
    }

    Ok(())
}

pub fn update_from_str(s: &str) -> Result<(String, String)> {
    let mut context = parse_from_str(&s)?;
    let client = ClientBuilder::new().user_agent("acbs").build()?;
    update_all_checksum(&client, &mut context)?;

    for (k, v) in context {
        if k.starts_with("CHKSUM") {
            let v = v
                .split_ascii_whitespace()
                .collect::<Vec<_>>()
                .join(&format!(" \\\n{}", " ".repeat(k.len() + 2)));

            return Ok((k.to_string(), format!("{k}=\"{v}\"")));
        }
    }

    bail!("has no checksum to update");
}

#[test]
fn test_update_all_checksum() {
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

    update_all_checksum(&client, &mut context).unwrap();

    assert_eq!(
        context.get("SRCS").unwrap(),
        "sha256::6925134bf76bb8bf6b3dabada008ded8f60fa196aa7a00c0c720c29008719d2f"
    );
}
