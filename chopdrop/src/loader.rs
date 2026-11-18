//! The background loader: file reading and decoding, off every thread that
//! cannot afford to wait for it.
//!
//! One worker thread per plugin instance ([`Loader`]), fed jobs of "this
//! lane, this [`Source`]". A job reads the file (or takes the bytes a
//! project embedded), decodes it through `sample.rs`, and then — this is the
//! part worth stating twice — **applies the result itself** through the
//! model's writer API. Nothing waits for the result and nothing has to be
//! told about it: the lane simply has a sample (or a message) the next time
//! anyone looks, whether that is the audio thread reading the published
//! snapshot or an editor that may not even be open.
//!
//! That last point is the design, not an incidental: a project restore
//! happens with the editor closed (the host calls `set_state` whenever it
//! likes), so a loader whose results were collected by the GUI would restore
//! a project's samples only if someone opened the window. Here the worker is
//! a writer like the GUI is — same mutex, same clamping setters, same
//! publish-and-collect tail ([`crate::model::SharedModel`]) — so a headless
//! restore ends with the samples published and playable.
//!
//! The one thing the worker does say out loud is that it has written:
//! [`Model::request_repaint`] once per landed result, because a window
//! nobody is touching draws no frames and the lane it changed would sit on
//! "loading…" until the next mouse move.
//!
//! # Why a thread at all
//!
//! Decoding is neither quick nor bounded: a four-minute MP3 is tens of
//! millions of samples and real tens of milliseconds, a cold file read is a
//! disk seek, and both scale with what the user drops. The audio thread
//! cannot block for any of it (it may not allocate, lock or block) and the
//! GUI thread must not either — a frame callback that decodes is a window
//! that freezes mid-drag, with eight files still to go.
//!
//! # What a lane shows while it waits
//!
//! [`Loader::queue`] marks the lane as loading before the job leaves, and
//! the result clears it, so the GUI has a flag to draw "loading…" from
//! without knowing anything about jobs or threads. A lane that is deleted
//! meanwhile takes no result: jobs name lanes by id, and ids are never
//! reused.
//!
//! # Several jobs for one lane
//!
//! A user who drops twice on the same lane, or drops while a restore is
//! still decoding, gets the **last** job's result and no flicker from the
//! earlier ones: every job carries a monotonic id, the newest id queued per
//! lane is kept beside the worker, and a result that is no longer the newest
//! is thrown away rather than written ([`Worker::claim`]). The worker is
//! single-threaded, so the last job would win on arrival order alone; what
//! dropping the stale result adds is that an earlier failure cannot put its
//! message on a lane that is already loading something else.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crate::chop::{self, ChopOrigin, Peaks};
use crate::model::{Model, SampleRef, SharedModel, lock_model};
use crate::sample::{self, DecodeError, SampleData};
use crate::state::PendingSample;

/// Where a job's audio comes from.
///
/// The two shapes a job's source takes — a path to read or bytes already
/// held — plus the one difference between them that outlives the decode: how
/// a failure is worded, and whether success is something a user can undo.
#[derive(Debug)]
pub enum Source {
    /// A file the user chose in this session: a drop or "reload from file".
    ///
    /// Its failures are the decoder's own ([`DecodeError::message`]) —
    /// the user is looking at the file they just dropped, so "unsupported
    /// audio file" is exactly the useful thing to say — and its success is
    /// an undo step, because dropping a sample onto a lane is a gesture.
    File(PathBuf),
    /// A sample a project load is restoring, embedded bytes or a path to go
    /// and read.
    ///
    /// Its failures are the project's words ([`PendingSample::failure_message`])
    /// rather than the decoder's, and its success is **not** an undo step:
    /// opening a project is not an edit, and the samples arriving are the
    /// state undo should return to ([`Model::rebase`]).
    Restore(PendingSample),
    /// A chop-editor source: a job with no lane. Read and decoded under
    /// [`sample::SOURCE_LIMITS`] and the seconds cap, its peaks built here,
    /// and the result handed to [`Model::set_source`] /
    /// [`Model::source_failed`]. Queue it with [`Loader::open_source`]
    /// (which [`Loader::queue`] forwards to).
    ChopSource(PathBuf),
}

impl Source {
    /// Take the source apart into what the worker needs, before anything is
    /// decoded.
    ///
    /// Only lane sources come here: the worker takes a
    /// [`Source::ChopSource`] apart itself.
    fn into_work(self) -> Work {
        match self {
            Source::File(path) | Source::ChopSource(path) => {
                Work { path, embedded: None, restore_failure: None, origin: None }
            }
            Source::Restore(pending) => Work {
                // Worked out here, ahead of the decode, because the sentence
                // depends on whether the project embedded the bytes — and
                // the next two lines move those bytes out.
                restore_failure: Some(pending.failure_message()),
                path: PathBuf::from(pending.path),
                embedded: pending.bytes,
                origin: pending.origin,
            },
        }
    }
}

/// A job's source, unpacked: what to decode and how to report it.
struct Work {
    /// The file the sample came from, for the decoder's extension hint and
    /// for the [`SampleRef`] the lane will show and save.
    path: PathBuf,
    /// The bytes, when the source brought its own (a project's embedded
    /// copy); `None` means read [`path`](Self::path).
    embedded: Option<Vec<u8>>,
    /// The project's wording for a failure, for a [`Source::Restore`]; the
    /// `None` of a [`Source::File`], whose failures the decoder words, is
    /// also what marks its success as undoable — see
    /// [`undoable`](Self::undoable).
    restore_failure: Option<String>,
    /// The chop origin a restored chop's [`SampleRef`] keeps; `None` for
    /// everything else.
    origin: Option<ChopOrigin>,
}

impl Work {
    /// The sentence this job's lane shows when the load fails.
    fn failure(&self, error: DecodeError) -> Cow<'_, str> {
        match &self.restore_failure {
            Some(failure) => Cow::Borrowed(failure),
            None => error.message(),
        }
    }

    /// Whether landing this job's sample is an undo step.
    ///
    /// The same bit as "does it have its own failure wording", and for the
    /// same underlying reason: a [`Source::File`] is a gesture a user just
    /// made, in this session, and a [`Source::Restore`] is a project
    /// arriving. Both answers follow from which of the two it is, so there
    /// is one field rather than two that could disagree.
    fn undoable(&self) -> bool {
        self.restore_failure.is_none()
    }
}

/// One unit of work for the worker thread.
struct Job {
    /// This job's place in the instance's single sequence of jobs, so that a
    /// result can tell whether its lane has moved on ([`Worker::claim`]).
    id: u64,
    /// Where the result belongs: a lane, by [`crate::model::Lane::id`] —
    /// never by index, which a reorder or a deletion would invalidate while
    /// the decode runs — or the chop editor.
    target: Target,
    /// What to load.
    source: Source,
}

/// What a job's result is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Target {
    /// A lane, by id.
    Lane(u32),
    /// The chop editor's source (one per instance).
    Chop,
}

/// The newest job id queued for each target with one outstanding.
///
/// Written by [`Loader::queue`], read and pruned by the worker as results
/// land. Holds an entry only while a lane has a job in flight, so its size
/// is the number of loads running, not the number of lanes a session has
/// ever loaded.
type Newest = HashMap<Target, u64>;

/// The background loader of one plugin instance: the worker thread, and the
/// way to give it work.
///
/// Created once with the instance's [`SharedModel`] and dropped with it
/// (`Drop` below shuts it down). Jobs go in through [`queue`](Loader::queue);
/// results do not come back here at all — they go into the model.
#[derive(Debug)]
pub struct Loader {
    /// The job channel's sending half, dropped at shutdown to end the
    /// worker's loop. `None` once that has happened.
    jobs: Option<Sender<Job>>,
    /// The worker, to join at shutdown so an instance never leaves a thread
    /// writing into a model it has finished with.
    thread: Option<JoinHandle<()>>,
    /// The source of job ids: monotonic for the life of the instance.
    ///
    /// Atomic so that [`queue`](Loader::queue) needs only `&self` — the
    /// editor holds the loader by shared reference and queues from the frame
    /// callback.
    next_id: AtomicU64,
    /// The newest job per lane, shared with the worker (see [`Newest`]).
    newest: Arc<Mutex<Newest>>,
    /// Set at shutdown: the worker abandons whatever is still queued instead
    /// of making the host wait for a drop's worth of decoding.
    stop: Arc<AtomicBool>,
}

impl Loader {
    /// Start the worker for an instance whose model is this.
    ///
    /// The thread is named, which is what makes it identifiable in a host's
    /// own thread list when something goes wrong in Bitwig. If it cannot be
    /// spawned at all, the loader still exists and still answers: every
    /// [`queue`](Loader::queue) reports `false` and leaves its lane with no
    /// sample and no "loading…", which is a plugin that cannot load samples
    /// rather than a plugin that has died.
    pub fn new(model: SharedModel) -> Self {
        let (jobs, receiver) = mpsc::channel();
        let newest = Arc::new(Mutex::new(Newest::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let worker =
            Worker { model, newest: Arc::clone(&newest), stop: Arc::clone(&stop) };
        let thread = std::thread::Builder::new()
            .name(String::from("chopdrop-loader"))
            .spawn(move || worker.run(receiver))
            .ok();

        Self { jobs: Some(jobs), thread, next_id: AtomicU64::new(0), newest, stop }
    }

    /// Queue a load for one lane, and mark that lane as loading.
    ///
    /// Reports whether the job was queued: `false` for a lane that is not
    /// there (a result would land nowhere, so the job is not worth making)
    /// and for a worker that is gone.
    ///
    /// The model comes in as the caller's own write guard rather than being
    /// locked here, which is deliberate twice over. It makes the "loading…"
    /// flag part of the same write as whatever gesture queued the job — a
    /// drop that adds three lanes and queues three loads is one lock, one
    /// publish — and it makes the deadlock impossible rather than unlikely:
    /// the one caller that matters is the editor's frame callback, which is
    /// already holding the writer lock when it gets here.
    ///
    /// A [`Source::ChopSource`] has no lane: it goes to
    /// [`open_source`](Loader::open_source) and `lane` is ignored.
    pub fn queue(&self, model: &mut Model, lane: u32, source: Source) -> bool {
        if let Source::ChopSource(path) = source {
            return self.open_source(model, &path);
        }
        if !model.set_loading(lane, true) {
            return false;
        }
        let sent = self.send(Target::Lane(lane), source);
        if !sent {
            // Nothing will ever answer this job, so the lane must not be
            // left waiting on it for the rest of the session.
            model.set_loading(lane, false);
        }
        sent
    }

    /// Open a chop-editor source: [`Model::open_source`] (path remembered,
    /// "loading…") and a [`Source::ChopSource`] job, under the caller's
    /// write guard as [`queue`](Loader::queue) is. A second open before the
    /// first lands wins (the first's result is dropped). `false` if the
    /// worker is gone, which leaves the strip with the unreadable message
    /// rather than loading forever.
    pub fn open_source(&self, model: &mut Model, path: &Path) -> bool {
        let shown = path.to_string_lossy().into_owned();
        model.open_source(&shown);
        let sent = self.send(Target::Chop, Source::ChopSource(path.to_path_buf()));
        if !sent {
            model.source_failed(&shown, &DecodeError::Unreadable.message());
        }
        sent
    }

    /// Reopen the source a chop lane was cut from (the lane menu's "open
    /// source in editor"): [`open_source`](Loader::open_source) on its path,
    /// with the cut's frames as the pending selection, so when the source
    /// lands the cut is selected and the view frames it. Under the caller's
    /// write guard.
    pub fn open_origin(&self, model: &mut Model, origin: &ChopOrigin) -> bool {
        let sent = self.open_source(model, Path::new(&origin.source_path));
        model.request_selection(origin.start_frame, origin.end_frame);
        sent
    }

    /// Send the job for a source the model is already waiting on — a
    /// restored project's ([`Model::load_snapshot`] leaves its saved path
    /// "loading…" with the view and selection pending, which
    /// [`open_source`](Loader::open_source) would clear). Under the
    /// caller's write guard. `false` (and nothing sent) when the editor is
    /// not loading anything; a lost worker leaves the unreadable message.
    pub fn reopen_source(&self, model: &mut Model) -> bool {
        let editor = model.chop();
        let Some(path) = editor.source_path.clone().filter(|_| editor.loading) else {
            return false;
        };
        let sent = self.send(Target::Chop, Source::ChopSource(PathBuf::from(&path)));
        if !sent {
            model.source_failed(&path, &DecodeError::Unreadable.message());
        }
        sent
    }

    /// Give the job a fresh id, record it as its target's newest, and send
    /// it; on a failed send the record is taken back.
    fn send(&self, target: Target, source: Source) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock_newest(&self.newest).insert(target, id);

        let sent =
            self.jobs.as_ref().is_some_and(|jobs| jobs.send(Job { id, target, source }).is_ok());
        if !sent {
            lock_newest(&self.newest).remove(&target);
        }
        sent
    }

    /// Wait for every queued job to land, then stop the worker.
    ///
    /// The graceful counterpart to `Drop`, which abandons the queue: this
    /// closes the channel without raising the stop flag, so the worker works
    /// through what is already queued before its loop ends.
    ///
    /// What the tests below use to observe results without sleeping or
    /// polling — and the only honest way to say "the loading is finished"
    /// to anything that has to know.
    ///
    /// Blocks, and so must not be called with the writer lock held: the jobs
    /// it is waiting for all end in taking that lock (see `Drop`).
    pub fn finish(mut self) {
        self.jobs = None;
        self.join();
    }

    /// Join the worker, if it is still there.
    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            // A worker that panicked is not something the host should hear
            // about: its lane is simply left loading, and the instance
            // carries on without a loader.
            let _ = thread.join();
        }
    }
}

impl Drop for Loader {
    /// Stop the worker and wait for it: the worker exits when the plugin
    /// drops.
    ///
    /// Queued jobs are abandoned rather than finished — nobody is waiting
    /// for a sample on a plugin that is being closed, and a project's worth
    /// of queued decodes would hold the host up. A decode already running is
    /// waited out: the flag is checked between jobs, not inside symphonia,
    /// so the wait is one file's decode (at most
    /// [`crate::sample::MAX_SAMPLE_BYTES`] of it) and not the queue's.
    ///
    /// Joining at all, rather than letting the thread outlive the instance,
    /// is what makes the teardown orderly: when this returns, nothing is
    /// writing into the model any more and the last handles on its samples
    /// are the model's own, which is what lets [`Model`]'s `Drop` hand the
    /// audio back.
    ///
    /// The one rule that comes with joining: **drop the loader without the
    /// writer lock held**. A worker in the middle of delivering a result is
    /// waiting for that lock, so a thread that holds it while waiting here
    /// would wait for itself. Nothing in the plugin does — the loader is a
    /// field of the instance and dies with it, when no gesture is in
    /// flight — and the same goes for [`finish`](Loader::finish).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Closing the channel is what ends the worker's loop; it has to
        // happen before the join or they would wait for each other.
        self.jobs = None;
        self.join();
    }
}

/// The worker's side of the loader: everything the thread holds for the life
/// of the instance.
struct Worker {
    /// The model to write results into — the same writer mutex the GUI and a
    /// project load take.
    model: SharedModel,
    /// The newest job per lane, for [`claim`](Worker::claim).
    newest: Arc<Mutex<Newest>>,
    /// Shutdown, checked between jobs.
    stop: Arc<AtomicBool>,
}

impl Worker {
    /// Decode jobs until the channel closes (the loader dropped it) or the
    /// stop flag goes up.
    fn run(&self, jobs: Receiver<Job>) {
        for Job { id, target, source } in jobs {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }

            let (lane, source) = match (target, source) {
                (Target::Lane(lane), source) => (lane, source),
                (Target::Chop, Source::ChopSource(path)) => {
                    let loaded = load_source(&path);
                    if self.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    self.deliver_source(id, &path, loaded);
                    continue;
                }
                // `send` pairs the chop target with a chop source only.
                (Target::Chop, _) => continue,
            };

            let mut work = source.into_work();
            let loaded = load(&work.path, work.embedded.take());

            // Checked again on the way out: the decode just took real time,
            // and the instance may have been closed during it. A result
            // written after that is harmless (the model is alive as long as
            // this thread holds it) but pointless, and it would publish a
            // snapshot nothing will read.
            if self.stop.load(Ordering::Relaxed) {
                break;
            }

            self.deliver(id, lane, &work, loaded);
        }
    }

    /// Write one result into the model, as a writer like any other: lock,
    /// mutate through the clamping setters, let them publish and collect.
    fn deliver(
        &self,
        id: u64,
        lane: u32,
        work: &Work,
        loaded: Result<(Vec<u8>, SampleData), DecodeError>,
    ) {
        let mut model = lock_model(&self.model);
        if !self.claim(id, Target::Lane(lane)) {
            return;
        }

        let landed = match loaded {
            Ok((bytes, data)) => {
                // Allocated against the model's own collector handle, which
                // is the only thing that will ever free them, and on the
                // right side: a voice still playing this sample later keeps
                // its own clone alive.
                let origin = work.origin.clone();
                let sample =
                    SampleRef::with_origin(model.handle(), &work.path, bytes, data, origin);
                let landed = model.set_sample(lane, sample);
                if landed {
                    if work.undoable() {
                        // A drop or a reload: a gesture, so one undo step.
                        model.commit();
                    } else {
                        // A project's sample: the state undo returns to.
                        model.rebase();
                    }
                }
                landed
            }
            // The lane keeps whatever sample it had — a failed reload leaves
            // the audio that is already playing alone — and the message
            // replaces its "loading…".
            Err(error) => model.set_error(lane, &work.failure(error)),
        };

        // And the window's wake-up call, once, after the lane has changed:
        // this thread is the one writer nothing is watching, so a result
        // that landed with nobody touching the window would otherwise show
        // as "loading…" until the next mouse move. Rung under the writer
        // lock because the hook is a relaxed atomic store that cannot block,
        // and only when something actually changed — a result for a lane
        // that has since been deleted redraws nothing.
        if landed {
            model.request_repaint();
        }
    }

    /// Write a chop-editor source's result into the model: the audio and its
    /// peaks, or the message. Dropped if a newer open (or none, after a
    /// close) is what the editor is waiting for.
    fn deliver_source(
        &self,
        id: u64,
        path: &Path,
        loaded: Result<(SampleData, Peaks), DecodeError>,
    ) {
        let mut model = lock_model(&self.model);
        if !self.claim(id, Target::Chop) {
            return;
        }
        let shown = path.to_string_lossy();
        let landed = match loaded {
            Ok((data, peaks)) => model.set_source(&shown, data, peaks),
            Err(DecodeError::Unreadable) => {
                model.source_failed(&shown, &chop::source_missing_message(&shown))
            }
            Err(error) => model.source_failed(&shown, &error.message()),
        };
        if landed {
            model.request_repaint();
        }
    }

    /// Whether this result is still the one its target is waiting for, and
    /// if so, mark the target as no longer waiting.
    ///
    /// Called with the model's writer lock held, which is what makes the
    /// answer true for as long as it takes to act on it: a competing
    /// [`Loader::queue`] cannot change the map without that lock either, so
    /// the locks are always taken model-first and never in a cycle.
    fn claim(&self, id: u64, target: Target) -> bool {
        let mut newest = lock_newest(&self.newest);
        match newest.get(&target) {
            // A later job for this lane has been queued since this one
            // started: its result is the lane's, and this one must not
            // clear the "loading…" the newer job is waiting behind, nor
            // write a message about a file the lane has moved on from.
            Some(&newest_id) if newest_id != id => false,
            _ => {
                newest.remove(&target);
                true
            }
        }
    }
}

/// Read and decode one sample: the whole of a job's work, and the only part
/// of the loader that has no threads in it.
///
/// `embedded` short-circuits the read for a project's own copy of the file
/// (the embedded bytes are the truth, the path is display) — and gets
/// screened exactly as a file's bytes are, because a project is as
/// untrusted a source as a drop is. Both halves of the
/// taxonomy come straight out of `sample.rs`: the size cap, the Bitwig-clip
/// magic and the decompression-bomb guard are all in there, so this adds no
/// policy of its own.
///
/// The bytes are returned beside the audio because the lane keeps both: the
/// decoded frames to play and the file as read to save.
fn load(path: &Path, embedded: Option<Vec<u8>>) -> Result<(Vec<u8>, SampleData), DecodeError> {
    let bytes = match embedded {
        Some(bytes) => bytes,
        None => sample::read_file(path)?,
    };
    // The extension is a hint and nothing more: probing works without it,
    // and a restored path whose file is long gone still names the format it
    // was.
    let extension = path.extension().and_then(|extension| extension.to_str());
    let data = sample::decode(&bytes, extension)?;
    Ok((bytes, data))
}

/// Read and decode a chop-editor source under [`sample::SOURCE_LIMITS`],
/// refuse it past [`chop::MAX_SOURCE_SECONDS`], and build its peaks — all
/// here, off the writer lock. The file's bytes are not kept: a source is
/// reopened by path, never saved as audio.
fn load_source(path: &Path) -> Result<(SampleData, Peaks), DecodeError> {
    let bytes = sample::read_file_with(path, &sample::SOURCE_LIMITS)?;
    let extension = path.extension().and_then(|extension| extension.to_str());
    let data = sample::decode_with(&bytes, extension, &sample::SOURCE_LIMITS)?;
    drop(bytes);
    if chop::source_too_long(data.frame_count() as u64, data.rate) {
        return Err(DecodeError::SourceTooLong);
    }
    let peaks = Peaks::build(&data);
    Ok((data, peaks))
}

/// Take the pending-job lock, recovering it if a thread panicked while
/// holding it — as [`lock_model`] does, and for the same reason: one panic
/// must not leave every later load stuck at "loading…".
///
/// Nothing calls out while holding this lock (it is taken for a single map
/// lookup and dropped again), so the map cannot be half-written and a
/// poisoned lock here means a panic somewhere this module cannot reach.
fn lock_newest(newest: &Mutex<Newest>) -> MutexGuard<'_, Newest> {
    newest.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicU32;

    use crate::model::RepaintHook;
    use crate::sample::tests::wav_i16;
    use crate::state::{SAMPLE_MISSING_PREFIX, SAMPLE_RESTORE_FAILED};

    /// The interleaved samples every generated WAV in these tests holds: a
    /// short mono ramp, recognisable in the decoded frames.
    const RAMP: [i16; 6] = [0, 4096, 8192, 12288, 16384, 20480];
    /// The rate those WAVs declare — not a common one, so a test that sees
    /// it knows it came from the file and not from a default.
    const RATE: u32 = 32_000;

    /// A model behind the writer mutex, with the eight default lanes.
    fn model() -> SharedModel {
        Arc::new(Mutex::new(Model::new()))
    }

    /// The id of the first default lane.
    fn first_lane(model: &SharedModel) -> u32 {
        lock_model(model).lanes()[0].id
    }

    /// A path in the temp directory that no other test uses, as
    /// `sample.rs`'s own tests do it.
    fn scratch_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("chopdrop-loader-{}-{name}", std::process::id()))
    }

    /// A scratch file holding these bytes, removed when the guard drops, so
    /// a failing assertion does not leave litter in `/tmp`.
    struct ScratchFile {
        path: PathBuf,
    }

    impl ScratchFile {
        fn new(name: &str, bytes: &[u8]) -> Self {
            let path = scratch_path(name);
            std::fs::write(&path, bytes).expect("temp dir is writable");
            Self { path }
        }
    }

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Queue one job and wait for it to land, without an editor, a sleep or
    /// a poll anywhere: `finish` joins the worker, so by the time it returns
    /// the result is in the model.
    fn run_one(model: &SharedModel, lane: u32, source: Source) -> bool {
        let loader = Loader::new(Arc::clone(model));
        let queued = loader.queue(&mut lock_model(model), lane, source);
        loader.finish();
        queued
    }

    /// The headline case: a dropped file is read, decoded and published by
    /// the loader alone — the model it lands in has never seen an editor,
    /// and nothing in the test does anything but wait for the worker.
    #[test]
    fn a_dropped_file_lands_on_its_lane_and_in_the_published_snapshot() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("drop.wav", &wav_i16(1, RATE, &RAMP));

        assert!(run_one(&model, lane, Source::File(file.path.clone())));

        let model = lock_model(&model);
        let loaded = model.lane(lane).expect("the lane is still there");
        let sample = loaded.sample.as_ref().expect("the loader gave the lane its sample");
        assert_eq!(sample.path, file.path.to_string_lossy());
        assert_eq!(sample.file_name, file.path.file_name().unwrap().to_string_lossy());
        assert_eq!(sample.bytes.len(), wav_i16(1, RATE, &RAMP).len());
        assert_eq!(sample.data.rate, RATE);
        assert_eq!(sample.data.channels, 1);
        assert_eq!(sample.data.frame_count(), RAMP.len());
        assert!(!loaded.loading, "the result clears the lane's loading flag");
        assert_eq!(loaded.error, None);

        // And the audio thread's own view of it: the lane is in the
        // published snapshot, with the same audio.
        let published = model.published().get();
        let entry = published.lane(lane).expect("a lane with a sample is published");
        assert_eq!(entry.sample.frames, sample.data.frames);
    }

    /// A drop is a gesture, so the sample landing is one undo step —
    /// Ctrl+Z after a drop puts the lane back as it was.
    #[test]
    fn a_dropped_sample_is_one_undo_step() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("undo.wav", &wav_i16(1, RATE, &RAMP));

        run_one(&model, lane, Source::File(file.path.clone()));

        let mut model = lock_model(&model);
        assert_eq!(model.undo_depth(), 1);
        assert!(model.undo());
        assert!(model.lane(lane).unwrap().sample.is_none());
    }

    /// The lane says it is loading from the moment the job is queued until
    /// its result lands. Deterministic without a sleep: the worker can only
    /// deliver through the writer lock, which this test holds.
    #[test]
    fn a_lane_says_it_is_loading_until_the_result_lands() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("loading.wav", &wav_i16(1, RATE, &RAMP));
        let loader = Loader::new(Arc::clone(&model));

        {
            let mut held = lock_model(&model);
            assert!(loader.queue(&mut held, lane, Source::File(file.path.clone())));
            assert!(held.lane(lane).unwrap().loading, "queued means loading");
        }
        loader.finish();

        assert!(!lock_model(&model).lane(lane).unwrap().loading);
    }

    /// A file that is not audio leaves the decoder's own words on the lane —
    /// the user chose this file a moment ago, so the decoder's complaint is
    /// the useful one.
    #[test]
    fn a_file_that_is_not_audio_leaves_the_decoder_s_message() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("junk.wav", b"not audio, not even close");

        run_one(&model, lane, Source::File(file.path.clone()));

        let model = lock_model(&model);
        let lane = model.lane(lane).unwrap();
        assert_eq!(lane.error.as_deref(), Some(&*DecodeError::Unsupported.message()));
        assert!(lane.sample.is_none());
        assert!(!lane.loading, "a failure ends the wait too");
        assert_eq!(model.undo_depth(), 0, "a failed load changes nothing to undo");
    }

    /// A Bitwig clip dropped out of Bitwig's own browser: the one rejection
    /// whose message exists because the mistake is so easy to make.
    #[test]
    fn a_bitwig_clip_says_what_to_drop_instead() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("beat.bwclip", b"BtWgsomething compressed");

        run_one(&model, lane, Source::File(file.path.clone()));

        assert_eq!(
            lock_model(&model).lane(lane).unwrap().error.as_deref(),
            Some(&*DecodeError::BitwigClip.message())
        );
    }

    /// A path that is not there at all.
    #[test]
    fn a_file_that_cannot_be_read_says_so() {
        let model = model();
        let lane = first_lane(&model);

        run_one(&model, lane, Source::File(scratch_path("no-such-file.wav")));

        assert_eq!(
            lock_model(&model).lane(lane).unwrap().error.as_deref(),
            Some(&*DecodeError::Unreadable.message())
        );
    }

    /// A whole project restore, headless: a saved snapshot's samples are
    /// handed to the loader as `load_snapshot` reports them, and every lane
    /// ends up playing its embedded audio with no editor anywhere in the
    /// picture — and with nothing on the undo stack, because opening a
    /// project is not an edit.
    #[test]
    fn a_restored_project_gets_its_samples_with_no_editor() {
        let bytes = wav_i16(2, RATE, &RAMP);

        // A project saved with two lanes holding the same sample.
        let saved = {
            let model = model();
            let mut model = lock_model(&model);
            let ids: Vec<u32> = model.lanes()[..2].iter().map(|lane| lane.id).collect();
            for id in ids {
                let data = sample::decode(&bytes, Some("wav")).expect("the fixture decodes");
                let path = Path::new("/samples/clap.wav");
                let sample = SampleRef::new(model.handle(), path, bytes.clone(), data);
                assert!(model.set_sample(id, sample));
            }
            model.snapshot()
        };

        // Opened again in a fresh instance.
        let model = model();
        let loader = Loader::new(Arc::clone(&model));
        let pending = {
            let mut opening = lock_model(&model);
            let pending = opening.load_snapshot(saved);
            assert_eq!(pending.len(), 2, "both lanes have a sample to restore");
            for (lane, sample) in &pending {
                assert!(loader.queue(&mut opening, *lane, Source::Restore(sample.clone())));
            }
            pending
        };
        loader.finish();

        let model = lock_model(&model);
        for (lane, _) in &pending {
            let lane = model.lane(*lane).expect("the restored lane is there");
            let sample = lane.sample.as_ref().expect("its sample was restored");
            assert_eq!(sample.path, "/samples/clap.wav");
            assert_eq!(sample.file_name, "clap.wav");
            assert_eq!(sample.data.channels, 2);
            assert_eq!(sample.data.frame_count(), RAMP.len() / 2);
            assert_eq!(lane.error, None);
            assert!(!lane.loading);
            assert!(model.published().get().lane(lane.id).is_some());
        }
        assert_eq!(model.undo_depth(), 0, "a project arriving is not an undo step");
    }

    /// A project load does not become undoable *later* either: the next
    /// real gesture's undo step has to be measured from the project as it
    /// was restored, samples included, or undoing a rename would take a
    /// restored sample off its lane ([`Model::rebase`]).
    #[test]
    fn undoing_a_later_gesture_keeps_the_restored_sample() {
        let model = model();
        let lane = first_lane(&model);
        let bytes = wav_i16(1, RATE, &RAMP);

        run_one(
            &model,
            lane,
            Source::Restore(PendingSample {
                path: String::from("/samples/snare.wav"),
                file_name: String::from("snare.wav"),
                bytes: Some(bytes),
                origin: None,
            }),
        );

        let mut model = lock_model(&model);
        assert!(model.lane(lane).unwrap().sample.is_some());

        // A gesture afterwards, undone: the rename goes, the sample stays.
        assert!(model.rename(lane, "Snare"));
        assert!(model.commit());
        assert!(model.undo());
        assert_eq!(model.lane(lane).unwrap().name, "Lane 1");
        assert!(
            model.lane(lane).unwrap().sample.is_some(),
            "undoing the rename must not undo the restore"
        );
    }

    /// A restored chop lands with the origin its project saved
    /// ([`SampleRef::with_origin`]); a plain restore lands with none.
    #[test]
    fn a_restored_chop_keeps_its_origin() {
        let model = model();
        let lanes: Vec<u32> = lock_model(&model).lanes().iter().map(|lane| lane.id).collect();
        let source_path = String::from("/songs/Eye.wav");
        let origin = ChopOrigin { source_path, start_frame: 4, end_frame: 9 };
        let restore = |origin| {
            Source::Restore(PendingSample {
                path: String::from("/tmp/Eye 1.wav"),
                file_name: String::from("Eye 1.wav"),
                bytes: Some(wav_i16(1, RATE, &RAMP)),
                origin,
            })
        };

        run_one(&model, lanes[0], restore(Some(origin.clone())));
        run_one(&model, lanes[1], restore(None));

        let model = lock_model(&model);
        let chop = model.lane(lanes[0]).unwrap().sample.as_ref().expect("the chop landed");
        assert_eq!(chop.origin, Some(origin));
        let plain = model.lane(lanes[1]).unwrap().sample.as_ref().expect("the sample landed");
        assert_eq!(plain.origin, None);
    }

    /// A project whose embedded sample will not decode: the lane keeps its
    /// settings and says the project's words, not the decoder's.
    #[test]
    fn an_embedded_sample_that_will_not_decode_says_the_project_s_words() {
        let model = model();
        let lane = first_lane(&model);

        run_one(
            &model,
            lane,
            Source::Restore(PendingSample {
                path: String::from("/samples/kick.wav"),
                file_name: String::from("kick.wav"),
                bytes: Some(b"not audio".to_vec()),
                origin: None,
            }),
        );

        let model = lock_model(&model);
        let lane = model.lane(lane).unwrap();
        assert_eq!(lane.error.as_deref(), Some(SAMPLE_RESTORE_FAILED));
        assert_eq!(lane.name, "Lane 1", "the lane keeps everything but the sample");
        assert!(lane.sample.is_none());
        assert!(!lane.loading);
    }

    /// A path-only save whose file has moved: the message names the file to
    /// go and find.
    #[test]
    fn a_path_only_restore_of_a_missing_file_names_the_file() {
        let model = model();
        let lane = first_lane(&model);
        let missing = scratch_path("moved-away.wav");

        run_one(
            &model,
            lane,
            Source::Restore(PendingSample {
                path: missing.to_string_lossy().into_owned(),
                file_name: String::from("moved-away.wav"),
                bytes: None,
                origin: None,
            }),
        );

        assert_eq!(
            lock_model(&model).lane(lane).unwrap().error.as_deref(),
            Some(&*format!("{SAMPLE_MISSING_PREFIX}moved-away.wav"))
        );
    }

    /// A path-only save whose file is still there is read from disk, which
    /// is the other half of a restore.
    #[test]
    fn a_path_only_restore_reads_the_file() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("still-here.wav", &wav_i16(1, RATE, &RAMP));

        run_one(
            &model,
            lane,
            Source::Restore(PendingSample {
                path: file.path.to_string_lossy().into_owned(),
                file_name: String::from("still-here.wav"),
                bytes: None,
                origin: None,
            }),
        );

        let model = lock_model(&model);
        let sample = model.lane(lane).unwrap().sample.as_ref().expect("read from the path");
        assert_eq!(sample.data.frame_count(), RAMP.len());
    }

    /// Two jobs for one lane: the newer wins and the older lands nowhere —
    /// not even its error message, which would otherwise sit on a lane that
    /// is already loading something else.
    ///
    /// Deterministic without a sleep: the worker cannot deliver anything
    /// while this test holds the writer lock, so both jobs are queued before
    /// either result can be written.
    #[test]
    fn the_newest_job_for_a_lane_is_the_one_that_lands() {
        let model = model();
        let lane = first_lane(&model);
        let good = ScratchFile::new("second.wav", &wav_i16(1, RATE, &RAMP));
        let bad = ScratchFile::new("first.wav", b"not audio at all");
        let loader = Loader::new(Arc::clone(&model));

        {
            let mut held = lock_model(&model);
            assert!(loader.queue(&mut held, lane, Source::File(bad.path.clone())));
            assert!(loader.queue(&mut held, lane, Source::File(good.path.clone())));
        }
        loader.finish();

        let model = lock_model(&model);
        let lane = model.lane(lane).unwrap();
        assert_eq!(lane.error, None, "the superseded failure landed nowhere");
        let sample = lane.sample.as_ref().expect("the newer job's sample");
        assert_eq!(sample.path, good.path.to_string_lossy());
    }

    /// A lane deleted while its sample decodes: the result has nowhere to
    /// land and lands nowhere — ids are never reused, so it cannot arrive
    /// on whichever lane took its place.
    #[test]
    fn a_result_for_a_deleted_lane_lands_nowhere() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("deleted.wav", &wav_i16(1, RATE, &RAMP));
        let loader = Loader::new(Arc::clone(&model));

        {
            let mut held = lock_model(&model);
            assert!(loader.queue(&mut held, lane, Source::File(file.path.clone())));
            assert!(held.delete_lane(lane));
        }
        loader.finish();

        let model = lock_model(&model);
        assert!(model.lane(lane).is_none());
        assert!(
            model.lanes().iter().all(|lane| lane.sample.is_none()),
            "the result went nowhere, not onto a neighbour"
        );
    }

    /// A repaint hook that counts, and the counter to read it by: the
    /// editor's notifier as these tests stand in for it.
    fn counting_hook() -> (RepaintHook, Arc<AtomicU32>) {
        let rings = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&rings);
        (
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
            rings,
        )
    }

    /// The repaint hook's headline case: a result landing wakes the window,
    /// once, and after the lane has changed — so a dropped sample appears
    /// without the pointer moving. Rung from the worker thread, which is the
    /// whole point: nothing on the GUI thread was involved in this write.
    #[test]
    fn a_landed_result_rings_the_repaint_hook_once() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("ring.wav", &wav_i16(1, RATE, &RAMP));
        let (hook, rings) = counting_hook();
        lock_model(&model).set_repaint_hook(hook);

        assert!(run_one(&model, lane, Source::File(file.path.clone())));

        assert_eq!(rings.load(Ordering::Relaxed), 1, "one result, one ring");
        // And the ring came after the lane was written, not before: by the
        // time the worker has ended, both have happened, and the frame the
        // ring asks for is drawn later still — so the window can only ever
        // read the landed lane.
        let model = lock_model(&model);
        assert!(model.lane(lane).unwrap().sample.is_some());
        assert!(!model.lane(lane).unwrap().loading);
    }

    /// A failure rings too: a lane that stops saying "loading…" and starts
    /// saying what went wrong has changed just as visibly.
    #[test]
    fn a_failed_load_rings_the_repaint_hook_too() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("ring-junk.wav", b"not audio");
        let (hook, rings) = counting_hook();
        lock_model(&model).set_repaint_hook(hook);

        run_one(&model, lane, Source::File(file.path.clone()));

        assert_eq!(rings.load(Ordering::Relaxed), 1);
        assert!(lock_model(&model).lane(lane).unwrap().error.is_some());
    }

    /// A result that lands nowhere asks for nothing: there is no lane whose
    /// row could have changed, so neither a deleted lane's result nor a
    /// superseded one costs the window a frame.
    #[test]
    fn a_result_that_lands_nowhere_rings_nothing() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("ring-gone.wav", &wav_i16(1, RATE, &RAMP));
        let stale = ScratchFile::new("ring-stale.wav", &wav_i16(1, RATE, &RAMP));
        let (hook, rings) = counting_hook();
        lock_model(&model).set_repaint_hook(hook);
        let loader = Loader::new(Arc::clone(&model));

        {
            // Two jobs the worker cannot deliver while the lock is held
            // here: the first for a lane that is about to be deleted, the
            // second superseded before it ever runs.
            let mut held = lock_model(&model);
            let second = held.lanes()[1].id;
            assert!(loader.queue(&mut held, lane, Source::File(file.path.clone())));
            assert!(held.delete_lane(lane));
            assert!(loader.queue(&mut held, second, Source::File(stale.path.clone())));
            assert!(loader.queue(&mut held, second, Source::File(stale.path.clone())));
        }
        loader.finish();

        assert_eq!(rings.load(Ordering::Relaxed), 1, "only the newest live job rang");
    }

    /// And the headless case the hook is optional for: with no editor ever
    /// built, the worker still lands its result — ringing nothing is not an
    /// error, it is a project restored with the window closed.
    #[test]
    fn a_result_lands_with_no_repaint_hook_installed() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("ring-none.wav", &wav_i16(1, RATE, &RAMP));

        assert!(run_one(&model, lane, Source::File(file.path.clone())));

        assert!(lock_model(&model).lane(lane).unwrap().sample.is_some());
    }

    /// Queueing for a lane that is not there is refused outright: there is
    /// nothing to show "loading…" on and nothing for a result to land on.
    #[test]
    fn a_job_for_a_lane_that_is_not_there_is_refused() {
        let model = model();
        let loader = Loader::new(Arc::clone(&model));

        let queued = loader.queue(&mut lock_model(&model), 9_999, Source::File(PathBuf::new()));

        assert!(!queued);
        assert!(lock_model(&model).lanes().iter().all(|lane| !lane.loading));
    }

    /// Dropping the loader ends the worker: once `drop` returns, nothing is
    /// holding the model but this test, which is the invariant that lets a
    /// plugin instance tear down and hand its audio back.
    #[test]
    fn dropping_the_loader_ends_the_worker() {
        let model = model();
        let loader = Loader::new(Arc::clone(&model));
        assert_eq!(Arc::strong_count(&model), 2, "the worker holds the model too");

        drop(loader);

        assert_eq!(Arc::strong_count(&model), 1, "the worker has let go and exited");
    }

    /// Embedded bytes are decoded without the path being read — which is
    /// what makes a restored project play the audio it was saved with even
    /// on a machine where the file never existed.
    #[test]
    fn embedded_bytes_are_decoded_without_reading_the_path() {
        let bytes = wav_i16(1, RATE, &RAMP);
        let gone = scratch_path("never-existed.wav");

        let (kept, data) = load(&gone, Some(bytes.clone())).expect("no path is read");

        assert_eq!(kept, bytes, "the file as read is kept for the next save");
        assert_eq!(data.rate, RATE);
        assert_eq!(data.frame_count(), RAMP.len());
        assert_eq!(load(&gone, None), Err(DecodeError::Unreadable), "the path really is gone");
    }

    /// The extension is only ever a hint: a WAV called `.mp3` still decodes,
    /// because symphonia probes the bytes.
    #[test]
    fn a_wrong_extension_is_only_a_hint() {
        let file = ScratchFile::new("mislabelled.mp3", &wav_i16(1, RATE, &RAMP));

        let (_, data) = load(&file.path, None).expect("probing beats the hint");

        assert_eq!(data.frame_count(), RAMP.len());
    }

    /// Open a chop source through the real worker and wait for it.
    fn open_one(model: &SharedModel, path: &Path) -> bool {
        let loader = Loader::new(Arc::clone(model));
        let queued = loader.open_source(&mut lock_model(model), path);
        loader.finish();
        queued
    }

    /// A generated WAV opened in the chop editor lands through the loader
    /// alone — audio, peaks built, view fitted — and is neither an undo step
    /// nor anything the audio thread sees.
    #[test]
    fn an_opened_source_lands_with_peaks_and_a_fitted_view() {
        let model = model();
        let published = Arc::clone(lock_model(&model).published());
        let before = published.get();
        let ramp: Vec<i16> = (0..5000).map(|n| (n % 3000) as i16).collect();
        let file = ScratchFile::new("source.wav", &wav_i16(2, RATE, &ramp));

        assert!(open_one(&model, &file.path));

        let model = lock_model(&model);
        let editor = model.chop();
        let source = editor.source.as_ref().expect("the source landed");
        let shown = file.path.to_string_lossy();
        assert_eq!(source.path, shown);
        assert_eq!(editor.source_path.as_deref(), Some(&*shown));
        assert_eq!(source.data.rate, RATE);
        assert_eq!(source.data.channels, 2);
        assert_eq!(source.total(), 2500);
        assert_eq!(*source.peaks, Peaks::build(&source.data), "peaks of the audio itself");
        assert_eq!(source.peaks.levels()[0].len(), 2500_usize.div_ceil(64));
        assert_eq!(editor.view, chop::View::fit(2500, chop::DEFAULT_VIEW_WIDTH_PX));
        assert!(!editor.loading);
        assert_eq!(editor.error, None);
        assert_eq!(editor.selection, None);
        assert_eq!(model.undo_depth(), 0, "opening a source is not an undo step");
        assert!(std::ptr::eq(&*before, &*published.get()), "nothing was published");
    }

    /// The view fits the width the strip last reported, not the default.
    #[test]
    fn a_source_fits_the_strip_s_own_width() {
        let model = model();
        lock_model(&model).set_view(chop::View::fit(0, 100.0), 1000.0);
        let ramp: Vec<i16> = (0..8000).map(|n| n as i16).collect();
        let file = ScratchFile::new("width.wav", &wav_i16(1, RATE, &ramp));

        open_one(&model, &file.path);

        assert_eq!(lock_model(&model).chop().view, chop::View::fit(8000, 1000.0));
    }

    /// A requested selection (the lane menu's "open source in editor")
    /// becomes the selection and the view frames it.
    #[test]
    fn a_pending_selection_is_applied_when_the_source_lands() {
        let model = model();
        let ramp: Vec<i16> = (0..20_000).map(|n| (n % 1000) as i16).collect();
        let file = ScratchFile::new("pending.wav", &wav_i16(1, RATE, &ramp));
        let loader = Loader::new(Arc::clone(&model));
        {
            let mut held = lock_model(&model);
            assert!(loader.open_source(&mut held, &file.path));
            assert!(held.request_selection(8000, 10_000));
        }
        loader.finish();

        let model = lock_model(&model);
        let editor = model.chop();
        let selection = editor.selection.expect("the pending selection applied");
        assert_eq!((selection.start, selection.end), (8000, 10_000));
        assert_eq!(editor.pending_selection, None);
        let view = editor.view;
        assert!(view.frame_at(0.0) <= 8000, "the selection's start is in view");
        assert!(view.frame_at(chop::DEFAULT_VIEW_WIDTH_PX) >= 10_000, "and its end");
        assert!(view.frames_per_pixel < chop::View::fit(20_000, 560.0).frames_per_pixel);
    }

    /// "open source in editor": the lane's origin reopens its source, and
    /// when it lands the selection is the cut and the view frames it
    /// exactly as `View::around` does.
    #[test]
    fn opening_an_origin_selects_the_cut_when_the_source_lands() {
        let model = model();
        let ramp: Vec<i16> = (0..20_000).map(|n| (n % 1000) as i16).collect();
        let file = ScratchFile::new("origin.wav", &wav_i16(1, RATE, &ramp));
        let loader = Loader::new(Arc::clone(&model));
        let origin = ChopOrigin {
            source_path: file.path.to_string_lossy().into_owned(),
            start_frame: 12_000,
            end_frame: 15_500,
        };
        {
            let mut held = lock_model(&model);
            assert!(loader.open_origin(&mut held, &origin));
            assert_eq!(held.chop().source_path.as_deref(), Some(origin.source_path.as_str()));
            assert!(held.chop().loading);
        }
        loader.finish();

        let model = lock_model(&model);
        let editor = model.chop();
        assert!(editor.source.is_some(), "the source landed");
        let selection = editor.selection.expect("the origin is selected");
        assert_eq!((selection.start, selection.end), (origin.start_frame, origin.end_frame));
        let width = editor.view_width_px;
        assert_eq!(editor.view, chop::View::around(12_000, 15_500, 20_000, width));
        assert!(editor.view.frame_at(0.0) <= 12_000, "the cut's start is in view");
        assert!(editor.view.frame_at(width) >= 15_500, "and its end");
    }

    /// A missing file leaves the strip's message, with the path kept for
    /// it to name — and no undo step.
    #[test]
    fn a_missing_source_sets_the_error_and_keeps_the_path() {
        let model = model();
        let gone = scratch_path("gone-song.wav");

        assert!(open_one(&model, &gone));

        let model = lock_model(&model);
        let editor = model.chop();
        assert!(editor.source.is_none());
        assert!(!editor.loading);
        let name = format!("chopdrop-loader-{}-gone-song.wav", std::process::id());
        let message = format!("{}{name}", chop::SOURCE_MISSING_PREFIX);
        assert_eq!(editor.error.as_deref(), Some(&*message));
        assert_eq!(editor.source_path.as_deref(), Some(&*gone.to_string_lossy()));
        assert_eq!(model.undo_depth(), 0);
    }

    /// A file that is not audio says the decoder's words.
    #[test]
    fn a_source_that_is_not_audio_says_so() {
        let model = model();
        let file = ScratchFile::new("junk-source.wav", b"definitely not audio");

        open_one(&model, &file.path);

        let model = lock_model(&model);
        assert_eq!(model.chop().error.as_deref(), Some(&*DecodeError::Unsupported.message()));
    }

    /// A source past the source cap names that cap, 512 MiB, in the strip's
    /// message — not a lane's 64 MiB. The file is sparse: it is refused from
    /// its size before a byte of it is read, so it costs no disk.
    #[test]
    fn a_source_over_the_source_cap_names_512_mib() {
        let model = model();
        let file = ScratchFile::new("huge-source.wav", b"");
        let huge = std::fs::OpenOptions::new().write(true).open(&file.path).expect("just made");
        huge.set_len(sample::MAX_SOURCE_BYTES as u64 + 1).expect("sparse file");
        drop(huge);

        open_one(&model, &file.path);

        let model = lock_model(&model);
        assert!(model.chop().source.is_none());
        assert_eq!(model.chop().error.as_deref(), Some("file too big (max 512 MiB)"));
        assert_eq!(model.undo_depth(), 0);
    }

    /// Past 15 minutes is refused: 901 frames at 1 Hz is 901 s.
    #[test]
    fn a_source_past_fifteen_minutes_is_too_long() {
        let model = model();
        let file = ScratchFile::new("long.wav", &wav_i16(1, 1, &[100; 901]));

        open_one(&model, &file.path);

        let model = lock_model(&model);
        assert!(model.chop().source.is_none());
        assert_eq!(model.chop().error.as_deref(), Some(sample::SOURCE_TOO_LONG));
    }

    /// `close_source` clears everything source-related, and a result still
    /// in flight when it ran is dropped on arrival.
    #[test]
    fn close_source_clears_and_drops_a_late_result() {
        let model = model();
        let file = ScratchFile::new("closed.wav", &wav_i16(1, RATE, &RAMP));

        open_one(&model, &file.path);
        {
            let mut held = lock_model(&model);
            assert!(held.chop().source.is_some());
            held.set_snap(false);
            held.close_source();
            let editor = held.chop();
            assert!(editor.source.is_none());
            assert_eq!(editor.source_path, None);
            assert_eq!(editor.error, None);
            assert_eq!(editor.selection, None);
            assert!(!editor.loading);
            assert!(!editor.snap, "close keeps the snap setting");
            assert_eq!(held.undo_depth(), 0, "closing is not an undo step");
        }

        // Closed while loading: the worker can only deliver through the
        // writer lock, which is held across both calls.
        let loader = Loader::new(Arc::clone(&model));
        {
            let mut held = lock_model(&model);
            loader.open_source(&mut held, &file.path);
            held.close_source();
        }
        loader.finish();
        let model = lock_model(&model);
        assert!(model.chop().source.is_none(), "the late result does not land");
        assert_eq!(model.chop().source_path, None);
    }

    /// The source job landing after `close_source` is discarded whichever
    /// way it went — a decoded source, a missing file or a file that is not
    /// audio leaves the strip closed (no audio, path, message or
    /// "loading…"), and nothing asks for a frame, since nothing changed.
    #[test]
    fn a_source_landing_after_close_is_discarded() {
        let good = ScratchFile::new("late-good.wav", &wav_i16(1, RATE, &RAMP));
        let junk = ScratchFile::new("late-junk.wav", b"not audio at all");
        let gone = scratch_path("late-missing.wav");
        for path in [good.path.clone(), junk.path.clone(), gone] {
            let model = model();
            let (hook, rings) = counting_hook();
            lock_model(&model).set_repaint_hook(hook);
            let loader = Loader::new(Arc::clone(&model));
            {
                // The worker delivers only through the writer lock, held
                // across both calls: its result lands after the close.
                let mut held = lock_model(&model);
                assert!(loader.open_source(&mut held, &path));
                assert!(held.chop().loading);
                held.close_source();
            }
            loader.finish();

            let held = lock_model(&model);
            let editor = held.chop();
            assert!(editor.source.is_none(), "{path:?}: the late result did not land");
            assert_eq!(editor.source_path, None, "{path:?}");
            assert_eq!(editor.error, None, "{path:?}: no message about a closed source");
            assert!(!editor.loading, "{path:?}");
            assert_eq!(editor.selection, None, "{path:?}");
            assert_eq!(rings.load(Ordering::Relaxed), 0, "{path:?}: a discard asks no frame");
        }
    }

    /// Closed and then reopened on another file before the first job
    /// lands: the first is discarded, the second is what shows.
    #[test]
    fn a_late_landing_does_not_overwrite_a_source_opened_after_the_close() {
        let model = model();
        let first = ScratchFile::new("closed-first.wav", &wav_i16(1, RATE, &RAMP));
        let second = ScratchFile::new("opened-second.wav", &wav_i16(2, RATE, &RAMP));
        let loader = Loader::new(Arc::clone(&model));
        {
            let mut held = lock_model(&model);
            loader.open_source(&mut held, &first.path);
            held.close_source();
            loader.open_source(&mut held, &second.path);
        }
        loader.finish();

        let model = lock_model(&model);
        let source = model.chop().source.as_ref().expect("the second source landed");
        assert_eq!(source.path, second.path.to_string_lossy());
        assert_eq!(source.data.channels, 2);
        assert!(!model.chop().loading);
    }

    /// Newest wins: a second open before the first lands is what shows,
    /// and the first's failure does not overwrite it.
    #[test]
    fn a_second_open_wins_over_the_first() {
        let model = model();
        let file = ScratchFile::new("second.wav", &wav_i16(1, RATE, &RAMP));
        let loader = Loader::new(Arc::clone(&model));
        {
            let mut held = lock_model(&model);
            loader.open_source(&mut held, &scratch_path("first-missing.wav"));
            // Through `queue`, which forwards a chop source whatever lane.
            assert!(loader.queue(&mut held, u32::MAX, Source::ChopSource(file.path.clone())));
        }
        loader.finish();

        let model = lock_model(&model);
        let editor = model.chop();
        assert_eq!(editor.error, None);
        assert_eq!(editor.source.as_ref().unwrap().path, file.path.to_string_lossy());
    }

    /// A chop source and a lane load in flight together do not cancel each
    /// other: their newest-job records are kept apart.
    #[test]
    fn a_chop_source_and_a_lane_load_do_not_cancel_each_other() {
        let model = model();
        let lane = first_lane(&model);
        let file = ScratchFile::new("both.wav", &wav_i16(1, RATE, &RAMP));
        let loader = Loader::new(Arc::clone(&model));
        {
            let mut held = lock_model(&model);
            loader.open_source(&mut held, &file.path);
            loader.queue(&mut held, lane, Source::File(file.path.clone()));
        }
        loader.finish();

        let model = lock_model(&model);
        assert!(model.chop().source.is_some());
        assert!(model.lane(lane).unwrap().sample.is_some());
    }

    /// Load `saved` into a fresh instance the way the shell does — lanes,
    /// their samples, then the editor's source — and wait for the worker.
    fn restore_into_fresh(saved: crate::state::Snapshot) -> SharedModel {
        let model = model();
        let loader = Loader::new(Arc::clone(&model));
        {
            let mut opening = lock_model(&model);
            for (lane, sample) in opening.load_snapshot(saved) {
                loader.queue(&mut opening, lane, Source::Restore(sample));
            }
            assert!(loader.reopen_source(&mut opening), "the saved source is reopened");
        }
        loader.finish();
        model
    }

    /// The chop editor's path, view, selection, snap, collapsed and height
    /// survive a save and a load through JSON, the source reopened through
    /// the real loader with no editor.
    #[test]
    fn the_chop_editor_round_trips_through_a_project() {
        let ramp: Vec<i16> = (0..40_000).map(|n| (n % 2000) as i16).collect();
        let file = ScratchFile::new("round-trip-song.wav", &wav_i16(1, RATE, &ramp));
        let saving = model();
        assert!(open_one(&saving, &file.path));
        let view = chop::View { first: 3000, frames_per_pixel: 7.5 };
        let selection = chop::Selection { start: 5000, end: 9000, active: chop::Edge::Start };
        let json = {
            let mut held = lock_model(&saving);
            held.set_view(view, 800.0);
            assert!(held.set_selection(Some(selection)));
            held.set_snap(false);
            held.set_strip(true, 321.0);
            held.set_looping(true);
            assert_eq!(held.chop().view, view, "the view is inside the source as set");
            serde_json::to_string(&held.snapshot()).expect("a snapshot serialises")
        };

        let parsed: crate::state::Snapshot = serde_json::from_str(&json).expect("it parses");
        let restored = restore_into_fresh(parsed);

        let model = lock_model(&restored);
        let editor = model.chop();
        let shown = file.path.to_string_lossy();
        assert_eq!(editor.source_path.as_deref(), Some(&*shown));
        assert_eq!(editor.source.as_ref().expect("the source landed").total(), 40_000);
        assert_eq!(editor.view, view);
        assert_eq!(editor.selection, Some(selection));
        assert!(!editor.snap);
        assert!(editor.collapsed);
        assert_eq!(editor.height_px, 321.0);
        assert!(!editor.looping, "looping is never saved on");
        assert!(!editor.loading);
        assert_eq!(editor.error, None);
        assert_eq!((editor.pending_view, editor.pending_selection), (None, None));
        assert_eq!(model.undo_depth(), 0);
    }

    /// A project whose source has gone loads its lanes as saved, and the
    /// strip says "source not found: <name>".
    #[test]
    fn a_project_whose_source_is_gone_keeps_its_lanes() {
        let gone = scratch_path("vanished-song.wav");
        let saved = {
            let saving = model();
            let mut held = lock_model(&saving);
            let first = held.lanes()[0].id;
            assert!(held.rename(first, "chop 1"));
            held.open_source(&gone.to_string_lossy());
            assert!(held.request_selection(10, 500));
            held.snapshot()
        };
        let names: Vec<String> = saved.lanes.iter().map(|lane| lane.name.clone()).collect();

        let restored = restore_into_fresh(saved);

        let model = lock_model(&restored);
        let editor = model.chop();
        let name = format!("chopdrop-loader-{}-vanished-song.wav", std::process::id());
        let message = format!("source not found: {name}");
        assert_eq!(editor.error.as_deref(), Some(&*message));
        assert_eq!(editor.source_path.as_deref(), Some(&*gone.to_string_lossy()));
        assert!(editor.source.is_none());
        assert!(!editor.loading);
        let restored_names: Vec<&str> = model.lanes().iter().map(|l| &*l.name).collect();
        assert_eq!(restored_names, names, "every lane is there, as saved");
        assert_eq!(restored_names[0], "chop 1");
        // Saving again while the file is still gone keeps what it waits for.
        let resaved = model.snapshot().chop_editor.expect("the editor is saved");
        assert_eq!(resaved.selection, Some((10, 500)));
    }
}
