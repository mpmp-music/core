//! A device-less output for tests and CI. Nothing runs on its own: the test
//! pulls buffers through [`NullOutputHandle`], so time advances only when it asks.

use std::sync::{Arc, Mutex};

use crate::output::{
    DeviceId, DeviceInfo, OutputBackend, OutputCaps, OutputError, OutputStream, RenderInfo,
    Renderer, SampleBufMut, SampleType, ShareMode, StreamFormat, StreamRequest,
};

type Slot = Arc<Mutex<Vec<(u64, Box<dyn Renderer>)>>>;

pub struct NullOutput {
    format: StreamFormat,
    slot: Slot,
}

impl NullOutput {
    pub fn new(format: StreamFormat) -> Self {
        Self {
            format,
            slot: Arc::default(),
        }
    }

    pub fn handle(&self) -> NullOutputHandle {
        NullOutputHandle {
            format: self.format,
            slot: Arc::clone(&self.slot),
        }
    }
}

impl OutputBackend for NullOutput {
    fn capability_query(
        &self,
        _: Option<&DeviceId>,
    ) -> Result<crate::output::CapabilityQuery, OutputError> {
        let format = self.format;
        Ok(Box::new(move |cancel| {
            cancel.check()?;
            Ok(OutputCaps {
                mix_format: format,
                supports_exclusive: false,
            })
        }))
    }
    fn device_discovery(&self) -> Option<crate::output::DeviceDiscovery> {
        Some(Arc::new(|cancel| {
            cancel.check()?;
            Ok(vec![null_identity()].into())
        }))
    }
    fn devices(&self) -> Result<Vec<DeviceInfo>, OutputError> {
        Ok(vec![null_identity()])
    }

    fn capabilities(&self, _device: Option<&DeviceId>) -> Result<OutputCaps, OutputError> {
        Ok(OutputCaps {
            mix_format: self.format,
            supports_exclusive: false,
        })
    }

    fn open(
        &mut self,
        request: StreamRequest,
        renderer: Box<dyn Renderer>,
    ) -> Result<Box<dyn OutputStream>, OutputError> {
        if request.format != self.format || request.share_mode != ShareMode::Shared {
            return Err(OutputError::UnsupportedFormat(format!(
                "{:?}",
                request.format
            )));
        }
        let mut slots = self.slot.lock().unwrap();
        let id = slots.last().map_or(1, |(id, _)| id + 1);
        slots.push((id, renderer));
        drop(slots);
        Ok(Box::new(NullStream {
            format: self.format,
            slot: Arc::clone(&self.slot),
            id,
        }))
    }
}

struct NullStream {
    id: u64,
    format: StreamFormat,
    slot: Slot,
}

impl OutputStream for NullStream {
    fn device_info(&self) -> Option<DeviceInfo> {
        Some(null_identity())
    }
    fn format(&self) -> StreamFormat {
        self.format
    }
}

impl Drop for NullStream {
    fn drop(&mut self) {
        self.slot.lock().unwrap().retain(|(id, _)| *id != self.id);
    }
}

/// Drives the renderer of whichever stream is currently open on a [`NullOutput`].
#[derive(Clone)]
pub struct NullOutputHandle {
    format: StreamFormat,
    slot: Slot,
}

impl NullOutputHandle {
    pub fn is_open(&self) -> bool {
        !self.slot.lock().unwrap().is_empty()
    }

    /// Renders `frames` frames as f32. Panics if the device format is not f32 or no
    /// stream is open.
    pub fn render_f32(&self, frames: usize) -> Vec<f32> {
        assert_eq!(self.format.sample_type, SampleType::F32);
        let mut buf = vec![0.0; frames * usize::from(self.format.channels)];
        self.render(SampleBufMut::F32(&mut buf));
        buf
    }

    pub fn render_i16(&self, frames: usize) -> Vec<i16> {
        assert_eq!(self.format.sample_type, SampleType::I16);
        let mut buf = vec![0; frames * usize::from(self.format.channels)];
        self.render(SampleBufMut::I16(&mut buf));
        buf
    }

    pub fn render_i32(&self, frames: usize) -> Vec<i32> {
        assert_eq!(self.format.sample_type, SampleType::I32);
        let mut buf = vec![0; frames * usize::from(self.format.channels)];
        self.render(SampleBufMut::I32(&mut buf));
        buf
    }

    fn render(&self, buf: SampleBufMut<'_>) {
        self.render_with_info(buf, &RenderInfo::default());
    }

    /// Drives a callback with an explicit device delay for clock/recovery tests.
    pub fn render_with_info(&self, buf: SampleBufMut<'_>, info: &RenderInfo) {
        let mut slot = self.slot.lock().unwrap();
        let (_, renderer) = slot
            .last_mut()
            .expect("no stream is open on the null output");
        renderer.render(buf, info);
    }
}

fn null_identity() -> DeviceInfo {
    DeviceInfo {
        id: DeviceId("null".into()),
        name: "Null output".into(),
        is_default: true,
        persistent: false,
    }
}
