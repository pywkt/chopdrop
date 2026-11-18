//! The render worker: time-stretch renders off every real-time thread.
//!
//! One thread per plugin instance (`chopdrop-render`), a writer like the
//! loader's: it reads what a Stretch lane wants under the model's writer lock,
//! renders **without** the lock, and lands the result through the writer API
//! ([`Model::set_render`] / [`Model::set_render_error`]), which ignores a
//! stale generation, publishes and collects. Then it rings the repaint hook,
//! once per landed result, so an untouched window shows the change.
//!
//! # Where requests come from
//!
//! The model is the queue. Every setter that changes what a Stretch lane's
//! render should be (chop, time mode, length, fine, pitch, "click to retry",
//! a project load) already bumps the lane's
//! [`render_generation`](crate::model::Lane::render_generation) and sets
//! [`render_pending`](crate::model::Lane::render_pending) — without knowing a
//! worker exists, so the GUI, a restore and a test all request the same way.
//! The worker wakes every [`DEBOUNCE`] (`recv_timeout` on its channel) and
//! scans the lanes; [`RenderWorker::wake`] sends a message that makes it scan
//! at once, so a request is seen when it is made rather than up to a tick
//! later. The channel carries only those wake-ups; closing it is shutdown.
//!
//! Each tick:
//!
//! 1. **Tempo.** Re-read the [`TempoCell`]; every Stretch lane with a landed
//!    render, nothing pending and no error whose render length at the new
//!    tempo differs from its render's by more than [`RETARGET_PCT`] gets a
//!    [`Model::request_render`] from the worker itself — what keeps renders
//!    following the tempo with the editor closed.
//! 2. **Coalesce and debounce.** A pending lane is due once its newest
//!    generation has stood unchanged for [`DEBOUNCE`]; three quick requests
//!    are three generations of which only the last is ever rendered.
//! 3. **Render** every due lane, lock released.
//! 4. **Apply** through the model, and ring the repaint hook.
//!
//! Pure: no nice-plug types; tested with real renders and a real thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::model::{Lane, SharedModel, lock_model};
use crate::stretch::{self, RenderRequest};
use crate::time::{self, Length, TempoCell, TimeMode};

/// How long a lane's newest request must stand before it is rendered, and
/// the worker's idle tick.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// How far, in percent, a Stretch lane's wanted render length may drift from
/// its landed render's before a tempo change re-renders it.
pub const RETARGET_PCT: f64 = 0.1;

/// The frames a Stretch lane's render should have at `tempo`: its note
/// length (with fine) at that tempo, or the chop's own length for
/// [`Length::AsCut`], clamped as [`stretch::render`] clamps. `None` for a
/// lane that is not in Stretch or has no sample.
pub fn wanted_frames(lane: &Lane, tempo: f64) -> Option<usize> {
    if lane.time_mode != TimeMode::Stretch {
        return None;
    }
    let chop = &lane.sample.as_ref()?.data;
    let chop_frames = chop.frame_count();
    let frames = match lane.length {
        Length::AsCut => chop_frames,
        Length::Beats(length) => {
            let secs = time::target_secs(length, lane.fine_pct, tempo);
            let frames = (secs * f64::from(chop.rate)).round();
            // `as` saturates (NaN → 0), and the clamp below bounds it anyway.
            frames.max(0.0) as usize
        }
    };
    Some(frames.min(stretch::target_frames_max(chop_frames)))
}

/// Whether a lane's landed render is off by more than [`RETARGET_PCT`] from
/// what it wants at `tempo`. A lane with no render, a request in flight or a
/// failed render is not re-targeted: the first two will be rendered anyway,
/// and the last waits for the user's retry rather than failing on a loop.
fn needs_retarget(lane: &Lane, tempo: f64) -> bool {
    if lane.render_pending || lane.render_error.is_some() {
        return false;
    }
    let (Some(render), Some(wanted)) = (&lane.render, wanted_frames(lane, tempo)) else {
        return false;
    };
    let have = render.data.frame_count();
    if have == wanted || have == 0 {
        return false;
    }
    let drift = (wanted as f64 / have as f64 - 1.0).abs() * 100.0;
    drift > RETARGET_PCT
}

/// The render worker of one plugin instance: the thread, and the way to wake
/// it. Created with the instance's model and [`TempoCell`]; dropping it is
/// the shutdown (abandons what is not yet rendered), [`finish`](Self::finish)
/// the graceful one tests use.
#[derive(Debug)]
pub struct RenderWorker {
    /// Wake-ups for the worker; dropped to end its loop.
    wake: Option<Sender<()>>,
    /// The worker, joined at shutdown.
    thread: Option<JoinHandle<()>>,
    /// Set by `Drop`: leave without rendering what is still due.
    stop: Arc<AtomicBool>,
    /// How many renders the worker has run (landed, stale or failed).
    renders: Arc<AtomicUsize>,
}

impl RenderWorker {
    /// Start the worker with the default [`DEBOUNCE`].
    pub fn new(model: SharedModel, tempo: Arc<TempoCell>) -> Self {
        Self::with_debounce(model, tempo, DEBOUNCE)
    }

    /// Start the worker with its own debounce — tests stretch it so that
    /// "quick" does not depend on the machine's load. If the thread cannot
    /// be spawned the worker still exists and renders nothing: Stretch lanes
    /// then play their chop, speed-corrected.
    pub fn with_debounce(model: SharedModel, tempo: Arc<TempoCell>, debounce: Duration) -> Self {
        let (wake, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let renders = Arc::new(AtomicUsize::new(0));
        let worker = Worker {
            model,
            tempo,
            debounce,
            stop: Arc::clone(&stop),
            renders: Arc::clone(&renders),
            seen: HashMap::new(),
            done: HashMap::new(),
        };
        let thread = std::thread::Builder::new()
            .name(String::from("chopdrop-render"))
            .spawn(move || worker.run(receiver))
            .ok();
        Self { wake: Some(wake), thread, stop, renders }
    }

    /// Make the worker scan the model now rather than at its next tick —
    /// call after a writer requested a render. Never blocks; harmless when
    /// nothing changed.
    pub fn wake(&self) {
        if let Some(wake) = &self.wake {
            let _ = wake.send(());
        }
    }

    /// How many renders the worker has run so far.
    pub fn render_count(&self) -> usize {
        self.renders.load(Ordering::Relaxed)
    }

    /// Wait until nothing is left to render — every pending lane's newest
    /// generation rendered (its debounce waited out) and no tempo re-target
    /// outstanding — then stop the worker. Returns how many renders it ran
    /// over its life. Blocks (the worker sleeps; nothing here polls), so it
    /// must not be called with the writer lock held.
    pub fn finish(mut self) -> usize {
        self.wake = None;
        self.join();
        self.render_count()
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            // A panicked worker leaves its lanes "rendering…"; the instance
            // carries on without renders rather than taking the host down.
            let _ = thread.join();
        }
    }
}

impl Drop for RenderWorker {
    /// Stop the worker and wait for it. Lanes still due are abandoned; a
    /// render already running is waited out (one render, not the queue).
    /// Drop it without the writer lock held, as the loader.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake = None;
        self.join();
    }
}

/// The worker thread's state.
struct Worker {
    model: SharedModel,
    tempo: Arc<TempoCell>,
    debounce: Duration,
    stop: Arc<AtomicBool>,
    renders: Arc<AtomicUsize>,
    /// Per lane: the newest generation seen pending, and when it was first
    /// seen — the debounce clock, restarted by every newer generation.
    seen: HashMap<u32, (u64, Instant)>,
    /// Per lane: the newest generation already rendered (or attempted).
    done: HashMap<u32, u64>,
}

/// What one scan found.
struct Scan {
    /// Requests whose debounce has run out, ready to render.
    due: Vec<RenderRequest>,
    /// The soonest a not-yet-due request comes due, if any is waiting.
    next: Option<Instant>,
}

impl Worker {
    fn run(mut self, wake: Receiver<()>) {
        let mut open = true;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            let Scan { due, next } = self.scan();
            let rendered_any = !due.is_empty();
            for request in due {
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                self.render_and_apply(request);
            }
            if rendered_any {
                // A render takes real time: look again before sleeping.
                continue;
            }

            let wait =
                next.map_or(self.debounce, |at| at.saturating_duration_since(Instant::now()));
            if open {
                match wake.recv_timeout(wait.min(self.debounce)) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => open = false,
                }
            } else if next.is_some() {
                // Finishing: sleep out the debounce of what is still waiting.
                std::thread::sleep(wait);
            } else {
                // Finishing, and nothing is pending: done.
                return;
            }
        }
    }

    /// Under the writer lock, briefly: re-target lanes the tempo has moved,
    /// then collect what is due. Copies `Shared` handles only — no render
    /// happens with the lock held.
    fn scan(&mut self) -> Scan {
        let tempo = self.tempo.get();
        let now = Instant::now();
        let mut model = lock_model(&self.model);

        let drifted: Vec<u32> = model
            .lanes()
            .iter()
            .filter(|lane| needs_retarget(lane, tempo))
            .map(|lane| lane.id)
            .collect();
        for id in drifted {
            model.request_render(id);
        }

        let mut due = Vec::new();
        let mut next: Option<Instant> = None;
        let mut live = Vec::new();
        for lane in model.lanes() {
            live.push(lane.id);
            let generation = lane.render_generation;
            let wanted = if lane.render_pending { wanted_frames(lane, tempo) } else { None };
            let (Some(target_frames), Some(sample)) = (wanted, &lane.sample) else {
                continue;
            };
            if self.done.get(&lane.id) == Some(&generation) {
                continue;
            }
            let since = match self.seen.get(&lane.id) {
                Some(&(seen, since)) if seen == generation => since,
                _ => {
                    self.seen.insert(lane.id, (generation, now));
                    now
                }
            };
            let at = since + self.debounce;
            if at <= now {
                due.push(RenderRequest {
                    lane: lane.id,
                    generation,
                    chop: sample.data.clone(),
                    target_frames,
                    pitch_st: lane.pitch_st,
                });
            } else {
                next = Some(next.map_or(at, |soonest| soonest.min(at)));
            }
        }
        // Forget deleted lanes, so the maps stay the size of the lane list.
        self.seen.retain(|id, _| live.contains(id));
        self.done.retain(|id, _| live.contains(id));
        drop(model);

        for request in &due {
            self.done.insert(request.lane, request.generation);
            self.seen.remove(&request.lane);
        }
        Scan { due, next }
    }

    /// Render one request with no lock held, then land it through the
    /// writer API and ring the repaint hook if anything changed.
    fn render_and_apply(&self, request: RenderRequest) {
        let result = stretch::render(&request.chop, request.target_frames, request.pitch_st);
        self.renders.fetch_add(1, Ordering::Relaxed);
        // The chop handle is released before the lock is taken; the
        // collector reclaims it on the next writer's collect.
        let RenderRequest { lane, generation, chop, .. } = request;
        drop(chop);

        let mut model = lock_model(&self.model);
        let landed = match result {
            Ok(data) => model.set_render(lane, generation, data),
            Err(error) => model.set_render_error(lane, generation, error.message()),
        };
        if landed {
            model.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};

    use crate::model::{Model, RepaintHook, SampleRef};
    use crate::sample::SampleData;
    use crate::time::NoteLength;

    const RATE: u32 = 48_000;
    /// A quarter note at 120 BPM: 0.5 s.
    const QUARTER_AT_120: usize = 24_000;

    fn shared_model() -> SharedModel {
        Arc::new(Mutex::new(Model::new()))
    }

    /// A 0.25 s mono sine — short, so a debug-build render is quick.
    fn chop() -> SampleData {
        let frames: Vec<f32> = (0..RATE as usize / 4)
            .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.5)
            .collect();
        SampleData { rate: RATE, channels: 1, frames: frames.into_boxed_slice() }
    }

    /// The first lane, given the chop and put into Stretch at a quarter
    /// note — which requests its first render.
    fn stretch_lane(model: &SharedModel) -> u32 {
        let mut model = lock_model(model);
        let id = model.lanes()[0].id;
        let sample = SampleRef::new(model.handle(), Path::new("/tmp/chop.wav"), Vec::new(), chop());
        assert!(model.set_sample(id, sample));
        assert!(model.set_length(id, Length::Beats(NoteLength::Quarter)));
        assert!(model.set_time_mode(id, TimeMode::Stretch));
        assert!(model.lane(id).unwrap().render_pending);
        id
    }

    /// The 8.2 counting hook: a repaint notifier that counts its rings.
    fn counting_hook() -> (RepaintHook, Arc<AtomicU32>) {
        let rings = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&rings);
        (Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }), rings)
    }

    fn worker(model: &SharedModel, debounce: Duration) -> RenderWorker {
        let tempo = Arc::clone(lock_model(model).tempo());
        RenderWorker::with_debounce(Arc::clone(model), tempo, debounce)
    }

    /// Render the first request to completion, so a test starts from a
    /// Stretch lane with a landed render at the current tempo.
    fn landed_lane(model: &SharedModel) -> u32 {
        let id = stretch_lane(model);
        assert_eq!(worker(model, Duration::ZERO).finish(), 1);
        let model = lock_model(model);
        let lane = model.lane(id).unwrap();
        assert!(!lane.render_pending && lane.render.is_some());
        id
    }

    #[test]
    fn wanted_frames_follow_length_tempo_and_as_cut() {
        let model = shared_model();
        let id = stretch_lane(&model);
        let mut model = lock_model(&model);
        let lane = model.lane(id).unwrap();
        assert_eq!(wanted_frames(lane, 120.0), Some(QUARTER_AT_120));
        assert_eq!(wanted_frames(lane, 60.0), Some(2 * QUARTER_AT_120));
        assert!(model.set_length(id, Length::AsCut));
        assert_eq!(wanted_frames(model.lane(id).unwrap(), 60.0), Some(RATE as usize / 4));
        // Clamped to 16× the chop, as stretch::render clamps.
        assert!(model.set_length(id, Length::Beats(NoteLength::EightBars)));
        assert_eq!(wanted_frames(model.lane(id).unwrap(), 20.0), Some(16 * RATE as usize / 4));
        assert!(model.set_time_mode(id, TimeMode::Off));
        assert_eq!(wanted_frames(model.lane(id).unwrap(), 120.0), None);
    }

    #[test]
    fn three_quick_requests_render_once_with_the_newest_settings() {
        let model = shared_model();
        let id = stretch_lane(&model);
        let worker = worker(&model, Duration::from_millis(400));
        // Three more generations, each in its own write, with wake-ups so
        // the worker sees every one of them.
        for pitch in [1.0, 2.0, 3.0] {
            assert!(lock_model(&model).set_pitch_st(id, pitch));
            worker.wake();
        }
        assert_eq!(worker.finish(), 1, "coalesced into one render");

        let model = lock_model(&model);
        let lane = model.lane(id).unwrap();
        let render = lane.render.as_ref().expect("the render landed");
        assert_eq!(render.generation, lane.render_generation, "the newest generation");
        assert_eq!(render.data.frame_count(), QUARTER_AT_120);
        assert!(!lane.render_pending && lane.render_error.is_none());
    }

    #[test]
    fn requests_spaced_inside_the_debounce_still_render_once() {
        let model = shared_model();
        let id = stretch_lane(&model);
        let worker = worker(&model, Duration::from_millis(400));
        let start = Instant::now();
        for pitch in [1.0, 2.0] {
            std::thread::sleep(Duration::from_millis(100));
            assert!(lock_model(&model).set_pitch_st(id, pitch));
            worker.wake();
        }
        assert_eq!(worker.finish(), 1);
        assert!(start.elapsed() >= Duration::from_millis(600), "the last request waited 400 ms");
    }

    #[test]
    fn a_tempo_change_of_ten_percent_re_renders_a_stretch_lane() {
        let model = shared_model();
        let id = landed_lane(&model);
        let before = lock_model(&model).lane(id).unwrap().render_generation;
        lock_model(&model).tempo().set(132.0);

        assert_eq!(worker(&model, Duration::ZERO).finish(), 1, "re-rendered for the new tempo");
        let model = lock_model(&model);
        let lane = model.lane(id).unwrap();
        let render = lane.render.as_ref().unwrap();
        assert!(lane.render_generation > before);
        assert_eq!(render.generation, lane.render_generation);
        let wanted = (0.5 * 120.0 / 132.0 * f64::from(RATE)).round() as usize;
        assert_eq!(render.data.frame_count(), wanted);
    }

    #[test]
    fn a_tempo_change_of_five_hundredths_of_a_percent_does_not() {
        let model = shared_model();
        let id = landed_lane(&model);
        let before = lock_model(&model).lane(id).unwrap().render_generation;
        lock_model(&model).tempo().set(120.0 * 1.0005);

        assert_eq!(worker(&model, Duration::ZERO).finish(), 0);
        let model = lock_model(&model);
        let lane = model.lane(id).unwrap();
        assert_eq!(lane.render_generation, before);
        assert_eq!(lane.render.as_ref().unwrap().data.frame_count(), QUARTER_AT_120);
    }

    #[test]
    fn tempo_never_re_renders_an_as_cut_lane() {
        let model = shared_model();
        let id = stretch_lane(&model);
        assert!(lock_model(&model).set_length(id, Length::AsCut));
        assert_eq!(worker(&model, Duration::ZERO).finish(), 1);
        lock_model(&model).tempo().set(60.0);
        assert_eq!(worker(&model, Duration::ZERO).finish(), 0);
    }

    #[test]
    fn results_land_through_the_writer_api_and_publish() {
        let model = shared_model();
        let id = stretch_lane(&model);
        let chop_frames = {
            let model = lock_model(&model);
            model.published().get().lane(id).unwrap().sample.frame_count()
        };
        assert_eq!(chop_frames, RATE as usize / 4, "the chop plays until the render lands");

        assert_eq!(worker(&model, Duration::ZERO).finish(), 1);
        let model = lock_model(&model);
        let published = model.published().get();
        assert_eq!(published.lane(id).unwrap().sample.frame_count(), QUARTER_AT_120);
        assert_eq!(model.undo_depth(), 0, "a landed render is not an undo step");
    }

    #[test]
    fn a_render_for_a_lane_that_left_stretch_is_discarded() {
        let model = shared_model();
        let id = stretch_lane(&model);
        let worker = worker(&model, Duration::from_millis(300));
        worker.wake();
        assert!(lock_model(&model).set_time_mode(id, TimeMode::Off));
        worker.wake();
        assert_eq!(worker.finish(), 0, "nothing pending once the lane left Stretch");
        assert!(lock_model(&model).lane(id).unwrap().render.is_none());
    }

    #[test]
    fn a_failed_render_reports_and_is_not_retried_on_a_loop() {
        let model = shared_model();
        let id = stretch_lane(&model);
        // A 1/16 triplet at 999 BPM is ~10 ms — shorter than the
        // stretcher's latency, so `exact` refuses it.
        lock_model(&model).tempo().set(999.0);
        assert!(lock_model(&model).set_length(id, Length::Beats(NoteLength::SixteenthTriplet)));
        assert_eq!(worker(&model, Duration::ZERO).finish(), 1);
        let model = lock_model(&model);
        let lane = model.lane(id).unwrap();
        assert!(!lane.render_pending);
        assert!(lane.render_error.is_some(), "the failure is shown");
        assert!(lane.render.is_none());
    }

    #[test]
    fn a_landed_render_rings_the_repaint_hook_once() {
        let model = shared_model();
        let id = stretch_lane(&model);
        let (hook, rings) = counting_hook();
        lock_model(&model).set_repaint_hook(hook);
        assert_eq!(rings.load(Ordering::Relaxed), 0);

        assert_eq!(worker(&model, Duration::ZERO).finish(), 1);
        assert_eq!(rings.load(Ordering::Relaxed), 1, "one ring for one landed render");
        assert!(lock_model(&model).lane(id).unwrap().render.is_some());
    }

    #[test]
    fn dropping_the_worker_stops_it_without_rendering_what_is_queued() {
        let model = shared_model();
        stretch_lane(&model);
        let worker = worker(&model, Duration::from_secs(60));
        drop(worker);
        let model = lock_model(&model);
        assert!(model.lanes()[0].render.is_none());
        assert!(model.lanes()[0].render_pending, "abandoned, still pending");
    }
}
