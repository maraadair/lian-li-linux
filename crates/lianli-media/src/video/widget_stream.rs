use crate::{resource_budget::RetainedBudget, MediaError, PreparationControl};
use image::RgbaImage;
use lianli_shared::template::ImageFit;
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CACHE_BYTES: usize = 32 * 1024 * 1024;
const CACHE_FRAMES: usize = 256;
const QUEUE_BYTES: usize = 16 * 1024 * 1024;
const MAX_DECODERS: usize = 8;
const READ_TIMEOUT: Duration = Duration::from_secs(20);
static DECODERS: AtomicUsize = AtomicUsize::new(0);

struct DecoderSlot;

impl DecoderSlot {
    fn acquire() -> Result<Self, MediaError> {
        DECODERS
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                (used < MAX_DECODERS).then_some(used + 1)
            })
            .map_err(|_| {
                MediaError::InvalidConfig(
                    "At most eight video widget decoders can be prepared or playing at once".into(),
                )
            })?;
        Ok(Self)
    }
}

impl Drop for DecoderSlot {
    fn drop(&mut self) {
        DECODERS.fetch_sub(1, Ordering::Relaxed);
    }
}

struct TimedFrame {
    image: RgbaImage,
    duration: Duration,
}

#[derive(Default)]
struct Cache {
    frames: Vec<Arc<TimedFrame>>,
    budget: RetainedBudget,
}

#[derive(Default)]
struct Frames {
    ready: VecDeque<Arc<TimedFrame>>,
    reusable: Vec<Vec<u8>>,
    cache: Option<Cache>,
    finished: bool,
    error: Option<String>,
}

type SharedFrames = Arc<(Mutex<Frames>, Condvar)>;

pub(crate) struct VideoStream {
    shared: SharedFrames,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    current: Option<Arc<TimedFrame>>,
    next: Option<Instant>,
    started: Option<Instant>,
    queue_capacity: usize,
    looping: bool,
    recycle: bool,
    sample_interval: Option<Duration>,
    sample_next: Option<Instant>,
    _budget: RetainedBudget,
}

fn queue_capacity(bytes: usize, fps: f32) -> usize {
    ((fps * 0.15).ceil() as usize)
        .clamp(2, 8)
        .min((QUEUE_BYTES / bytes).max(2))
}

struct Producer {
    shared: SharedFrames,
    stop: Arc<AtomicBool>,
    capacity: usize,
    bytes: usize,
    candidate: Option<Cache>,
}

impl Producer {
    fn check(&self) -> Result<(), MediaError> {
        if self.stop.load(Ordering::Relaxed) {
            Err(MediaError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn buffer(&self) -> Result<Vec<u8>, MediaError> {
        self.check()?;
        let buffer = self.shared.0.lock().reusable.pop();
        Ok(buffer.unwrap_or_else(|| vec![0; self.bytes]))
    }

    fn emit(&mut self, image: RgbaImage, duration: Duration) -> Result<(), MediaError> {
        self.check()?;
        let frame = Arc::new(TimedFrame { image, duration });
        if let Some(cache) = &mut self.candidate {
            if cache.frames.len() < CACHE_FRAMES
                && cache.frames.len() < (CACHE_BYTES / self.bytes).max(1)
                && cache.budget.reserve(self.bytes).is_ok()
            {
                cache.frames.push(frame);
                return Ok(());
            }
            // A cache is optional: memory pressure falls back to bounded playback.
            let cache = self.candidate.take().expect("candidate cache");
            for cached in cache.frames {
                self.enqueue(cached)?;
            }
        }
        self.enqueue(frame)
    }

    fn enqueue(&self, frame: Arc<TimedFrame>) -> Result<(), MediaError> {
        let mut frames = self.shared.0.lock();
        while frames.ready.len() >= self.capacity {
            self.check()?;
            self.shared.1.wait(&mut frames);
        }
        self.check()?;
        frames.ready.push_back(frame);
        self.shared.1.notify_all();
        Ok(())
    }

    fn finish_cache(&mut self) -> bool {
        if let Some(cache) = self.candidate.take() {
            self.shared.0.lock().cache = Some(cache);
            true
        } else {
            false
        }
    }
}

impl VideoStream {
    pub(crate) fn new(
        path: &Path,
        fps: f32,
        size: (u32, u32),
        fit: ImageFit,
        looping: bool,
        control: &PreparationControl,
    ) -> Result<Self, MediaError> {
        control.check()?;
        if !fps.is_finite() || !(1.0..=60.0).contains(&fps) {
            return Err(MediaError::InvalidFps);
        }
        let bytes = super::frame_budget::rgba_bytes(size.0, size.1)?;
        let slot = DecoderSlot::acquire()?;
        let capacity = queue_capacity(bytes, fps);
        let mut budget = RetainedBudget::default();
        // Include the displayed frame, producer handoff and raw pipe assembly.
        budget.reserve(bytes * (capacity + 3))?;
        let shared = Arc::new((Mutex::new(Frames::default()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut producer = Producer {
            shared: shared.clone(),
            stop: stop.clone(),
            capacity,
            bytes,
            candidate: Some(Cache::default()),
        };
        let recycle = !super::widget_animation::is_animation(path);
        let path = path.to_owned();
        let hardware = control.hardware_video;
        let worker = thread::Builder::new()
            .name("widget-video".into())
            .spawn(move || {
                let _slot = slot;
                let result = produce(&path, fps, size, fit, looping, hardware, &mut producer);
                let mut frames = producer.shared.0.lock();
                frames.finished = true;
                if let Err(error) = result {
                    if !matches!(error, MediaError::Cancelled) {
                        frames.error = Some(error.to_string());
                    }
                }
                producer.shared.1.notify_all();
            })?;
        let mut stream = Self {
            shared,
            stop,
            worker: Some(worker),
            current: None,
            next: None,
            started: None,
            queue_capacity: capacity,
            looping,
            recycle,
            sample_interval: (!recycle).then(|| Duration::from_secs_f64(1.0 / f64::from(fps))),
            sample_next: None,
            _budget: budget,
        };
        {
            let mut frames = stream.shared.0.lock();
            loop {
                control.check()?;
                let first = frames.ready.pop_front().or_else(|| {
                    frames
                        .cache
                        .as_ref()
                        .and_then(|cache| cache.frames.first().cloned())
                });
                if let Some(first) = first {
                    stream.current = Some(first);
                    stream.shared.1.notify_all();
                    break;
                }
                if frames.finished {
                    return Err(frames
                        .error
                        .as_ref()
                        .map_or(MediaError::EmptyVideo, |error| {
                            MediaError::Ffmpeg(error.clone())
                        }));
                }
                stream
                    .shared
                    .1
                    .wait_for(&mut frames, Duration::from_millis(20));
            }
        }
        if stream.shared.0.lock().cache.is_some() {
            if let Some(worker) = stream.worker.take() {
                worker
                    .join()
                    .map_err(|_| MediaError::Ffmpeg("Video widget decoder panicked".into()))?;
            }
            stream._budget = RetainedBudget::default();
        }
        Ok(stream)
    }

    pub(crate) fn advance(&mut self, now: Instant) -> Result<bool, MediaError> {
        if let Some(interval) = self.sample_interval {
            let next = self.sample_next.unwrap_or(now);
            if now < next {
                return Ok(false);
            }
            self.sample_next = Some(if next + interval > now {
                next + interval
            } else {
                now + interval
            });
        }
        let started = *self.started.get_or_insert(now);
        let current = self.current.as_ref().expect("prepared frame");
        let mut frames = self.shared.0.lock();
        if let Some(cache) = &frames.cache {
            let total: Duration = cache.frames.iter().map(|frame| frame.duration).sum();
            let elapsed = now.saturating_duration_since(started).as_nanos();
            let mut position = if self.looping {
                elapsed % total.as_nanos()
            } else {
                elapsed.min(total.as_nanos() - 1)
            };
            let frame = cache
                .frames
                .iter()
                .find(|frame| {
                    if position < frame.duration.as_nanos() {
                        true
                    } else {
                        position -= frame.duration.as_nanos();
                        false
                    }
                })
                .expect("position within cache");
            let changed = !Arc::ptr_eq(current, frame);
            self.current = Some(frame.clone());
            return Ok(changed);
        }
        let mut next = *self.next.get_or_insert(now + current.duration);
        if now < next {
            return Ok(false);
        }
        if frames.ready.is_empty() {
            if let Some(error) = &frames.error {
                return Err(MediaError::Ffmpeg(error.clone()));
            }
            return Ok(false);
        }
        let mut changed = false;
        for _ in 0..self.queue_capacity {
            if now < next {
                break;
            }
            let Some(frame) = frames.ready.pop_front() else {
                break;
            };
            next += frame.duration;
            if let Some(old) = self.current.replace(frame) {
                if self.recycle && frames.ready.len() + frames.reusable.len() < self.queue_capacity
                {
                    if let Ok(old) = Arc::try_unwrap(old) {
                        frames.reusable.push(old.image.into_raw());
                    }
                }
            }
            changed = true;
        }
        self.next = Some(if next > now {
            next
        } else {
            now + self.current.as_ref().expect("prepared frame").duration
        });
        self.shared.1.notify_all();
        Ok(changed)
    }

    pub(crate) fn frame(&self) -> &RgbaImage {
        &self
            .current
            .as_ref()
            .expect("prepared video has its first frame")
            .image
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        {
            let _frames = self.shared.0.lock();
            self.stop.store(true, Ordering::Relaxed);
            self.shared.1.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::warn!("Video widget decoder panicked during shutdown");
            }
        }
    }
}

fn produce(
    path: &Path,
    fps: f32,
    size: (u32, u32),
    fit: ImageFit,
    looping: bool,
    hardware: bool,
    producer: &mut Producer,
) -> Result<(), MediaError> {
    let animation = super::widget_animation::is_animation(path);
    let mut first_pass = true;
    loop {
        producer.check()?;
        if animation {
            let cancelled = producer.stop.clone();
            super::widget_animation::decode(path, size, fit, fps, &cancelled, |frame, delay| {
                producer.emit(frame, delay)
            })?;
        } else {
            let command =
                super::ffmpeg::rgba_command(path, fps, size, fit, looping && !first_pass, hardware);
            let cancelled = producer.stop.clone();
            let mut count = 0;
            let output = super::process::stream_live_frames(
                command,
                READ_TIMEOUT,
                &cancelled,
                producer.bytes,
                |rgba| {
                    count += 1;
                    let mut buffer = producer.buffer()?;
                    buffer.copy_from_slice(rgba);
                    let frame =
                        RgbaImage::from_raw(size.0, size.1, buffer).expect("validated RGBA size");
                    producer.emit(frame, Duration::from_secs_f64(1.0 / f64::from(fps)))
                },
            )?;
            super::ffmpeg::check_rgba_output(output)?;
            if count == 0 {
                return Err(MediaError::EmptyVideo);
            }
        }
        if producer.finish_cache() || !looping {
            return Ok(());
        }
        first_pass = false;
    }
}

#[cfg(test)]
#[path = "widget_stream_tests.rs"]
mod tests;
