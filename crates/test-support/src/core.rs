//! Shared barriers for integration tests that observe command-caused events.

use std::time::{Duration, Instant};

use mpmp_core::contract::*;
use mpmp_core::{CoreHandle, Subscription};

const TIMEOUT: Duration = Duration::from_secs(30);

/// Wait for the nonempty initial inventory of a NullOutput (or equivalent fixture).
/// A successful actor query is a barrier after discovery committed and published both
/// OutputDevicesChanged and OutputChanged. Subscribe after this call; do not discard
/// output events in the operation's observation window. Empty/unsupported backends
/// must use their own initialization condition rather than this fixture helper.
pub fn wait_for_output_initialization(core: &CoreHandle) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = core.query(Query::OutputDevices);
        match &result {
            QueryResult::OutputDevices { generation, .. } if *generation > 0 => return,
            QueryResult::OutputDevices { .. } => {}
            _ => panic!("initial output discovery failed: {result:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "initial output discovery did not complete: {result:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub fn ok(result: CommandResult) -> Receipt {
    match result {
        CommandResult::Ok(receipt) => receipt,
        other => panic!("{other:?}"),
    }
}

pub fn err(result: CommandResult) -> ContractError {
    match result {
        CommandResult::Err { error } => error,
        other => panic!("{other:?}"),
    }
}

pub fn run(core: &CoreHandle, command: Command, origin: Origin) -> CommandResult {
    core.execute(Request::new(1, command), origin).result
}

pub fn wait_event(sub: &Subscription, pred: impl Fn(&EventEnvelope) -> bool) -> EventEnvelope {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "event never came");
        if let Ok(envelope) = sub.events.recv_timeout(Duration::from_millis(20))
            && pred(&envelope)
        {
            return envelope;
        }
    }
}

pub fn null_output() -> mpmp_engine::NullOutput {
    mpmp_engine::NullOutput::new(mpmp_engine::output::StreamFormat {
        sample_rate: 44_100,
        channels: 2,
        sample_type: mpmp_engine::output::SampleType::F32,
    })
}
