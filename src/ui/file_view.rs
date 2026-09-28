//! The two file presentations: an icon grid and a details list.
//!
//! Both are driven by one model chain — `ListStore -> FilterListModel ->
//! SortListModel -> MultiSelection` — so switching views keeps the selection,
//! the sort and the filter exactly as they were. The `ColumnView`'s own sorter
//! is the single source of sort truth for *both* views, which is what lets the
//! grid follow a column header click.
use crate::tr;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
};

use gtk::{gdk, gio, glib, prelude::*};

use crate::{
    config::{Config, SortKey, ViewMode},
    fs::FileEntry,
    ui::{file_object::FileObject, thumbs},
};

/// Icons never render larger than this in the details list; a 96px row would
/// fit three files on screen.
const LIST_ICON_MAX: i32 = 32;

type Callback<T> = RefCell<Option<Rc<dyn Fn(T)>>>;

/// Invokes a stored callback without holding its `RefCell` borrow.
///
/// Calling through `slot.borrow().as_ref()` keeps the borrow alive for the
/// whole call, so any handler that re-entered the same slot — or replaced the
/// callback — aborted the process. Cloning the `Rc` out first makes re-entry
/// harmless.
fn emit<T>(slot: &Callback<T>, arg: T) {
    let callback = slot.borrow().clone();
    if let Some(callback) = callback {
        callback(arg);
    }
}

/// The same, for callbacks that take no argument.
fn emit_unit(slot: &RefCell<Option<Rc<dyn Fn()>>>) {
    let callback = slot.borrow().clone();
    if let Some(callback) = callback {
        callback();
    }
}

pub struct FileView {
    stack: gtk::Stack,
    grid: gtk::GridView,
    columns: gtk::ColumnView,
    pub model: gio::ListStore,
    pub selection: gtk::MultiSelection,
    filter: gtk::CustomFilter,
    config: Rc<RefCell<Config>>,
    /// Lowercased live-search text; empty means "show everything".
    search: Rc<RefCell<String>>,
    name_column: gtk::ColumnViewColumn,
    size_column: gtk::ColumnViewColumn,
    modified_column: gtk::ColumnViewColumn,
    kind_column: gtk::ColumnViewColumn,

    on_activate: Callback<FileObject>,
    on_selection_changed: RefCell<Option<Rc<dyn Fn()>>>,
    on_context_menu: Callback<(f64, f64)>,
    /// Files dropped, plus the directory they were dropped onto (`None` = here).
    on_drop: Callback<(Vec<PathBuf>, Option<PathBuf>)>,
}

impl FileView {
    pub fn new(config: Rc<RefCell<Config>>) -> Rc<Self> {
        let model = gio::ListStore::new::<FileObject>();

        let search = Rc::new(RefCell::new(String::new()));
        let filter = build_filter(Rc::clone(&config), Rc::clone(&search));
        let filter_model = gtk::FilterListModel::new(Some(model.clone()), Some(filter.clone()));
        // Filtering a large folder is spread across frames rather than done in
        // one blocking pass, so typing stays responsive at 50k entries.
        filter_model.set_incremental(true);

        // Sorting runs incrementally so a directory with 100k entries doesn't
        // freeze the frame that reveals it.
        let sort_model = gtk::SortListModel::new(Some(filter_model), None::<gtk::Sorter>);
        sort_model.set_incremental(true);

        let selection = gtk::MultiSelection::new(Some(sort_model.clone()));

        let dirs_first = Rc::new(Cell::new(config.borrow().dirs_first));
        let (columns, name_column, size_column, modified_column, kind_column) =
            build_column_view(&selection, Rc::clone(&config), Rc::clone(&dirs_first));

        // Both views sort by the ColumnView's sorter, so a header click
        // reorders the grid too.
        sort_model.set_sorter(columns.sorter().as_ref());

        let grid = build_grid_view(&selection, Rc::clone(&config));

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(100)
            .vexpand(true)
            .build();
        stack.add_named(&wrap_scrolled(&grid), Some("grid"));
        stack.add_named(&wrap_scrolled(&columns), Some("list"));

        let this = Rc::new(Self {
            stack,
            grid,
            columns,
            model,
            selection,
            filter,
            config,
            search,
            name_column,
            size_column,
            modified_column,
            kind_column,
            on_activate: RefCell::new(None),
            on_selection_changed: RefCell::new(None),
            on_context_menu: RefCell::new(None),
            on_drop: RefCell::new(None),
        });

        this.wire_up();
        this.apply_sort_from_config();
        this.set_view_mode(this.config.borrow().view_mode);
        this
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    // ── callbacks ──────────────────────────────────────────────────────────

    pub fn connect_activate(&self, f: impl Fn(FileObject) + 'static) {
        *self.on_activate.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_selection_changed(&self, f: impl Fn() + 'static) {
        *self.on_selection_changed.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_context_menu(&self, f: impl Fn((f64, f64)) + 'static) {
        *self.on_context_menu.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_drop(&self, f: impl Fn((Vec<PathBuf>, Option<PathBuf>)) + 'static) {
        *self.on_drop.borrow_mut() = Some(Rc::new(f));
    }

    // ── content ────────────────────────────────────────────────────────────

    pub fn clear(&self) {
        self.model.remove_all();
    }

    pub fn append_batch(&self, entries: Vec<FileEntry>) {
        let objects: Vec<FileObject> = entries.into_iter().map(FileObject::new).collect();
        // Splicing the whole batch emits one items-changed instead of one per
        // row, which is the difference between a smooth and a stuttering scan.
        self.model.splice(self.model.n_items(), 0, &objects);
    }

    /// Brings the view in line with a fresh listing of the same folder,
    /// touching only the rows that actually differ.
    ///
    /// The old way to show a change was to clear the model and stream the
    /// whole folder back in. Every row was destroyed and rebuilt, so every icon
    /// and thumbnail was dropped and reloaded — a visible flicker across the
    /// whole window each time *anything* in the folder changed, which in a
    /// Downloads folder with a browser writing to it is constantly.
    ///
    /// Here an unchanged file keeps its object, and with it its widget, its
    /// loaded thumbnail and its place in the selection. Only files that
    /// appeared, vanished or changed are touched.
    pub fn apply_listing(&self, fresh: Vec<FileEntry>) {
        // Rebinding a row makes GTK treat it as a new item, which drops it from
        // the selection. Remembered so a selected file that merely changed
        // stays selected.
        let selected: HashSet<PathBuf> = self.selected_paths().into_iter().collect();
        if merge_listing(&self.model, fresh, &selected) {
            self.restore_selection(&selected);
        }
    }

    /// Re-selects `paths` without scrolling — unlike [`Self::select_paths`],
    /// which exists to jump to a result. Moving the view because a file in it
    /// was touched in the background would be its own kind of flicker.
    fn restore_selection(&self, paths: &HashSet<PathBuf>) {
        let total = self.selection.n_items();
        let wanted = gtk::Bitset::new_empty();
        for index in 0..total {
            if let Some(object) = self.selection.item(index).and_downcast::<FileObject>()
                && paths.contains(&object.path())
            {
                wanted.add(index);
            }
        }
        self.selection.set_selection(&wanted, &gtk::Bitset::new_range(0, total));
    }

    /// Every item currently shown, in the order shown — after sorting and
    /// filtering, so stepping through a preview visits what the user sees.
    pub fn visible_items(&self) -> Vec<FileObject> {
        (0..self.selection.n_items())
            .filter_map(|i| self.selection.item(i).and_downcast::<FileObject>())
            .collect()
    }

    /// Position of the first selected item among [`Self::visible_items`].
    pub fn first_selected_index(&self) -> Option<u32> {
        let selected = self.selection.selection();
        (!selected.is_empty()).then(|| selected.minimum())
    }

    /// Items currently selected, in view order.
    pub fn selected(&self) -> Vec<FileObject> {
        let bitset = self.selection.selection();
        let mut out = Vec::with_capacity(bitset.size() as usize);
        for i in 0..bitset.size() {
            let index = bitset.nth(i as u32);
            if let Some(obj) = self.selection.item(index).and_downcast::<FileObject>() {
                out.push(obj);
            }
        }
        out
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected().iter().map(|o| o.path()).collect()
    }

    pub fn select_all(&self) {
        self.selection.select_all();
    }

    pub fn select_none(&self) {
        self.selection.unselect_all();
    }

    /// Inverts the selection, which is genuinely useful for "everything but
    /// these three" and cheap to provide.
    pub fn invert_selection(&self) {
        let total = self.selection.n_items();
        let current = self.selection.selection();
        let inverted = gtk::Bitset::new_range(0, total);
        inverted.difference(&current);
        self.selection.set_selection(&inverted, &gtk::Bitset::new_range(0, total));
    }

    /// Selects the given paths and scrolls the first into view.
    ///
    /// Used after a paste or an extraction so the new files are obvious.
    pub fn select_paths(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let total = self.selection.n_items();
        let wanted = gtk::Bitset::new_empty();
        let mut first: Option<u32> = None;

        for index in 0..total {
            let Some(obj) = self.selection.item(index).and_downcast::<FileObject>() else {
                continue;
            };
            if paths.contains(&obj.path()) {
                wanted.add(index);
                first.get_or_insert(index);
            }
        }

        self.selection.set_selection(&wanted, &gtk::Bitset::new_range(0, total));
        if let Some(index) = first {
            self.scroll_to(index);
        }
    }

    fn scroll_to(&self, index: u32) {
        match self.stack.visible_child_name().as_deref() {
            Some("list") => self.columns.scroll_to(index, None, gtk::ListScrollFlags::NONE, None),
            _ => self.grid.scroll_to(index, gtk::ListScrollFlags::NONE, None),
        }
    }

    /// Number of items passing the current filter, and their total size.
    pub fn visible_stats(&self) -> (u32, u64, u32) {
        let mut bytes = 0u64;
        let mut folders = 0u32;
        let count = self.selection.n_items();
        for index in 0..count {
            let Some(obj) = self.selection.item(index).and_downcast::<FileObject>() else {
                continue;
            };
            let entry = obj.entry();
            if entry.is_dir {
                folders += 1;
            } else {
                bytes += entry.size;
            }
        }
        (count, bytes, folders)
    }

    // ── view state ─────────────────────────────────────────────────────────

    pub fn set_view_mode(&self, mode: ViewMode) {
        // Detach the model from the view that isn't showing. A GtkStack keeps
        // its hidden pages alive, so both views were binding every row —
        // building two sets of widgets and, worse, requesting every thumbnail
        // twice at two different sizes.
        match mode {
            ViewMode::Grid => {
                self.columns.set_model(None::<&gtk::MultiSelection>);
                self.grid.set_model(Some(&self.selection));
                self.stack.set_visible_child_name("grid");
            }
            ViewMode::List => {
                self.grid.set_model(None::<&gtk::MultiSelection>);
                self.columns.set_model(Some(&self.selection));
                self.stack.set_visible_child_name("list");
            }
        }
    }

    pub fn set_search(&self, text: &str) {
        let query = text.to_lowercase();
        // Telling GTK *how* the filter changed lets it re-test only the items
        // that can possibly have flipped: extending a query can only remove
        // matches, shortening it can only add them.
        let change = {
            let previous = self.search.borrow();
            if query == *previous {
                return;
            } else if query.starts_with(previous.as_str()) {
                gtk::FilterChange::MoreStrict
            } else if previous.starts_with(&query) {
                gtk::FilterChange::LessStrict
            } else {
                gtk::FilterChange::Different
            }
        };
        *self.search.borrow_mut() = query;
        self.filter.changed(change);
    }

    pub fn refilter(&self) {
        self.filter.changed(gtk::FilterChange::Different);
    }

    /// Re-runs both factories so a new icon size takes effect immediately.
    pub fn refresh_items(&self) {
        let mode = self.config.borrow().view_mode;
        self.grid.set_factory(Some(&grid_factory(Rc::clone(&self.config))));
        self.name_column.set_factory(Some(&name_cell_factory(Rc::clone(&self.config))));
        self.set_view_mode(mode);
    }

    /// Points the ColumnView's sorter at the column named by the config.
    pub fn apply_sort_from_config(&self) {
        let (key, descending) = {
            let cfg = self.config.borrow();
            (cfg.sort_key, cfg.sort_descending)
        };
        let column = match key {
            SortKey::Name => &self.name_column,
            SortKey::Size => &self.size_column,
            SortKey::Modified => &self.modified_column,
            SortKey::Kind => &self.kind_column,
        };
        let order = if descending { gtk::SortType::Descending } else { gtk::SortType::Ascending };
        self.columns.sort_by_column(Some(column), order);
    }

    /// Puts keyboard focus on the first item and scrolls it into view.
    ///
    /// A plain `grab_focus` on a freshly filled view lands on whichever item
    /// the widget last tracked — after 50,000 appends that is the *last* one,
    /// which leaves the user staring at the bottom of the folder. Scrolling
    /// with `FOCUS` makes the target explicit.
    pub fn focus_first(&self) {
        if self.selection.n_items() == 0 {
            self.widget().grab_focus();
            return;
        }
        match self.stack.visible_child_name().as_deref() {
            Some("list") => self.columns.scroll_to(0, None, gtk::ListScrollFlags::FOCUS, None),
            _ => self.grid.scroll_to(0, gtk::ListScrollFlags::FOCUS, None),
        }
    }

    // ── wiring ─────────────────────────────────────────────────────────────

    fn wire_up(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.grid.connect_activate(move |view, position| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(obj) = view.model().and_then(|m| m.item(position)).and_downcast::<FileObject>() {
                emit(&this.on_activate, obj);
            }
        });

        let weak = Rc::downgrade(self);
        self.columns.connect_activate(move |view, position| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(obj) = view.model().and_then(|m| m.item(position)).and_downcast::<FileObject>() {
                emit(&this.on_activate, obj);
            }
        });

        let weak = Rc::downgrade(self);
        self.selection.connect_selection_changed(move |_, _, _| {
            let Some(this) = weak.upgrade() else { return };
            emit_unit(&this.on_selection_changed);
        });

        // Keep the config in step when the user clicks a column header, so the
        // grid and the sort menu agree with the list.
        if let Some(sorter) = self.columns.sorter().and_downcast::<gtk::ColumnViewSorter>() {
            let weak = Rc::downgrade(self);
            sorter.connect_changed(move |sorter, _| {
                let Some(this) = weak.upgrade() else { return };
                let Some(column) = sorter.primary_sort_column() else { return };
                let key = if column == this.size_column {
                    SortKey::Size
                } else if column == this.modified_column {
                    SortKey::Modified
                } else if column == this.kind_column {
                    SortKey::Kind
                } else {
                    SortKey::Name
                };
                let mut cfg = this.config.borrow_mut();
                cfg.sort_key = key;
                cfg.sort_descending = sorter.primary_sort_order() == gtk::SortType::Descending;
            });
        }

        for widget in [self.grid.upcast_ref::<gtk::Widget>(), self.columns.upcast_ref::<gtk::Widget>()] {
            self.attach_context_menu(widget);
            self.attach_drag_source(widget);
            self.attach_drop_target(widget);
        }

        self.grid.set_enable_rubberband(true);
        self.columns.set_enable_rubberband(true);

        for widget in [self.grid.upcast_ref::<gtk::Widget>(), self.columns.upcast_ref::<gtk::Widget>()] {
            self.attach_empty_click(widget);
        }
    }

    /// Clicking blank space clears the selection, the way every file manager
    /// behaves and GTK's list widgets do not.
    fn attach_empty_click(self: &Rc<Self>, widget: &gtk::Widget) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_PRIMARY);
        // Bubble phase: a click that lands on a row is handled by the view
        // first, and only reaches here for the gaps between items.
        gesture.set_propagation_phase(gtk::PropagationPhase::Bubble);

        let pressed_at = Rc::new(Cell::new((0.0f64, 0.0f64)));
        let start = Rc::clone(&pressed_at);
        gesture.connect_pressed(move |_, _, x, y| start.set((x, y)));

        let weak = Rc::downgrade(self);
        let source = widget.clone();
        gesture.connect_released(move |_, _, x, y| {
            let Some(this) = weak.upgrade() else { return };

            // A rubber-band drag also ends in a release over empty space;
            // clearing then would throw away the selection it just made.
            let (px, py) = pressed_at.get();
            if (px - x).abs() > 4.0 || (py - y).abs() > 4.0 {
                return;
            }
            if this.item_at(&source, x, y).is_none() {
                this.selection.unselect_all();
            }
        });

        widget.add_controller(gesture);
    }

    /// The `FileObject` under the given coordinates of `view`, if any.
    fn item_at(&self, view: &gtk::Widget, x: f64, y: f64) -> Option<FileObject> {
        let picked = view.pick(x, y, gtk::PickFlags::DEFAULT)?;
        let mut widget = Some(picked);
        while let Some(current) = widget {
            if let Some(obj) = unsafe { current.data::<FileObject>("cadrocfile-item") } {
                return Some(unsafe { obj.as_ref() }.clone());
            }
            widget = current.parent();
        }
        None
    }

    fn attach_context_menu(self: &Rc<Self>, widget: &gtk::Widget) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        let weak = Rc::downgrade(self);
        let source = widget.clone();
        gesture.connect_pressed(move |gesture, _, x, y| {
            let Some(this) = weak.upgrade() else { return };
            // Claim the sequence so the view doesn't also treat this as a
            // selection change and clear what the user right-clicked.
            gesture.set_state(gtk::EventSequenceState::Claimed);

            // A GridView inside a ScrolledWindow is as tall as its *content*,
            // so `y` here can be tens of thousands of pixels. Translate into
            // the window, which is what the menu is parented to.
            let point = source
                .root()
                .map(|root| root.upcast::<gtk::Widget>())
                .and_then(|root| {
                    source.compute_point(&root, &gtk::graphene::Point::new(x as f32, y as f32))
                })
                .map(|p| (p.x() as f64, p.y() as f64))
                .unwrap_or((x, y));

            emit(&this.on_context_menu, point);
        });
        widget.add_controller(gesture);

        // Keys that act on the selection live here rather than as window
        // accelerators.
        //
        // GTK dispatches a window accelerator before the focused widget sees the
        // key, so binding bare `Delete` and `BackSpace` at the window meant they
        // never reached a text entry: correcting a search query fired "move to
        // Trash" on every keystroke, and with nothing selected the user got a
        // "Select something to delete" toast for each one. Scoped to the view,
        // they do the file-manager thing when the files have focus and edit text
        // when a text box does.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

            if key == gdk::Key::Menu || (key == gdk::Key::F10 && shift) {
                // Anchor to the top-left of the visible view for keyboard use.
                emit(&this.on_context_menu, (8.0, 8.0));
                return glib::Propagation::Stop;
            }

            let action = match key {
                gdk::Key::Delete | gdk::Key::KP_Delete if ctrl && shift => "win.shred",
                gdk::Key::Delete | gdk::Key::KP_Delete if shift => "win.delete-permanently",
                gdk::Key::Delete | gdk::Key::KP_Delete | gdk::Key::BackSpace => "win.trash",
                gdk::Key::F2 => "win.rename",
                _ => return glib::Propagation::Proceed,
            };

            // Actions live on the window; lookup walks up from here.
            let _ = WidgetExt::activate_action(&this.stack, action, None);
            glib::Propagation::Stop
        });
        widget.add_controller(keys);

        // Space previews. Capture phase, because the list views bind Space to
        // selecting the item under the cursor and would consume it before a
        // bubble-phase handler saw it. Nothing inside the view is editable —
        // rename is a dialog — so taking Space here cannot eat a typed space.
        let space = gtk::EventControllerKey::new();
        space.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        space.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            let modified = state.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::SHIFT_MASK
                    | gdk::ModifierType::ALT_MASK,
            );
            // Ctrl+Space keeps its GTK meaning of toggling the selection.
            if key != gdk::Key::space || modified {
                return glib::Propagation::Proceed;
            }
            let _ = WidgetExt::activate_action(&this.stack, "win.preview", None);
            glib::Propagation::Stop
        });
        widget.add_controller(space);
    }

    fn attach_drag_source(self: &Rc<Self>, widget: &gtk::Widget) {
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);

        let weak = Rc::downgrade(self);
        source.connect_prepare(move |_, _, _| {
            let this = weak.upgrade()?;
            let paths = this.selected_paths();
            if paths.is_empty() {
                return None;
            }
            let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
            Some(gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value()))
        });

        widget.add_controller(source);
    }

    fn attach_drop_target(self: &Rc<Self>, widget: &gtk::Widget) {
        let target = gtk::DropTarget::new(
            gdk::FileList::static_type(),
            gdk::DragAction::COPY | gdk::DragAction::MOVE,
        );

        let weak = Rc::downgrade(self);
        target.connect_drop(move |_, value, x, y| {
            let Some(this) = weak.upgrade() else { return false };
            let Ok(list) = value.get::<gdk::FileList>() else { return false };

            let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }

            // Dropping directly onto a folder row means "into that folder";
            // anywhere else means "into the folder being viewed".
            let onto = this.folder_at(x, y);
            emit(&this.on_drop, (paths, onto));
            true
        });

        widget.add_controller(target);
    }

    /// The directory under the given view coordinates, if the pointer is over
    /// a folder row.
    fn folder_at(&self, x: f64, y: f64) -> Option<PathBuf> {
        let view: &gtk::Widget = match self.stack.visible_child_name().as_deref() {
            Some("list") => self.columns.upcast_ref(),
            _ => self.grid.upcast_ref(),
        };
        let obj = self.item_at(view, x, y)?;
        obj.is_dir().then(|| obj.path())
    }
}

/// Width of one grid tile, and therefore of its caption.
///
/// Kept close to the icon so the slack lands *between* tiles as spacing rather
/// than inside them as padding, but wide enough that a caption has somewhere to
/// go: at the old `icon_size + 28` a 64px icon left about thirteen characters a
/// line, so almost every real filename overflowed and ran into its neighbour.
fn tile_width_for(icon_size: i32) -> i32 {
    (icon_size + 56).max(96)
}

/// Roughly how many characters fit on one caption line at `tile_width`.
///
/// Seven pixels is an average advance for the caption font. It only has to be
/// close: [`elide_name`] uses it to pick a length, and the label's own
/// ellipsis catches anything wider than the average.
fn caption_chars_for(tile_width: i32) -> i32 {
    (tile_width / 7).max(8)
}

/// Columns a character occupies in a monospaced-ish sense: 2 for East Asian
/// wide and fullwidth forms and for emoji, 1 for everything else.
///
/// The caption budget is a column count, not a character count. A name in
/// Japanese or Chinese is half as many characters for the same width, so
/// counting characters let those names run past the edge of their tile — which
/// is the exact overflow this truncation exists to prevent.
fn char_columns(c: char) -> usize {
    let c = c as u32;
    let wide = matches!(c,
        0x1100..=0x115F        // Hangul Jamo
        | 0x2E80..=0x303E      // CJK radicals, Kangxi, CJK symbols
        | 0x3041..=0x33FF      // kana, Bopomofo, CJK compatibility
        | 0x3400..=0x4DBF      // CJK extension A
        | 0x4E00..=0x9FFF      // CJK unified ideographs
        | 0xA000..=0xA4CF      // Yi
        | 0xAC00..=0xD7A3      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compatibility ideographs
        | 0xFE30..=0xFE6F      // CJK compatibility forms
        | 0xFF00..=0xFF60      // fullwidth forms
        | 0xFFE0..=0xFFE6      // fullwidth signs
        | 0x1F300..=0x1F64F    // emoji
        | 0x1F900..=0x1F9FF    // supplemental emoji
        | 0x20000..=0x3FFFD    // CJK extensions B and beyond
    );
    if wide { 2 } else { 1 }
}

fn display_columns(s: &str) -> usize {
    s.chars().map(char_columns).sum()
}

/// The longest prefix of `s` that fits in `columns`.
///
/// Trailing whitespace is dropped, because a cut that lands after a space would
/// otherwise render as `A Folder With A Fairly …` with a gap before the
/// ellipsis.
fn take_columns(s: &str, columns: usize) -> String {
    let mut used = 0;
    let taken: String = s
        .chars()
        .take_while(|&c| {
            used += char_columns(c);
            used <= columns
        })
        .collect();
    taken.trim_end().to_string()
}

/// The longest suffix of `s` that fits in `columns`.
fn take_columns_from_end(s: &str, columns: usize) -> String {
    let mut used = 0;
    let kept: String = s
        .chars()
        .rev()
        .take_while(|&c| {
            used += char_columns(c);
            used <= columns
        })
        .collect();
    kept.chars().rev().collect()
}

/// The trailing extension worth protecting, including a compound one.
///
/// The last dot alone is not enough: it turns `backup.tar.gz` into `.gz` and
/// throws away the part that says it is a tarball. So when the segment before
/// the final dot is itself short and wordlike — `tar`, `min`, `user` — it is
/// taken as part of the extension, which covers `.tar.gz`, `.tar.xz`,
/// `.tar.zst` and friends without a list of them to keep up to date.
///
/// Returns `None` for a dotfile (`.bashrc`, where the dot starts the name) and
/// for anything too long to be an extension.
fn extension_of(name: &str) -> Option<&str> {
    let last = name.rfind('.').filter(|&dot| dot > 0)?;

    let start = match name[..last].rfind('.') {
        Some(prev) if prev > 0 => {
            let middle = &name[prev + 1..last];
            let wordlike = (1..=4).contains(&middle.chars().count())
                && middle.chars().all(|c| c.is_ascii_alphanumeric());
            if wordlike { prev } else { last }
        }
        _ => last,
    };

    let extension = &name[start..];
    (2..=10).contains(&extension.chars().count()).then_some(extension)
}

/// Shortens `name` to about `budget` characters, keeping the extension.
///
/// Truncating a filename from the end is the worst thing to do to it: files in
/// one folder tend to share a prefix and differ at the tail, and the extension
/// — the thing that says what the file *is* — is the first casualty.
///
/// So the cut is taken out of the middle of the stem, keeping its last few
/// characters as well as the extension: `2026-08-30_backup_…l_v2.tar.gz`. Those
/// few characters are worth their width twice over. They are usually the
/// version or date that distinguishes one file from its neighbours, and they
/// keep the ellipsis from butting up against the extension's dot, which
/// otherwise renders as a run of four — `2026-08-30_backup_of_i….tar.gz`.
///
/// Done here rather than with Pango's `Middle` ellipsis because that mode
/// ellipsises the string and then wraps the result, which strands a fragment of
/// the tail alone on the second line.
fn elide_name(name: &str, budget: usize) -> String {
    if display_columns(name) <= budget {
        return name.to_string();
    }

    let Some(extension) = extension_of(name) else {
        // Nothing to protect at the end, so a plain trailing cut is both the
        // clearest and the one that keeps the most of what identifies the file.
        return format!("{}\u{2026}", take_columns(name, budget.saturating_sub(1)));
    };

    let ext_columns = display_columns(extension);
    // Keeping the extension has to still leave a useful amount of the stem,
    // otherwise the name would be almost entirely ellipsis and suffix.
    if budget <= ext_columns + 4 {
        return format!("{}\u{2026}", take_columns(name, budget.saturating_sub(1)));
    }

    let stem = &name[..name.len() - extension.len()];
    let available = budget - ext_columns - 1;
    // A tail worth showing, but never more than a third of what is left — the
    // front of the name is still what the eye reads first.
    let tail = take_columns_from_end(stem, STEM_TAIL.min(available / 3));
    let head = take_columns(stem, available - display_columns(&tail));

    format!("{head}\u{2026}{tail}{extension}")
}

/// Characters of the stem's tail kept before the extension.
const STEM_TAIL: usize = 4;

fn wrap_scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .child(child)
        .build()
}

/// Hidden-file and live-search filtering.
fn build_filter(config: Rc<RefCell<Config>>, search: Rc<RefCell<String>>) -> gtk::CustomFilter {
    gtk::CustomFilter::new(move |obj| {
        let Some(file) = obj.downcast_ref::<FileObject>() else { return true };

        if file.is_hidden() && !config.borrow().show_hidden {
            return false;
        }
        let query = search.borrow();
        if query.is_empty() {
            return true;
        }
        file.matches_search(query.as_str())
    })
}

/// A sorter for one column, with the directories-first grouping folded in.
fn column_sorter(key: SortKey, config: Rc<RefCell<Config>>) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| {
        let (Some(a), Some(b)) = (a.downcast_ref::<FileObject>(), b.downcast_ref::<FileObject>())
        else {
            return gtk::Ordering::Equal;
        };
        let dirs_first = config.borrow().dirs_first;
        // `descending` is false here: the ColumnView applies the direction
        // itself, and reversing twice would undo the dirs-first grouping.
        a.compare_to(b, key, false, dirs_first).into()
    })
}

fn build_column_view(
    selection: &gtk::MultiSelection,
    config: Rc<RefCell<Config>>,
    _dirs_first: Rc<Cell<bool>>,
) -> (
    gtk::ColumnView,
    gtk::ColumnViewColumn,
    gtk::ColumnViewColumn,
    gtk::ColumnViewColumn,
    gtk::ColumnViewColumn,
) {
    let view = gtk::ColumnView::builder()
        .model(selection)
        .show_column_separators(false)
        .show_row_separators(false)
        .reorderable(false)
        .css_classes(["file-list"])
        .build();

    let name = gtk::ColumnViewColumn::builder()
        .title(tr!("Name"))
        .expand(true)
        .resizable(true)
        .factory(&name_cell_factory(Rc::clone(&config)))
        .build();
    name.set_sorter(Some(&column_sorter(SortKey::Name, Rc::clone(&config))));

    let size = gtk::ColumnViewColumn::builder()
        .title(tr!("Size"))
        .resizable(true)
        .fixed_width(110)
        .factory(&text_cell_factory(|e| e.size_label(), gtk::Align::End))
        .build();
    size.set_sorter(Some(&column_sorter(SortKey::Size, Rc::clone(&config))));

    let modified = gtk::ColumnViewColumn::builder()
        .title(tr!("Modified"))
        .resizable(true)
        .fixed_width(170)
        .factory(&text_cell_factory(|e| e.modified_label(), gtk::Align::Start))
        .build();
    modified.set_sorter(Some(&column_sorter(SortKey::Modified, Rc::clone(&config))));

    let kind = gtk::ColumnViewColumn::builder()
        .title(tr!("Type"))
        .resizable(true)
        .fixed_width(160)
        .factory(&text_cell_factory(|e| e.kind_label(), gtk::Align::Start))
        .build();
    kind.set_sorter(Some(&column_sorter(SortKey::Kind, Rc::clone(&config))));

    for column in [&name, &size, &modified, &kind] {
        view.append_column(column);
    }

    (view, name, size, modified, kind)
}

fn build_grid_view(selection: &gtk::MultiSelection, config: Rc<RefCell<Config>>) -> gtk::GridView {
    gtk::GridView::builder()
        .model(selection)
        .factory(&grid_factory(config))
        .max_columns(24)
        .min_columns(1)
        .css_classes(["file-grid"])
        .build()
}

/// Widget data key holding a row's liveness flag for in-flight thumbnails.
const ALIVE_KEY: &str = "cadrocfile-thumb-alive";

/// Marks a row's pending thumbnail request as no longer wanted.
fn mark_row_dead(item: &gtk::ListItem) {
    let Some(child) = item.child() else { return };
    // SAFETY: the key is only ever set by `apply_icon` with this exact type.
    if let Some(flag) = unsafe { child.data::<Rc<Cell<bool>>>(ALIVE_KEY) } {
        unsafe { flag.as_ref() }.set(false);
    }
}

/// Attaches the bound `FileObject` to a widget so hit-testing can recover it
/// during a drop.
fn tag_widget(widget: &impl IsA<gtk::Widget>, obj: &FileObject) {
    unsafe { widget.as_ref().set_data("cadrocfile-item", obj.clone()) };
}

/// Makes `model` match `fresh`, reusing every object whose file is unchanged.
///
/// Returns whether a changed item was in `selected`, in which case the caller
/// has to restore the selection — rebinding a row drops it.
///
/// Walked backwards so removing an item never shifts one still to be visited,
/// and adjacent removals are gathered into one splice: a folder emptied by a
/// bulk delete would otherwise emit one signal, and one re-sort, per file.
fn merge_listing(
    model: &gio::ListStore,
    fresh: Vec<FileEntry>,
    selected: &HashSet<PathBuf>,
) -> bool {
    let mut fresh: HashMap<PathBuf, FileEntry> =
        fresh.into_iter().map(|entry| (entry.path.clone(), entry)).collect();
    let mut reselect = false;

    let mut index = model.n_items();
    let mut run_end: Option<u32> = None;
    while index > 0 {
        index -= 1;
        let Some(object) = model.item(index).and_downcast::<FileObject>() else { continue };
        match fresh.remove(&object.path()) {
            None => {
                run_end.get_or_insert(index + 1);
            }
            Some(entry) => {
                if let Some(end) = run_end.take() {
                    model.splice(index + 1, end - (index + 1), &[] as &[FileObject]);
                }
                if !object.shows(&entry) {
                    reselect |= selected.contains(&object.path());
                    object.replace(entry);
                    // Same object, new contents: this is what makes GTK rebind
                    // the one row, and only that row.
                    model.items_changed(index, 1, 1);
                }
            }
        }
    }
    if let Some(end) = run_end {
        model.splice(0, end, &[] as &[FileObject]);
    }

    // Whatever is left was not on screen before. One splice, one signal.
    if !fresh.is_empty() {
        let added: Vec<FileObject> = fresh.into_values().map(FileObject::new).collect();
        model.splice(model.n_items(), 0, &added);
    }
    reselect
}

fn grid_factory(config: Rc<RefCell<Config>>) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let icon_size = config.borrow().icon_size;

    // Tiles are a fixed width derived from the icon size, and the caption is
    // pinned to match so one long filename cannot widen every column.
    //
    // Kept deliberately tight: GridView stretches its columns to fill the row,
    // so a generous tile width does not produce generous tiles — it produces
    // fewer columns with the slack dumped between them, which reads as huge
    // horizontal padding.
    let tile_width = tile_width_for(icon_size);
    let caption_chars = caption_chars_for(tile_width);
    // Two lines' worth, less a little slack: word-boundary wrapping rarely
    // fills a line exactly, so budgeting the full two lines would let a name
    // spill onto a third and be cut by Pango anyway.
    let name_budget = (caption_chars as usize * 2).saturating_sub(4).max(8);

    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };

        let image = gtk::Image::builder()
            .pixel_size(icon_size)
            .halign(gtk::Align::Center)
            .build();

        let label = gtk::Label::builder()
            .justify(gtk::Justification::Center)
            .halign(gtk::Align::Center)
            // The name is already shortened to fit by `elide_name`; `End` here
            // is only a backstop for a name whose glyphs are wider than the
            // average this budget assumes. It is deliberately not `Middle`:
            // Pango ellipsises the whole string and *then* wraps it, so a
            // middle ellipsis on two lines left a fragment of the tail stranded
            // on the second line — "A Folder With A Fairly Lo… s Going".
            .ellipsize(pango::EllipsizeMode::End)
            .lines(2)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .width_chars(caption_chars)
            .max_width_chars(caption_chars)
            .css_classes(["caption"])
            .build();

        // Pango hyphenates when it breaks a word across lines, which is right
        // for prose and wrong for filenames: `backup-2026-08-30.tar.gz` came
        // out as `backup-2026-08-30.t-` / `ar.gz`, inventing a hyphen that is
        // not in the name.
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrInt::new_insert_hyphens(false));
        label.set_attributes(Some(&attrs));

        // `halign: Center` is what actually makes the caption truncate.
        //
        // GridView stretches its columns to fill the row, and a box that fills
        // its cell hands that whole width to the label inside it — so the
        // ellipsis only ever bit at the stretched width, and captions ran wider
        // than the icon they belong to and crowded the neighbouring tiles.
        // Centring the box at its requested width instead pins the label to
        // `tile_width`, so a long name is cut to the tile it sits in and the
        // slack stays *between* tiles where it reads as spacing.
        let boxed = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .width_request(tile_width)
            .halign(gtk::Align::Center)
            .css_classes(["file-tile"])
            .build();
        boxed.append(&image);
        boxed.append(&label);

        item.set_child(Some(&boxed));
    });

    let bind_config = Rc::clone(&config);
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
        let Some(obj) = item.item().and_downcast::<FileObject>() else { return };
        let Some(boxed) = item.child().and_downcast::<gtk::Box>() else { return };
        let Some(image) = boxed.first_child().and_downcast::<gtk::Image>() else { return };
        let Some(label) = boxed.last_child().and_downcast::<gtk::Label>() else { return };

        let entry = obj.entry();
        label.set_text(&elide_name(&entry.display_name, name_budget));
        // The full name is only ever partly visible in a tile, so the tooltip
        // carries the whole path rather than repeating the truncated name.
        boxed.set_tooltip_text(Some(&entry.path.to_string_lossy()));
        boxed.set_opacity(if entry.is_hidden { 0.6 } else { 1.0 });
        tag_widget(&boxed, &obj);

        apply_icon(&image, &entry, &bind_config, item, boxed.upcast_ref());
    });

    factory.connect_unbind(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            mark_row_dead(item);
        }
    });

    factory
}

fn name_cell_factory(config: Rc<RefCell<Config>>) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let icon_size = config.borrow().icon_size.min(LIST_ICON_MAX);

    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };

        let image = gtk::Image::builder().pixel_size(icon_size).build();
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .hexpand(true)
            .build();

        let boxed = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .build();
        boxed.append(&image);
        boxed.append(&label);
        item.set_child(Some(&boxed));
    });

    let bind_config = Rc::clone(&config);
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
        let Some(obj) = item.item().and_downcast::<FileObject>() else { return };
        let Some(boxed) = item.child().and_downcast::<gtk::Box>() else { return };
        let Some(image) = boxed.first_child().and_downcast::<gtk::Image>() else { return };
        let Some(label) = boxed.last_child().and_downcast::<gtk::Label>() else { return };

        let entry = obj.entry();
        label.set_text(&entry.display_name);
        boxed.set_opacity(if entry.is_hidden { 0.6 } else { 1.0 });
        tag_widget(&boxed, &obj);

        apply_icon(&image, &entry, &bind_config, item, boxed.upcast_ref());
    });

    factory.connect_unbind(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            mark_row_dead(item);
        }
    });

    factory
}

fn text_cell_factory(
    extract: fn(&FileEntry) -> String,
    align: gtk::Align,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
        let label = gtk::Label::builder()
            .halign(align)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        item.set_child(Some(&label));
    });

    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
        let Some(obj) = item.item().and_downcast::<FileObject>() else { return };
        let Some(label) = item.child().and_downcast::<gtk::Label>() else { return };
        label.set_text(&extract(&obj.entry()));
    });

    factory
}

/// Sets the themed icon immediately, then upgrades to a thumbnail if one is
/// warranted and can be decoded.
fn apply_icon(
    image: &gtk::Image,
    entry: &FileEntry,
    config: &Rc<RefCell<Config>>,
    _item: &gtk::ListItem,
    row: &gtk::Widget,
) {
    let (want_thumbs, max_bytes, size) = {
        let cfg = config.borrow();
        (cfg.show_thumbnails, cfg.thumbnail_max_bytes, image.pixel_size())
    };

    image.set_from_gicon(&thumbs::icon_for(&entry.content_type, entry.is_dir, entry.is_symlink));

    if !want_thumbs || entry.is_dir || !thumbs::worth_thumbnailing(&entry.content_type, entry.size, max_bytes) {
        return;
    }

    let mtime = entry.modified.unwrap_or(0);
    if let Some(texture) = thumbs::cached(&entry.path, mtime, size) {
        image.set_paintable(Some(&texture));
        return;
    }

    // A row is "alive" from bind until unbind. Comparing the list item's path
    // instead looks equivalent but isn't: GridView rebinds items several times
    // while it settles an initial layout, and a path check reports those as
    // stale, cancelling work that is still wanted.
    let alive = Rc::new(Cell::new(true));
    unsafe { row.set_data(ALIVE_KEY, Rc::clone(&alive)) };

    let path = entry.path.clone();
    let content_type = entry.content_type.clone();
    let image = image.clone();

    glib::spawn_future_local(async move {
        let still_current = {
            let alive = Rc::clone(&alive);
            move || alive.get()
        };

        let Some(texture) = thumbs::load(path, &content_type, mtime, size, still_current).await else {
            return;
        };
        if alive.get() {
            image.set_paintable(Some(&texture));
        }
    });
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn short_names_are_left_alone() {
        assert_eq!(elide_name("short.txt", 24), "short.txt");
        assert_eq!(elide_name("b.md", 24), "b.md");
        // Exactly at the budget is still a fit.
        assert_eq!(elide_name("123456789012", 12), "123456789012");
    }

    #[test]
    fn the_extension_survives_truncation() {
        let out = elide_name("2026-08-30_backup_of_important_documents.tar.gz", 24);
        assert!(out.ends_with(".tar.gz"), "extension must be kept, got {out:?}");
        assert!(out.contains('\u{2026}'), "must show an ellipsis, got {out:?}");
        assert!(out.chars().count() <= 24, "must fit the budget, got {out:?}");
        assert!(out.starts_with("2026-08-30"), "must keep the front, got {out:?}");
    }

    #[test]
    fn a_name_with_no_usable_extension_is_cut_at_the_end() {
        let out = elide_name("AnotherVeryLongNameWithNoSpacesAtAll", 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.ends_with('\u{2026}'));
        assert!(out.starts_with("AnotherVeryLongName"));
    }

    /// A dotfile's leading dot is not an extension, and an over-long "extension"
    /// (`archive.verylongsuffix`) is not worth protecting either.
    #[test]
    fn dotfiles_and_absurd_extensions_fall_back_to_a_plain_cut() {
        let out = elide_name(".bashrc-with-a-very-long-tail", 12);
        assert_eq!(out.chars().count(), 12);
        assert!(out.starts_with(".bashrc"));

        let out = elide_name("archive.thisisnotanextension", 14);
        assert_eq!(out.chars().count(), 14);
        assert!(out.ends_with('\u{2026}'));
    }

    /// A budget too small to hold the extension plus something of the stem must
    /// not degenerate into almost-all-ellipsis.
    #[test]
    fn a_tiny_budget_still_produces_something_readable() {
        let out = elide_name("photograph.jpeg", 8);
        assert_eq!(out.chars().count(), 8);
        assert!(out.starts_with("photogr"));
    }

    /// The ellipsis must never land straight before the extension's dot: on
    /// screen `name….tar.gz` reads as a run of four dots.
    #[test]
    fn the_ellipsis_is_never_followed_by_a_dot() {
        for name in [
            "2026-08-30_backup_of_important_project_documents_final_v2.tar.gz",
            "a-moderately-long-file-name.txt",
            "AnotherVeryLongNameWithNoSpacesAtAllWhichCannot.pdf",
            "IMG_20260830_113512_HDR_Pixel8Pro_wide_angle.jpg",
        ] {
            for budget in 10..40 {
                let out = elide_name(name, budget);
                assert!(
                    !out.contains("\u{2026}."),
                    "budget {budget}: ellipsis butts against a dot in {out:?}"
                );
                assert!(
                    display_columns(&out) <= budget,
                    "budget {budget}: overflowed with {out:?}"
                );
            }
        }
    }

    /// The characters just before the extension are usually the version or date
    /// that tells two neighbouring files apart, so they are kept.
    #[test]
    fn the_tail_of_the_stem_is_kept() {
        let out = elide_name("report_2026_final_v2.pdf", 18);
        assert!(out.ends_with("_v2.pdf"), "expected the version to survive, got {out:?}");
        assert!(out.starts_with("report"), "expected the front to survive, got {out:?}");
    }

    #[test]
    fn multibyte_names_are_cut_on_character_boundaries() {
        // Would panic on a byte slice.
        let out = elide_name("площадь-очень-длинное-имя-файла.txt", 20);
        assert!(display_columns(&out) <= 20);
        assert!(out.ends_with(".txt"));
        assert!(out.starts_with("площадь"));
    }

    /// A name in Japanese is half as many characters for the same width, so a
    /// character budget let it run past the edge of its tile.
    #[test]
    fn wide_glyphs_are_budgeted_by_the_space_they_take() {
        let name = "\u{87BA}\u{65CB}\u{72B6}\u{306E}\u{975E}\u{5E38}\u{306B}\u{9577}\u{3044}\u{30D5}\u{30A1}\u{30A4}\u{30EB}\u{540D}.txt";
        assert_eq!(display_columns(name), 14 * 2 + 4, "each kanji is two columns");

        let out = elide_name(name, 20);
        assert!(display_columns(&out) <= 20, "must fit its tile, got {out:?}");
        assert!(out.ends_with(".txt"));

        // Latin names of the same character count are not penalised.
        assert_eq!(elide_name("abcdefgh.txt", 20), "abcdefgh.txt");
    }

    #[test]
    fn a_cut_after_a_space_does_not_leave_a_gap_before_the_ellipsis() {
        let out = elide_name("A Folder With A Fairly Long Name Indeed", 24);
        assert!(!out.contains(" \u{2026}"), "gap before the ellipsis in {out:?}");
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn column_widths_match_the_glyphs() {
        assert_eq!(display_columns("abc"), 3);
        assert_eq!(display_columns("\u{4E00}\u{4E8C}"), 4, "kanji are wide");
        assert_eq!(display_columns("\u{D55C}\u{AE00}"), 4, "hangul is wide");
        assert_eq!(display_columns("\u{1F600}"), 2, "emoji are wide");
        assert_eq!(display_columns("\u{E9}\u{440}"), 2, "accented latin and cyrillic are not");
    }

    #[test]
    fn tiles_stay_close_to_their_icon_but_leave_room_for_text() {
        for icon in crate::config::ICON_SIZES {
            let w = tile_width_for(icon);
            assert!(w >= icon, "tile must at least hold its icon");
            assert!(caption_chars_for(w) >= 8, "caption must have usable width");
        }
    }
}

#[cfg(test)]
mod listing_tests {
    use super::*;
    use crate::fs::entry::QUERY_ATTRS;

    /// Lists a real folder the way the app does, so the entries carry real
    /// sizes and timestamps rather than hand-built values.
    fn scan(dir: &std::path::Path) -> Vec<FileEntry> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let info = gio::File::for_path(&path)
                    .query_info(QUERY_ATTRS, gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
                    .unwrap();
                FileEntry::from_info(dir, &info)
            })
            .collect()
    }

    fn objects(model: &gio::ListStore) -> Vec<FileObject> {
        (0..model.n_items()).filter_map(|i| model.item(i).and_downcast::<FileObject>()).collect()
    }

    fn find(model: &gio::ListStore, name: &str) -> Option<FileObject> {
        objects(model).into_iter().find(|o| o.path().file_name().unwrap() == name)
    }

    fn loaded(dir: &std::path::Path) -> gio::ListStore {
        let model = gio::ListStore::new::<FileObject>();
        let objects: Vec<FileObject> = scan(dir).into_iter().map(FileObject::new).collect();
        model.splice(0, 0, &objects);
        model
    }

    /// The property the whole change rests on. A row's widget, and the
    /// thumbnail loaded into it, live exactly as long as its object; replacing
    /// the object is what made every icon flicker on any change in the folder.
    #[test]
    fn unchanged_files_keep_their_objects_across_a_refresh() {
        let dir = crate::testing::TempDir::new("listing");
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        let model = loaded(dir.path());
        let before = objects(&model);

        // Something unrelated appears — the case that used to rebuild it all.
        std::fs::write(dir.join("new.txt"), "x").unwrap();
        merge_listing(&model, scan(dir.path()), &HashSet::new());

        assert_eq!(model.n_items(), 4);
        for original in &before {
            let name = original.path().file_name().unwrap().to_owned();
            let now = find(&model, name.to_str().unwrap()).expect("an unchanged file vanished");
            assert_eq!(&now, original, "{name:?} was replaced by a new object");
        }
        assert!(find(&model, "new.txt").is_some(), "the new file was not added");
    }

    #[test]
    fn a_refresh_that_finds_no_change_emits_nothing_at_all() {
        let dir = crate::testing::TempDir::new("listing-quiet");
        for name in ["a", "b", "c"] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        let model = loaded(dir.path());

        let signals = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = std::rc::Rc::clone(&signals);
        model.connect_items_changed(move |_, _, _, _| counter.set(counter.get() + 1));

        // A monitor event with nothing behind it — an atime update, say.
        merge_listing(&model, scan(dir.path()), &HashSet::new());
        assert_eq!(signals.get(), 0, "an idle refresh must not touch the view");
    }

    #[test]
    fn deleted_files_go_and_changed_files_update_in_place() {
        let dir = crate::testing::TempDir::new("listing-change");
        for name in ["keep", "gone-1", "gone-2", "edit"] {
            std::fs::write(dir.join(name), "1").unwrap();
        }
        let model = loaded(dir.path());
        let edit_before = find(&model, "edit").unwrap();

        std::fs::remove_file(dir.join("gone-1")).unwrap();
        std::fs::remove_file(dir.join("gone-2")).unwrap();
        std::fs::write(dir.join("edit"), "now a good deal longer").unwrap();
        merge_listing(&model, scan(dir.path()), &HashSet::new());

        assert_eq!(model.n_items(), 2);
        assert!(find(&model, "gone-1").is_none() && find(&model, "gone-2").is_none());
        let edit_after = find(&model, "edit").unwrap();
        assert_eq!(edit_after, edit_before, "a changed file should be updated, not replaced");
        assert_eq!(edit_after.entry().size, 22, "the new size was not applied");
    }

    /// Rebinding a changed row drops it from the selection, so the caller has
    /// to be told — but only when it actually matters.
    #[test]
    fn a_selected_file_that_changed_asks_for_its_selection_back() {
        let dir = crate::testing::TempDir::new("listing-select");
        std::fs::write(dir.join("picked"), "1").unwrap();
        std::fs::write(dir.join("other"), "1").unwrap();
        let model = loaded(dir.path());
        let picked: HashSet<PathBuf> = [dir.join("picked")].into_iter().collect();

        std::fs::write(dir.join("other"), "changed").unwrap();
        assert!(!merge_listing(&model, scan(dir.path()), &picked), "selection untouched");

        std::fs::write(dir.join("picked"), "changed too").unwrap();
        assert!(merge_listing(&model, scan(dir.path()), &picked), "selection must be restored");
    }
}
