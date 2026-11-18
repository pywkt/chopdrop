//! The window's way out: stop the preview when the host closes the editor.
//!
//! nice-plug-egui 0.6 never calls `NiceEguiApp::editor_closed`, so nothing
//! inside the app learns that its window went away; a looping chop preview
//! would keep sounding until the next window's `build()` stopped it. What
//! the host *does* do on close is drop the [`EditorHandle`] that `spawn`
//! returned (nice-plug-core: "When this handle is dropped, the editor
//! instance is also dropped"). So [`StopsOnClose`] wraps nice-plug-egui's
//! editor and hands the host a [`ClosingHandle`] around its handle, whose
//! `Drop` stops the previewer. Everything else is passed straight through.
//!
//! Generic over the wrapped editor so the handle's drop is tested with no
//! display ([`ClosingHandle::new`] over a stand-in handle).

use std::error::Error;
use std::sync::Arc;

use nice_plug::context::gui::GuiContext;
use nice_plug::editor::dpi::NativeSize;
use nice_plug::editor::{
    Editor, EditorHandle, HostMethods, Modifiers, ParentWindowHandle, ResizeHint, SpawnedEditor,
    VirtualKeyCode,
};
use nice_plug::plugin::TrackInfo;

use crate::preview::Previewer;

/// An editor whose spawned windows stop the instance's preview when the
/// host closes them.
pub struct StopsOnClose<E: Editor> {
    inner: E,
    previewer: Arc<Previewer>,
}

impl<E: Editor> StopsOnClose<E> {
    /// `inner`, with `previewer` stopped whenever one of its windows closes.
    pub fn new(inner: E, previewer: Arc<Previewer>) -> Self {
        StopsOnClose { inner, previewer }
    }

    /// The wrapped editor.
    pub fn inner(&self) -> &E {
        &self.inner
    }
}

impl<E: Editor> Editor for StopsOnClose<E> {
    type Handle = ClosingHandle<E::Handle>;

    fn spawn(
        &self,
        parent: Option<ParentWindowHandle>,
        wait_for_parent: bool,
        fallback_scale_factor: Option<f64>,
        gui_context: GuiContext,
        host: Option<HostMethods>,
    ) -> Result<SpawnedEditor<Self::Handle>, Box<dyn Error>> {
        let SpawnedEditor { handle, window } =
            self.inner.spawn(parent, wait_for_parent, fallback_scale_factor, gui_context, host)?;
        let handle = ClosingHandle::new(handle, Arc::clone(&self.previewer));
        Ok(SpawnedEditor { handle, window })
    }

    fn size(&self) -> NativeSize<u32> {
        self.inner.size()
    }

    fn resize_hint(&self) -> ResizeHint {
        self.inner.resize_hint()
    }

    fn track_info_updated(&self, info: TrackInfo) {
        self.inner.track_info_updated(info);
    }

    unsafe fn assume_standalone_in_process(&self) {
        // SAFETY: the caller upholds the wrapped editor's own contract,
        // which this forwards unchanged.
        unsafe { self.inner.assume_standalone_in_process() }
    }
}

/// The handle the host holds while a window is open; dropping it (the host
/// closing the editor) stops the preview, then drops the wrapped handle.
pub struct ClosingHandle<H: EditorHandle> {
    inner: H,
    previewer: Arc<Previewer>,
}

impl<H: EditorHandle> ClosingHandle<H> {
    /// `inner`, stopping `previewer` when dropped.
    pub fn new(inner: H, previewer: Arc<Previewer>) -> Self {
        ClosingHandle { inner, previewer }
    }
}

impl<H: EditorHandle> Drop for ClosingHandle<H> {
    fn drop(&mut self) {
        self.previewer.stop();
    }
}

impl<H: EditorHandle> EditorHandle for ClosingHandle<H> {
    type Window = H::Window;
    type Error = H::Error;

    fn run_until_closed(window: Self::Window) -> Result<(), Self::Error> {
        H::run_until_closed(window)
    }

    fn set_parent(
        &self,
        parent: ParentWindowHandle,
        window: &Self::Window,
    ) -> Result<(), Self::Error> {
        self.inner.set_parent(parent, window)
    }

    fn show(&self, window: &Self::Window) -> Result<(), Self::Error> {
        self.inner.show(window)
    }

    fn hide(&self, window: &Self::Window) -> Result<(), Self::Error> {
        self.inner.hide(window)
    }

    fn set_size(
        &self,
        new_size: NativeSize<u32>,
        window: &Self::Window,
    ) -> Result<(), Self::Error> {
        self.inner.set_size(new_size, window)
    }

    fn host_main_thread_callback(&self, window: &Self::Window) {
        self.inner.host_main_thread_callback(window);
    }

    fn adjust_size(
        &self,
        new_size: NativeSize<u32>,
        window: &Self::Window,
    ) -> Option<NativeSize<u32>> {
        self.inner.adjust_size(new_size, window)
    }

    fn set_fallback_scale_factor(
        &self,
        scale_factor: f64,
        window: &Self::Window,
    ) -> Result<(), Self::Error> {
        self.inner.set_fallback_scale_factor(scale_factor, window)
    }

    fn on_virtual_key_from_host(
        &self,
        key_code: VirtualKeyCode,
        is_down: bool,
        modifiers: Modifiers,
    ) -> bool {
        self.inner.on_virtual_key_from_host(key_code, is_down, modifiers)
    }

    fn state_changed(&self) {
        self.inner.state_changed();
    }

    fn param_value_changed(&self, id: &str, normalized_value: f32) {
        self.inner.param_value_changed(id, normalized_value);
    }

    fn param_modulation_changed(&self, id: &str, modulation_offset: f32) {
        self.inner.param_modulation_changed(id, modulation_offset);
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::editor::tests::{fresh_params, fresh_previewer};
    use crate::preview::PreviewState;
    use crate::sample::SampleData;

    /// A stand-in for nice-plug-egui's handle, counting its own drop.
    struct FakeHandle(Arc<AtomicUsize>);

    impl Drop for FakeHandle {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Debug)]
    struct NoError;

    impl fmt::Display for NoError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("none")
        }
    }

    impl Error for NoError {}

    impl EditorHandle for FakeHandle {
        type Window = ();
        type Error = NoError;

        fn run_until_closed(_window: ()) -> Result<(), NoError> {
            Ok(())
        }
        fn set_parent(&self, _parent: ParentWindowHandle, _window: &()) -> Result<(), NoError> {
            Ok(())
        }
        fn show(&self, _window: &()) -> Result<(), NoError> {
            Ok(())
        }
        fn hide(&self, _window: &()) -> Result<(), NoError> {
            Ok(())
        }
        fn set_size(&self, _new_size: NativeSize<u32>, _window: &()) -> Result<(), NoError> {
            Ok(())
        }
        fn host_main_thread_callback(&self, _window: &()) {}
        fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}
        fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}
    }

    /// A looping chop preview is sounding when the host closes
    /// the window (drops the handle) — the preview is stopped (silence
    /// published under a newer generation, live loop off) and the wrapped
    /// handle is dropped too.
    #[test]
    fn closing_the_window_stops_the_strips_preview() {
        let params = fresh_params();
        let previewer = fresh_previewer(&params);
        let model = Arc::clone(params.lanes.model());
        let handle = crate::model::lock_model(&model).handle().clone();
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 100].into() };
        let source = basedrop::Shared::new(&handle, data);
        previewer.play_region(source, crate::preview::Region { start: 0, end: 50 }, true);
        let channel = Arc::clone(previewer.channel());
        let playing: basedrop::Shared<PreviewState> = channel.get();
        assert!(playing.sample.is_some() && playing.looping, "a looping preview is up");
        assert!(channel.live_looping());
        let generation = playing.generation;
        drop(playing);

        let dropped = Arc::new(AtomicUsize::new(0));
        let closing = ClosingHandle::new(FakeHandle(Arc::clone(&dropped)), Arc::clone(&previewer));
        assert!(channel.get().sample.is_some(), "an open window leaves it playing");

        drop(closing);
        let after = channel.get();
        assert!(after.sample.is_none(), "closing the window silenced the preview");
        assert!(after.generation > generation, "a newer generation than the preview's");
        assert!(!channel.live_looping(), "nothing restarts at a loop seam");
        assert_eq!(dropped.load(Ordering::Relaxed), 1, "the wrapped handle went too");
    }

    /// The wrapper changes nothing the host asks of the editor before a
    /// window exists: the same size and resize hint as the editor inside.
    #[test]
    fn the_wrapper_passes_the_size_and_hint_through() {
        let params = fresh_params();
        let previewer = fresh_previewer(&params);
        let editor = crate::editor::create(
            crate::editor::default_state(),
            nice_plug_egui::RepaintNotifier::new(),
            Arc::clone(&params),
            Arc::new(crate::engine::AuditionRing::new()),
            previewer,
        )
        .expect("an editor is always built");
        let hint = editor.inner().resize_hint();
        assert_eq!(editor.resize_hint().can_resize, hint.can_resize);
        assert_eq!(editor.resize_hint().size_constraints, hint.size_constraints);
        assert_eq!(editor.size(), editor.inner().size());
    }
}
