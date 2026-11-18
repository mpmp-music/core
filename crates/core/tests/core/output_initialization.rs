use super::*;
use mpmp_engine::NullOutput;
use mpmp_engine::output::{
    DeviceDiscovery, DeviceId, DeviceInfo, OutputBackend, OutputCaps, OutputError, OutputStream,
    Renderer, StreamRequest,
};

struct DelayedOutput {
    inner: NullOutput,
    entered: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

impl OutputBackend for DelayedOutput {
    fn device_discovery(&self) -> Option<DeviceDiscovery> {
        let discover = self.inner.device_discovery()?;
        let entered = self.entered.clone();
        let release = self.release.clone();
        Some(Arc::new(move |cancel| {
            entered.send(()).unwrap();
            release.recv_timeout(TIMEOUT).unwrap();
            discover(cancel)
        }))
    }
    fn devices(&self) -> Result<Vec<DeviceInfo>, OutputError> {
        self.inner.devices()
    }
    fn capabilities(&self, device: Option<&DeviceId>) -> Result<OutputCaps, OutputError> {
        self.inner.capabilities(device)
    }
    fn open(
        &mut self,
        request: StreamRequest,
        renderer: Box<dyn Renderer>,
    ) -> Result<Box<dyn OutputStream>, OutputError> {
        self.inner.open(request, renderer)
    }
}

#[test]
fn initialization_barrier_waits_for_delayed_discovery_without_filtering_events() {
    let (entered, discovering) = crossbeam_channel::bounded(1);
    let (release, gate) = crossbeam_channel::bounded(1);
    let core = CoreHandle::start(
        Box::new(DelayedOutput {
            inner: support::null_output(),
            entered,
            release: gate,
        }),
        Arc::new(FileResolver),
        Arc::new(MemoryCatalog::standard()),
        CoreConfig::default(),
    )
    .unwrap();
    discovering.recv_timeout(TIMEOUT).unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    assert!(matches!(
        core.query(Query::OutputDevices),
        QueryResult::OutputDevices { generation: 0, .. }
    ));
    let (done, finished) = crossbeam_channel::bounded(1);
    let waiting_core = core.clone();
    let waiting = std::thread::spawn(move || {
        support::wait_for_output_initialization(&waiting_core);
        done.send(()).unwrap();
    });
    assert!(matches!(
        finished.recv_timeout(Duration::from_millis(20)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    ));
    release.send(()).unwrap();
    finished.recv_timeout(TIMEOUT).unwrap();
    waiting.join().unwrap();

    // The barrier observes completion; it neither consumes nor hides startup events.
    let startup: Vec<_> = sub.events.try_iter().collect();
    assert_eq!(startup.len(), 2, "{startup:?}");
    assert!(matches!(
        startup[0].event,
        Event::OutputDevicesChanged { generation: 1 }
    ));
    assert!(matches!(&startup[1].event, Event::OutputChanged {output} if output.generation == 1));
    assert!(startup.iter().all(|event| event.origin.is_none()));

    // A later subscription cannot receive initialization events in its no-op window.
    let clean = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    let receipt = core.execute(
        Request::new(1, Command::SetMuted { muted: false }),
        Origin::Ui,
    );
    assert!(matches!(
        receipt.result,
        CommandResult::Ok(receipt) if !receipt.changed
    ));
    assert!(clean.events.try_iter().collect::<Vec<_>>().is_empty());
    core.shutdown();
}
