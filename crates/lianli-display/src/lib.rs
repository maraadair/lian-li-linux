pub mod backend;
pub mod buffer;
pub mod channel;
pub mod dmabuf;
pub mod evdi;
pub mod frame;
pub mod gpu;
pub mod hermes;
pub mod hyprland;
pub mod login;
mod socket;
pub mod wayland;

use anyhow::Result;
use frame::{Frame, Mode, PixelFormat};
use std::time::Duration;

pub use lianli_shared::display::{OutputRequest, MAX_FRAME_BYTES};

pub fn socket_accepts_connections(path: &std::path::Path, deadline: std::time::Instant) -> bool {
    socket::connect(path, deadline).is_ok()
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    ModeChanged(Mode, PixelFormat),
    FrameReady,
    PowerChanged(bool),
}

pub trait Capture {
    fn poll_events(
        &mut self,
        timeout: Duration,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<Event>>;
    fn request_update(&mut self) -> Result<bool>;
    fn frame(&mut self, cancel: &std::sync::atomic::AtomicBool) -> Result<Frame<'_>>;
    /// Finish consuming a returned frame before requesting another; discard GPU storage after failure.
    fn gpu_frame(
        &mut self,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<gpu::GpuFrame<'_>>> {
        Ok(None)
    }
    fn discard_gpu(&mut self) -> Result<()> {
        Ok(())
    }
    fn invalidate(&mut self) -> Result<()>;
}
