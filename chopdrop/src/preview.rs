//! Sample previews for the browser and the chop editor.
//!
//! Two halves around one [`PreviewChannel`]. The [`Previewer`], on the GUI
//! side, decodes the file the browser asks for on a worker thread of its own
//! and publishes it into the channel. The engine reads the channel once a
//! block and plays what it finds on a voice of its own, outside the lane pool
//! ([`crate::engine::Engine::with_preview`]).
//!
//! The chop editor's source is already decoded, so its requests
//! ([`Previewer::play_region`], [`Previewer::play_from`]) skip the worker and
//! publish at once. A region's edges and loop flag are also mirrored in live
//! atomics on the channel, which the GUI rewrites on every edge drag and the
//! engine rereads at the loop seam, so dragging an edge never republishes.
//!
//! Pure: no nice-plug types. The channel is a `basedrop::SharedCell`, like the
//! published lanes, so the audio thread only ever does a `get` and a relaxed
//! store, and everything replaced is freed by the model's collector on the GUI
//! side.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use basedrop::{Handle, Shared, SharedCell};

use crate::model::RepaintHook;
use crate::sample::{self, SampleData};

/// What the engine should be previewing.
///
/// A new `generation` is the signal: the engine remembers the last one it
/// acted on, and a different one means "stop what you are previewing, and
/// play `sample` if there is one".
pub struct PreviewState {
    /// Bumped by every [`Previewer::play`], [`Previewer::play_region`],
    /// [`Previewer::play_from`] and [`Previewer::stop`].
    pub generation: u64,
    /// The decoded audio to play, or `None` for silence.
    pub sample: Option<Shared<SampleData>>,
    /// The frames to play, or `None` for the whole sample from its start
    /// (the browser's previews).
    pub region: Option<Region>,
    /// Whether the region starts as a loop. The live flag on the channel
    /// ([`PreviewChannel::live_looping`]) is what the engine obeys afterwards.
    pub looping: bool,
}

impl PreviewState {
    /// A request for silence under `generation`.
    fn silence(generation: u64) -> PreviewState {
        PreviewState { generation, sample: None, region: None, looping: false }
    }
}

/// A span of a sample in frames, `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

impl Region {
    /// `self` clamped into a sample of `total` frames: `start <= end <= total`.
    pub fn clamped(self, total: u64) -> Region {
        let end = self.end.min(total);
        Region { start: self.start.min(end), end }
    }
}

/// The one place the GUI side and the audio thread meet over previews.
pub struct PreviewChannel {
    /// What to preview. Written by the previewer's worker (and by `stop`),
    /// read by the engine once a block.
    state: SharedCell<PreviewState>,
    /// Whether a preview voice was sounding at the end of the last block.
    /// Written by the engine, read by the browser's play button.
    playing: AtomicBool,
    /// The region's current edges and loop flag. Written by the GUI (on
    /// publish and on every edge change), read by the engine at the seam.
    live_start: AtomicU64,
    live_end: AtomicU64,
    live_looping: AtomicBool,
    /// The preview voice's frame in its sample, written by the engine each
    /// block, read by the chop strip for its play hairline.
    position: AtomicU64,
}

impl PreviewChannel {
    /// A channel previewing nothing, allocated against `handle` (the model's
    /// collector, which frees whatever the channel lets go of).
    pub fn new(handle: &Handle) -> PreviewChannel {
        PreviewChannel {
            state: SharedCell::new(Shared::new(
                handle,
                PreviewState::silence(0),
            )),
            playing: AtomicBool::new(false),
            live_start: AtomicU64::new(0),
            live_end: AtomicU64::new(0),
            live_looping: AtomicBool::new(false),
            position: AtomicU64::new(0),
        }
    }

    /// The current request. One refcount bump, so safe on the audio thread.
    pub fn get(&self) -> Shared<PreviewState> {
        self.state.get()
    }

    /// The engine saying whether a preview is sounding. A relaxed store.
    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Relaxed);
    }

    /// Whether a preview was sounding at the end of the last block.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Moves the live region's edges (GUI side, on every edge change). The
    /// engine picks them up at the next seam. Stored as given: the engine
    /// clamps against the sample it is playing.
    pub fn set_live_region(&self, region: Region) {
        self.live_start.store(region.start, Ordering::Relaxed);
        self.live_end.store(region.end, Ordering::Relaxed);
    }

    /// The live region's edges. Relaxed loads, safe on the audio thread.
    pub fn live_region(&self) -> Region {
        Region {
            start: self.live_start.load(Ordering::Relaxed),
            end: self.live_end.load(Ordering::Relaxed),
        }
    }

    /// Turns looping on or off for the region already playing.
    pub fn set_live_looping(&self, looping: bool) {
        self.live_looping.store(looping, Ordering::Relaxed);
    }

    /// Whether the region should loop at its next seam.
    pub fn live_looping(&self) -> bool {
        self.live_looping.load(Ordering::Relaxed)
    }

    /// The engine reporting the preview voice's frame. A relaxed store.
    pub fn set_position(&self, frame: u64) {
        self.position.store(frame, Ordering::Relaxed);
    }

    /// The preview voice's frame as of the engine's last block.
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    /// Replaces the request. GUI side only: allocates.
    pub(crate) fn publish(&self, handle: &Handle, state: PreviewState) {
        self.state.set(Shared::new(handle, state));
    }
}

/// Who asked for the current preview: the browser's [`Previewer::play`], or
/// the chop strip's [`Previewer::play_region`] / [`Previewer::play_from`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewOwner {
    Browser,
    Strip,
}

/// [`PreviewOwner`] as stored in [`Inner::owner`]: 0 is nobody.
const OWNER_NONE: u8 = 0;
const OWNER_BROWSER: u8 = 1;
const OWNER_STRIP: u8 = 2;

impl PreviewOwner {
    fn code(owner: Option<PreviewOwner>) -> u8 {
        match owner {
            None => OWNER_NONE,
            Some(PreviewOwner::Browser) => OWNER_BROWSER,
            Some(PreviewOwner::Strip) => OWNER_STRIP,
        }
    }

    fn from_code(code: u8) -> Option<PreviewOwner> {
        match code {
            OWNER_BROWSER => Some(PreviewOwner::Browser),
            OWNER_STRIP => Some(PreviewOwner::Strip),
            _ => None,
        }
    }
}

/// Where the previewer has got to, for the browser's status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewStatus {
    /// Nothing asked for, or stopped.
    Idle,
    /// Decoding this file.
    Loading(PathBuf),
    /// Handed this file to the engine.
    Playing(PathBuf),
    /// This file could not be previewed, and why (the loader's own words).
    Failed(PathBuf, String),
}

/// One preview request for the worker.
struct Job {
    generation: u64,
    path: PathBuf,
}

/// What the worker and the GUI side share.
struct Inner {
    channel: Arc<PreviewChannel>,
    handle: Handle,
    /// The newest generation asked for, by `play` or `stop`.
    latest: AtomicU64,
    /// Who made the newest `play`/`play_region`/`play_from`, cleared by
    /// `stop` (a [`PreviewOwner`] code). GUI side only.
    owner: AtomicU8,
    status: Mutex<PreviewStatus>,
    repaint: Mutex<Option<RepaintHook>>,
}

impl Inner {
    fn status(&self) -> MutexGuard<'_, PreviewStatus> {
        self.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn ring(&self) {
        let hook = self.repaint.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// The GUI side of previews: one per plugin instance.
///
/// `play` and `stop` return at once; decoding happens on the
/// `chopdrop-preview` thread. Asking again before a decode finishes simply
/// replaces the request, so arrowing quickly through a folder plays the file
/// the selection stops on, not every file it passed.
pub struct Previewer {
    shared: Arc<Inner>,
    jobs: Option<Sender<Job>>,
    worker: Option<JoinHandle<()>>,
}

impl Previewer {
    /// A previewer publishing into `channel`, allocating against `handle`
    /// (the model's collector). Starts its worker thread; a thread that
    /// cannot be started leaves previews silently unavailable rather than
    /// failing the plugin.
    pub fn new(channel: Arc<PreviewChannel>, handle: Handle) -> Previewer {
        let shared = Arc::new(Inner {
            channel,
            handle,
            latest: AtomicU64::new(0),
            owner: AtomicU8::new(OWNER_NONE),
            status: Mutex::new(PreviewStatus::Idle),
            repaint: Mutex::new(None),
        });
        let (jobs, receiver) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("chopdrop-preview".into())
            .spawn(move || work(&worker_shared, &receiver))
            .ok();
        let jobs = worker.as_ref().map(|_| jobs);
        Previewer { shared, jobs, worker }
    }

    /// The channel the engine reads.
    pub fn channel(&self) -> &Arc<PreviewChannel> {
        &self.shared.channel
    }

    /// Rung when a decode finishes or fails, so the browser's status line
    /// changes without waiting for the pointer to move.
    pub fn set_repaint_hook(&self, hook: RepaintHook) {
        *self.shared.repaint.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook);
    }

    /// Preview `path` from its start, replacing whatever is previewing.
    pub fn play(&self, path: &Path) {
        let generation = self.shared.latest.fetch_add(1, Ordering::Relaxed) + 1;
        self.set_owner(Some(PreviewOwner::Browser));
        *self.shared.status() = PreviewStatus::Loading(path.to_path_buf());
        let sent = self
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(Job { generation, path: path.to_path_buf() }).is_ok());
        if !sent {
            *self.shared.status() =
                PreviewStatus::Failed(path.to_path_buf(), "preview unavailable".into());
        }
    }

    /// Preview `region` of an already decoded `source` (the chop editor's),
    /// looping it if `looping`, replacing whatever is previewing. No decode
    /// and no worker: published at once, and any decode still in flight is
    /// made stale. The region is clamped into the source; the live edges and
    /// loop flag are reset to it before it is published. The browser's
    /// status goes back to [`PreviewStatus::Idle`]: it names files the
    /// browser asked for, and this is not one.
    pub fn play_region(&self, source: Shared<SampleData>, region: Region, looping: bool) {
        let region = region.clamped(source.frame_count() as u64);
        let generation = self.shared.latest.fetch_add(1, Ordering::Relaxed) + 1;
        self.set_owner(Some(PreviewOwner::Strip));
        let channel = &self.shared.channel;
        channel.set_live_region(region);
        channel.set_live_looping(looping);
        channel.set_position(region.start);
        channel.publish(
            &self.shared.handle,
            PreviewState { generation, sample: Some(source), region: Some(region), looping },
        );
        *self.shared.status() = PreviewStatus::Idle;
    }

    /// Preview `source` from `frame` to its end, once (a click in the chop
    /// strip's main view).
    pub fn play_from(&self, source: Shared<SampleData>, frame: u64) {
        let total = source.frame_count() as u64;
        self.play_region(source, Region { start: frame, end: total }, false);
    }

    /// Stop previewing (with the voice's short fade). Cheap when nothing is.
    /// Also clears the live loop flag, so nothing restarts at a seam.
    pub fn stop(&self) {
        let generation = self.shared.latest.fetch_add(1, Ordering::Relaxed) + 1;
        self.set_owner(None);
        self.shared.channel.set_live_looping(false);
        self.shared.channel.publish(&self.shared.handle, PreviewState::silence(generation));
        *self.shared.status() = PreviewStatus::Idle;
    }

    /// [`Previewer::stop`], but only if the browser asked for what is
    /// previewing: the browser's own gestures (closing the dock, switching
    /// its tab, changing folder) leave the chop strip's preview sounding.
    pub fn stop_browser(&self) {
        if self.owner() == Some(PreviewOwner::Browser) {
            self.stop();
        }
    }

    /// Who asked for the current preview: `Browser` after [`Previewer::play`],
    /// `Strip` after [`Previewer::play_region`] or [`Previewer::play_from`],
    /// `None` before any and after [`Previewer::stop`]. Says who asked, not
    /// whether it sounds ([`Previewer::is_playing`] says that).
    pub fn owner(&self) -> Option<PreviewOwner> {
        PreviewOwner::from_code(self.shared.owner.load(Ordering::Relaxed))
    }

    fn set_owner(&self, owner: Option<PreviewOwner>) {
        self.shared.owner.store(PreviewOwner::code(owner), Ordering::Relaxed);
    }

    /// Where the previewer has got to.
    pub fn status(&self) -> PreviewStatus {
        self.shared.status().clone()
    }

    /// Whether a preview is sounding (as of the engine's last block).
    pub fn is_playing(&self) -> bool {
        self.shared.channel.is_playing()
    }
}

impl Drop for Previewer {
    /// Closes the queue and waits for the worker, which finishes at most the
    /// one decode it is in.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The worker: decode the newest request, publish it if it is still wanted.
fn work(shared: &Inner, jobs: &Receiver<Job>) {
    while let Ok(mut job) = jobs.recv() {
        // Only the newest queued request matters.
        while let Ok(newer) = jobs.try_recv() {
            job = newer;
        }
        if job.generation != shared.latest.load(Ordering::Relaxed) {
            continue;
        }

        let decoded = decode(&job.path);

        // Checked again: the decode took real time, and a newer `play` or a
        // `stop` in the meantime means this one is no longer wanted.
        if job.generation != shared.latest.load(Ordering::Relaxed) {
            continue;
        }
        match decoded {
            Ok(data) => {
                let sample = Shared::new(&shared.handle, data);
                shared.channel.publish(
                    &shared.handle,
                    PreviewState {
                        generation: job.generation,
                        sample: Some(sample),
                        region: None,
                        looping: false,
                    },
                );
                *shared.status() = PreviewStatus::Playing(job.path);
            }
            Err(message) => {
                // Silence whatever was previewing, so a failed file is not
                // heard as the file before it.
                shared.channel.publish(&shared.handle, PreviewState::silence(job.generation));
                *shared.status() = PreviewStatus::Failed(job.path, message.into());
            }
        }
        shared.ring();
    }
}

/// Read and decode a file exactly as the loader does, same caps and same
/// messages.
fn decode(path: &Path) -> Result<SampleData, Cow<'static, str>> {
    let bytes = sample::read_file(path).map_err(|error| error.message())?;
    let extension = path.extension().and_then(|extension| extension.to_str());
    sample::decode(&bytes, extension).map_err(|error| error.message())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use basedrop::Collector;

    use super::*;

    /// A tiny mono 16-bit WAV of `frames` frames, every sample `value`.
    fn wav(frames: usize, value: i16) -> Vec<u8> {
        let data_len = (frames * 2) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&96_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for _ in 0..frames {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn scratch(name: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chopdrop-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("scratch file");
        path
    }

    /// Waits (bounded) for the previewer to leave `Loading`.
    fn settle(previewer: &Previewer) -> PreviewStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = previewer.status();
            if !matches!(status, PreviewStatus::Loading(_)) || Instant::now() > deadline {
                return status;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_played_file_is_published_with_a_new_generation() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());
        let path = scratch("one.wav", &wav(100, 1000));

        assert_eq!(channel.get().generation, 0);
        previewer.play(&path);
        assert_eq!(settle(&previewer), PreviewStatus::Playing(path.clone()));

        let state = channel.get();
        assert_eq!(state.generation, 1);
        let sample = state.sample.as_ref().expect("a sample");
        assert_eq!(sample.frame_count(), 100);
    }

    #[test]
    fn stop_publishes_silence_under_a_newer_generation() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());
        let path = scratch("two.wav", &wav(50, 1000));

        previewer.play(&path);
        settle(&previewer);
        previewer.stop();

        let state = channel.get();
        assert_eq!(state.generation, 2);
        assert!(state.sample.is_none());
        assert_eq!(previewer.status(), PreviewStatus::Idle);
    }

    #[test]
    fn a_file_that_will_not_decode_says_why_and_silences_the_last_one() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());
        let good = scratch("good.wav", &wav(50, 1000));
        let bad = scratch("bad.wav", b"not audio at all");

        previewer.play(&good);
        settle(&previewer);
        previewer.play(&bad);
        match settle(&previewer) {
            PreviewStatus::Failed(path, message) => {
                assert_eq!(path, bad);
                assert!(!message.is_empty());
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(channel.get().sample.is_none());
    }

    #[test]
    fn a_stop_while_decoding_wins_over_the_decode() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());
        let path = scratch("three.wav", &wav(20_000, 1000));

        previewer.play(&path);
        previewer.stop();
        // Whatever the worker does with the stale job, it must not publish it.
        thread::sleep(Duration::from_millis(200));
        let state = channel.get();
        assert_eq!(state.generation, 2);
        assert!(state.sample.is_none());
    }

    #[test]
    fn the_repaint_hook_is_rung_when_a_preview_lands() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(channel, collector.handle());
        let rung = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&rung);
        previewer.set_repaint_hook(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));

        previewer.play(&scratch("four.wav", &wav(10, 1)));
        // The hook is rung just after the status changes, so wait on the hook.
        let deadline = Instant::now() + Duration::from_secs(5);
        while rung.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(rung.load(Ordering::Relaxed), 1);
    }

    // ---- The chop editor's previews ----------------------------------------

    /// An already decoded mono source of `frames` frames, as the chop editor
    /// holds one.
    fn source(collector: &Collector, frames: usize) -> Shared<SampleData> {
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.25; frames].into() };
        Shared::new(&collector.handle(), data)
    }

    #[test]
    fn play_region_publishes_the_region_at_once_under_a_new_generation() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());
        let src = source(&collector, 1000);

        previewer.play_region(src.clone(), Region { start: 100, end: 400 }, true);

        // No worker involved: visible straight away.
        let state = channel.get();
        assert_eq!(state.generation, 1);
        assert!(std::ptr::eq(&**state.sample.as_ref().expect("a sample"), &*src));
        assert_eq!(state.region, Some(Region { start: 100, end: 400 }));
        assert!(state.looping);
        assert_eq!(channel.live_region(), Region { start: 100, end: 400 });
        assert!(channel.live_looping());
        assert_eq!(channel.position(), 100);
        assert_eq!(previewer.status(), PreviewStatus::Idle);

        previewer.play_region(src, Region { start: 0, end: 10 }, false);
        let state = channel.get();
        assert_eq!(state.generation, 2);
        assert_eq!(state.region, Some(Region { start: 0, end: 10 }));
        assert!(!state.looping);
        assert!(!channel.live_looping());
    }

    #[test]
    fn play_region_clamps_into_the_source() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());

        previewer.play_region(source(&collector, 500), Region { start: 300, end: 9000 }, false);
        assert_eq!(channel.get().region, Some(Region { start: 300, end: 500 }));

        previewer.play_region(source(&collector, 500), Region { start: 800, end: 700 }, false);
        assert_eq!(channel.get().region, Some(Region { start: 500, end: 500 }));
        assert_eq!(channel.live_region(), Region { start: 500, end: 500 });
    }

    #[test]
    fn play_from_publishes_frame_to_the_end_without_looping() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());
        channel.set_live_looping(true);

        previewer.play_from(source(&collector, 1000), 250);

        let state = channel.get();
        assert_eq!(state.generation, 1);
        assert_eq!(state.region, Some(Region { start: 250, end: 1000 }));
        assert!(!state.looping);
        assert_eq!(channel.live_region(), Region { start: 250, end: 1000 });
        assert!(!channel.live_looping());
    }

    #[test]
    fn stop_after_a_region_clears_it_and_the_live_loop() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());

        previewer.play_region(source(&collector, 1000), Region { start: 10, end: 20 }, true);
        previewer.stop();

        let state = channel.get();
        assert_eq!(state.generation, 2);
        assert!(state.sample.is_none());
        assert!(state.region.is_none());
        assert!(!state.looping);
        assert!(!channel.live_looping());
        assert_eq!(previewer.status(), PreviewStatus::Idle);
    }

    #[test]
    fn live_edges_change_without_republishing() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());

        previewer.play_region(source(&collector, 1000), Region { start: 10, end: 20 }, false);
        channel.set_live_region(Region { start: 30, end: 90 });
        channel.set_live_looping(true);
        channel.set_position(42);

        assert_eq!(channel.live_region(), Region { start: 30, end: 90 });
        assert!(channel.live_looping());
        assert_eq!(channel.position(), 42);
        let state = channel.get();
        assert_eq!(state.generation, 1, "edge changes publish nothing");
        assert_eq!(state.region, Some(Region { start: 10, end: 20 }));
        assert!(!state.looping);
    }

    #[test]
    fn a_region_makes_a_pending_decode_stale() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());
        let path = scratch("five.wav", &wav(20_000, 1000));

        previewer.play(&path);
        previewer.play_region(source(&collector, 100), Region { start: 0, end: 100 }, false);
        thread::sleep(Duration::from_millis(200));

        let state = channel.get();
        assert_eq!(state.generation, 2);
        assert_eq!(state.region, Some(Region { start: 0, end: 100 }));
        assert_eq!(state.sample.as_ref().map(|sample| sample.frame_count()), Some(100));
    }

    #[test]
    fn the_browsers_play_still_publishes_a_whole_file_without_a_region() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(Arc::clone(&channel), collector.handle());
        let path = scratch("six.wav", &wav(64, 1000));

        previewer.play_region(source(&collector, 100), Region { start: 0, end: 50 }, true);
        previewer.play(&path);
        assert_eq!(settle(&previewer), PreviewStatus::Playing(path));

        let state = channel.get();
        assert_eq!(state.generation, 2);
        assert_eq!(state.sample.as_ref().map(|sample| sample.frame_count()), Some(64));
        assert!(state.region.is_none());
        assert!(!state.looping);
    }

    // ---- Whose preview it is -----------------------------------------------

    #[test]
    fn owner_follows_every_play_call_and_stop_clears_it() {
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(channel, collector.handle());
        let path = scratch("seven.wav", &wav(32, 1000));
        assert_eq!(previewer.owner(), None, "nobody yet");

        previewer.play(&path);
        assert_eq!(previewer.owner(), Some(PreviewOwner::Browser), "at once, before the decode");
        settle(&previewer);
        assert_eq!(previewer.owner(), Some(PreviewOwner::Browser));

        previewer.play_region(source(&collector, 100), Region { start: 0, end: 50 }, true);
        assert_eq!(previewer.owner(), Some(PreviewOwner::Strip));
        previewer.stop();
        assert_eq!(previewer.owner(), None);

        previewer.play_from(source(&collector, 100), 10);
        assert_eq!(previewer.owner(), Some(PreviewOwner::Strip));
        previewer.play(&path);
        assert_eq!(previewer.owner(), Some(PreviewOwner::Browser), "the newest asker wins");
        previewer.stop();
        assert_eq!(previewer.owner(), None);
        previewer.stop();
        assert_eq!(previewer.owner(), None, "a second stop is harmless");
    }
}
