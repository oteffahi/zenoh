//
// Copyright (c) 2023 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use clap::Parser;
use sha3::{Digest, Keccak256};
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};
use tokio::select;
use zenoh::{key_expr::KeyExpr, Config};
use zenoh_examples::CommonArgs;

#[tokio::main]
async fn main() {
    let mut local_store: HashMap<String, Vec<u8>> = HashMap::new();
    let mut digest_store: HashMap<String, HashSet<String>> = HashMap::new();

    let sub_file_keyexpr = KeyExpr::new("DS/FILE").unwrap();
    let sub_digest_keyexpr = KeyExpr::new("DS/DIGEST").unwrap();

    // initiate logging
    zenoh::init_log_from_env_or("error");

    let (config, ..) = parse_args();

    println!("Opening session...");
    let session = zenoh::open(config).await.unwrap();

    // to avoid broadcasting the same file multiple times, need to have a way to target only one queryable in the network
    let zid = session.info().zid().await.to_string();
    let queryable_store_keyexpr = KeyExpr::new(format!("DS/STORE/{zid}")).unwrap();
    let queryable_store = session
        .declare_queryable(&queryable_store_keyexpr)
        .await
        .unwrap();

    let sub_file = session.declare_subscriber(&sub_file_keyexpr).await.unwrap();

    let sub_digest = session
        .declare_subscriber(&sub_digest_keyexpr)
        .await
        .unwrap();

    println!("Press CTRL-C to quit...");
    loop {
        select! {
            Ok(file_sample) = sub_file.recv_async() => {
                println!();
                println!(">> [FILE] Received sample");
                if file_sample.payload().len() > 0 {
                    let file = file_sample.payload().deserialize::<Vec<u8>>().unwrap();

                    // compute hash
                    let mut hasher = Keccak256::new();
                    hasher.update(&file);
                    let mut hash = [0u8; 64];
                    hex::encode_to_slice(hasher.finalize(), &mut hash).unwrap();
                    let hash_string = String::from_utf8(hash.to_vec()).unwrap();

                    // store in kvs
                    local_store.insert(hash_string.to_string(), file);
                    println!(">> [FILE] Stored file {}", hash_string.get(0..5).unwrap());

                    // compute digest
                    let signature = "PLACEHOLDER_SIGNATURE";

                    // send digest
                    // TODO: proper serialization
                    let message = std::format!("{hash_string}|{signature}");
                    session.put(&sub_digest_keyexpr, message).await.unwrap();
                    println!(
                        ">> [FILE] Sent digest for file {}",
                        hash_string.get(0..5).unwrap()
                    );
                }
            }

            Ok(file_query) = queryable_store.recv_async() => {
                println!();
                println!(">> [STORE] Received upload request");
                match file_query.payload() {
                    Some(file_payload) if file_payload.len() > 0 => {
                        session.put(&sub_file_keyexpr, file_payload).await.unwrap();
                    }
                    _ => {
                        println!(">> [STORE] Skipping empty request");
                        // handle error
                        file_query
                            .reply_err("File payload should not be empty")
                            .await
                            .unwrap();
                        continue;
                    }
                }
                let response = "OK".to_string();
                println!(
                    ">> [STORE] Responding ('{}': '{}')",
                    queryable_store_keyexpr.as_str(),
                    response,
                );
                file_query
                    .reply(queryable_store_keyexpr.clone(), response)
                    .await
                    .unwrap_or_else(|e| println!(">> [Queryable ] Error sending reply: {e}"));
            }

            Ok(digest_sample) = sub_digest.recv_async() => {
                println!();
                println!(">> [DIGEST] Received sample");
                if digest_sample.payload().len() > 0 {
                    // TODO: proper deserialization
                    let digest = digest_sample.payload().deserialize::<String>().unwrap();
                    let deserialized: Vec<&str> = digest.split("|").collect();
                    if deserialized.len() != 2 {
                        println!(">> [DIGEST] Skipping invalid message");
                    }

                    let (hash, signature) = (deserialized[0], deserialized[1]);
                    println!(
                        ">> [DIGEST] Received digest for file {}",
                        hash.get(0..5).unwrap()
                    );

                    if digest_store
                        .entry(hash.to_string())
                        .or_insert(HashSet::new())
                        .insert(signature.to_string())
                    {
                        println!(
                            ">> [DIGEST] New digest added for file {}",
                            hash.get(0..5).unwrap()
                        );
                    } else {
                        println!(
                            ">> [DIGEST] Skipping redundant digest for file {}",
                            hash.get(0..5).unwrap()
                        );
                    }
                    // TODO: check if quorum reached (use liveliness token to count number of replicas in the network in real time)
                }
            }
        }
    }
}

#[derive(clap::Parser, Clone, PartialEq, Eq, Hash, Debug)]
struct Args {
    #[arg(short, long, default_value = "demo/example/zenoh-rs-queryable")]
    /// The key expression matching queries to reply to.
    key: KeyExpr<'static>,
    #[arg(short, long, default_value = "Queryable from Rust!")]
    /// The payload to reply to queries.
    payload: String,
    #[arg(long)]
    /// Declare the queryable as complete w.r.t. the key expression.
    complete: bool,
    #[command(flatten)]
    common: CommonArgs,
}

fn parse_args() -> (Config, KeyExpr<'static>, String, bool) {
    let args = Args::parse();
    (args.common.into(), args.key, args.payload, args.complete)
}
