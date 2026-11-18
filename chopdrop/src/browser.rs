//! The sample browser's model: folder listing, the filter, the selection, the
//! quick places, and the remembered settings.
//!
//! Pure: no egui and no nice-plug, so all of it is tested here against
//! scratch folders. The panel that draws it is `editor/browser_panel.rs`.

use std::cmp::Ordering;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file extensions the browser lists, lower case: what the loader
/// decodes. Anything else in a folder is hidden.
pub const AUDIO_EXTENSIONS: [&str; 9] =
    ["wav", "wave", "aif", "aiff", "aifc", "flac", "mp3", "ogg", "oga"];

/// The most entries one folder lists. A folder with more shows the first
/// this many (folders first, sorted), so a huge one can't stall a frame.
pub const MAX_ENTRIES: usize = 5000;

/// How many recent folders are remembered.
pub const MAX_RECENT: usize = 8;

/// Whether `path` has one of the [`AUDIO_EXTENSIONS`], in any case.
pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            AUDIO_EXTENSIONS.iter().any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A folder, opened on activation.
    Folder,
    /// An audio file, loaded on activation.
    Audio,
}

/// One line of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The file or folder name, as shown.
    pub name: String,
    /// Where it is.
    pub path: PathBuf,
    /// Folder or audio file.
    pub kind: Kind,
}

/// The folders and audio files in `dir`: folders first, then files, each
/// sorted case-insensitively by name. Hidden entries (a leading `.`) and
/// everything that is neither a folder nor an audio file are left out, as are
/// entries that can't be read. A symlink counts as what it points at.
pub fn list_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = item.path();
        // `metadata` follows symlinks, which is what a user means by a
        // linked sample folder.
        let Ok(metadata) = std::fs::metadata(&path) else { continue };
        let kind = if metadata.is_dir() {
            Kind::Folder
        } else if metadata.is_file() && is_audio(&path) {
            Kind::Audio
        } else {
            continue;
        };
        entries.push(Entry { name, path, kind });
    }
    entries.sort_by(compare);
    entries.truncate(MAX_ENTRIES);
    Ok(entries)
}

/// Folders before files, then by name without regard to case (ties broken by
/// case, so the order is total and stable).
fn compare(a: &Entry, b: &Entry) -> Ordering {
    let rank = |entry: &Entry| match entry.kind {
        Kind::Folder => 0,
        Kind::Audio => 1,
    };
    rank(a)
        .cmp(&rank(b))
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

/// What activating the selection did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activation {
    /// Nothing was selected.
    Nothing,
    /// A folder was opened (the browser now shows it).
    Opened,
    /// This audio file is to be loaded.
    Load(PathBuf),
}

/// The browser's state: a folder, what is in it, a filter and a selection.
#[derive(Debug, Default)]
pub struct Browser {
    /// The folder being shown. `None` before the first [`open`](Self::open).
    folder: Option<PathBuf>,
    /// Everything listed in it.
    entries: Vec<Entry>,
    /// The filter text: entries whose name contains it, ignoring case.
    filter: String,
    /// Index into the *filtered* list, or `None`.
    selected: Option<usize>,
    /// Why the last folder asked for could not be shown, if it couldn't.
    error: Option<String>,
}

impl Browser {
    /// The folder being shown.
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    /// Why the last folder asked for could not be shown, if it couldn't.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The filter text.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Shows `folder`, clearing the filter and the selection. A folder that
    /// can't be read is reported ([`error`](Self::error)) and the nearest
    /// readable ancestor is shown instead, so the browser always shows
    /// somewhere.
    pub fn open(&mut self, folder: &Path) {
        self.filter.clear();
        self.selected = None;
        self.error = None;
        let mut candidate = Some(folder);
        while let Some(dir) = candidate {
            match list_dir(dir) {
                Ok(entries) => {
                    self.folder = Some(dir.to_path_buf());
                    self.entries = entries;
                    return;
                }
                Err(error) => {
                    if self.error.is_none() {
                        self.error = Some(format!("can't open {}: {}", dir.display(), error));
                    }
                    candidate = dir.parent();
                }
            }
        }
        self.folder = None;
        self.entries.clear();
    }

    /// Shows the parent folder, with the folder just left selected.
    pub fn up(&mut self) {
        let Some(folder) = self.folder.clone() else { return };
        let Some(parent) = folder.parent() else { return };
        self.open(parent);
        self.select_path(&folder);
    }

    /// Sets the filter, keeping the selection on the same entry when it still
    /// shows, and on the first entry otherwise.
    pub fn set_filter(&mut self, filter: &str) {
        if filter == self.filter {
            return;
        }
        let selected = self.selected().map(|entry| entry.path.clone());
        self.filter = filter.to_string();
        self.selected = None;
        if let Some(path) = selected {
            self.select_path(&path);
        }
        if self.selected.is_none() && !self.visible().is_empty() {
            self.selected = Some(0);
        }
    }

    /// The entries the filter lets through, in order.
    pub fn visible(&self) -> Vec<&Entry> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            .collect()
    }

    /// Selects the `index`th visible entry (clamped), or nothing for `None`.
    pub fn select(&mut self, index: Option<usize>) {
        let count = self.visible().len();
        self.selected = index.filter(|_| count > 0).map(|index| index.min(count - 1));
    }

    /// Selects the visible entry at `path`, if there is one.
    pub fn select_path(&mut self, path: &Path) {
        if let Some(index) = self.visible().iter().position(|entry| entry.path == path) {
            self.selected = Some(index);
        }
    }

    /// The index of the selected visible entry.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected.filter(|&index| index < self.visible().len())
    }

    /// The selected entry.
    pub fn selected(&self) -> Option<&Entry> {
        let index = self.selected_index()?;
        self.visible().get(index).copied()
    }

    /// Moves the selection by `delta` (clamped to the list). With nothing
    /// selected, a move down selects the first entry and a move up the last.
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.visible().len();
        if count == 0 {
            self.selected = None;
            return;
        }
        let next = match self.selected_index() {
            Some(index) => index.saturating_add_signed(delta).min(count - 1),
            None if delta < 0 => count - 1,
            None => 0,
        };
        self.selected = Some(next);
    }

    /// Opens the selected folder, or says which file to load.
    pub fn activate(&mut self) -> Activation {
        let Some(entry) = self.selected().cloned() else { return Activation::Nothing };
        match entry.kind {
            Kind::Folder => {
                self.open(&entry.path);
                Activation::Opened
            }
            Kind::Audio => Activation::Load(entry.path),
        }
    }
}

/// One of the panel's quick places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// The button's label.
    pub label: String,
    /// Where it goes.
    pub path: PathBuf,
}

/// The quick places that exist, in order: the target lane's sample folder,
/// Bitwig's installed packages, the Bitwig library, Music, Home. Recent
/// folders are separate ([`Settings::recent`]).
pub fn places(home: &Path, lane_folder: Option<&Path>) -> Vec<Place> {
    let mut found = Vec::new();
    let mut push = |label: &str, path: PathBuf| {
        if path.is_dir() && !found.iter().any(|place: &Place| place.path == path) {
            found.push(Place { label: label.to_string(), path });
        }
    };
    if let Some(folder) = lane_folder {
        push("lane", folder.to_path_buf());
    }
    push("packages", home.join(".BitwigStudio").join("installed-packages"));
    push("library", home.join("Bitwig Studio"));
    push("music", home.join("Music"));
    push("home", home.to_path_buf());
    found
}

/// The user's home folder, from `$HOME`.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|home| !home.is_empty()).map(PathBuf::from)
}

/// The browser's remembered settings, shared by every instance: where your
/// samples live is about you, not a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Whether selecting a file previews it. On by default.
    pub auto_preview: bool,
    /// The folder last loaded from.
    pub folder: Option<PathBuf>,
    /// Folders recently loaded from, newest first, at most [`MAX_RECENT`].
    pub recent: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto_preview: true, folder: None, recent: Vec::new() }
    }
}

impl Settings {
    /// Where the settings live: `$XDG_CONFIG_HOME/chopdrop/browser.json`, or
    /// `~/.config/chopdrop/browser.json`.
    pub fn path() -> Option<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| home().map(|home| home.join(".config")))?;
        Some(config.join("chopdrop").join("browser.json"))
    }

    /// Reads the settings at `path`. A missing or broken file gives the
    /// defaults, never an error; the recent list is trimmed to
    /// [`MAX_RECENT`].
    pub fn load_from(path: &Path) -> Settings {
        let mut settings: Settings = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        settings.recent.truncate(MAX_RECENT);
        settings
    }

    /// Writes the settings to `path`, making its folder if need be.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Notes that `file` was loaded: its folder becomes the folder to open
    /// next time and the newest recent folder.
    pub fn remember_load(&mut self, file: &Path) {
        let Some(folder) = file.parent() else { return };
        self.folder = Some(folder.to_path_buf());
        self.recent.retain(|recent| recent != folder);
        self.recent.insert(0, folder.to_path_buf());
        self.recent.truncate(MAX_RECENT);
    }
}

/// Where the browser opens: the target lane's sample folder, the folder last
/// loaded from, Music, then Home — the first that exists.
pub fn start_folder(
    lane_folder: Option<&Path>,
    settings: &Settings,
    home: Option<&Path>,
) -> PathBuf {
    let candidates = [
        lane_folder.map(Path::to_path_buf),
        settings.folder.clone(),
        home.map(|home| home.join("Music")),
        home.map(Path::to_path_buf),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|path| path.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch folder for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("chopdrop-browser-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").expect("scratch file");
    }

    fn names(browser: &Browser) -> Vec<String> {
        browser.visible().iter().map(|entry| entry.name.clone()).collect()
    }

    /// A folder with two subfolders, three audio files in mixed case, and
    /// things that must not show.
    fn kit(name: &str) -> PathBuf {
        let dir = scratch(name);
        std::fs::create_dir(dir.join("snares")).unwrap();
        std::fs::create_dir(dir.join("Kicks")).unwrap();
        std::fs::create_dir(dir.join(".hidden")).unwrap();
        touch(&dir, "hat.WAV");
        touch(&dir, "Clap.flac");
        touch(&dir, "bd.aif");
        touch(&dir, "notes.txt");
        touch(&dir, ".secret.wav");
        touch(&dir, "clip.bwclip");
        dir
    }

    #[test]
    fn audio_is_known_by_extension_in_any_case() {
        for name in ["a.wav", "a.WAV", "a.Wave", "a.aiff", "a.aif", "a.aifc", "a.flac", "a.mp3"] {
            assert!(is_audio(Path::new(name)), "{name}");
        }
        for name in ["a.ogg", "a.oga"] {
            assert!(is_audio(Path::new(name)), "{name}");
        }
        for name in ["a.txt", "a.bwclip", "a", "wav", "a.wav.bak"] {
            assert!(!is_audio(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn a_folder_lists_folders_then_audio_sorted_without_case_or_clutter() {
        let dir = kit("list");
        let mut browser = Browser::default();
        browser.open(&dir);
        assert_eq!(names(&browser), ["Kicks", "snares", "bd.aif", "Clap.flac", "hat.WAV"]);
        assert_eq!(browser.visible()[0].kind, Kind::Folder);
        assert_eq!(browser.visible()[2].kind, Kind::Audio);
        assert_eq!(browser.folder(), Some(dir.as_path()));
        assert!(browser.error().is_none());
    }

    #[test]
    fn the_filter_matches_names_ignoring_case() {
        let dir = kit("filter");
        let mut browser = Browser::default();
        browser.open(&dir);
        browser.set_filter("CL");
        assert_eq!(names(&browser), ["Clap.flac"]);
        assert_eq!(browser.selected().map(|entry| entry.name.as_str()), Some("Clap.flac"));
        browser.set_filter("");
        assert_eq!(names(&browser).len(), 5);
        assert_eq!(
            browser.selected().map(|entry| entry.name.as_str()),
            Some("Clap.flac"),
            "the selection stays on the entry when the filter widens"
        );
    }

    #[test]
    fn the_selection_moves_and_stops_at_the_ends() {
        let dir = kit("select");
        let mut browser = Browser::default();
        browser.open(&dir);
        assert!(browser.selected().is_none());
        browser.move_selection(1);
        assert_eq!(browser.selected_index(), Some(0));
        browser.move_selection(10);
        assert_eq!(browser.selected_index(), Some(4));
        browser.move_selection(-2);
        assert_eq!(browser.selected_index(), Some(2));
        browser.move_selection(-10);
        assert_eq!(browser.selected_index(), Some(0));

        let mut fresh = Browser::default();
        fresh.open(&dir);
        fresh.move_selection(-1);
        assert_eq!(fresh.selected_index(), Some(4), "up from nothing selects the last");
    }

    #[test]
    fn activating_a_folder_opens_it_and_a_file_asks_to_load_it() {
        let dir = kit("activate");
        touch(&dir.join("Kicks"), "808.wav");
        let mut browser = Browser::default();
        browser.open(&dir);

        browser.select(Some(0));
        assert_eq!(browser.activate(), Activation::Opened);
        assert_eq!(browser.folder(), Some(dir.join("Kicks").as_path()));
        assert_eq!(names(&browser), ["808.wav"]);

        browser.select(Some(0));
        assert_eq!(browser.activate(), Activation::Load(dir.join("Kicks").join("808.wav")));

        browser.select(None);
        assert_eq!(browser.activate(), Activation::Nothing);
    }

    #[test]
    fn up_shows_the_parent_with_the_folder_left_selected() {
        let dir = kit("up");
        let mut browser = Browser::default();
        browser.open(&dir.join("snares"));
        browser.up();
        assert_eq!(browser.folder(), Some(dir.as_path()));
        assert_eq!(browser.selected().map(|entry| entry.name.as_str()), Some("snares"));
    }

    #[test]
    fn a_folder_that_is_gone_shows_its_nearest_ancestor_and_says_so() {
        let dir = kit("gone");
        let mut browser = Browser::default();
        browser.open(&dir.join("not").join("there"));
        assert_eq!(browser.folder(), Some(dir.as_path()));
        assert!(browser.error().is_some_and(|error| error.contains("can't open")));
    }

    #[test]
    fn places_are_the_ones_that_exist_in_order_without_repeats() {
        let home = scratch("home");
        std::fs::create_dir_all(home.join(".BitwigStudio").join("installed-packages")).unwrap();
        std::fs::create_dir_all(home.join("Music")).unwrap();
        let labels = |places: Vec<Place>| -> Vec<String> {
            places.into_iter().map(|place| place.label).collect()
        };
        assert_eq!(labels(places(&home, None)), ["packages", "music", "home"]);
        assert_eq!(
            labels(places(&home, Some(&home.join("Music")))),
            ["lane", "packages", "home"],
            "the lane's folder comes first and is not repeated as music"
        );
        assert_eq!(labels(places(&home, Some(&home.join("nope")))), ["packages", "music", "home"]);
    }

    #[test]
    fn settings_default_to_auto_preview_on() {
        assert!(Settings::default().auto_preview);
    }

    #[test]
    fn settings_round_trip_and_survive_a_broken_file() {
        let dir = scratch("settings");
        let path = dir.join("sub").join("browser.json");
        assert_eq!(Settings::load_from(&path), Settings::default(), "no file: defaults");

        let mut settings = Settings { auto_preview: false, ..Settings::default() };
        settings.remember_load(&dir.join("kick.wav"));
        settings.save_to(&path).expect("saved");
        assert_eq!(Settings::load_from(&path), settings);

        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        std::fs::write(&path, br#"{"auto_preview": false, "unknown": 3}"#).unwrap();
        let partial = Settings::load_from(&path);
        assert!(!partial.auto_preview, "known fields kept, unknown ignored");
        assert!(partial.recent.is_empty());
    }

    #[test]
    fn remembering_a_load_keeps_recent_folders_newest_first_and_bounded() {
        let mut settings = Settings::default();
        for n in 0..10 {
            settings.remember_load(&PathBuf::from(format!("/s/{n}/x.wav")));
        }
        settings.remember_load(&PathBuf::from("/s/5/y.wav"));
        assert_eq!(settings.recent.len(), MAX_RECENT);
        assert_eq!(settings.recent[0], PathBuf::from("/s/5"));
        assert_eq!(settings.recent.iter().filter(|p| p.as_path() == Path::new("/s/5")).count(), 1);
        assert_eq!(settings.folder, Some(PathBuf::from("/s/5")));
    }

    #[test]
    fn the_browser_opens_on_the_lane_folder_then_the_last_folder_then_music() {
        let home = scratch("start");
        std::fs::create_dir_all(home.join("Music")).unwrap();
        let last = home.join("last");
        std::fs::create_dir_all(&last).unwrap();
        let lane = home.join("lane");
        std::fs::create_dir_all(&lane).unwrap();
        let settings = Settings { folder: Some(last.clone()), ..Settings::default() };

        assert_eq!(start_folder(Some(&lane), &settings, Some(&home)), lane);
        assert_eq!(start_folder(None, &settings, Some(&home)), last);
        assert_eq!(start_folder(None, &Settings::default(), Some(&home)), home.join("Music"));
        let gone = Settings { folder: Some(home.join("gone")), ..Settings::default() };
        assert_eq!(start_folder(Some(&home.join("x")), &gone, Some(&home)), home.join("Music"));
    }
}
