//! Embeds the core without an audio device: opens a library, starts a core with a null
//! output, optionally scans a music folder, then sends commands and runs queries both as
//! typed Rust values and as contract JSON.
//!
//! Usage: `cargo run -p mpmp-core --example embed -- [music-folder]`

use std::sync::Arc;
use std::time::Duration;

use mpmp_core::contract::{Command, CommandResult, Event, Origin, Query, QueryResult, Request};
use mpmp_core::{Catalog, CoreConfig, CoreHandle, SubscribeOptions};
use mpmp_engine::NullOutput;
use mpmp_engine::output::{SampleType, StreamFormat};
use mpmp_library::Library;
use mpmp_media_io::FileResolver;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let music = std::env::args_os().nth(1);

    // The library database; a real host keeps it in its data directory.
    let data = tempfile::tempdir()?;
    let library = Library::open(&data.path().join("library.db"))?;

    // NullOutput consumes audio at real-time pace without a device.
    let output = NullOutput::new(StreamFormat {
        sample_rate: 44_100,
        channels: 2,
        sample_type: SampleType::F32,
    });
    // Background loudness measurement is on by default; off keeps this example short.
    let config = CoreConfig {
        automatic_loudness_scan: Some(false),
        ..CoreConfig::default()
    };
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(FileResolver),
        Arc::new(library) as Arc<dyn Catalog>,
        config,
    )?;

    // A snapshot of the current state, then every event after it, in `seq` order.
    let options = SubscribeOptions::default().with_positions(false);
    let subscription = core.subscribe(Origin::Ui, options).ok_or("core stopped")?;
    let snapshot = &subscription.snapshot;
    println!(
        "API {}.{}, state {:?}, {} queue entries",
        snapshot.contract_version,
        snapshot.contract_minor,
        snapshot.state,
        snapshot.queue.entry_count,
    );

    if let Some(music) = music {
        // Typed command. The adapter, not the client, decides the origin.
        let path = std::path::absolute(music)?.to_string_lossy().into_owned();
        let request = Request {
            request_id: 1,
            idempotency_key: Some("embed-add-root".into()),
            command: Command::AddRoot { path },
        };
        let receipt = match core.execute(request, Origin::Ui).result {
            CommandResult::Ok(receipt) => receipt,
            CommandResult::Err { error } => return Err(format!("add_root: {error:?}").into()),
            _ => return Err("add_root: unknown result".into()),
        };
        // The scan runs as a job; its results are visible once its JobFinished arrives.
        let job = receipt.job.ok_or("folder is already in the library")?;
        for envelope in subscription.events.iter() {
            if let Event::JobFinished {
                job: id, outcome, ..
            } = envelope.event
                && id == job
            {
                println!("scan: {}", serde_json::to_string(&outcome)?);
                break;
            }
        }
    }

    // Typed query.
    if let QueryResult::Roots { roots } = core.query(Query::Roots) {
        for root in roots {
            println!("root: {} ({} tracks)", root.path, root.track_count);
        }
    }

    // The same contract as JSON, as adapters for other languages and processes send it.
    let response = core.execute_json(
        r#"{"request_id":2,"idempotency_key":null,"command":{"type":"set_shuffle","shuffle":true}}"#,
        Origin::Ui,
    );
    println!("{response}");
    let tracks = core.query_json(r#"{"type":"tracks","offset":0,"limit":0}"#);
    let tracks: serde_json::Value = serde_json::from_str(&tracks)?;
    println!("tracks in library: {}", tracks["total"]);

    // Wait for running jobs and the library to close before the process exits.
    core.shutdown_with_result(Duration::from_secs(10))?;
    Ok(())
}
