//! Aptimizer Studio: AptCAD (drafting) and AptBIM (building modelling) in one window.
//!
//! Both apps are libraries with a thin host around them (`aptcad_ui_egui::CadApp`,
//! `aptbim_ui_egui::BimApp`). This host owns one of each and shows one at a time below a bar with
//! the two modes. They share the window's egui context, and each sets the context's fonts and
//! style to its own, so a switch asks the incoming app to apply its look again (`restyle`).
//!
//! Usage: `aptimizer-studio [--cad | --bim] [FILE…]`. A drawing (DXF, DWG, DWT, SVG) opens in
//! AptCAD, a project (AptBIM, IFC, Revit) in AptBIM, and the window starts in the mode of the last
//! file given, or the one asked for, or AptBIM.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aptbim_ui_egui::{BimApp, lang};
use aptcad_ui_egui::{CadApp, Services, i18n};
use egui::{Color32, RichText, TextureHandle, vec2};

/// What the desktop knows the app by: Wayland's app id, X11's window class and the storage folder.
const APP_ID: &str = "io.github.anuj09k.aptimizer-studio";

/// The height of the bar with the two modes, above both apps.
const BAR_HEIGHT: f32 = 30.0;

const USAGE: &str = "USAGE: aptimizer-studio [--cad | --bim] [FILE...]
  FILE   a drawing (.dxf, .dwg, .dwt, .svg) opens in AptCAD; a project (.aptbim, .ifc, .rvt) in AptBIM
  --cad  start in AptCAD (drafting)
  --bim  start in AptBIM (building modelling); the default";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Cad,
    Bim,
}

/// Which app a file belongs to, by its extension; `None` for one neither opens.
fn mode_for(path: &Path) -> Option<Mode> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match ext.as_str() {
        "dxf" | "dwg" | "dwt" | "svg" | "svgz" => Some(Mode::Cad),
        "aptbim" | "ifc" | "rvt" => Some(Mode::Bim),
        _ => None,
    }
}

/// The parsed command line.
#[derive(Debug, Default, PartialEq)]
struct Launch {
    mode: Option<Mode>,
    cad_files: Vec<PathBuf>,
    bim_files: Vec<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Launch, String> {
    let mut launch = Launch::default();
    let (mut asked, mut last) = (None, None);
    for arg in args {
        match arg.as_str() {
            "--cad" => asked = Some(Mode::Cad),
            "--bim" => asked = Some(Mode::Bim),
            flag if flag.starts_with('-') => return Err(format!("unknown option `{flag}`")),
            file => {
                let path = PathBuf::from(file);
                match mode_for(&path) {
                    Some(Mode::Cad) => launch.cad_files.push(path),
                    Some(Mode::Bim) => launch.bim_files.push(path),
                    None => {
                        return Err(format!(
                            "`{file}` is neither a drawing nor a project this app opens"
                        ));
                    }
                }
                last = mode_for(Path::new(file));
            }
        }
    }
    // A mode asked for wins; otherwise the last file given decides.
    launch.mode = asked.or(last);
    Ok(launch)
}

struct Studio {
    mode: Mode,
    cad: CadApp,
    bim: BimApp,
    /// The two modes' icons for the bar, made on the first frame.
    icons: Option<[TextureHandle; 2]>,
    /// The window title last sent, so it is sent again only when it changes.
    title: String,
}

/// The window's title: what the app shown names (the drawing or the project and view, with `*` while unsaved), then the studio.
fn studio_title(app: &str) -> String {
    if app.is_empty() {
        "Aptimizer Studio".to_string()
    } else {
        format!("{app} — Aptimizer Studio")
    }
}

/// What AptCAD's mode puts in the title: its drawing, `*` while unsaved, and its name.
fn cad_title(name: Option<&str>, dirty: bool) -> String {
    match name {
        Some(name) => format!("{name}{} · AptCAD", if dirty { "*" } else { "" }),
        None => "AptCAD".to_string(),
    }
}

impl Studio {
    fn switch_to(&mut self, mode: Mode, ctx: &egui::Context) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        // The other app set the context's fonts and style to its own: the incoming one applies its look again.
        match mode {
            Mode::Cad => self.cad.restyle(),
            Mode::Bim => self.bim.restyle(ctx),
        }
        ctx.request_repaint();
    }

    /// The studio owns the window's title. AptBIM sets it to its own whenever its project or view changes; this runs after the
    /// app each frame, so the studio's title is the last asked for, and is sent again whenever the app's part changes.
    fn keep_title(&mut self, ctx: &egui::Context) {
        let app = match self.mode {
            Mode::Bim => self.bim.window_title().to_string(),
            Mode::Cad => {
                let state = self.cad.session.state().ok();
                cad_title(
                    state.map(|d| d.title.as_str()),
                    state.is_some_and(|d| d.is_dirty()),
                )
            }
        };
        let title = studio_title(&app);
        // AptBIM sends its title when it changes; its change is ours too, so ours always follows it in the same frame.
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn icons(&mut self, ctx: &egui::Context) -> &[TextureHandle; 2] {
        self.icons.get_or_insert_with(|| {
            [
                texture(
                    ctx,
                    "studio-aptcad",
                    include_bytes!("../assets/aptcad-64.png"),
                ),
                texture(
                    ctx,
                    "studio-aptbim",
                    include_bytes!("../assets/aptbim-64.png"),
                ),
            ]
        })
    }

    /// Keep the strip at the top of the window for the bar, before an app lays itself out below it.
    fn reserve_bar(ui: &mut egui::Ui) -> egui::Rect {
        egui::Panel::top("studio-mode-bar")
            .exact_size(BAR_HEIGHT)
            .resizable(false)
            .show(ui, |_| {})
            .response
            .rect
    }

    /// The bar above both apps: the studio's name and the two modes, the active one marked. Drawn after the app, on a
    /// layer above it, in the strip `reserve_bar` kept: AptBIM paints a ground across the whole window, which covered a bar
    /// drawn before it.
    fn mode_bar(&mut self, ctx: &egui::Context, strip: egui::Rect) {
        let [cad_icon, bim_icon] = self.icons(ctx).clone();
        let mut wanted = None;
        egui::Area::new(egui::Id::unique("studio-mode-bar-area")).order(egui::Order::Middle).fixed_pos(strip.min).show(ctx, |ui| {
            ui.set_min_size(strip.size());
            ui.set_max_size(strip.size());
            let visuals = ui.visuals().clone();
            ui.painter().rect_filled(strip, 0.0, visuals.panel_fill);
            ui.painter().hline(strip.x_range(), strip.bottom() - 0.5, visuals.widgets.noninteractive.bg_stroke);
            ui.allocate_ui_with_layout(strip.size(), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                ui.label(RichText::new("Aptimizer Studio").strong());
                ui.add_space(12.0);
                for (mode, icon, name, what) in [
                    (Mode::Cad, &cad_icon, "AptCAD", "Drafting: drawings, layouts and plots (DXF, DWG)"),
                    (Mode::Bim, &bim_icon, "AptBIM", "Building modelling: walls, rooms, sheets and schedules (AptBIM, IFC, Revit)"),
                ] {
                    let image = egui::Image::from_texture(egui::load::SizedTexture::new(icon.id(), vec2(18.0, 18.0)));
                    let tab = egui::Button::selectable(self.mode == mode, (image, RichText::new(name).strong()));
                    if ui.add(tab).on_hover_text(what).clicked() {
                        wanted = Some(mode);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new("by Aptimizer").small().color(Color32::GRAY));
                });
            });
        });
        if let Some(mode) = wanted {
            self.switch_to(mode, ctx);
        }
    }
}

fn texture(ctx: &egui::Context, name: &str, png: &[u8]) -> TextureHandle {
    let image = eframe::icon_data::from_png_bytes(png)
        .map(|icon| {
            egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            )
        })
        .unwrap_or_else(|_| egui::ColorImage::filled([1, 1], Color32::TRANSPARENT));
    ctx.load_texture(name, image, egui::TextureOptions::LINEAR)
}

impl eframe::App for Studio {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // AptCAD's per-frame work (its control queue, autosave, theme) runs while it is shown.
        if self.mode == Mode::Cad {
            self.cad.guarded_logic(ctx);
            if std::mem::take(&mut self.cad.quit_requested) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        match self.mode {
            Mode::Cad => {
                self.cad.raw_input_hook(ctx, raw);
                aptcad_ui_egui::cmdline::capture_tab(ctx, raw);
            }
            Mode::Bim => eframe::App::raw_input_hook(&mut self.bim, ctx, raw),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let strip = Self::reserve_bar(ui);
        match self.mode {
            Mode::Cad => self.cad.guarded_ui(ui),
            Mode::Bim => eframe::App::ui(&mut self.bim, ui, frame),
        }
        let ctx = ui.ctx().clone();
        self.mode_bar(&ctx, strip);
        self.keep_title(&ctx);
    }

    fn on_exit(&mut self) {
        self.cad.cancel_screenshots();
        self.cad.shutdown_autosave();
    }

    /// AptCAD's preferences survive restarts here; AptBIM keeps its own settings file.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(
            aptcad_ui_egui::options::PROFILE_KEY,
            self.cad.profile_json(),
        );
        storage.set_string(aptcad_ui_egui::PREFS_KEY, self.cad.prefs_json());
        storage.set_string(
            aptcad_ui_egui::autosave::PREFS_KEY,
            aptcad_ui_egui::autosave::prefs_json(),
        );
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }
}

/// AptCAD's file format hooks (its engine stays I/O-agnostic), as its own app installs them.
fn install_cad_io() {
    aptcad_engine::cmd::file::set_io(aptcad_engine::cmd::file::IoHooks {
        read: |b, name| aptcad_io::read(b, name).map_err(|e| e.to_string()),
        write: |d, name| aptcad_io::write(d, name).map_err(|e| e.to_string()),
        plot: Some(|d, space, opts| {
            aptcad_io::plotter::plot(d, space, opts).map_err(|e| e.to_string())
        }),
    });
    aptcad_engine::cmd::file::set_write_as(|d, name, o| {
        aptcad_io::write_with(
            d,
            name,
            &aptcad_io::SaveOptions {
                version: o.version,
                binary: o.binary,
            },
        )
        .map_err(|e| e.to_string())
    });
    aptcad_engine::cmd::file::set_entity_codec(aptcad_engine::cmd::file::EntityCodec {
        groups: aptcad_io::entity_groups,
        entity: aptcad_io::entity_from_groups,
    });
}

/// AptCAD's file dialogs.
fn cad_services() -> Services {
    Services {
        pick_open: Some(Box::new(|| {
            rfd::FileDialog::new()
                .add_filter(i18n::t("Drawings (DWG, DXF)"), &["dwg", "dxf", "dwt"])
                .add_filter(i18n::t("SVG drawings (SVG, SVGZ)"), &["svg", "svgz"])
                .add_filter(i18n::t("All files"), &["*"])
                .pick_file()
                .map(|p| p.to_string_lossy().to_string())
        })),
        pick_save: Some(Box::new(|name: &str| {
            let lower = name.to_ascii_lowercase();
            let mut filters = [
                ("Drawing (DXF)", "dxf"),
                ("Drawing (DWG)", "dwg"),
                ("SVG", "svg"),
                ("PNG", "png"),
                ("PDF", "pdf"),
            ];
            // The suggested name's type first: Windows adds the first filter's extension, macOS replaces others with it.
            filters.sort_by_key(|(_, ext)| !lower.ends_with(&format!(".{ext}")));
            filters
                .iter()
                .fold(
                    rfd::FileDialog::new().set_file_name(name),
                    |d, (label, ext)| d.add_filter(i18n::t(label), &[*ext]),
                )
                .save_file()
                .map(|p| p.to_string_lossy().to_string())
        })),
        pick_lisp: Some(Box::new(|dir: &str| {
            let dialog = rfd::FileDialog::new()
                .add_filter(i18n::t("AutoLISP (LSP)"), &["lsp"])
                .add_filter(i18n::t("All files"), &["*"]);
            let dialog = if dir.is_empty() {
                dialog
            } else {
                dialog.set_directory(dir)
            };
            dialog.pick_file().map(|p| p.to_string_lossy().to_string())
        })),
        no_file_system: false,
    }
}

#[cfg(feature = "rvt")]
fn revit_reader() -> Option<aptbim_ui_egui::revit::Importer> {
    Some(Arc::new(|path: &Path| {
        let stem = path.file_stem().map_or_else(
            || "Project".to_string(),
            |s| s.to_string_lossy().into_owned(),
        );
        aptbim_rvt::import_path(path, &stem).map_err(|e| e.to_string())
    }))
}

#[cfg(not(feature = "rvt"))]
fn revit_reader() -> Option<aptbim_ui_egui::revit::Importer> {
    None
}

/// AptBIM, set up as its own app sets it up: its settings, language, recent files and readers.
fn bim_app(files: &[PathBuf]) -> BimApp {
    let favorites = aptbim_core::favorites::default_path();
    let settings = favorites.as_deref().map(aptbim_ui_egui::settings::beside);
    let language = settings
        .as_deref()
        .and_then(aptbim_ui_egui::settings::language_in)
        .unwrap_or_else(lang::from_env);
    let mut session = aptbim_core::engine::Session::new();
    session.dwg = aptbim_dwg::reader();
    let mut later = None;
    let mut opened = Vec::new();
    let mut problem = None;
    for file in files {
        if aptbim_ui_egui::revit::is_revit(file) {
            later = Some(file.clone());
        } else {
            match session.execute("open", &serde_json::json!({ "path": file })) {
                Ok(_) => opened.push(file.clone()),
                Err(e) => problem = Some(format!("{}: {e}", file.display())),
            }
        }
    }
    if opened.is_empty() {
        let _ = session.execute(
            "new",
            &serde_json::json!({"language": language.project().code()}),
        );
    }
    // What the arguments built is where the user starts: nothing to undo, nothing unsaved.
    let (model, path) = ((*session.model).clone(), session.path.clone());
    session.reset(model, path);
    let mut app = BimApp::new(session);
    app.lang = language;
    if files.is_empty() {
        app.show_start_page();
    }
    app.revit = revit_reader();
    if let Some(file) = later {
        app.open_later(file);
    }
    if let Some(path) = settings {
        app.use_settings(path);
    }
    if let Some(path) = &favorites {
        app.keep_recent_in(aptbim_ui_egui::recent::beside(path));
        for file in &opened {
            app.note_recent(file);
        }
    }
    if let Some(text) = problem {
        app.status = aptbim_ui_egui::Status { text, error: true };
    }
    // The studio's window has the system's title bar, round both apps.
    app.own_title_bar = false;
    app
}

/// AptCAD's settings folder inside the studio's own: profile, autosave files and LISP support.
fn cad_settings_dir() -> Option<PathBuf> {
    eframe::storage_dir(APP_ID).map(|dir| dir.join("aptcad"))
}

fn main() -> std::process::ExitCode {
    // A copy of this program started by either app to read or write one DWG does only that.
    if let Some(code) = aptcad_io::dwg_worker::serve_if_asked() {
        std::process::exit(code);
    }
    aptcad_io::dwg_worker::use_own_worker();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = aptbim_dwg::child_main(&args) {
        return code;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("aptimizer-studio {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    let launch = match parse_args(args) {
        Ok(launch) => launch,
        Err(e) => {
            eprintln!("aptimizer-studio: {e}\n\n{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };
    install_cad_io();
    let cad_dir = cad_settings_dir();
    aptcad_engine::settings_dir::set(cad_dir.clone());

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Aptimizer Studio")
        .with_app_id(APP_ID)
        .with_inner_size([1600.0, 1000.0])
        .with_min_inner_size([960.0, 600.0])
        .with_drag_and_drop(true);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/studio-256.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    let mut options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };
    // No stencil buffer and no depth buffer: AptBIM's canvas pipelines are made for a render pass without them, and
    // AptCAD then keeps clipped layout viewports in their display list, as its web build does.
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup {
        let base = Arc::clone(&create.device_descriptor);
        create.device_descriptor = Arc::new(move |adapter| {
            let mut d = base(adapter);
            // AptBIM's 3D view asks for the card's limits; AptCAD's large previews need its biggest buffers.
            d.required_limits = aptbim_ui_egui::limits_for(adapter, d.required_limits);
            d.required_limits.max_buffer_size = d
                .required_limits
                .max_buffer_size
                .max(adapter.limits().max_buffer_size);
            d
        });
    }
    let start_mode = launch.mode.unwrap_or(Mode::Bim);
    let cad_files = launch.cad_files;
    let bim = bim_app(&launch.bim_files);
    let run = eframe::run_native(
        "Aptimizer Studio",
        options,
        Box::new(move |cc| {
            let mut cad = CadApp::new(aptcad_engine::Session::empty(), cad_services());
            cad.system_languages = sys_locale::get_locales().collect();
            if let Some(profile) = cc
                .storage
                .and_then(|s| s.get_string(aptcad_ui_egui::options::PROFILE_KEY))
            {
                cad.load_profile(&profile);
            }
            if let Some(prefs) = cc
                .storage
                .and_then(|s| s.get_string(aptcad_ui_egui::PREFS_KEY))
            {
                cad.load_prefs(&prefs);
            }
            if let Some(prefs) = cc
                .storage
                .and_then(|s| s.get_string(aptcad_ui_egui::autosave::PREFS_KEY))
            {
                aptcad_ui_egui::autosave::load_prefs(&prefs);
            }
            if let Some(dir) = &cad_dir {
                cad.enable_autosave(&dir.join("autosave"));
                let support = dir.join("support");
                let _ = std::fs::create_dir_all(&support);
                cad.session.set_lisp_support_dir(Some(support));
            }
            // The menus stay inside the window (on macOS AptCAD alone puts them in the system's menu bar).
            cad.ui.in_window_menu = true;
            cad.integrated_titlebar = false;
            for file in &cad_files {
                cad.open_path(file);
            }
            if cad.session.docs.is_empty() {
                let metric = cad.session.settings.measureinit;
                cad.session.new_drawing(metric);
            }
            let mut bim = bim;
            bim.enable_autosave(std::env::temp_dir().join("aptbim-autosave"), &cc.egui_ctx);
            if let Some(state) = cc.wgpu_render_state.as_ref() {
                cad.set_wgpu(state, None);
                bim.use_graphics_card(state);
            }
            let mut studio = Studio {
                mode: start_mode,
                cad,
                bim,
                icons: None,
                title: String::new(),
            };
            // Whichever app starts applies its look on its first frame anyway; asked again for certainty.
            match start_mode {
                Mode::Cad => studio.cad.restyle(),
                Mode::Bim => studio.bim.restyle(&cc.egui_ctx),
            }
            Ok(Box::new(studio))
        }),
    );
    match run {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aptimizer-studio: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_open_in_the_app_that_reads_them_and_the_last_decides_the_mode() {
        let launch =
            parse_args(["plan.dwg".to_string(), "house.aptbim".to_string()]).unwrap_or_default();
        assert_eq!(launch.cad_files, vec![PathBuf::from("plan.dwg")]);
        assert_eq!(launch.bim_files, vec![PathBuf::from("house.aptbim")]);
        assert_eq!(launch.mode, Some(Mode::Bim), "the last file decides");
        assert_eq!(
            parse_args(["tower.IFC".to_string()]).map(|l| l.mode),
            Ok(Some(Mode::Bim))
        );
        assert_eq!(
            parse_args(["--cad".to_string(), "x.rvt".to_string()]).map(|l| l.mode),
            Ok(Some(Mode::Cad))
        );
        assert_eq!(parse_args(Vec::<String>::new()), Ok(Launch::default()));
    }

    #[test]
    fn the_title_names_what_the_mode_shows_then_the_studio() {
        assert_eq!(
            studio_title("Sample House · Floor Plan: Level 1 · AptBIM"),
            "Sample House · Floor Plan: Level 1 · AptBIM — Aptimizer Studio"
        );
        assert_eq!(
            studio_title(&cad_title(Some("plan.dxf"), true)),
            "plan.dxf* · AptCAD — Aptimizer Studio"
        );
        assert_eq!(
            studio_title(&cad_title(Some("plan.dxf"), false)),
            "plan.dxf · AptCAD — Aptimizer Studio"
        );
        assert_eq!(
            studio_title(&cad_title(None, false)),
            "AptCAD — Aptimizer Studio"
        );
        assert_eq!(studio_title(""), "Aptimizer Studio");
    }

    #[test]
    fn unknown_files_and_options_are_refused() {
        assert!(parse_args(["notes.txt".to_string()]).is_err());
        assert!(parse_args(["--fast".to_string()]).is_err());
    }
}
