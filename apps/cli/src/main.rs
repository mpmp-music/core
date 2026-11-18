//! Minimal command-line player for trying the engine on real hardware.
//!
//! Usage: `mpmp-cli <file>...` plays the files gaplessly, in order.
//! Keys (+ Enter): `p` pause/resume, `n` next track, `f`/`b` seek ±10 s, `q` quit.

use std::io::BufRead;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{never, select, tick, unbounded};
use mpmp_engine::{BitPerfect, Engine, EngineConfig, EngineEvent, EntryId, TrackRef};
use mpmp_media_io::{FileResolver, MediaUri};
use mpmp_output_cpal::CpalBackend;

fn main() -> ExitCode {
    // Diagnostics go to stderr, off unless asked for, e.g. RUST_LOG=mpmp_engine=debug.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_writer(std::io::stderr)
        .init();
    let paths: Vec<_> = std::env::args_os().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: mpmp-cli <file>...");
        return ExitCode::FAILURE;
    }
    match run(&paths) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(paths: &[std::ffi::OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let tracks = paths
        .iter()
        .enumerate()
        .map(|(i, path)| Ok(TrackRef::new(EntryId(i as u64), MediaUri::from_path(path)?)))
        .collect::<Result<Vec<_>, std::io::Error>>()?;
    let mut engine = Engine::new(
        Box::new(CpalBackend::new()),
        Arc::new(FileResolver),
        EngineConfig::default(),
    );
    let events = engine.events();
    let output_events = engine.output_events();
    engine.play(tracks[0].clone())?;
    engine.set_next(tracks.get(1).cloned());

    let (keys_tx, mut keys) = unbounded();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if keys_tx.send(line.trim().to_owned()).is_err() {
                break;
            }
        }
    });
    let ticker = tick(Duration::from_millis(250));
    let index_of = |entry: EntryId| entry.0 as usize;

    loop {
        select! {
            recv(events) -> event => {
                match event? {
                    EngineEvent::TrackStarted { entry, signal_path } => {
                        let i = index_of(entry);
                        let source = &signal_path.source;
                        println!(
                            concat!(
                                "\n[{}/{}] {}\n  source: {} {} Hz, {} ch, {} bit | output: {} Hz, {} ch, ",
                                "{:?}",
                            ),
                            i + 1,
                            tracks.len(),
                            tracks[i].uri,
                            source.codec,
                            source.sample_rate,
                            source.channels,
                            source.bits_per_sample.map_or("?".into(), |b| b.to_string()),
                            signal_path.output.sample_rate,
                            signal_path.output.channels,
                            signal_path.output.sample_type,
                        );
                        for stage in &signal_path.stages {
                            println!(
                                "  stage: {} ({}){}",
                                stage.name,
                                stage.detail,
                                if stage.alters_samples {
                                    ""
                                } else {
                                    ", unchanged"
                                }
                            );
                        }
                        match &signal_path.bit_perfect {
                            BitPerfect::Yes => println!("  bit-perfect: yes"),
                            BitPerfect::No(reasons) => {
                                println!("  bit-perfect: no ({})", reasons.join("; "));
                            }

                            BitPerfect::Unknown(why) => {
                                println!("  bit-perfect: unknown ({why})");
                            }
                        }
                        engine.set_next(tracks.get(i + 1).cloned());
                    }
                    EngineEvent::NextAlreadyStarted { entry } => {
                        println!("\nnext already playing: {}", tracks[index_of(entry)].uri);
                    }
                    EngineEvent::TrackFailed { entry, reason } => {
                        println!("\nfailed: {}: {reason}", tracks[index_of(entry)].uri);
                    }
                    EngineEvent::EndOfQueue { .. } => {
                        println!("\nfinished");
                        return Ok(());
                    }
                }
            },
            recv(output_events) -> event => {
                println!("\noutput event: {:?}", event?);
            },
            recv(keys) -> key => {
                match key.as_deref() {
                    Ok("p") if engine.is_paused() => engine.resume(),
                    Ok("p") => engine.pause(),
                    Ok("n") => {
                        let next = engine.position().map_or(1, |p| index_of(p.entry) + 1);
                        match tracks.get(next) {
                            Some(track) => {
                                engine.play(track.clone())?;
                                engine.set_next(tracks.get(next + 1).cloned());
                            }
                            None => return Ok(()),
                        }
                    }
                    Ok(key @ ("f" | "b")) => {
                        if let Some(p) = engine.position() {
                            let step = u64::from(p.sample_rate) * 10;
                            let target = if key == "f" {
                                p.frame + step
                            } else {
                                p.frame.saturating_sub(step)
                            };

                            engine.seek(p.entry, target);
                        }
                    }
                    Ok("q") => return Ok(()),
                    Ok(_) => {}
                    // stdin closed (e.g. redirected from a file): keep playing without controls.
                    Err(_) => keys = never(),
                }
            },
            recv(ticker) -> _ => {
                if let (Some(position), Some(buffer)) =
                    (engine.position(), engine.buffer_status())
                {
                    print!(
                        concat!(
                            "\r[{}/{}] {:>8.2} s  underruns: {}  late callbacks: {} (max gap {} ms)  ",
                            "{}   ",
                        ),
                        index_of(position.entry) + 1,
                        tracks.len(),
                        position.as_duration().as_secs_f64(),
                        buffer.underruns,
                        buffer.late_callbacks,
                        buffer.max_callback_gap.as_millis(),
                        if engine.is_paused() { "paused" } else { "" },
                    );
                    std::io::Write::flush(&mut std::io::stdout())?;
                }
            },
        }
    }
}
