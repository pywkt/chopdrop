//! Which lanes the lane panel acts on.
//!
//! Pure: a [`Selection`] is a set of lane **ids** plus an anchor, and the
//! three gestures over a row — click, Ctrl+click, Shift+click — are methods
//! on it. Nothing here knows about egui; the rows translate a pointer press
//! and its modifiers into one of [`Selection::click`],
//! [`Selection::ctrl_click`] or [`Selection::shift_click`].
//!
//! # Ids, not indices
//!
//! Lanes are reordered, inserted and deleted under the selection, so it is
//! kept over ids, which survive all three. Only the Shift range needs
//! positions, and it takes them from the display order passed in at the
//! moment of the click: a range across a reordered list is the run of rows
//! the user sees between the anchor and the clicked lane, not whatever was
//! between them when the anchor was set.
//!
//! # Anchor
//!
//! A click or Ctrl+click sets the anchor to the clicked lane (whether the
//! Ctrl+click turned it on or off, as file managers do); a Shift+click keeps
//! it, so successive Shift+clicks re-range from the same lane. A Shift+click
//! with no anchor, or with an anchor no longer in the list, behaves as a
//! plain click.
//!
//! # Pruning
//!
//! The selection is per window and pruned every frame ([`Selection::prune`])
//! to the lanes that still exist: a deleted lane leaves the set, and if it
//! was the anchor the anchor goes too. Pruning does not allocate.

/// The selected lanes of one editor window, by lane id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Where a Shift+click range starts; the last lane clicked or
    /// Ctrl+clicked.
    anchor: Option<u32>,
    /// Selected lane ids, no duplicates. A Shift range is stored in display
    /// order; Ctrl+click appends.
    ids: Vec<u32>,
}

impl Selection {
    /// An empty selection with no anchor.
    pub fn new() -> Self {
        Self::default()
    }

    /// The Shift+click anchor, if any.
    pub fn anchor(&self) -> Option<u32> {
        self.anchor
    }

    /// The selected lane ids.
    pub fn ids(&self) -> &[u32] {
        &self.ids
    }

    /// Whether `id` is selected (its row gets `SELECTED_ROW`).
    pub fn contains(&self, id: u32) -> bool {
        self.ids.contains(&id)
    }

    /// Whether no lane is selected.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// How many lanes are selected ("N lanes" in the panel header).
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Plain click on a row's name or background: just this lane.
    pub fn click(&mut self, id: u32) {
        self.ids.clear();
        self.ids.push(id);
        self.anchor = Some(id);
    }

    /// Ctrl+click: toggle this lane, leaving the others; it becomes the
    /// anchor either way.
    pub fn ctrl_click(&mut self, id: u32) {
        if let Some(i) = self.ids.iter().position(|&x| x == id) {
            self.ids.remove(i);
        } else {
            self.ids.push(id);
        }
        self.anchor = Some(id);
    }

    /// Shift+click: select the display-order range from the anchor to `id`,
    /// inclusive, replacing the selection; the anchor stays. `order` is the
    /// lane ids as the rows show them now. Without a usable anchor (none,
    /// or not in `order`) this is a plain click; a clicked id missing from
    /// `order` is ignored.
    pub fn shift_click(&mut self, id: u32, order: &[u32]) {
        let Some(to) = order.iter().position(|&x| x == id) else {
            return;
        };
        let from = self.anchor.and_then(|a| order.iter().position(|&x| x == a));
        let Some(from) = from else {
            self.click(id);
            return;
        };
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        self.ids.clear();
        self.ids.extend_from_slice(&order[lo..=hi]);
    }

    /// Drop every id not in `existing` (the lanes that still exist); the
    /// anchor too when its lane is gone. Called every frame.
    pub fn prune(&mut self, existing: &[u32]) {
        self.ids.retain(|id| existing.contains(id));
        if self.anchor.is_some_and(|a| !existing.contains(&a)) {
            self.anchor = None;
        }
    }

    /// Deselect everything and forget the anchor.
    pub fn clear(&mut self) {
        self.ids.clear();
        self.anchor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_empty() {
        let s = Selection::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.anchor(), None);
    }

    #[test]
    fn click_selects_only_that_lane() {
        let mut s = Selection::new();
        s.click(3);
        assert_eq!(s.ids(), &[3]);
        assert_eq!(s.anchor(), Some(3));
        s.ctrl_click(5);
        s.click(7);
        assert_eq!(s.ids(), &[7]);
        assert_eq!(s.anchor(), Some(7));
        assert!(!s.contains(3) && !s.contains(5) && s.contains(7));
    }

    #[test]
    fn click_on_selected_lane_collapses_to_it() {
        let mut s = Selection::new();
        s.click(1);
        s.ctrl_click(2);
        s.click(2);
        assert_eq!(s.ids(), &[2]);
    }

    #[test]
    fn ctrl_click_toggles_and_moves_anchor() {
        let mut s = Selection::new();
        s.ctrl_click(4);
        assert_eq!(s.ids(), &[4]);
        assert_eq!(s.anchor(), Some(4));
        s.ctrl_click(9);
        assert_eq!(s.ids(), &[4, 9]);
        assert_eq!(s.anchor(), Some(9));
        s.ctrl_click(4);
        assert_eq!(s.ids(), &[9]);
        assert_eq!(s.anchor(), Some(4));
        s.ctrl_click(9);
        assert!(s.is_empty());
        assert_eq!(s.anchor(), Some(9));
    }

    #[test]
    fn shift_click_ranges_forward_and_backward() {
        let order = [10, 11, 12, 13, 14];
        let mut s = Selection::new();
        s.click(11);
        s.shift_click(13, &order);
        assert_eq!(s.ids(), &[11, 12, 13]);
        assert_eq!(s.anchor(), Some(11));
        // Re-range from the same anchor, backwards, replacing the last range.
        s.shift_click(10, &order);
        assert_eq!(s.ids(), &[10, 11]);
        assert_eq!(s.anchor(), Some(11));
        // Onto the anchor itself: just the anchor.
        s.shift_click(11, &order);
        assert_eq!(s.ids(), &[11]);
    }

    #[test]
    fn shift_click_replaces_ctrl_selection() {
        let order = [1, 2, 3, 4, 5];
        let mut s = Selection::new();
        s.click(5);
        s.ctrl_click(2);
        s.shift_click(4, &order);
        assert_eq!(s.ids(), &[2, 3, 4]);
        assert_eq!(s.anchor(), Some(2));
    }

    #[test]
    fn shift_click_without_anchor_is_a_click() {
        let order = [1, 2, 3];
        let mut s = Selection::new();
        s.shift_click(2, &order);
        assert_eq!(s.ids(), &[2]);
        assert_eq!(s.anchor(), Some(2));
    }

    #[test]
    fn shift_click_on_unknown_id_is_ignored() {
        let order = [1, 2, 3];
        let mut s = Selection::new();
        s.click(1);
        s.shift_click(99, &order);
        assert_eq!(s.ids(), &[1]);
        assert_eq!(s.anchor(), Some(1));
    }

    #[test]
    fn shift_range_follows_reordered_display_order() {
        let mut s = Selection::new();
        s.click(1);
        s.shift_click(3, &[1, 2, 3, 4]);
        assert_eq!(s.ids(), &[1, 2, 3]);
        // Lanes reordered: 4 now sits between 1 and 3, 2 moved to the end.
        let reordered = [1, 4, 3, 2];
        s.shift_click(3, &reordered);
        assert_eq!(s.ids(), &[1, 4, 3]);
        // Anchor moved below the clicked lane: the range is still contiguous.
        let moved = [3, 2, 4, 1];
        s.shift_click(2, &moved);
        assert_eq!(s.ids(), &[2, 4, 1]);
        assert_eq!(s.anchor(), Some(1));
    }

    #[test]
    fn prune_drops_deleted_lane() {
        let mut s = Selection::new();
        s.click(1);
        s.ctrl_click(2);
        s.ctrl_click(3);
        s.prune(&[1, 2, 3, 4]);
        assert_eq!(s.ids(), &[1, 2, 3]);
        assert_eq!(s.anchor(), Some(3));
        s.prune(&[1, 3, 4]);
        assert_eq!(s.ids(), &[1, 3]);
        assert_eq!(s.anchor(), Some(3));
    }

    #[test]
    fn prune_drops_deleted_anchor() {
        let mut s = Selection::new();
        s.click(1);
        s.ctrl_click(2);
        s.prune(&[1, 3]);
        assert_eq!(s.ids(), &[1]);
        assert_eq!(s.anchor(), None);
        // With the anchor gone, Shift+click falls back to a plain click.
        s.shift_click(3, &[1, 3]);
        assert_eq!(s.ids(), &[3]);
        assert_eq!(s.anchor(), Some(3));
    }

    #[test]
    fn prune_everything_and_clear() {
        let mut s = Selection::new();
        s.click(1);
        s.prune(&[]);
        assert!(s.is_empty());
        assert_eq!(s.anchor(), None);
        s.click(2);
        s.clear();
        assert_eq!(s, Selection::new());
    }
}
