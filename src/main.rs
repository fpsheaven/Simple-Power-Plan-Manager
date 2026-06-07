#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{Arc, LazyLock},
    time::{SystemTime, UNIX_EPOCH},
};

use eframe::egui::{
    self, Align, Button, Color32, Context, FontData, FontDefinitions, FontFamily, FontId, Layout,
    RichText, ScrollArea, TextEdit, TextStyle, Ui, Vec2,
};
use regex::Regex;
use rfd::FileDialog;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static GUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("GUID regex should compile")
});

static REG_DESCRIPTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*Description\s+REG_\w+\s+(.*)$")
        .expect("registry description regex should compile")
});

const DEFAULT_POWER_PLANS: &[DefaultPowerPlan] = &[
    DefaultPowerPlan {
        guid: "381b4222-f694-41f0-9685-ff5bb260df2e",
        name: "Balanced",
        description: "Automatically balances performance with energy consumption on capable hardware.",
        restored_by_defaultschemes: true,
    },
    DefaultPowerPlan {
        guid: "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c",
        name: "High performance",
        description: "Favors performance, but may use more energy.",
        restored_by_defaultschemes: true,
    },
    DefaultPowerPlan {
        guid: "a1841308-3541-4fab-bc81-f71556f20b4a",
        name: "Power saver",
        description: "Saves energy by reducing your computer's performance where possible.",
        restored_by_defaultschemes: true,
    },
    DefaultPowerPlan {
        guid: "e9a42b02-d5df-448d-aa00-03f14749eb61",
        name: "Ultimate Performance",
        description: "Provides ultimate performance on higher end PCs.",
        restored_by_defaultschemes: false,
    },
];

const YOUTUBE_CHANNEL_URL: &str = "https://www.youtube.com/@fpsheaven";

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 640.0])
            .with_min_inner_size([760.0, 500.0])
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Simple Power Plan Manager by @FPSHEAVEN.com",
        options,
        Box::new(|cc| Ok(Box::new(PowerPlanApp::new(cc)))),
    )
}

fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/app.png"))
        .expect("embedded app icon PNG should be valid")
}

#[derive(Clone, Debug)]
struct PowerPlan {
    guid: String,
    name: String,
    description: String,
    active: bool,
}

#[derive(Clone, Copy, Debug)]
struct DefaultPowerPlan {
    guid: &'static str,
    name: &'static str,
    description: &'static str,
    restored_by_defaultschemes: bool,
}

#[derive(Clone, Debug)]
struct PreservedPowerPlan {
    guid: String,
    path: PathBuf,
}

#[derive(Clone, Debug)]
enum StatusKind {
    Info,
    Success,
    Error,
}

#[derive(Clone, Debug)]
struct StatusMessage {
    kind: StatusKind,
    text: String,
}

impl StatusMessage {
    fn info(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Info,
            text: text.into(),
        }
    }

    fn success(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Success,
            text: text.into(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            kind: StatusKind::Error,
            text: text.into(),
        }
    }
}

struct PowerPlanApp {
    plans: Vec<PowerPlan>,
    selected_guid: Option<String>,
    rename_text: String,
    description_text: String,
    pending_delete_guid: Option<String>,
    status: StatusMessage,
}

impl PowerPlanApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_fonts(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);

        let mut app = Self {
            plans: Vec::new(),
            selected_guid: None,
            rename_text: String::new(),
            description_text: String::new(),
            pending_delete_guid: None,
            status: StatusMessage::info("Ready"),
        };

        app.reload_with_status("Loaded power plans");
        app
    }

    fn reload(&mut self) -> Result<(), String> {
        let previous_selection = self.selected_guid.clone();
        let plans = load_power_plans()?;
        self.plans = plans;

        self.selected_guid = previous_selection
            .filter(|guid| self.plans.iter().any(|plan| &plan.guid == guid))
            .or_else(|| {
                self.plans
                    .iter()
                    .find(|plan| plan.active)
                    .map(|plan| plan.guid.clone())
            })
            .or_else(|| self.plans.first().map(|plan| plan.guid.clone()));

        self.sync_rename_text();
        Ok(())
    }

    fn reload_with_status(&mut self, success_message: impl Into<String>) {
        match self.reload() {
            Ok(()) => self.status = StatusMessage::success(success_message),
            Err(error) => self.status = StatusMessage::error(error),
        }
    }

    fn selected_plan(&self) -> Option<&PowerPlan> {
        let selected_guid = self.selected_guid.as_deref()?;
        self.plans.iter().find(|plan| plan.guid == selected_guid)
    }

    fn select_plan(&mut self, guid: String) {
        self.selected_guid = Some(guid);
        self.sync_rename_text();
    }

    fn sync_rename_text(&mut self) {
        let selected = self.selected_plan().cloned();
        self.rename_text = selected
            .as_ref()
            .map(|plan| plan.name.clone())
            .unwrap_or_default();
        self.description_text = selected.map(|plan| plan.description).unwrap_or_default();
    }

    fn import_plans(&mut self) {
        let Some(paths) = FileDialog::new()
            .add_filter("Power plan", &["pow"])
            .set_title("Import power plan")
            .pick_files()
        else {
            return;
        };

        if paths.is_empty() {
            return;
        }

        let mut imported = 0usize;
        let mut errors = Vec::new();

        for path in paths {
            match import_power_plan(&path) {
                Ok(()) => imported += 1,
                Err(error) => errors.push(format!("{}: {error}", path.display())),
            }
        }

        let _ = self.reload();

        if errors.is_empty() {
            self.status =
                StatusMessage::success(format!("Imported {imported} power plan(s) successfully"));
        } else {
            self.status = StatusMessage::error(format!(
                "Imported {imported} power plan(s); {} failed\n{}",
                errors.len(),
                errors.join("\n")
            ));
        }
    }

    fn export_selected_plan(&mut self) {
        let Some(plan) = self.selected_plan().cloned() else {
            self.status = StatusMessage::error("Select a power plan first");
            return;
        };

        let default_name = format!("{}.pow", sanitize_file_stem(&plan.name));
        let Some(path) = FileDialog::new()
            .add_filter("Power plan", &["pow"])
            .set_file_name(&default_name)
            .set_title("Export power plan")
            .save_file()
        else {
            return;
        };

        match export_power_plan(&plan.guid, &path) {
            Ok(()) => {
                self.status = StatusMessage::success(format!(
                    "Exported \"{}\" to {}",
                    plan.name,
                    path.display()
                ));
            }
            Err(error) => self.status = StatusMessage::error(error),
        }
    }

    fn export_all_plans(&mut self) {
        if self.plans.is_empty() {
            self.status = StatusMessage::error("No power plans to export");
            return;
        }

        let Some(folder) = FileDialog::new()
            .set_title("Choose export folder")
            .pick_folder()
        else {
            return;
        };

        let mut exported = 0usize;
        let mut errors = Vec::new();

        for plan in &self.plans {
            let file_name = export_file_name(plan);
            let path = folder.join(file_name);
            match export_power_plan(&plan.guid, &path) {
                Ok(()) => exported += 1,
                Err(error) => errors.push(format!("{}: {error}", plan.name)),
            }
        }

        if errors.is_empty() {
            self.status = StatusMessage::success(format!(
                "Exported {exported} power plan(s) to {}",
                folder.display()
            ));
        } else {
            self.status = StatusMessage::error(format!(
                "Exported {exported} power plan(s); {} failed\n{}",
                errors.len(),
                errors.join("\n")
            ));
        }
    }

    fn activate_selected_plan(&mut self) {
        let Some(plan) = self.selected_plan().cloned() else {
            self.status = StatusMessage::error("Select a power plan first");
            return;
        };

        match set_active_plan(&plan.guid) {
            Ok(()) => {
                let _ = self.reload();
                self.status = StatusMessage::success(format!("Activated \"{}\"", plan.name));
            }
            Err(error) => self.status = StatusMessage::error(error),
        }
    }

    fn duplicate_selected_plan(&mut self) {
        let Some(plan) = self.selected_plan().cloned() else {
            self.status = StatusMessage::error("Select a power plan first");
            return;
        };

        match duplicate_power_plan(&plan.guid) {
            Ok(new_guid) => {
                let _ = self.reload();
                if let Some(guid) = new_guid {
                    self.select_plan(guid);
                }
                self.status = StatusMessage::success(format!("Duplicated \"{}\"", plan.name));
            }
            Err(error) => self.status = StatusMessage::error(error),
        }
    }

    fn save_selected_plan_metadata(&mut self) {
        let Some(plan) = self.selected_plan().cloned() else {
            self.status = StatusMessage::error("Select a power plan first");
            return;
        };

        let new_name = self.rename_text.trim();
        if new_name.is_empty() {
            self.status = StatusMessage::error("Plan name cannot be empty");
            return;
        }

        let new_description = self.description_text.trim();

        let description_changed = new_description != plan.description.as_str();
        let description = description_changed.then_some(new_description);

        match update_power_plan_metadata(&plan.guid, new_name, description) {
            Ok(()) => {
                let _ = self.reload();
                self.status = StatusMessage::success(format!("Updated \"{}\"", plan.name));
            }
            Err(error) => self.status = StatusMessage::error(error),
        }
    }

    fn delete_selected_plan(&mut self) {
        let Some(plan) = self.selected_plan().cloned() else {
            self.status = StatusMessage::error("Select a power plan first");
            return;
        };

        if plan.active {
            self.status = StatusMessage::error("Activate another plan before deleting this one");
        } else {
            self.pending_delete_guid = Some(plan.guid);
        }
    }

    fn delete_plan(&mut self, guid: &str) {
        let Some(plan) = self.plans.iter().find(|plan| plan.guid == guid).cloned() else {
            self.status = StatusMessage::error("Power plan no longer exists");
            return;
        };

        if plan.active {
            self.status = StatusMessage::error("Activate another plan before deleting this one");
            return;
        }

        match delete_power_plan(&plan.guid) {
            Ok(()) => {
                let _ = self.reload();
                self.status = StatusMessage::success(format!("Deleted \"{}\"", plan.name));
            }
            Err(error) => self.status = StatusMessage::error(error),
        }
    }

    fn open_windows_editor(&mut self) {
        let Some(plan) = self.selected_plan().cloned() else {
            self.status = StatusMessage::error("Select a power plan first");
            return;
        };

        match open_advanced_settings_for_plan(&plan.guid) {
            Ok(()) => {
                let _ = self.reload();
                self.status =
                    StatusMessage::success(format!("Opened Windows editor for \"{}\"", plan.name));
            }
            Err(error) => self.status = StatusMessage::error(error),
        }
    }

    fn open_youtube_channel(&mut self) {
        match webbrowser::open(YOUTUBE_CHANNEL_URL) {
            Ok(()) => self.status = StatusMessage::success("Opened @fpsheaven on YouTube"),
            Err(error) => {
                self.status = StatusMessage::error(format!("Could not open YouTube: {error}"))
            }
        }
    }

    fn enable_default_windows_plans(&mut self) {
        let missing_defaults = missing_default_power_plans(&self.plans);

        if missing_defaults.is_empty() {
            self.status = StatusMessage::info("Default Windows power plans are already available");
            return;
        }

        let active_guid = self
            .plans
            .iter()
            .find(|plan| plan.active)
            .map(|plan| plan.guid.clone());

        let result = if missing_defaults
            .iter()
            .any(|plan| plan.restored_by_defaultschemes)
        {
            restore_windows_defaults_preserving_custom_plans(&self.plans, active_guid.as_deref())
        } else {
            enable_missing_duplicate_templates(&missing_defaults)
        };

        match result {
            Ok(message) => {
                let _ = self.reload();
                self.status = StatusMessage::success(message);
            }
            Err(error) => {
                let _ = self.reload();
                self.status = StatusMessage::error(error);
            }
        }
    }

    fn draw_toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh").clicked() {
                self.reload_with_status("Refreshed power plans");
            }

            if ui.button("Import").clicked() {
                self.import_plans();
            }

            if ui.button("Enable Windows Defaults").clicked() {
                self.enable_default_windows_plans();
            }

            let has_selection = self.selected_plan().is_some();
            if ui
                .add_enabled(has_selection, Button::new("Export Selected"))
                .clicked()
            {
                self.export_selected_plan();
            }

            let can_delete_selected = self
                .selected_plan()
                .map(|plan| !plan.active)
                .unwrap_or(false);
            if ui
                .add_enabled(can_delete_selected, Button::new("Delete Selected"))
                .clicked()
            {
                self.delete_selected_plan();
            }

            if ui
                .add_enabled(!self.plans.is_empty(), Button::new("Export All"))
                .clicked()
            {
                self.export_all_plans();
            }

            if ui.button("Sub on youtube!").clicked() {
                self.open_youtube_channel();
            }
        });
    }

    fn draw_plan_list(&mut self, ui: &mut Ui) {
        ui.heading("Power Plans");
        ui.add_space(6.0);

        if self.plans.is_empty() {
            ui.label("No power plans found");
            return;
        }

        let plans = self.plans.clone();
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for plan in plans {
                    let selected = self.selected_guid.as_deref() == Some(plan.guid.as_str());
                    let row = ui
                        .horizontal(|ui| {
                            let clicked = ui
                                .selectable_label(selected, RichText::new(&plan.name).strong())
                                .clicked();

                            if plan.active {
                                ui.label(
                                    RichText::new("ACTIVE")
                                        .small()
                                        .monospace()
                                        .color(Color32::from_rgb(38, 166, 91)),
                                );
                            }

                            clicked
                        })
                        .inner;

                    if row {
                        self.select_plan(plan.guid.clone());
                    }

                    ui.add_space(3.0);
                }
            });
    }

    fn draw_details(&mut self, ui: &mut Ui) {
        let Some(plan) = self.selected_plan().cloned() else {
            ui.centered_and_justified(|ui| {
                ui.label("Select a power plan");
            });
            return;
        };

        ui.horizontal(|ui| {
            ui.heading(&plan.name);
            if plan.active {
                ui.label(
                    RichText::new("ACTIVE")
                        .small()
                        .monospace()
                        .color(Color32::from_rgb(38, 166, 91)),
                );
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("GUID");
            ui.monospace(&plan.guid);
        });

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(14.0);

        ui.label(RichText::new("Name and Description").strong());
        ui.horizontal(|ui| {
            let available = (ui.available_width() - 112.0).max(160.0);
            ui.add_sized(
                Vec2::new(available, 28.0),
                TextEdit::singleline(&mut self.rename_text),
            );

            let metadata_changed = self.rename_text.trim() != plan.name.as_str()
                || self.description_text.trim() != plan.description.as_str();
            let can_save = !self.rename_text.trim().is_empty() && metadata_changed;
            if ui.add_enabled(can_save, Button::new("Save")).clicked() {
                self.save_selected_plan_metadata();
            }
        });

        ui.add_sized(
            Vec2::new(ui.available_width(), 92.0),
            TextEdit::multiline(&mut self.description_text)
                .hint_text("Description")
                .desired_rows(4),
        );

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(14.0);

        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!plan.active, Button::new("Activate"))
                .clicked()
            {
                self.activate_selected_plan();
            }

            if ui.button("Duplicate").clicked() {
                self.duplicate_selected_plan();
            }

            if ui.button("Activate & Edit").clicked() {
                self.open_windows_editor();
            }

            if ui.button("Export").clicked() {
                self.export_selected_plan();
            }

            if ui
                .add_enabled(!plan.active, Button::new("Delete"))
                .clicked()
            {
                self.delete_selected_plan();
            }
        });
    }

    fn draw_delete_dialog(&mut self, ctx: &Context) {
        let Some(guid) = self.pending_delete_guid.clone() else {
            return;
        };

        let plan_name = self
            .plans
            .iter()
            .find(|plan| plan.guid == guid)
            .map(|plan| plan.name.clone())
            .unwrap_or_else(|| "Selected plan".to_owned());

        let mut cancel = false;
        let mut delete = false;

        egui::Window::new("Delete Power Plan")
            .collapsible(false)
            .resizable(false)
            .fixed_size(Vec2::new(340.0, 136.0))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(316.0);
                ui.label(format!("Delete \"{plan_name}\"?"));
                ui.label("This removes the plan from Windows.");
                ui.add_space(14.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            Button::new(RichText::new("Delete").color(Color32::WHITE))
                                .fill(Color32::from_rgb(190, 55, 45)),
                        )
                        .clicked()
                    {
                        delete = true;
                    }

                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if delete {
            self.delete_plan(&guid);
            self.pending_delete_guid = None;
        } else if cancel {
            self.pending_delete_guid = None;
        }
    }

    fn draw_status(&self, ui: &mut Ui) {
        let color = match self.status.kind {
            StatusKind::Info => ui.visuals().text_color(),
            StatusKind::Success => Color32::from_rgb(38, 166, 91),
            StatusKind::Error => Color32::from_rgb(210, 72, 62),
        };

        ui.label(RichText::new(&self.status.text).color(color));
    }
}

impl eframe::App for PowerPlanApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("toolbar")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.add_space(6.0);
                self.draw_toolbar(ui);
                ui.add_space(6.0);
            });

        egui::Panel::bottom("status")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.add_space(6.0);
                self.draw_status(ui);
                ui.add_space(6.0);
            });

        egui::Panel::left("plans")
            .resizable(true)
            .default_size(360.0)
            .size_range(280.0..=480.0)
            .show_inside(ui, |ui| {
                ui.add_space(10.0);
                self.draw_plan_list(ui);
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.add_space(10.0);
            self.draw_details(ui);
        });

        self.draw_delete_dialog(ui.ctx());
    }
}

fn configure_fonts(ctx: &Context) {
    let Some(font_bytes) = read_windows_font("arial.ttf") else {
        return;
    };

    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "Arial".to_owned(),
        Arc::new(FontData::from_owned(font_bytes)),
    );

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        if let Some(font_names) = fonts.families.get_mut(&family) {
            font_names.retain(|font_name| font_name != "Arial");
            font_names.insert(0, "Arial".to_owned());
        }
    }

    ctx.set_fonts(fonts);
}

fn read_windows_font(file_name: &str) -> Option<Vec<u8>> {
    windows_font_paths(file_name)
        .into_iter()
        .find_map(|path| fs::read(path).ok())
}

fn windows_font_paths(file_name: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(windir) = env::var_os("WINDIR") {
        paths.push(PathBuf::from(windir).join("Fonts").join(file_name));
    }

    paths.push(PathBuf::from(r"C:\Windows\Fonts").join(file_name));
    paths
}

fn configure_style(ctx: &Context) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(16.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(16.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(15.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(22.0, FontFamily::Proportional),
    );
    style.visuals.widgets.active.corner_radius = 4.into();
    style.visuals.widgets.hovered.corner_radius = 4.into();
    style.visuals.widgets.inactive.corner_radius = 4.into();
    style.visuals.widgets.open.corner_radius = 4.into();
    style.visuals.window_corner_radius = 6.into();
    ctx.set_global_style(style);
}

fn load_power_plans() -> Result<Vec<PowerPlan>, String> {
    ensure_windows()?;

    let active_guid = active_plan_guid().ok().flatten();
    let output = run_powercfg(&[OsString::from("/list")])?;
    let mut plans = Vec::new();

    for line in output.lines() {
        let Some(guid_match) = GUID_RE.find(line) else {
            continue;
        };

        let guid = guid_match.as_str().to_ascii_lowercase();
        let name = parse_plan_name(line, guid_match.end());
        let description = read_power_plan_description(&guid).unwrap_or_default();
        let active =
            active_guid.as_deref() == Some(guid.as_str()) || line.trim_end().ends_with('*');

        plans.push(PowerPlan {
            guid,
            name,
            description,
            active,
        });
    }

    if plans.is_empty() {
        return Err("powercfg did not return any power plans".to_owned());
    }

    Ok(plans)
}

fn active_plan_guid() -> Result<Option<String>, String> {
    let output = run_powercfg(&[OsString::from("/getactivescheme")])?;
    Ok(parse_guid(&output))
}

fn missing_default_power_plans(plans: &[PowerPlan]) -> Vec<DefaultPowerPlan> {
    DEFAULT_POWER_PLANS
        .iter()
        .copied()
        .filter(|default_plan| !has_default_power_plan(plans, default_plan))
        .collect()
}

fn has_default_power_plan(plans: &[PowerPlan], default_plan: &DefaultPowerPlan) -> bool {
    plans.iter().any(|plan| {
        plan.guid.eq_ignore_ascii_case(default_plan.guid)
            || (!default_plan.restored_by_defaultschemes
                && plan.name.eq_ignore_ascii_case(default_plan.name))
    })
}

fn is_default_windows_plan(plan: &PowerPlan) -> bool {
    DEFAULT_POWER_PLANS.iter().any(|default_plan| {
        plan.guid.eq_ignore_ascii_case(default_plan.guid)
            || (!default_plan.restored_by_defaultschemes
                && plan.name.eq_ignore_ascii_case(default_plan.name))
    })
}

fn import_power_plan(path: &Path) -> Result<(), String> {
    ensure_windows()?;
    run_powercfg(&[OsString::from("/import"), path.as_os_str().to_os_string()]).map(|_| ())
}

fn import_power_plan_with_guid(path: &Path, guid: &str) -> Result<(), String> {
    ensure_windows()?;
    run_powercfg(&[
        OsString::from("/import"),
        path.as_os_str().to_os_string(),
        OsString::from(guid),
    ])
    .map(|_| ())
}

fn export_power_plan(guid: &str, path: &Path) -> Result<(), String> {
    ensure_windows()?;
    run_powercfg(&[
        OsString::from("/export"),
        path.as_os_str().to_os_string(),
        OsString::from(guid),
    ])
    .map(|_| ())
}

fn set_active_plan(guid: &str) -> Result<(), String> {
    ensure_windows()?;
    run_powercfg(&[OsString::from("/setactive"), OsString::from(guid)]).map(|_| ())
}

fn duplicate_power_plan(guid: &str) -> Result<Option<String>, String> {
    ensure_windows()?;
    let output = run_powercfg(&[OsString::from("/duplicatescheme"), OsString::from(guid)])?;
    Ok(parse_guid(&output))
}

fn duplicate_default_power_plan(default_plan: &DefaultPowerPlan) -> Result<(), String> {
    let Some(new_guid) = duplicate_power_plan(default_plan.guid)? else {
        return Ok(());
    };

    update_power_plan_metadata(&new_guid, default_plan.name, Some(default_plan.description))
}

fn enable_missing_duplicate_templates(defaults: &[DefaultPowerPlan]) -> Result<String, String> {
    let mut enabled = Vec::new();
    let mut errors = Vec::new();

    for default_plan in defaults {
        match duplicate_default_power_plan(default_plan) {
            Ok(()) => enabled.push(default_plan.name),
            Err(error) => errors.push(format!("{}: {error}", default_plan.name)),
        }
    }

    if errors.is_empty() {
        Ok(format!(
            "Enabled {} default Windows power plan(s): {}",
            enabled.len(),
            enabled.join(", ")
        ))
    } else {
        Err(format!(
            "Enabled {} default plan(s); {} failed\n{}",
            enabled.len(),
            errors.len(),
            errors.join("\n")
        ))
    }
}

fn restore_windows_defaults_preserving_custom_plans(
    plans: &[PowerPlan],
    active_guid: Option<&str>,
) -> Result<String, String> {
    let temp_dir = create_backup_dir()?;
    let preserved = match export_custom_power_plans(plans, &temp_dir) {
        Ok(preserved) => preserved,
        Err(error) => {
            cleanup_backup_dir(&temp_dir);
            return Err(error);
        }
    };

    if let Err(error) = restore_default_power_schemes() {
        cleanup_backup_dir(&temp_dir);
        return Err(error);
    }

    let mut errors = Vec::new();
    let mut imported_custom = 0usize;

    for preserved_plan in &preserved {
        match import_power_plan_with_guid(&preserved_plan.path, &preserved_plan.guid) {
            Ok(()) => imported_custom += 1,
            Err(error) => errors.push(format!(
                "Could not re-import custom plan {}: {error}",
                preserved_plan.guid
            )),
        }
    }

    let mut enabled_extra = Vec::new();
    match load_power_plans() {
        Ok(current_plans) => {
            for default_plan in missing_default_power_plans(&current_plans) {
                match duplicate_default_power_plan(&default_plan) {
                    Ok(()) => enabled_extra.push(default_plan.name),
                    Err(error) => errors.push(format!("{}: {error}", default_plan.name)),
                }
            }
        }
        Err(error) => errors.push(format!(
            "Windows defaults were restored, but the refreshed plan list could not be read: {error}"
        )),
    }

    if let Some(active_guid) = active_guid
        && let Err(error) = set_active_plan(active_guid)
    {
        errors.push(format!(
            "Could not reactivate previous active plan: {error}"
        ));
    }

    cleanup_backup_dir(&temp_dir);

    if errors.is_empty() {
        let mut message = format!(
            "Restored default Windows power plans and preserved {imported_custom} custom plan(s)"
        );
        if !enabled_extra.is_empty() {
            message.push_str(&format!("; enabled {}", enabled_extra.join(", ")));
        }
        Ok(message)
    } else {
        Err(format!(
            "Restored Windows defaults and preserved {imported_custom} custom plan(s), but {} step(s) failed\n{}",
            errors.len(),
            errors.join("\n")
        ))
    }
}

fn export_custom_power_plans(
    plans: &[PowerPlan],
    folder: &Path,
) -> Result<Vec<PreservedPowerPlan>, String> {
    let mut preserved = Vec::new();
    let mut errors = Vec::new();

    for plan in plans.iter().filter(|plan| !is_default_windows_plan(plan)) {
        let path = folder.join(format!("{}.pow", plan.guid));
        match export_power_plan(&plan.guid, &path) {
            Ok(()) => preserved.push(PreservedPowerPlan {
                guid: plan.guid.clone(),
                path,
            }),
            Err(error) => errors.push(format!("{}: {error}", plan.name)),
        }
    }

    if errors.is_empty() {
        Ok(preserved)
    } else {
        Err(format!(
            "Could not back up custom power plans, so Windows defaults were not restored\n{}",
            errors.join("\n")
        ))
    }
}

fn restore_default_power_schemes() -> Result<(), String> {
    run_powercfg(&[OsString::from("-restoredefaultschemes")])
        .or_else(|first_error| {
            run_powercfg(&[OsString::from("/restoredefaultschemes")]).map_err(|second_error| {
                format!(
                    "Could not restore Windows default power plans\n{first_error}\n{second_error}"
                )
            })
        })
        .map(|_| ())
}

fn create_backup_dir() -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let path = env::temp_dir().join(format!(
        "simple_power_plan_manager_{}_{}",
        process::id(),
        stamp
    ));

    fs::create_dir_all(&path)
        .map_err(|error| format!("Could not create temporary backup folder: {error}"))?;
    Ok(path)
}

fn cleanup_backup_dir(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn update_power_plan_metadata(
    guid: &str,
    name: &str,
    description: Option<&str>,
) -> Result<(), String> {
    ensure_windows()?;
    let mut args = vec![
        OsString::from("/changename"),
        OsString::from(guid),
        OsString::from(name),
    ];

    if let Some(description) = description {
        args.push(OsString::from(description));
    }

    run_powercfg(&args).map(|_| ())
}

fn delete_power_plan(guid: &str) -> Result<(), String> {
    ensure_windows()?;
    run_powercfg(&[OsString::from("/delete"), OsString::from(guid)]).map(|_| ())
}

fn open_advanced_settings_for_plan(guid: &str) -> Result<(), String> {
    set_active_plan(guid)?;

    let mut command = Command::new("control.exe");
    command.arg("powercfg.cpl,,3");
    hide_window(&mut command);
    command
        .spawn()
        .map_err(|error| format!("Failed to open Windows power options: {error}"))?;

    Ok(())
}

fn run_powercfg(args: &[OsString]) -> Result<String, String> {
    run_command("powercfg", args)
}

fn run_reg(args: &[OsString]) -> Result<String, String> {
    run_command("reg", args)
}

fn run_command(program: &str, args: &[OsString]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    hide_window(&mut command);

    let output = command
        .output()
        .map_err(|error| format!("Failed to run powercfg: {error}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();

    if output.status.success() {
        Ok(stdout)
    } else {
        let details = if stderr.is_empty() { stdout } else { stderr };
        Err(format!(
            "{program} {} failed: {}",
            display_args(args),
            if details.is_empty() {
                "no error details returned".to_owned()
            } else {
                details
            }
        ))
    }
}

fn read_power_plan_description(guid: &str) -> Result<String, String> {
    ensure_windows()?;
    let key = format!(r"HKLM\SYSTEM\CurrentControlSet\Control\Power\User\PowerSchemes\{guid}");
    let output = run_reg(&[
        OsString::from("query"),
        OsString::from(key),
        OsString::from("/v"),
        OsString::from("Description"),
    ])?;

    for line in output.lines() {
        if let Some(captures) = REG_DESCRIPTION_RE.captures(line) {
            let raw = captures
                .get(1)
                .map(|capture| capture.as_str().trim())
                .unwrap_or_default();
            return Ok(display_registry_description(raw));
        }
    }

    Ok(String::new())
}

#[cfg(windows)]
fn hide_window(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_window(_command: &mut Command) {}

fn ensure_windows() -> Result<(), String> {
    if cfg!(windows) {
        Ok(())
    } else {
        Err("This app manages Windows power plans and must be run on Windows".to_owned())
    }
}

fn parse_guid(text: &str) -> Option<String> {
    GUID_RE
        .find(text)
        .map(|guid| guid.as_str().to_ascii_lowercase())
}

fn parse_plan_name(line: &str, guid_end: usize) -> String {
    let after_guid = &line[guid_end..];
    let Some(open_index) = after_guid.find('(') else {
        return "Unnamed plan".to_owned();
    };

    let after_open = &after_guid[open_index + 1..];
    let Some(close_index) = after_open.rfind(')') else {
        return "Unnamed plan".to_owned();
    };

    let name = after_open[..close_index].trim();
    if name.is_empty() {
        "Unnamed plan".to_owned()
    } else {
        name.to_owned()
    }
}

fn display_registry_description(raw: &str) -> String {
    if raw.starts_with('@') {
        let mut parts = raw.splitn(3, ',');
        let _resource_path = parts.next();
        let _resource_id = parts.next();
        if let Some(description) = parts.next() {
            return description.trim().to_owned();
        }
    }

    raw.to_owned()
}

fn sanitize_file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect();

    let trimmed = cleaned.trim_matches(['.', ' ']).trim();
    if trimmed.is_empty() {
        "power-plan".to_owned()
    } else {
        trimmed.chars().take(80).collect()
    }
}

fn export_file_name(plan: &PowerPlan) -> String {
    let stem = sanitize_file_stem(&plan.name);
    let guid_part = plan.guid.get(..8).unwrap_or(plan.guid.as_str());
    format!("{stem}_{guid_part}.pow")
}

fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_guid_from_powercfg_output() {
        let text = "Power Scheme GUID: 381b4222-f694-41f0-9685-ff5bb260df2e  (Balanced) *";

        assert_eq!(
            parse_guid(text).as_deref(),
            Some("381b4222-f694-41f0-9685-ff5bb260df2e")
        );
    }

    #[test]
    fn parses_plan_name_with_parentheses() {
        let text = "Power Scheme GUID: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee  (Gaming (Quiet)) *";
        let guid_end = GUID_RE.find(text).unwrap().end();

        assert_eq!(parse_plan_name(text, guid_end), "Gaming (Quiet)");
    }

    #[test]
    fn extracts_display_text_from_registry_resource_description() {
        assert_eq!(
            display_registry_description(
                r"@C:\Windows\system32\powrprof.dll,-14,Automatically balances performance"
            ),
            "Automatically balances performance"
        );
        assert_eq!(
            display_registry_description("Plain imported plan description"),
            "Plain imported plan description"
        );
    }

    #[test]
    fn detects_missing_default_power_plans_by_guid() {
        let plans = vec![PowerPlan {
            guid: "381b4222-f694-41f0-9685-ff5bb260df2e".to_owned(),
            name: "Balanced".to_owned(),
            description: String::new(),
            active: false,
        }];

        let missing = missing_default_power_plans(&plans);

        assert!(missing.iter().any(|plan| plan.name == "High performance"));
        assert!(!missing.iter().any(|plan| plan.name == "Balanced"));
    }

    #[test]
    fn treats_named_ultimate_performance_as_available() {
        let plans = vec![PowerPlan {
            guid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            name: "Ultimate Performance".to_owned(),
            description: String::new(),
            active: false,
        }];

        let missing = missing_default_power_plans(&plans);

        assert!(
            !missing
                .iter()
                .any(|plan| plan.name == "Ultimate Performance")
        );
    }

    #[test]
    fn sanitizes_windows_file_names() {
        assert_eq!(sanitize_file_stem("Ultra<Fast>:Plan?"), "Ultra_Fast__Plan_");
        assert_eq!(sanitize_file_stem("...   "), "power-plan");
    }
}
