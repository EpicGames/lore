// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Diagnostic: fetch CAS blobs by digest and decode the REAPI messages among them.
//!
//! Reads `<sha256> <size>` lines on stdin, fetches them from the Lore server in batches, writes each
//! blob to `OUT/<sha256>` and a decoded description to `OUT/<sha256>.txt`, and prints one summary
//! line per blob: its size, what it decodes as, and the first thing that identifies it.
//!
//!     cas_dump LORE_URL REPO_DIR OUT_DIR < digests.txt

use std::io::BufRead;

use prost::Message;
use rbe_lore::LoreBlobStore;
use rbe_lore::Ns;
use rbe_proto::reapi::Action;
use rbe_proto::reapi::Command;
use rbe_proto::reapi::Directory;

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What a blob is, judged by whether it decodes cleanly as a message whose digests are real.
fn describe(blob: &[u8]) -> (String, String) {
    if let Ok(a) = Action::decode(blob)
        && a.command_digest.as_ref().is_some_and(|d| is_hash(&d.hash))
        && a.input_root_digest
            .as_ref()
            .is_some_and(|d| is_hash(&d.hash))
    {
        let first = format!("command {}", &a.command_digest.as_ref().unwrap().hash[..12]);
        return ("Action".into(), format!("{first}\n{a:#?}"));
    }
    if let Ok(d) = Directory::decode(blob)
        && (!d.files.is_empty() || !d.directories.is_empty() || !d.symlinks.is_empty())
        && d.files
            .iter()
            .all(|f| f.digest.as_ref().is_some_and(|g| is_hash(&g.hash)))
        && d.directories
            .iter()
            .all(|f| f.digest.as_ref().is_some_and(|g| is_hash(&g.hash)))
    {
        let names: Vec<_> = d
            .directories
            .iter()
            .map(|n| format!("{}/", n.name))
            .chain(d.files.iter().map(|n| n.name.clone()))
            .take(6)
            .collect();
        return ("Directory".into(), format!("{}\n{d:#?}", names.join(" ")));
    }
    if let Ok(c) = Command::decode(blob)
        && !c.arguments.is_empty()
    {
        let first = c
            .arguments
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        return ("Command".into(), format!("{first}\n{c:#?}"));
    }
    (
        "other".into(),
        String::from_utf8_lossy(&blob[..blob.len().min(200)]).into_owned(),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (url, repo, out) = (&args[1], &args[2], &args[3]);
    std::fs::create_dir_all(out)?;
    let store = LoreBlobStore::open(repo, 0, Some(url)).await?;
    let keys: Vec<(String, i64)> = std::io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.parse().ok()?))
        })
        .collect();
    for chunk in keys.chunks(500) {
        for ((hash, size), blob) in chunk.iter().zip(store.get_many(Ns::Cas, chunk).await?) {
            let Some(blob) = blob else {
                println!("{hash} {size} MISSING");
                continue;
            };
            std::fs::write(format!("{out}/{hash}"), &blob)?;
            let (kind, text) = describe(&blob);
            let head = text.lines().next().unwrap_or("").to_string();
            std::fs::write(format!("{out}/{hash}.txt"), &text)?;
            println!("{hash} {size} {kind} {head}");
        }
    }
    Ok(())
}
