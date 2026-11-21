//! Desktop GUI for `everyrust`.
//!
//! A search toolbar, a virtualized table with sortable columns, and a status bar.
//! Scans run in the background so the window stays responsive.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::emath::GuiRounding;
use egui::epaint::text::{HintingTarget, SmoothHinting};
use egui::{
    Align, Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2,
};

use everyrust::efu;
use everyrust::model::Entry;
use everyrust::scan;
use everyrust::search::{self, Query, SearchIndex, SearchOptions, SortKey};
use everyrust::timeutil;

#[path = "gui/live.rs"]
mod live;

const ROW_STRIDE: f32 = 19.0;
const ROW_ICON_SIZE: f32 = 16.0;
const HEADER_HEIGHT: f32 = 24.0;
const FONT_SIZE: f32 = 12.0;
const COLUMN_GRIP: f32 = 4.0;
const ACCENT: Color32 = Color32::WHITE;
const SEARCH_FIELD_ID: &str = "search_field";

fn main() -> eframe::Result<()> {
    // Elevation allows the scanner to read more protected folders.
    let arguments: Vec<String> = std::env::args().collect();
    let no_elevate = arguments.iter().any(|argument| argument == "--no-elevate");
    if !no_elevate && !is_elevated() {
        if relaunch_as_admin(&arguments[1..]) {
            return Ok(());
        }
    }

    let title = if is_elevated() {
        "EveryRust [Administrator]"
    } else {
        "EveryRust"
    };
    let options = eframe::NativeOptions {
        run_and_return: false,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1060.0, 700.0])
            .with_min_inner_size([720.0, 420.0])
            .with_icon(app_icon())
            .with_title(title),
        ..Default::default()
    };
    eframe::run_native(
        title,
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

fn app_icon() -> egui::IconData {
    let image = image::load_from_memory_with_format(
        include_bytes!("../../assets/logo.ico"),
        image::ImageFormat::Ico,
    )
    .expect("decode bundled EveryRust icon")
    .into_rgba8();
    egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    OpenFolder,
    OpenFileList,
    Export,
    Reindex,
    OpenSelected,
    CopySelectedPath,
    RestartAsAdmin,
}

struct App {
    index: Option<SearchIndex>,
    hits: Vec<usize>,
    result_order: Vec<usize>,
    path_order: Vec<usize>,
    deleted_files: HashSet<PathBuf>,
    watch: Option<live::Watch>,
    live_warning: Option<String>,
    order_sort: (SortKey, bool),
    date_labels: HashMap<u64, String>,
    query: String,
    last_query: String,
    options: SearchOptions,
    last_options: SearchOptions,
    sort: SortKey,
    descending: bool,
    column_widths: [f32; 4],
    selected: Option<usize>,
    search_time: Option<Duration>,
    scanning: bool,
    receiver: Option<Receiver<Result<IndexedScan, String>>>,
    status_error: Option<String>,
    unreadable: usize,
    elevated: bool,
    all_drives: bool,
    dark: bool,
    show_about: bool,
    focus_search: bool,
    scroll_to_selection: bool,
    pending: Option<Action>,
    last_scan_source: Option<PathBuf>,
    recent_searches: Vec<String>,
    bookmarks: Vec<String>,
    icons: IconCache,
}

struct IndexedScan {
    index: SearchIndex,
    unreadable: usize,
    order: Vec<usize>,
    sort: (SortKey, bool),
    path_order: Vec<usize>,
    watch: Option<live::Watch>,
    warning: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        load_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, true);

        let mut app = Self {
            index: None,
            hits: Vec::new(),
            result_order: Vec::new(),
            path_order: Vec::new(),
            deleted_files: HashSet::new(),
            watch: None,
            live_warning: None,
            order_sort: (SortKey::Size, true),
            date_labels: HashMap::new(),
            query: String::new(),
            last_query: String::new(),
            options: SearchOptions::default(),
            last_options: SearchOptions::default(),
            sort: SortKey::Size,
            descending: true,
            column_widths: [244.0, 280.0, 108.0, 150.0],
            selected: None,
            search_time: None,
            scanning: false,
            receiver: None,
            status_error: None,
            unreadable: 0,
            elevated: is_elevated(),
            all_drives: false,
            dark: true,
            show_about: false,
            focus_search: true,
            scroll_to_selection: false,
            pending: None,
            last_scan_source: None,
            recent_searches: Vec::new(),
            bookmarks: Vec::new(),
            icons: IconCache::new(),
        };

        let start = std::env::args_os()
            .skip(1)
            .map(PathBuf::from)
            .find(|path| path.is_dir());
        if let Some(root) = start {
            app.start_scan(root, &cc.egui_ctx);
        } else {
            app.start_scan_all_drives(&cc.egui_ctx);
        }
        app
    }

    fn start_scan(&mut self, root: PathBuf, ctx: &egui::Context) {
        let root = std::path::absolute(&root).unwrap_or(root);
        self.last_scan_source = Some(root.clone());
        self.all_drives = false;
        let roots = vec![root.clone()];
        self.start_indexing(ctx, roots, move || {
            scan::scan_with_stats(&root).map(|(entries, stats)| (entries, stats.unreadable))
        });
    }

    fn start_scan_all_drives(&mut self, ctx: &egui::Context) {
        self.all_drives = true;
        self.last_scan_source = None;
        let mut roots = scan::local_fixed_drives();
        let fixed_drives = !roots.is_empty();
        if !fixed_drives {
            roots.extend(std::env::var_os("USERPROFILE").map(PathBuf::from));
        }
        let home = roots.first().cloned();
        self.start_indexing(ctx, roots, move || {
            let (entries, stats) = if fixed_drives {
                scan::scan_local_drives()?
            } else {
                let home = home.ok_or_else(|| anyhow::anyhow!("cannot determine a folder to index"))?;
                scan::scan_with_stats(&home)?
            };
            Ok((entries, stats.unreadable))
        });
    }

    fn start_load_file_list(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.last_scan_source = Some(path.clone());
        self.all_drives = false;
        self.start_indexing(ctx, Vec::new(), move || {
            efu::read_file_list(&path).map(|entries| (entries, 0))
        });
    }

    fn start_indexing(
        &mut self,
        ctx: &egui::Context,
        roots: Vec<PathBuf>,
        load: impl FnOnce() -> anyhow::Result<(Vec<Entry>, usize)> + Send + 'static,
    ) {
        self.watch = None;
        self.deleted_files.clear();
        self.live_warning = None;
        self.scanning = true;
        self.status_error = None;
        self.unreadable = 0;
        self.selected = None;
        let sort = (self.sort, self.descending);
        let ctx = ctx.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (watch, warning) = if roots.is_empty() {
                (None, None)
            } else {
                live::Watch::start(&roots, ctx.clone())
            };
            let result = load()
                .map(|(entries, unreadable)| {
                    let index = SearchIndex::new(entries, true);
                    let mut order: Vec<usize> = (0..index.len()).collect();
                    search::sort_hits(&index, &mut order, sort.0, sort.1);
                    let path_order = if watch.is_some() {
                        let mut paths: Vec<usize> = (0..index.len()).collect();
                        search::sort_hits(&index, &mut paths, SortKey::Path, false);
                        paths
                    } else {
                        Vec::new()
                    };
                    IndexedScan {
                        index,
                        unreadable,
                        order,
                        sort,
                        path_order,
                        watch,
                        warning,
                    }
                })
                .map_err(|error| format!("{error:#}"));
            if sender.send(result).is_ok() {
                ctx.request_repaint();
            }
        });
        self.receiver = Some(receiver);
    }

    fn poll_scan(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(scan)) => {
                self.hits.clear();
                self.selected = None;
                self.date_labels.clear();
                self.index = Some(scan.index);
                self.result_order = scan.order;
                self.path_order = scan.path_order;
                self.watch = scan.watch;
                self.live_warning = scan.warning;
                self.order_sort = scan.sort;
                self.unreadable = scan.unreadable;
                self.receiver = None;
                self.scanning = false;
                self.recompute();
            }
            Ok(Err(error)) => {
                self.receiver = None;
                self.scanning = false;
                self.status_error = Some(error);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.receiver = None;
                self.scanning = false;
            }
        }
    }

    fn poll_live(&mut self) {
        let Some(watch) = &self.watch else {
            return;
        };
        let mut changes = HashMap::new();
        while let Ok(update) = watch.poll() {
            match update {
                live::Update::Paths(paths) => changes.extend(paths),
                live::Update::Warning(warning) => self.live_warning = Some(warning),
                live::Update::Rescan => {
                    self.pending = Some(Action::Reindex);
                    return;
                }
            }
        }
        // The watcher may have captured an update before a file was recycled.
        changes.retain(|path, entry| {
            if !self.deleted_files.contains(path) {
                return true;
            }
            if entry.is_some() {
                let Ok(current) = scan::read_entry(path.clone()) else {
                    return false;
                };
                *entry = Some(current);
            }
            self.deleted_files.remove(path);
            true
        });
        self.apply_index_changes(changes);
    }

    fn apply_index_changes(&mut self, changes: HashMap<PathBuf, Option<Entry>>) {
        if changes.is_empty() {
            return;
        }
        let selected_path = self.selected_entry().map(|entry| entry.path.clone());
        let Some(index) = self.index.as_mut() else {
            return;
        };
        if !live::apply(
            index,
            &mut self.path_order,
            &mut self.result_order,
            self.order_sort,
            changes,
        ) {
            return;
        }
        self.selected = None;
        self.hits.clear();
        self.recompute();
        if let (Some(path), Some(index)) = (selected_path, &self.index) {
            self.selected = live::find(index, &self.path_order, &path)
                .and_then(|id| self.hits.iter().position(|&hit| hit == id));
        }
    }

    fn recompute(&mut self) {
        let Some(index) = &self.index else {
            self.hits.clear();
            return;
        };
        let started = Instant::now();
        let trimmed = self.query.trim();
        let query = if trimmed.is_empty() {
            None
        } else {
            match Query::parse(trimmed, self.options) {
                Ok(query) => Some(query),
                Err(error) => {
                    self.status_error = Some(format!("{error:#}"));
                    return;
                }
            }
        };
        let sort = (self.sort, self.descending);
        if self.order_sort != sort {
            // Rebuild from index order to keep ties stable as sort keys change.
            self.result_order.clear();
            self.result_order.extend(0..index.len());
            search::sort_hits(index, &mut self.result_order, sort.0, sort.1);
            self.order_sort = sort;
        }
        let selected_entry = self.selected.and_then(|row| self.hits.get(row)).copied();
        if let Some(query) = &query {
            self.hits = search::filter_ordered_indices(index, query, &self.result_order);
        } else {
            self.hits.clone_from(&self.result_order);
        }
        self.status_error = None;
        self.search_time = Some(started.elapsed());
        self.selected = selected_entry
            .and_then(|entry| self.hits.iter().position(|&candidate| candidate == entry));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.hits.len() as isize - 1) as usize;
        self.selected = Some(next);
        self.scroll_to_selection = true;
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let index = self.index.as_ref()?;
        let position = self.selected?;
        let entry_index = *self.hits.get(position)?;
        Some(index.entry(entry_index))
    }

    fn record_search(&mut self) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.recent_searches.retain(|existing| existing != &query);
        self.recent_searches.insert(0, query);
        self.recent_searches.truncate(10);
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (
            down,
            up,
            page_down,
            page_up,
            enter,
            ctrl_f,
            f5,
            escape,
            copy,
            open_folder,
        ) = ctx.input(|input| {
            let ctrl = input.modifiers.command || input.modifiers.ctrl;
            (
                input.key_pressed(Key::ArrowDown),
                input.key_pressed(Key::ArrowUp),
                input.key_pressed(Key::PageDown),
                input.key_pressed(Key::PageUp),
                input.key_pressed(Key::Enter),
                ctrl && input.key_pressed(Key::F),
                input.key_pressed(Key::F5),
                input.key_pressed(Key::Escape),
                ctrl && input.key_pressed(Key::C),
                ctrl && input.key_pressed(Key::O),
            )
        });

        if ctrl_f {
            self.focus_search = true;
        }
        if open_folder {
            self.pending = Some(Action::OpenFolder);
        }
        if f5 {
            self.pending = Some(Action::Reindex);
        }
        if escape {
            self.query.clear();
            self.focus_search = true;
        }
        if down {
            self.move_selection(1);
        }
        if up {
            self.move_selection(-1);
        }
        if page_down {
            self.move_selection(15);
        }
        if page_up {
            self.move_selection(-15);
        }
        if (down || up || page_down || page_up) && self.selected.is_some() {
            ctx.memory_mut(|memory| memory.surrender_focus(egui::Id::unique(SEARCH_FIELD_ID)));
        }
        if enter {
            self.record_search();
            self.pending = Some(Action::OpenSelected);
        }
        if copy {
            self.pending = Some(Action::CopySelectedPath);
        }
        if self.pending.is_none()
            && !self.scanning
            && !self.show_about
            && !self.focus_search
            && !ctx.memory(|memory| memory.has_focus(egui::Id::unique(SEARCH_FIELD_ID)))
            && ctx.input(|input| {
                input.events.iter().any(|event| matches!(
                    event,
                    egui::Event::Key {
                        key: Key::Delete,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                        ..
                    }
                ))
            })
        {
            self.delete_selected_file();
        }
    }

    fn delete_selected_file(&mut self) {
        let Some(entry) = self.selected_entry().filter(|entry| !entry.is_dir) else {
            return;
        };
        let path = entry.path.clone();
        match recycle_file(&path) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                self.status_error = Some(format!("Could not delete {}: {error}", path.display()));
                return;
            }
        }
        // Imported lists build the path order on demand.
        if let Some(index) = &self.index {
            if self.path_order.is_empty() {
                self.path_order.extend(0..index.len());
                search::sort_hits(index, &mut self.path_order, SortKey::Path, false);
            }
        }
        if self.watch.is_some() {
            self.deleted_files.insert(path.clone());
        }
        self.apply_index_changes(HashMap::from([(path, None)]));
    }

    fn actions_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        for (label, action) in [
            ("Open Folder…", Action::OpenFolder),
            ("Open File List…", Action::OpenFileList),
            ("Export Results…", Action::Export),
            ("Reindex (F5)", Action::Reindex),
        ] {
            if ui.button(label).clicked() {
                self.pending = Some(action);
                ui.close();
            }
        }

        ui.separator();
        ui.menu_button("History", |ui| {
            if self.recent_searches.is_empty() {
                ui.weak("No search history");
            }
            for query in &self.recent_searches {
                if ui.button(query).clicked() {
                    self.query.clone_from(query);
                    self.focus_search = true;
                    ui.close();
                }
            }
        });
        ui.menu_button("Bookmarks", |ui| {
            let query = self.query.trim();
            if ui
                .add_enabled(!query.is_empty(), egui::Button::new("Add Current Search"))
                .clicked()
            {
                if !self.bookmarks.iter().any(|bookmark| bookmark == query) {
                    self.bookmarks.push(query.to_owned());
                }
                ui.close();
            }
            for bookmark in &self.bookmarks {
                if ui.button(bookmark).clicked() {
                    self.query.clone_from(bookmark);
                    self.focus_search = true;
                    ui.close();
                }
            }
        });

        ui.separator();
        if ui.checkbox(&mut self.dark, "Dark Theme").changed() {
            apply_theme(ctx, self.dark);
        }
        if !self.elevated && ui.button("Restart as Administrator…").clicked() {
            self.pending = Some(Action::RestartAsAdmin);
            ui.close();
        }
        if ui.button("About").clicked() {
            self.show_about = true;
            ui.close();
        }
    }

    fn apply_pending(&mut self, ctx: &egui::Context) {
        let Some(action) = self.pending.take() else {
            return;
        };
        match action {
            Action::OpenFolder => {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.start_scan(folder, ctx);
                }
            }
            Action::OpenFileList => {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("File lists (EFU, CSV, TXT)", &["efu", "csv", "txt"])
                    .pick_file()
                {
                    self.start_load_file_list(file, ctx);
                }
            }
            Action::Export => {
                if let Some(file) = rfd::FileDialog::new()
                    .set_file_name("everyrust-results.txt")
                    .add_filter("Text", &["txt"])
                    .save_file()
                {
                    if let Some(index) = &self.index {
                        let mut text = String::new();
                        for hit in &self.hits {
                            text.push_str(&index.entry(*hit).path.display().to_string());
                            text.push('\n');
                        }
                        let _ = std::fs::write(&file, text);
                    }
                }
            }
            Action::Reindex => {
                if self.all_drives {
                    self.start_scan_all_drives(ctx);
                } else if let Some(source) = self.last_scan_source.clone() {
                    if source.is_dir() {
                        self.start_scan(source, ctx);
                    } else {
                        self.start_load_file_list(source, ctx);
                    }
                }
            }
            Action::OpenSelected => {
                if let Some(entry) = self.selected_entry() {
                    open_path(&entry.path);
                }
            }
            Action::CopySelectedPath => {
                if let Some(entry) = self.selected_entry() {
                    ctx.copy_text(entry.path.display().to_string());
                }
            }
            Action::RestartAsAdmin => {
                let arguments: Vec<String> = std::env::args().skip(1).collect();
                relaunch_as_admin(&arguments);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        if ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        self.poll_scan();
        if self.pending.is_none() {
            self.poll_live();
        }
        self.handle_keys(&ctx);

        if self.query != self.last_query || self.options != self.last_options {
            self.last_query = self.query.clone();
            self.last_options = self.options;
            self.recompute();
        }

        let palette = Palette::new(self.dark);
        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(palette.header_bg)
                    .inner_margin(egui::Margin::symmetric(5, 4)),
            )
            .show(root_ui, |ui| {
                let response = ui.add_sized(
                    [ui.available_width(), 24.0],
                    egui::TextEdit::singleline(&mut self.query)
                        .id(egui::Id::unique(SEARCH_FIELD_ID))
                        .desired_width(f32::INFINITY),
                );
                if self.focus_search {
                    response.request_focus();
                    self.focus_search = false;
                }
                response.context_menu(|ui| {
                    ui.menu_button("Filters", |ui| {
                        ui.checkbox(&mut self.options.case_sensitive, "Match Case");
                        ui.checkbox(&mut self.options.whole_word, "Match Whole Word");
                        ui.checkbox(&mut self.options.match_path, "Match Path");
                        ui.checkbox(&mut self.options.regex, "Regular Expression");
                    });
                    if ui.button("Clear Search").clicked() {
                        self.query.clear();
                        self.focus_search = true;
                        ui.close();
                    }
                    ui.separator();
                    self.actions_menu(ui, &ctx);
                });
            });

        egui::Panel::bottom("statusbar")
            .frame(
                egui::Frame::new()
                    .fill(palette.header_bg)
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    let count = self.hits.len();
                    ui.label(
                        RichText::new(format!(
                            "{} item{}",
                            format_count(count as u64),
                            if count == 1 { "" } else { "s" }
                        ))
                        .size(12.0),
                    );
                    if self.unreadable > 0 {
                        ui.label(
                            RichText::new(format!(
                                "· {} inaccessible",
                                format_count(self.unreadable as u64),
                            ))
                            .size(12.0)
                            .weak(),
                        );
                    }
                    if let Some(error) = &self.status_error {
                        ui.label(RichText::new(error).size(12.0).color(Color32::from_rgb(190, 40, 40)));
                    }
                    if let Some(warning) = &self.live_warning {
                        ui.weak(warning);
                    }
                    if self.scanning {
                        ui.spinner();
                        ui.weak("Scanning…");
                    }
                    if let Some(entry) = self.selected_entry() {
                        if let Some(bytes) = entry.size.filter(|_| !entry.is_dir) {
                            ui.separator();
                            ui.label(format!("Size: {}", status_size(bytes)));
                        }
                        if let Some(date) = entry.modified {
                            let date = local_datetime(date);
                            if !date.is_empty() {
                                ui.separator();
                                ui.label(format!("Date Modified: {date}"));
                            }
                        }
                        ui.separator();
                        ui.add(
                            egui::Label::new(format!("Path: {}", status_folder(entry))).truncate(),
                        );
                    }
                });
            });

        let selected = self.selected;
        let sort = self.sort;
        let descending = self.descending;
        let scroll_to_selection = self.scroll_to_selection;
        self.scroll_to_selection = false;
        let event = egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(palette.bg))
            .show(root_ui, |ui| match &self.index {
                None => {
                    ui.centered_and_justified(|ui| {
                        let text = if self.scanning {
                            "Scanning the folder…"
                        } else {
                            "Right-click the search field to open a folder or file list."
                        };
                        ui.label(RichText::new(text).size(14.0).weak());
                    });
                    TableEvent::default()
                }
                Some(index) => {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let width = ui
                        .available_width()
                        .max(self.column_widths.iter().sum());
                    egui::ScrollArea::horizontal()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_min_width(width);
                            draw_table(
                                ui,
                                index,
                                &self.hits,
                                selected,
                                sort,
                                descending,
                                &palette,
                                width,
                                &mut self.column_widths,
                                scroll_to_selection,
                                &self.icons,
                                &mut self.date_labels,
                            )
                        })
                        .inner
                }
            })
            .inner;

        if let Some(clicked) = event.sort_click {
            if clicked == self.sort {
                self.descending = !self.descending;
            } else {
                self.sort = clicked;
                self.descending = clicked == SortKey::Size;
            }
            self.recompute();
        }
        if let Some(row) = event.select {
            self.selected = Some(row);
            ctx.memory_mut(|memory| memory.surrender_focus(egui::Id::unique(SEARCH_FIELD_ID)));
        } else if event.clear_selection {
            self.selected = None;
        }
        if let Some(row) = event.open {
            self.selected = Some(row);
            self.record_search();
            if let Some(entry) = self.selected_entry() {
                open_path(&entry.path);
            }
        }
        if let Some(row) = event.open_containing {
            self.selected = Some(row);
            if let Some(entry) = self.selected_entry() {
                if entry.is_dir {
                    let _ = std::process::Command::new("explorer").arg(&entry.path).spawn();
                } else {
                    reveal_path(&entry.path);
                }
            }
        }
        if let Some(row) = event.copy_path {
            self.selected = Some(row);
            if let Some(entry) = self.selected_entry() {
                ctx.copy_text(entry.path.display().to_string());
            }
        }

        if self.show_about {
            let last_search = self.search_time;
            egui::Window::new("About")
                .collapsible(false)
                .resizable(false)
                .open(&mut self.show_about)
                .show(&ctx, |ui| {
                    ui.heading("EveryRust");
                    ui.label("Fast local file search.");
                    ui.label(format!("everyrust {}", env!("CARGO_PKG_VERSION")));
                    if let Some(took) = last_search {
                        ui.label(format!(
                            "Last search took {:.2} ms",
                            took.as_secs_f64() * 1000.0
                        ));
                    }
                });
        }

        self.apply_pending(&ctx);

        if self.scanning {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.watch = None;
        self.receiver = None;
    }
}

#[derive(Default)]
struct TableEvent {
    select: Option<usize>,
    clear_selection: bool,
    open: Option<usize>,
    open_containing: Option<usize>,
    copy_path: Option<usize>,
    sort_click: Option<SortKey>,
}

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    text: Color32,
    header_bg: Color32,
    header_text: Color32,
    active_header_bg: Color32,
    border: Color32,
    hover: Color32,
    sorted_column_bg: Color32,
    selected_bg: Color32,
    selected_text: Color32,
    selected_outline: Color32,
}

impl Palette {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                bg: Color32::BLACK,
                text: Color32::from_rgb(228, 233, 239),
                header_bg: Color32::from_gray(23),
                header_text: Color32::from_rgb(197, 206, 217),
                active_header_bg: Color32::from_gray(40),
                border: Color32::from_gray(49),
                hover: Color32::from_gray(34),
                sorted_column_bg: Color32::from_gray(15),
                selected_bg: Color32::from_gray(99),
                selected_text: Color32::WHITE,
                selected_outline: Color32::from_gray(235),
            }
        } else {
            Self {
                bg: Color32::WHITE,
                text: Color32::from_rgb(30, 30, 30),
                header_bg: Color32::from_rgb(243, 243, 243),
                header_text: Color32::from_rgb(30, 30, 30),
                active_header_bg: Color32::from_gray(66),
                border: Color32::from_rgb(214, 214, 214),
                hover: Color32::from_gray(232),
                sorted_column_bg: Color32::from_gray(245),
                selected_bg: Color32::from_gray(224),
                selected_text: Color32::from_gray(22),
                selected_outline: Color32::from_gray(68),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_table(
    ui: &mut egui::Ui,
    index: &SearchIndex,
    hits: &[usize],
    selected: Option<usize>,
    sort: SortKey,
    descending: bool,
    palette: &Palette,
    width: f32,
    column_widths: &mut [f32; 4],
    scroll_to_selection: bool,
    icons: &IconCache,
    date_labels: &mut HashMap<u64, String>,
) -> TableEvent {
    let mut event = TableEvent::default();
    let widths = *column_widths;
    let table_width: f32 = widths.iter().sum();

    let (header_rect, _) = ui.allocate_exact_size(Vec2::new(width, HEADER_HEIGHT), Sense::hover());
    let blank_header = Rect::from_min_max(
        header_rect.left_top() + Vec2::new(table_width, 0.0),
        header_rect.right_bottom(),
    );
    if blank_header.is_positive() {
        event.clear_selection = ui
            .interact(
                blank_header,
                ui.make_persistent_id("blank-header"),
                Sense::click(),
            )
            .clicked();
    }
    let painter = ui.painter();
    painter.rect_filled(header_rect, 0.0, palette.header_bg);
    painter.line_segment(
        [header_rect.left_bottom(), header_rect.right_bottom()],
        Stroke::new(1.0, palette.border),
    );

    let mut x = header_rect.left();
    for (column, ((key, label, min_width), column_width)) in [
        (SortKey::Name, "Name", 84.0),
        (SortKey::Path, "Path", 80.0),
        (SortKey::Size, "Size", 76.0),
        (SortKey::Modified, "Date Modified", 120.0),
    ]
    .into_iter()
    .zip(widths)
    .enumerate()
    {
        let cell = Rect::from_min_size(
            Pos2::new(x, header_rect.top()),
            Vec2::new(column_width, header_rect.height()),
        );
        let mut button_rect = cell;
        button_rect.max.x -= COLUMN_GRIP;
        if column > 0 {
            button_rect.min.x += COLUMN_GRIP;
        }
        let response = ui.interact(
            button_rect,
            ui.make_persistent_id(("column-header", column)),
            Sense::click(),
        );
        let grip = Rect::from_center_size(
            cell.right_center(),
            Vec2::new(COLUMN_GRIP * 2.0, HEADER_HEIGHT),
        );
        let resize = ui
            .interact(
                grip,
                ui.make_persistent_id(("column-grip", column)),
                Sense::drag(),
            )
            .on_hover_and_drag_cursor(egui::CursorIcon::ResizeColumn);
        if resize.hovered() || resize.dragged() {
            ui.ctx().set_cursor_image(Some(column_resize_cursor().clone()));
        }
        if resize.dragged() {
            column_widths[column] = (column_width + resize.drag_delta().x).max(min_width);
            ui.ctx().request_repaint();
        }
        let active = sort == key;
        if active {
            ui.painter().rect_filled(cell, 0.0, palette.active_header_bg);
        } else if response.hovered() {
            ui.painter().rect_filled(cell, 0.0, palette.hover);
        }
        if active {
            let column_body = Rect::from_min_max(
                cell.left_bottom(),
                Pos2::new(cell.right(), ui.max_rect().bottom()),
            );
            ui.painter().rect_filled(column_body, 0.0, palette.sorted_column_bg);
            let arrow_center = Pos2::new(cell.center().x, cell.top() + 5.0);
            let direction = if descending { 1.0 } else { -1.0 };
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    arrow_center + Vec2::new(-3.0, -2.0 * direction),
                    arrow_center + Vec2::new(3.0, -2.0 * direction),
                    arrow_center + Vec2::new(0.0, 2.0 * direction),
                ],
                ACCENT,
                Stroke::NONE,
            ));
        }
        let divider = if resize.hovered() || resize.dragged() {
            ACCENT
        } else {
            palette.border
        };
        ui.painter().line_segment(
            [cell.right_top(), cell.right_bottom()],
            Stroke::new(1.0, divider),
        );
        let (position, align) = if key == SortKey::Size {
            (cell.right_center() - Vec2::new(6.0, 0.0), Align2::RIGHT_CENTER)
        } else {
            (cell.left_center() + Vec2::new(6.0, 0.0), Align2::LEFT_CENTER)
        };
        let label_offset = if active { Vec2::new(0.0, 2.0) } else { Vec2::ZERO };
        ui.painter().text(
            position + label_offset,
            align,
            label,
            FontId::proportional(FONT_SIZE),
            if active { ACCENT } else { palette.header_text },
        );
        if response.clicked() {
            event.sort_click = Some(key);
        }
        x += column_width;
    }

    if hits.is_empty() {
        ui.centered_and_justified(|ui| ui.weak("No results"));
        return event;
    }

    // Reuse formatted local timestamps across frames without retaining a
    // label for every entry in the index.
    if date_labels.len() >= 4096 {
        date_labels.clear();
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let start = ui.max_rect().min;
            let width = ui.available_width();
            let rows_height = hits.len() as f32 * ROW_STRIDE;
            ui.set_height(rows_height);
            let visible = viewport.translate(start.to_vec2()).intersect(ui.clip_rect());
            let blank_areas = [
                (
                    "below-results",
                    Rect::from_min_max(
                        Pos2::new(visible.left(), start.y + rows_height),
                        visible.max,
                    ),
                ),
                (
                    "beside-results",
                    Rect::from_min_max(
                        Pos2::new(start.x + table_width, visible.top()),
                        Pos2::new(visible.right(), start.y + rows_height),
                    ),
                ),
            ];
            for (id, area) in blank_areas {
                let area = area.intersect(visible);
                if area.is_positive() {
                    event.clear_selection |= ui
                        .interact(area, ui.make_persistent_id(id), Sense::click())
                        .clicked();
                }
            }
            let range = visible_rows(viewport.y_range(), hits.len());
            if let Some(row) = selected.filter(|_| scroll_to_selection) {
                let row_rect = Rect::from_min_size(
                    start + Vec2::new(0.0, row as f32 * ROW_STRIDE),
                    Vec2::new(width, ROW_STRIDE),
                );
                ui.scroll_to_rect(row_rect, Some(Align::Center));
            }
            let visible_rect = Rect::from_min_max(
                start + Vec2::new(0.0, range.start as f32 * ROW_STRIDE),
                start + Vec2::new(width, range.end as f32 * ROW_STRIDE),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(visible_rect), |ui| {
                ui.skip_ahead_auto_ids(range.start);
                for row in range {
                    let entry = index.entry(hits[row]);
                    let (rect, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), ROW_STRIDE),
                        Sense::hover(),
                    );
                    let content_rect = Rect::from_min_size(
                        rect.min,
                        Vec2::new(table_width, rect.height()),
                    );
                    let response = ui.interact(
                        content_rect,
                        ui.make_persistent_id(("file", &entry.path)),
                        Sense::click(),
                    );
                    let is_selected = selected == Some(row);
                    let hovered = response.hovered() && !is_selected;
                    let background = if is_selected {
                        palette.selected_bg
                    } else if hovered {
                        palette.hover
                    } else {
                        Color32::TRANSPARENT
                    };
                    if background != Color32::TRANSPARENT {
                        ui.painter().rect_filled(content_rect, 0.0, background);
                    }
                    if is_selected {
                        paint_selection_outline(ui, content_rect, palette.selected_outline);
                    }
                    let color = if is_selected {
                        palette.selected_text
                    } else {
                        palette.text
                    };

                    let icon_rect = Rect::from_min_size(
                        Pos2::new(
                            rect.left() + 3.0,
                            rect.center().y - ROW_ICON_SIZE / 2.0,
                        ),
                        Vec2::splat(ROW_ICON_SIZE),
                    );
                    let icon_rect = icon_rect.round_to_pixels(ui.pixels_per_point());
                    let opacity = if is_selected { 135 } else { 255 };
                    if let Some(texture) = icons.texture_id(ui.ctx(), entry) {
                        ui.painter().image(
                            texture,
                            icon_rect,
                            egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                            Color32::from_white_alpha(opacity),
                        );
                    } else {
                        paint_icon(ui, icon_rect, entry.is_dir, opacity);
                    }

                    let mut x = rect.left();
                    let values = [
                        entry.name_string(),
                        containing_folder(entry).display().to_string(),
                        human_size(entry.size, entry.is_dir),
                        entry
                            .modified
                            .map(|time| {
                                date_labels.entry(time).or_insert_with(|| local_datetime(time)).clone()
                            })
                            .unwrap_or_default(),
                    ];
                    for (column, (value, column_width)) in values.into_iter().zip(widths).enumerate() {
                        if !value.is_empty() {
                            let inset = if column == 0 {
                                ROW_ICON_SIZE + 6.0
                            } else {
                                6.0
                            };
                            let mut job = egui::text::LayoutJob::simple_singleline(
                                value,
                                FontId::proportional(FONT_SIZE),
                                color,
                            );
                            job.wrap = egui::text::TextWrapping::truncate_at_width(
                                column_width - inset - 6.0,
                            );
                            let galley = ui.painter().layout_job(job);
                            let text_x = if column == 2 {
                                x + column_width - 6.0 - galley.size().x
                            } else {
                                x + inset
                            };
                            let text_y = rect.center().y - galley.size().y / 2.0;
                            let origin = Pos2::new(text_x, text_y).round_to_pixels(ui.pixels_per_point());
                            ui.painter().galley(origin, galley, color);
                        }
                        x += column_width;
                    }

                    if response.clicked() {
                        event.select = Some(row);
                    }
                    if response.double_clicked() {
                        event.open = Some(row);
                    }
                    egui::Popup::context_menu(&response)
                        .width(146.0)
                        .frame(
                            egui::Frame::new()
                                .fill(palette.header_bg)
                                .stroke(Stroke::new(1.0, palette.border))
                                .inner_margin(egui::Margin::symmetric(2, 3)),
                        )
                        .show(|ui| {
                            ui.set_width(142.0);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for (label, action) in [
                                ("Open Folder", &mut event.open_containing),
                                ("Copy Path", &mut event.copy_path),
                            ] {
                                let button = egui::Button::new(label)
                                    .frame_when_inactive(false)
                                    .min_size(Vec2::new(ui.available_width(), 23.0));
                                if ui.add(button).clicked() {
                                    *action = Some(row);
                                    ui.close();
                                }
                            }
                        });
                }
            });
        });

    event
}

fn visible_rows(viewport: egui::emath::Rangef, total: usize) -> std::ops::Range<usize> {
    let stride = f64::from(ROW_STRIDE);
    let first = ((f64::from(viewport.min) / stride).floor() as usize).min(total);
    if viewport.max <= viewport.min {
        return first..first;
    }
    let end = ((f64::from(viewport.max) / stride).ceil() as usize).min(total);
    first..end
}

fn column_resize_cursor() -> &'static egui::CustomCursorImage {
    static CURSOR: OnceLock<egui::CustomCursorImage> = OnceLock::new();
    CURSOR.get_or_init(|| {
        const INK: [&str; 13] = [
            "........#........",
            "........#........",
            "........#........",
            "....#...#...#....",
            "...##...#...##...",
            "..###...#...###..",
            ".######.#.######.",
            "..###...#...###..",
            "...##...#...##...",
            "....#...#...#....",
            "........#........",
            "........#........",
            "........#........",
        ];
        let width = INK[0].len() + 2;
        let height = INK.len() + 2;
        let mut rgba = vec![0; width * height * 4];
        let mut ink_pixels = Vec::new();
        for (y, row) in INK.iter().enumerate() {
            for (x, byte) in row.bytes().enumerate() {
                if byte == b'#' {
                    ink_pixels.push((x + 1, y + 1));
                }
            }
        }
        // Outline the cursor in white so it stays legible over dark rows.
        for &(x, y) in &ink_pixels {
            for outline_y in y - 1..=y + 1 {
                let start = 4 * (outline_y * width + x - 1);
                rgba[start..start + 12].fill(255);
            }
        }
        for (x, y) in ink_pixels {
            let start = 4 * (y * width + x);
            rgba[start..start + 4].copy_from_slice(&[0, 0, 0, 255]);
        }
        egui::CustomCursorImage {
            rgba: rgba.into(),
            size: [width as u16, height as u16],
            hotspot: [(width / 2) as u16, (height / 2) as u16],
        }
    })
}

fn paint_selection_outline(ui: &egui::Ui, rect: Rect, color: Color32) {
    let scale = ui.pixels_per_point();
    let bounds = rect.round_to_pixels(scale);
    let width = (bounds.width() * scale).round() as usize;
    let height = (bounds.height() * scale).round() as usize;
    if width == 0 || height < 2 {
        return;
    }
    let pixel = 1.0 / scale;
    let mut mesh = egui::Mesh::default();
    let mut dot = |x, y| {
        let origin = bounds.min + Vec2::new(x as f32 * pixel, y as f32 * pixel);
        mesh.add_colored_rect(Rect::from_min_size(origin, Vec2::splat(pixel)), color);
    };
    for x in (0..width).step_by(2) {
        dot(x, 0);
        dot(x, height - 1);
    }
    for y in (2..height - 1).step_by(2) {
        dot(0, y);
        dot(width - 1, y);
    }
    ui.painter().add(egui::Shape::mesh(mesh));
}

fn paint_icon(ui: &egui::Ui, rect: Rect, is_dir: bool, opacity: u8) {
    let mut painter = ui.painter().clone();
    painter.set_opacity(f32::from(opacity) / 255.0);
    if is_dir {
        let tab = Rect::from_min_size(
            rect.left_top() + Vec2::new(0.0, 1.0),
            Vec2::new(rect.width() * 0.5, 3.5),
        );
        painter.rect_filled(tab, 1.0, Color32::from_rgb(222, 168, 42));
        let body = Rect::from_min_size(
            rect.left_top() + Vec2::new(0.0, 3.5),
            Vec2::new(rect.width(), rect.height() - 3.5),
        );
        painter.rect_filled(body, 1.5, Color32::from_rgb(247, 196, 61));
        let highlight = Rect::from_min_size(
            body.left_top() + Vec2::new(1.0, 1.0),
            Vec2::new(body.width() - 2.0, 1.5),
        );
        painter.rect_filled(highlight, 1.0, Color32::from_rgb(255, 219, 110));
    } else {
        let page = rect.shrink2(Vec2::new(1.5, 0.5));
        painter.rect_filled(page, 1.0, Color32::from_rgb(238, 238, 238));
        painter.rect_filled(
            Rect::from_min_size(page.left_top(), Vec2::new(1.0, page.height())),
            0.0,
            Color32::from_rgb(170, 170, 170),
        );
        painter.rect_filled(
            Rect::from_min_size(page.right_top() - Vec2::new(1.0, 0.0), Vec2::new(1.0, page.height())),
            0.0,
            Color32::from_rgb(170, 170, 170),
        );
        painter.rect_filled(
            Rect::from_min_size(page.left_top(), Vec2::new(page.width(), 1.0)),
            0.0,
            Color32::from_rgb(170, 170, 170),
        );
        let fold = 4.0;
        painter.add(egui::Shape::convex_polygon(
            vec![
                page.right_top() + Vec2::new(-fold, 0.0),
                page.right_top() + Vec2::new(0.0, fold),
                page.right_top(),
            ],
            Color32::from_rgb(200, 200, 200),
            Stroke::NONE,
        ));
    }
}

fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            result.push(' ');
        }
        result.push(ch);
    }
    result
}

fn containing_folder(entry: &Entry) -> &Path {
    entry.path.parent().unwrap_or(&entry.path)
}

fn status_folder(entry: &Entry) -> String {
    let mut folder = containing_folder(entry).display().to_string();
    if matches!(folder.as_bytes(), [letter, b':', b'\\' | b'/'] if letter.is_ascii_alphabetic()) {
        folder.truncate(2);
    }
    folder
}

fn human_size(size: Option<u64>, is_dir: bool) -> String {
    if is_dir {
        return String::new();
    }
    let Some(bytes) = size else {
        return String::new();
    };
    let kilobytes = bytes.div_ceil(1024);
    format!("{} KB", format_count(kilobytes))
}

fn status_size(bytes: u64) -> String {
    for (factor, unit) in [(1 << 40, "TB"), (1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB")] {
        if bytes >= factor {
            let value = format!("{:.2}", bytes as f64 / factor as f64).replace('.', ",");
            return format!("{value} {unit}");
        }
    }
    format!("{bytes} B")
}

fn local_datetime(filetime: u64) -> String {
    let seconds = timeutil::filetime_to_unix_secs(filetime);
    match chrono::DateTime::from_timestamp(seconds, 0) {
        Some(utc) => {
            let local: chrono::DateTime<chrono::Local> = utc.into();
            local.format("%d.%m.%Y %H:%M").to_string()
        }
        None => String::new(),
    }
}

fn open_path(path: &Path) {
    let _ = std::process::Command::new("cmd")
        .arg("/C")
        .arg("start")
        .arg("")
        .arg(path)
        .spawn();
}

#[cfg(windows)]
fn is_elevated() -> bool {
    #[link(name = "shell32")]
    extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
fn is_elevated() -> bool {
    false
}

#[cfg(windows)]
fn relaunch_as_admin(arguments: &[String]) -> bool {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            lp_operation: *const u16,
            lp_file: *const u16,
            lp_parameters: *const u16,
            lp_directory: *const u16,
            n_show_cmd: i32,
        ) -> isize;
    }

    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let quote = |argument: &str| {
        if argument.contains(' ') {
            format!("\"{argument}\"")
        } else {
            argument.to_owned()
        }
    };
    let parameters_text = arguments
        .iter()
        .filter(|argument| argument.as_str() != "--no-elevate")
        .map(|argument| quote(argument))
        .collect::<Vec<_>>()
        .join(" ");

    let wide = |text: &str| -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    };
    let executable_wide: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let operation = wide("runas");
    let parameters = wide(&parameters_text);
    let result = unsafe {
        ShellExecuteW(
            0,
            operation.as_ptr(),
            executable_wide.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            1,
        )
    };
    result > 32
}

#[cfg(not(windows))]
fn relaunch_as_admin(_arguments: &[String]) -> bool {
    false
}

#[cfg(windows)]
fn reveal_path(path: &Path) {
    use std::ffi::OsString;
    use std::os::windows::process::CommandExt;

    let path = path.as_os_str();
    if path.as_encoded_bytes().contains(&b'"') {
        return;
    }

    // Explorer expects quotes around the path after the comma.
    let mut argument = OsString::from("/select,\"");
    argument.push(path);
    argument.push("\"");

    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(argument)
        .spawn();
}

#[cfg(not(windows))]
fn reveal_path(_path: &Path) {}

#[cfg(windows)]
fn recycle_file(path: &Path) -> anyhow::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    use windows_sys::Win32::UI::Shell::{
        FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_NO_CONNECTED_ELEMENTS,
        FOF_WANTNUKEWARNING, FO_DELETE, SHFILEOPSTRUCTW, SHFileOperationW,
    };

    for component in path.components() {
        if let Component::Normal(name) = component {
            let bytes = name.as_encoded_bytes();
            anyhow::ensure!(
                !bytes.ends_with(b".") && !bytes.ends_with(b" ")
                    && !bytes.iter().any(|&byte| matches!(byte, 0..=31 | b':' | b'*' | b'?')),
                "Windows Shell cannot safely recycle this path"
            );
        }
    }
    let path = std::path::absolute(path)?;
    let ordinary_path = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::UNC(..))
    );
    let mut source: Vec<u16> = path.as_os_str().encode_wide().collect();
    anyhow::ensure!(
        ordinary_path && !source.iter().any(|&unit| matches!(unit, 0 | 42 | 63)),
        "Windows Shell cannot safely recycle this path"
    );
    match scan::read_entry(path) {
        Ok(entry) if entry.is_dir => anyhow::bail!("The path now points to a folder"),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    }

    // The Shell expects a double-NUL-terminated list of full paths.
    source.extend([0, 0]);
    let mut operation = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: source.as_ptr(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI
            | FOF_NO_CONNECTED_ELEMENTS | FOF_WANTNUKEWARNING) as u16,
        ..Default::default()
    };
    let result = unsafe { SHFileOperationW(&mut operation) };
    if result == 0 && operation.fAnyOperationsAborted != 0 {
        return Ok(false);
    }
    anyhow::ensure!(result == 0, "Windows Shell error {result:#x}");
    Ok(true)
}

#[cfg(not(windows))]
fn recycle_file(_path: &Path) -> anyhow::Result<bool> {
    anyhow::bail!("Moving files to the Recycle Bin requires Windows");
}

fn load_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for candidate in [
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\tahoma.ttf",
        r"C:\Windows\Fonts\arial.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(candidate) {
            let mut font = egui::FontData::from_owned(bytes);
            font.tweak.hinting = Some(true);
            font.tweak.hinting_target = HintingTarget::Smooth(SmoothHinting {
                symmetric_rendering: false,
                preserve_linear_metrics: false,
                ..Default::default()
            });
            font.tweak.subpixel_binning = Some(false);
            fonts
                .font_data
                .insert("ui".to_owned(), font.into());
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.insert(0, "ui".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn apply_theme(ctx: &egui::Context, dark: bool) {
    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    let mut style = (*ctx.style_of(theme)).clone();
    let palette = Palette::new(dark);
    style.spacing.item_spacing = Vec2::new(6.0, 4.0);
    style.spacing.button_padding = Vec2::new(8.0, 3.0);
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = if dark {
        palette.header_bg
    } else {
        Color32::from_rgb(250, 250, 250)
    };
    visuals.window_fill = visuals.panel_fill;
    visuals.extreme_bg_color = palette.bg;
    visuals.faint_bg_color = if dark {
        Color32::from_rgb(32, 32, 32)
    } else {
        Color32::from_rgb(243, 243, 243)
    };
    visuals.selection.bg_fill = palette.selected_bg;
    visuals.selection.stroke.color = if dark { ACCENT } else { palette.selected_outline };
    visuals.text_cursor.stroke.color = visuals.selection.stroke.color;
    if dark {
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(48, 48, 48));
        visuals.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(208, 208, 208);
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(95, 95, 95));
        visuals.widgets.inactive.fg_stroke.color = Color32::from_rgb(240, 240, 240);
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(42, 42, 42);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(127, 127, 127));
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(51, 51, 51);
        visuals.widgets.active.weak_bg_fill = Color32::from_rgb(64, 64, 64);
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.bg_fill = palette.header_bg;
        }
    }
    style.visuals = visuals;
    style.visuals.menu_corner_radius = egui::CornerRadius::ZERO;
    style.visuals.window_corner_radius = egui::CornerRadius::ZERO;
    for widget in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::ZERO;
    }
    for (text_style, size) in [
        (egui::TextStyle::Body, FONT_SIZE),
        (egui::TextStyle::Button, FONT_SIZE),
        (egui::TextStyle::Small, 11.5),
    ] {
        style
            .text_styles
            .insert(text_style, FontId::proportional(size));
    }
    ctx.set_style_of(theme, style);
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
}

#[cfg(windows)]
struct IconCache {
    inner: std::cell::RefCell<shell_icons::IconCache>,
}

#[cfg(windows)]
impl IconCache {
    fn new() -> Self {
        Self {
            inner: std::cell::RefCell::new(shell_icons::IconCache::default()),
        }
    }

    fn texture_id(&self, ctx: &egui::Context, entry: &Entry) -> Option<egui::TextureId> {
        self.inner.borrow_mut().texture_id(ctx, entry)
    }
}

#[cfg(not(windows))]
struct IconCache;

#[cfg(not(windows))]
impl IconCache {
    fn new() -> Self {
        Self
    }

    fn texture_id(&self, _ctx: &egui::Context, _entry: &Entry) -> Option<egui::TextureId> {
        None
    }
}

/// Real shell icons straight from Windows: `SHGetFileInfoW` for the icon of a
/// file type / folder, then `HICON` -> RGBA via a top-down 32bpp DIB.
#[cfg(windows)]
mod shell_icons {
    use std::collections::HashMap;
    use std::ffi::c_void;

    use everyrust::model::Entry;
    use eframe::egui;

    const SHGFI_ICON: u32 = 0x100;
    const SHGFI_SMALLICON: u32 = 0x1;
    const SHGFI_USEFILEATTRIBUTES: u32 = 0x10;

    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

    const DIB_RGB_COLORS: u32 = 0;
    const DI_NORMAL: u32 = 0x0003;

    #[repr(C)]
    struct ShFileInfoW {
        h_icon: isize,
        i_icon: i32,
        attributes: u32,
        display_name: [u16; 260],
        type_name: [u16; 80],
    }

    impl Default for ShFileInfoW {
        fn default() -> Self {
            Self {
                h_icon: 0,
                i_icon: 0,
                attributes: 0,
                display_name: [0; 260],
                type_name: [0; 80],
            }
        }
    }

    #[repr(C)]
    struct BitmapInfoHeader {
        bi_size: u32,
        bi_width: i32,
        bi_height: i32,
        bi_planes: u16,
        bi_bit_count: u16,
        bi_compression: u32,
        bi_size_image: u32,
        bi_x_pels_per_meter: i32,
        bi_y_pels_per_meter: i32,
        bi_clr_used: u32,
        bi_clr_important: u32,
    }

    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [u32; 1],
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHGetFileInfoW(
            psz_path: *const u16,
            dw_file_attributes: u32,
            psfi: *mut ShFileInfoW,
            cb_file_info: u32,
            u_flags: u32,
        ) -> usize;
    }

    #[link(name = "user32")]
    extern "system" {
        fn DestroyIcon(hicon: isize) -> i32;
        fn GetDC(hwnd: isize) -> isize;
        fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
        fn DrawIconEx(
            hdc: isize,
            x: i32,
            y: i32,
            hicon: isize,
            cx: i32,
            cy: i32,
            istep_if_ani: u32,
            hbr_flicker_free: isize,
            di_flags: u32,
        ) -> i32;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateDIBSection(
            hdc: isize,
            pbmi: *const BitmapInfo,
            usage: u32,
            bits: *mut *mut c_void,
            section: isize,
            offset: u32,
        ) -> isize;
        fn CreateCompatibleDC(hdc: isize) -> isize;
        fn SelectObject(hdc: isize, object: isize) -> isize;
        fn GdiFlush() -> i32;
        fn DeleteDC(hdc: isize) -> i32;
        fn DeleteObject(object: isize) -> i32;
    }

    #[derive(Default)]
    pub struct IconCache {
        textures: HashMap<(String, bool), Option<egui::TextureHandle>>,
    }

    impl IconCache {
        pub fn texture_id(
            &mut self,
            ctx: &egui::Context,
            entry: &Entry,
        ) -> Option<egui::TextureId> {
            let key = if entry.is_dir {
                "dir".to_owned()
            } else {
                let extension = entry
                    .path
                    .extension()
                    .map(|extension| extension.to_string_lossy().to_lowercase())
                    .unwrap_or_else(|| "noext".to_owned());
                format!("ext:{extension}")
            };

            let use_small_icon = super::ROW_ICON_SIZE * ctx.pixels_per_point() <= 16.0;
            self.textures
                .entry((key.clone(), use_small_icon))
                .or_insert_with(|| {
                    let image = load_icon(&key, entry.is_dir, use_small_icon)?;
                    Some(ctx.load_texture(&key, image, egui::TextureOptions::LINEAR))
                })
                .as_ref()
                .map(egui::TextureHandle::id)
        }
    }

    fn load_icon(key: &str, is_dir: bool, use_small_icon: bool) -> Option<egui::ColorImage> {
        let fake_name = if is_dir {
            "folder".to_owned()
        } else {
            match key.strip_prefix("ext:") {
                Some(extension) if !extension.is_empty() && extension != "noext" => {
                    format!("file.{extension}")
                }
                _ => "file".to_owned(),
            }
        };
        let wide: Vec<u16> = fake_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let mut info = ShFileInfoW::default();
        let attributes = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        let (size, size_flag) = if use_small_icon { (16, SHGFI_SMALLICON) } else { (32, 0) };
        let flags = SHGFI_ICON | SHGFI_USEFILEATTRIBUTES | size_flag;
        let result = unsafe {
            SHGetFileInfoW(
                wide.as_ptr(),
                attributes,
                &mut info,
                std::mem::size_of::<ShFileInfoW>() as u32,
                flags,
            )
        };
        if result == 0 || info.h_icon == 0 {
            return None;
        }

        let rgba = unsafe { icon_to_rgba(info.h_icon, size) };
        unsafe {
            DestroyIcon(info.h_icon);
        }
        rgba.map(|rgba| {
            egui::ColorImage::from_rgba_premultiplied(
                [size as usize, size as usize],
                &rgba,
            )
        })
    }

    unsafe fn icon_to_rgba(hicon: isize, size: u32) -> Option<Vec<u8>> {
        let screen = GetDC(0);
        if screen == 0 {
            return None;
        }

        let info = BitmapInfo {
            header: BitmapInfoHeader {
                bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                bi_width: size as i32,
                bi_height: -(size as i32),
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: 0, // BI_RGB
                bi_size_image: 0,
                bi_x_pels_per_meter: 0,
                bi_y_pels_per_meter: 0,
                bi_clr_used: 0,
                bi_clr_important: 0,
            },
            colors: [0],
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, 0, 0);
        let mut rgba = None;

        if bitmap != 0 && !bits.is_null() {
            let memory = CreateCompatibleDC(screen);
            if memory != 0 {
                let old = SelectObject(memory, bitmap);
                rgba = render_rgba(memory, hicon, bits.cast(), size);
                SelectObject(memory, old);
                DeleteDC(memory);
            }
            DeleteObject(bitmap);
        }
        ReleaseDC(0, screen);
        rgba
    }

    unsafe fn render_rgba(dc: isize, icon: isize, bits: *mut u8, size: u32) -> Option<Vec<u8>> {
        let len = (size * size * 4) as usize;
        let render = |background| {
            std::ptr::write_bytes(bits, background, len);
            let drawn = DrawIconEx(dc, 0, 0, icon, size as i32, size as i32, 0, 0, DI_NORMAL);
            GdiFlush();
            (drawn != 0).then(|| std::slice::from_raw_parts(bits, len).to_vec())
        };
        // The difference between white and black reveals transparency even
        // for older icons whose mask is applied without writing an alpha byte.
        let mut rgba = render(0)?;
        let on_white = render(255)?;
        for (pixel, white) in rgba.chunks_exact_mut(4).zip(on_white.chunks_exact(4)) {
            let transparency = white[0]
                .saturating_sub(pixel[0])
                .min(white[1].saturating_sub(pixel[1]))
                .min(white[2].saturating_sub(pixel[2]));
            pixel.swap(0, 2);
            pixel[3] = 255 - transparency;
        }
        Some(rgba)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_logo_decodes_to_an_rgba_window_icon() {
        let icon = app_icon();
        assert!(icon.width > 0 && icon.height > 0);
        assert_eq!(icon.rgba.len(), icon.width as usize * icon.height as usize * 4);
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn visible_rows_include_partial_rows_without_overscan() {
        assert_eq!(visible_rows(egui::emath::Rangef::new(19.0, 57.0), 10), 1..3);
        assert_eq!(visible_rows(egui::emath::Rangef::new(20.0, 58.0), 10), 1..4);
        assert_eq!(visible_rows(egui::emath::Rangef::new(175.0, 300.0), 10), 9..10);
        assert_eq!(visible_rows(egui::emath::Rangef::new(20.0, 20.0), 10), 1..1);
    }

    #[test]
    fn size_display_rounds_partial_kilobytes_up() {
        assert_eq!(human_size(Some(0), false), "0 KB");
        assert_eq!(human_size(Some(1), false), "1 KB");
        assert_eq!(human_size(Some(1024), false), "1 KB");
        assert_eq!(human_size(Some(1025), false), "2 KB");
        assert_eq!(human_size(Some(u64::MAX), false), "18 014 398 509 481 984 KB");
        assert_eq!(human_size(Some(1024), true), "");
        assert_eq!(human_size(None, false), "");
    }

    #[test]
    fn status_size_uses_readable_units_and_decimal_comma() {
        assert_eq!(status_size(17), "17 B");
        assert_eq!(status_size(1536), "1,50 KB");
        assert_eq!(status_size(5 << 30), "5,00 GB");
    }
}
