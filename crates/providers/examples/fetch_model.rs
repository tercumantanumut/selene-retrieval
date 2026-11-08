//! Manual check of the downloader against the real Hugging Face origin (not run in CI):
//! `cargo run -p sr-providers --example fetch_model -- <models dir> <standard|high> [interrupt_secs]`
//! Downloads the tier's GPU file + tokenizer files, optionally cancelling after `interrupt_secs`
//! and resuming (HTTP Range), then prints the outcome. Writes only below `<models dir>`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use sr_common::events::EngineEvent;
use sr_common::tier::{HIGH, STANDARD};
use sr_providers::download::{
    DownloadConfig, DownloadConsent, DownloadSink, Downloader, ModelStore,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let store = ModelStore::new(a.get(1).expect("models dir"));
    let tier = match a.get(2).map(String::as_str) {
        Some("high") => &HIGH,
        _ => &STANDARD,
    };
    let interrupt = a.get(3).and_then(|s| s.parse::<u64>().ok());
    let file = tier.gpu_file;
    let d = Downloader::new(DownloadConfig::default()).unwrap();
    println!("metered: {}", sr_providers::download::metered::is_metered());
    let last = Arc::new(std::sync::Mutex::new(Instant::now()));
    let sink: DownloadSink = Arc::new(move |e| match &e {
        EngineEvent::ModelDownloadProgress { bytes, total, .. } => {
            let mut l = last.lock().unwrap();
            if l.elapsed() > Duration::from_secs(2) || bytes == total {
                *l = Instant::now();
                println!("progress {bytes}/{total}");
            }
        }
        other => println!("event {other:?}"),
    });
    let consent = DownloadConsent::for_file(tier, &file);
    let t0 = Instant::now();
    if let Some(s) = interrupt {
        let c = CancellationToken::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(s)).await;
            c2.cancel();
        });
        let r = d
            .download_model(&store, tier, &file, &consent, &sink, &c)
            .await;
        println!(
            "first run (cancelled after {s} s): {r:?}, state {:?}",
            store.state(tier, &file)
        );
    }
    let r = d
        .download_model(
            &store,
            tier,
            &file,
            &consent,
            &sink,
            &CancellationToken::new(),
        )
        .await;
    println!(
        "result: {r:?} in {:.1} s, state {:?}",
        t0.elapsed().as_secs_f64(),
        store.state(tier, &file)
    );
}
