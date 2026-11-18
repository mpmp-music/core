//! Renderer ownership across a failed open and its endpoint retry.
use crossbeam_queue::ArrayQueue;
use mpmp_engine::output::{OutputError, RenderInfo, Renderer, SampleBufMut};

pub(crate) fn discard_failed_candidate(events: &mpmp_engine::output::OutputEventSink) {
    // Caller has joined the failed stream. Only its transient bits are discarded;
    // take() leaves the shared disconnect conclusion and persistent device sink alone.
    let _ = events.take();
}
use std::sync::Arc;

/// The callback exclusively owns its renderer. Dropping a failed stream returns it
/// through a preallocated lock-free slot, so a second open can reuse it safely.
/// No mutex, allocation or queue operation is needed by render().
#[derive(Clone)]
pub(crate) struct Renderers(Arc<ArrayQueue<Box<dyn Renderer>>>);

impl Renderers {
    pub fn new(renderer: Box<dyn Renderer>) -> Self {
        let slot = Arc::new(ArrayQueue::new(1));
        assert!(slot.push(renderer).is_ok());
        Self(slot)
    }

    pub fn take(&self) -> Result<RendererLease, OutputError> {
        Ok(RendererLease {
            renderer: Some(self.0.pop().ok_or_else(|| {
                OutputError::Backend("previous output still owns renderer".into())
            })?),
            slot: self.clone(),
        })
    }
}

pub(crate) struct RendererLease {
    renderer: Option<Box<dyn Renderer>>,
    slot: Renderers,
}

impl Renderer for RendererLease {
    fn render(&mut self, out: SampleBufMut<'_>, info: &RenderInfo) {
        self.renderer
            .as_mut()
            .expect("renderer lease is live")
            .render(out, info);
    }
}

impl Drop for RendererLease {
    fn drop(&mut self) {
        if let Some(renderer) = self.renderer.take() {
            assert!(self.slot.0.push(renderer).is_ok());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpmp_engine::output::retry::retry_on_endpoint;
    use mpmp_engine::{NullOutput, output::*};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Signal(Arc<AtomicUsize>);
    impl Renderer for Signal {
        fn render(&mut self, out: SampleBufMut<'_>, _: &RenderInfo) {
            self.0.fetch_add(1, Ordering::Relaxed);
            if let SampleBufMut::F32(samples) = out {
                samples.fill(0.5);
            }
        }
    }

    #[test]
    fn discarding_failed_candidate_keeps_disconnect_latch_and_native_facts() {
        let (tx, _) = crossbeam_channel::bounded(1);
        let native = OutputEventSink::new(0, tx);
        let candidate = native.stream(1);
        native.report_route(RouteFacts {
            default_changed: true,
            ..Default::default()
        });
        candidate.report(OutputEvent::Unknown);
        candidate.report(OutputEvent::DeviceUnplugged);
        discard_failed_candidate(&candidate);
        assert!(candidate.take().is_none());
        assert!(native.take_disconnect());
        assert!(native.take().unwrap().route.default_changed);
    }

    #[test]
    fn failed_default_build_or_start_returns_renderer_for_one_endpoint_retry() {
        for start_failed in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let renderers = Renderers::new(Box::new(Signal(Arc::clone(&calls))));
            let format = StreamFormat {
                sample_rate: 48000,
                channels: 2,
                sample_type: SampleType::F32,
            };
            let (tx, _) = crossbeam_channel::bounded(1);
            let request = StreamRequest {
                device: None,
                format,
                share_mode: ShareMode::Shared,
                events: OutputEventSink::new(1, tx),
            };
            let mut primary = NullOutput::new(format);
            let mut concrete = NullOutput::new(format);
            let handle = concrete.handle();
            let renderer = renderers.take().unwrap();
            if start_failed {
                let failed = primary.open(request.clone(), Box::new(renderer)).unwrap();
                assert!(
                    renderers.take().is_err(),
                    "a live stream exclusively owns its renderer"
                );
                drop(failed);
            } else {
                drop(renderer);
            }
            let stream = retry_on_endpoint(
                Err(OutputError::Backend("async activation failed".into())),
                || Ok(((), "endpoint-A".into())),
                |_| concrete.open(request, Box::new(renderers.take()?)),
                "open",
            )
            .unwrap();
            assert_eq!(handle.render_f32(64), vec![0.5; 128]);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            drop(stream);
            assert!(renderers.take().is_ok());
        }
    }

    #[test]
    fn default_success_never_resolves_endpoint_and_double_failure_retains_both_errors() {
        assert_eq!(
            retry_on_endpoint(
                Ok(42),
                || -> Result<((), String), OutputError> { panic!("no fallback") },
                |_| unreachable!(),
                "capabilities"
            )
            .unwrap(),
            42
        );
        let calls = AtomicUsize::new(0);
        let error = retry_on_endpoint::<(), _>(
            Err(OutputError::Backend("RPC_E_CHANGED_MODE".into())),
            || Ok(((), "endpoint-T60".into())),
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Err(OutputError::Backend("specific activation failed".into()))
            },
            "capabilities",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        for text in [
            "RPC_E_CHANGED_MODE",
            "endpoint-T60",
            "specific activation failed",
        ] {
            assert!(error.contains(text));
        }
        let resolution = retry_on_endpoint::<(), ()>(
            Err(OutputError::Backend("original".into())),
            || Err(OutputError::NoDevice),
            |_| unreachable!(),
            "open",
        )
        .unwrap_err()
        .to_string();
        assert!(resolution.contains("original"));
        assert!(resolution.contains("resolving concrete default endpoint failed"));
    }
}
