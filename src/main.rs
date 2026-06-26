#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        Arc, LazyLock,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
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
const FPSHEAVEN_POWER_PLANS_URL: &str =
    "https://fpsheaven.com/wp-content/uploads/2026/06/fpsheaven_powerplans.zip";
const FPSHEAVEN_POWER_PLANS_ZIP_FILE: &str = "fpsheaven_powerplans.zip";
const FPSHEAVEN_POWER_PLANS_FOLDER: &str = "FPSHEAVEN Power Plans";
const APP_DATA_FOLDER: &str = "Simple Power Plan Manager";
const FPSHEAVEN_DOWNLOADS_FOLDER: &str = "FPSHEAVEN Downloads";
const TOOLBAR_BUTTON_HEIGHT: f32 = 30.0;
const FPSHEAVEN_IMPORT_BUSY_MESSAGE: &str =
    "FPSHEAVEN power plan import is running; wait for it to finish";

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

#[derive(Clone, Copy, Debug)]
enum FpsheavenPowerPlanKind {
    Intel,
    Amd,
}

impl FpsheavenPowerPlanKind {
    fn label(self) -> &'static str {
        match self {
            Self::Intel => "INTEL",
            Self::Amd => "AMD",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Intel => "fpsheaven2026_intel.pow",
            Self::Amd => "fpsheaven2026_amd.pow",
        }
    }

    fn file_needle(self) -> &'static str {
        match self {
            Self::Intel => "intel",
            Self::Amd => "amd",
        }
    }
}

#[derive(Clone, Debug)]
struct FpsheavenImportResult {
    guid: String,
    plan_path: PathBuf,
    replaced_active_plan_name: Option<String>,
}

struct FpsheavenImportJob {
    plan_kind: FpsheavenPowerPlanKind,
    receiver: Receiver<Result<FpsheavenImportResult, String>>,
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
    pending_fpsheaven_import: bool,
    pending_default_reset_error: Option<String>,
    fpsheaven_import_job: Option<FpsheavenImportJob>,
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
            pending_fpsheaven_import: false,
            pending_default_reset_error: None,
            fpsheaven_import_job: None,
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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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

    fn fpsheaven_import_running(&self) -> bool {
        self.fpsheaven_import_job.is_some()
    }

    fn block_if_fpsheaven_import_running(&mut self) -> bool {
        if self.fpsheaven_import_running() {
            self.status = StatusMessage::info(FPSHEAVEN_IMPORT_BUSY_MESSAGE);
            true
        } else {
            false
        }
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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

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
        if self.block_if_fpsheaven_import_running() {
            return;
        }

        let missing_defaults = missing_default_power_plans(&self.plans);

        if missing_defaults.is_empty() {
            self.status = StatusMessage::info("Default Windows power plans are already available");
            return;
        }

        let has_custom_plans = self.plans.iter().any(|plan| !is_default_windows_plan(plan));

        let result = match enable_missing_duplicate_templates(&missing_defaults) {
            Ok(message) => Ok(message),
            Err(error) if has_custom_plans => {
                self.pending_default_reset_error = Some(error);
                self.status = StatusMessage::info(
                    "A full Windows power plan reset is required to restore those defaults",
                );
                return;
            }
            Err(_error) => {
                let active_guid = self
                    .plans
                    .iter()
                    .find(|plan| plan.active)
                    .map(|plan| plan.guid.clone());
                restore_windows_defaults_preserving_custom_plans(
                    &self.plans,
                    active_guid.as_deref(),
                )
            }
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

    fn reset_default_windows_plans_anyway(&mut self) {
        if self.block_if_fpsheaven_import_running() {
            return;
        }

        if let Err(error) = self.reload() {
            self.status = StatusMessage::error(error);
            return;
        }

        let active_guid = self
            .plans
            .iter()
            .find(|plan| plan.active)
            .map(|plan| plan.guid.clone());

        match restore_windows_defaults_preserving_custom_plans(&self.plans, active_guid.as_deref())
        {
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

    fn start_fpsheaven_power_plan_import(&mut self, plan_kind: FpsheavenPowerPlanKind) {
        if self.fpsheaven_import_job.is_some() {
            self.status = StatusMessage::info("FPSHEAVEN power plan import is already running");
            return;
        }

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(download_extract_and_import_fpsheaven_power_plan(plan_kind));
        });

        self.fpsheaven_import_job = Some(FpsheavenImportJob {
            plan_kind,
            receiver,
        });
        self.status = StatusMessage::info(format!(
            "Downloading FPSHEAVEN {} power plan...",
            plan_kind.label()
        ));
    }

    fn poll_fpsheaven_import(&mut self, ctx: &Context) {
        let Some(job) = self.fpsheaven_import_job.as_ref() else {
            return;
        };

        match job.receiver.try_recv() {
            Ok(result) => {
                let plan_kind = job.plan_kind;
                self.fpsheaven_import_job = None;
                self.finish_fpsheaven_power_plan_import(plan_kind, result);
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            Err(TryRecvError::Disconnected) => {
                self.fpsheaven_import_job = None;
                self.status =
                    StatusMessage::error("FPSHEAVEN power plan import stopped unexpectedly");
            }
        }
    }

    fn finish_fpsheaven_power_plan_import(
        &mut self,
        plan_kind: FpsheavenPowerPlanKind,
        result: Result<FpsheavenImportResult, String>,
    ) {
        match result {
            Ok(result) => {
                let _ = self.reload();
                if self.plans.iter().any(|plan| plan.guid == result.guid) {
                    self.select_plan(result.guid);
                }

                let mut message = format!(
                    "Imported and activated FPSHEAVEN {} power plan from {}",
                    plan_kind.label(),
                    result.plan_path.display()
                );
                if let Some(replaced_plan_name) = result.replaced_active_plan_name {
                    message.push_str(&format!("; replaced \"{replaced_plan_name}\""));
                }
                self.status = StatusMessage::success(message);
            }
            Err(error) => self.status = StatusMessage::error(error),
        }
    }

    fn draw_toolbar(&mut self, ui: &mut Ui) {
        let has_selection = self.selected_plan().is_some();
        let can_delete_selected = self
            .selected_plan()
            .map(|plan| !plan.active)
            .unwrap_or(false);
        let fpsheaven_import_running = self.fpsheaven_import_running();
        let can_run_power_plan_action = !fpsheaven_import_running;

        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);

        ui.horizontal_wrapped(|ui| {
            if toolbar_enabled_button(ui, can_run_power_plan_action, "Refresh", 86.0).clicked() {
                self.reload_with_status("Refreshed power plans");
            }

            if toolbar_enabled_button(ui, can_run_power_plan_action, "Restore Defaults", 132.0)
                .clicked()
            {
                self.enable_default_windows_plans();
            }

            toolbar_separator(ui);

            if toolbar_enabled_button(ui, can_run_power_plan_action, "Import", 86.0).clicked() {
                self.import_plans();
            }

            if toolbar_enabled_button(
                ui,
                can_run_power_plan_action && !self.plans.is_empty(),
                "Export All",
                104.0,
            )
            .clicked()
            {
                self.export_all_plans();
            }

            toolbar_separator(ui);

            if toolbar_enabled_button(
                ui,
                can_run_power_plan_action,
                "Download FPSHEAVEN's power plan",
                270.0,
            )
            .clicked()
            {
                self.pending_fpsheaven_import = true;
            }

            if toolbar_button(ui, "YouTube", 92.0).clicked() {
                self.open_youtube_channel();
            }
        });

        ui.add_space(2.0);

        ui.horizontal_wrapped(|ui| {
            toolbar_label(ui, "Selected");

            if toolbar_enabled_button(
                ui,
                can_run_power_plan_action && has_selection,
                "Export",
                92.0,
            )
            .clicked()
            {
                self.export_selected_plan();
            }

            if toolbar_enabled_button(
                ui,
                can_run_power_plan_action && can_delete_selected,
                "Delete",
                92.0,
            )
            .clicked()
            {
                self.delete_selected_plan();
            }
        });
    }

    fn draw_fpsheaven_import_dialog(&mut self, ctx: &Context) {
        if !self.pending_fpsheaven_import {
            return;
        }

        let mut selected_kind = None;
        let mut cancel = false;

        egui::Window::new("Download FPSHEAVEN's Power Plan")
            .collapsible(false)
            .resizable(false)
            .fixed_size(Vec2::new(376.0, 132.0))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(352.0);
                ui.label("Hi, import INTEL or AMD?");
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if ui.button("INTEL").clicked() {
                        selected_kind = Some(FpsheavenPowerPlanKind::Intel);
                    }

                    if ui.button("AMD").clicked() {
                        selected_kind = Some(FpsheavenPowerPlanKind::Amd);
                    }

                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if let Some(plan_kind) = selected_kind {
            self.pending_fpsheaven_import = false;
            self.start_fpsheaven_power_plan_import(plan_kind);
        } else if cancel {
            self.pending_fpsheaven_import = false;
        }
    }

    fn draw_default_reset_dialog(&mut self, ctx: &Context) {
        let Some(error) = self.pending_default_reset_error.clone() else {
            return;
        };

        let can_run_power_plan_action = !self.fpsheaven_import_running();
        let mut cancel = false;
        let mut reset = false;

        egui::Window::new("Reset Windows Power Plans")
            .collapsible(false)
            .resizable(false)
            .fixed_size(Vec2::new(460.0, 234.0))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(436.0);
                ui.label("Windows needs a full power-plan reset to restore the missing defaults.");
                ui.label("Custom plans will be exported first, then re-imported after the reset.");
                ui.label("If any re-import fails, the backup folder will be kept and shown.");
                ui.add_space(12.0);
                ui.label(RichText::new("Original error").small().strong());
                ui.label(RichText::new(error).small());
                ui.add_space(14.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            can_run_power_plan_action,
                            Button::new(RichText::new("Reset Anyway").color(Color32::WHITE))
                                .fill(Color32::from_rgb(190, 55, 45)),
                        )
                        .clicked()
                    {
                        reset = true;
                    }

                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if reset {
            self.pending_default_reset_error = None;
            self.reset_default_windows_plans_anyway();
        } else if cancel {
            self.pending_default_reset_error = None;
            self.status = StatusMessage::info("Windows default reset canceled");
        }
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
        let can_run_power_plan_action = !self.fpsheaven_import_running();

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
            let can_save = can_run_power_plan_action
                && !self.rename_text.trim().is_empty()
                && metadata_changed;
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
                .add_enabled(
                    can_run_power_plan_action && !plan.active,
                    Button::new("Activate"),
                )
                .clicked()
            {
                self.activate_selected_plan();
            }

            if ui
                .add_enabled(can_run_power_plan_action, Button::new("Duplicate"))
                .clicked()
            {
                self.duplicate_selected_plan();
            }

            if ui
                .add_enabled(can_run_power_plan_action, Button::new("Activate & Edit"))
                .clicked()
            {
                self.open_windows_editor();
            }

            if ui
                .add_enabled(can_run_power_plan_action, Button::new("Export"))
                .clicked()
            {
                self.export_selected_plan();
            }

            if ui
                .add_enabled(
                    can_run_power_plan_action && !plan.active,
                    Button::new("Delete"),
                )
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

        let can_run_power_plan_action = !self.fpsheaven_import_running();
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
                        .add_enabled(
                            can_run_power_plan_action,
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

fn toolbar_button(ui: &mut Ui, label: &str, width: f32) -> egui::Response {
    ui.add(Button::new(label).min_size(Vec2::new(width, TOOLBAR_BUTTON_HEIGHT)))
}

fn toolbar_enabled_button(ui: &mut Ui, enabled: bool, label: &str, width: f32) -> egui::Response {
    ui.add_enabled(
        enabled,
        Button::new(label).min_size(Vec2::new(width, TOOLBAR_BUTTON_HEIGHT)),
    )
}

fn toolbar_label(ui: &mut Ui, label: &str) {
    ui.allocate_ui_with_layout(
        Vec2::new(74.0, TOOLBAR_BUTTON_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.label(RichText::new(label).small().strong());
        },
    );
}

fn toolbar_separator(ui: &mut Ui) {
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
}

impl eframe::App for PowerPlanApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.poll_fpsheaven_import(ui.ctx());

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

        self.draw_fpsheaven_import_dialog(ui.ctx());
        self.draw_default_reset_dialog(ui.ctx());
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
        let description = read_power_plan_description(&guid, &name).unwrap_or_default();
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
    run_powercfg(&import_power_plan_args(path, None)).map(|_| ())
}

fn import_power_plan_with_guid(path: &Path, guid: &str) -> Result<(), String> {
    ensure_windows()?;
    run_powercfg(&import_power_plan_args(path, Some(guid))).map(|_| ())
}

fn import_power_plan_args(path: &Path, guid: Option<&str>) -> Vec<OsString> {
    let mut args = vec![OsString::from("/import"), path.as_os_str().to_os_string()];
    if let Some(guid) = guid {
        args.push(OsString::from(guid));
    }
    args
}

fn download_extract_and_import_fpsheaven_power_plan(
    plan_kind: FpsheavenPowerPlanKind,
) -> Result<FpsheavenImportResult, String> {
    ensure_windows()?;

    let download_dir = fpsheaven_download_dir()?;
    let zip_path = download_dir.join(FPSHEAVEN_POWER_PLANS_ZIP_FILE);
    let extract_dir = download_dir.join(FPSHEAVEN_POWER_PLANS_FOLDER);

    download_file(FPSHEAVEN_POWER_PLANS_URL, &zip_path)?;
    extract_zip(&zip_path, &extract_dir)?;

    let plan_path = find_fpsheaven_power_plan(&extract_dir, plan_kind)?;
    let replaced_active_plan_name = replace_active_fpsheaven_plan_if_needed()?;
    let guid = new_power_plan_guid()?;

    import_power_plan_with_guid(&plan_path, &guid)?;
    set_active_plan(&guid)?;
    let _ = fs::remove_file(&zip_path);

    Ok(FpsheavenImportResult {
        guid,
        plan_path,
        replaced_active_plan_name,
    })
}

fn fpsheaven_download_dir() -> Result<PathBuf, String> {
    let base_dir = fpsheaven_download_base_dir();
    fpsheaven_download_dir_under(&base_dir)
}

fn fpsheaven_download_base_dir() -> PathBuf {
    fpsheaven_download_base_dir_from(env::var_os("LOCALAPPDATA"))
}

fn fpsheaven_download_base_dir_from(local_app_data: Option<OsString>) -> PathBuf {
    local_app_data
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

fn fpsheaven_download_dir_under(base_dir: &Path) -> Result<PathBuf, String> {
    let path = base_dir
        .join(APP_DATA_FOLDER)
        .join(FPSHEAVEN_DOWNLOADS_FOLDER);

    fs::create_dir_all(&path)
        .map_err(|error| format!("Could not create FPSHEAVEN download folder: {error}"))?;
    Ok(path)
}

fn download_file(url: &str, destination: &Path) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create download folder: {error}"))?;
    }

    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
         Invoke-WebRequest -Uri {} -OutFile {} -UseBasicParsing",
        ps_single_quote(url),
        ps_single_quote_path(destination)
    );

    run_powershell_script(&script).map(|_| ())
}

fn extract_zip(zip_path: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        fs::remove_dir_all(destination)
            .map_err(|error| format!("Could not clear old extract folder: {error}"))?;
    }

    fs::create_dir_all(destination)
        .map_err(|error| format!("Could not create extract folder: {error}"))?;

    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         Expand-Archive -LiteralPath {} -DestinationPath {} -Force",
        ps_single_quote_path(zip_path),
        ps_single_quote_path(destination)
    );

    run_powershell_script(&script).map(|_| ())
}

fn find_fpsheaven_power_plan(
    folder: &Path,
    plan_kind: FpsheavenPowerPlanKind,
) -> Result<PathBuf, String> {
    let mut power_plans = Vec::new();
    collect_power_plan_files(folder, &mut power_plans)?;
    power_plans.sort();

    if let Some(path) = power_plans.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.eq_ignore_ascii_case(plan_kind.file_name()))
            .unwrap_or(false)
    }) {
        return Ok(path.clone());
    }

    power_plans
        .into_iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_ascii_lowercase().contains(plan_kind.file_needle()))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            format!(
                "Could not find a FPSHEAVEN {} .pow file in {}",
                plan_kind.label(),
                folder.display()
            )
        })
}

fn collect_power_plan_files(folder: &Path, power_plans: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(folder).map_err(|error| {
        format!(
            "Could not read extracted folder {}: {error}",
            folder.display()
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Could not read an extracted file in {}: {error}",
                folder.display()
            )
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;

        if file_type.is_dir() {
            collect_power_plan_files(&path, power_plans)?;
        } else if is_power_plan_file(&path) {
            power_plans.push(path);
        }
    }

    Ok(())
}

fn is_power_plan_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("pow"))
        .unwrap_or(false)
}

fn replace_active_fpsheaven_plan_if_needed() -> Result<Option<String>, String> {
    let plans = load_power_plans()?;
    let Some(active_plan) = plans
        .iter()
        .find(|plan| plan.active && is_fpsheaven_power_plan(plan))
        .cloned()
    else {
        return Ok(None);
    };

    let replacement =
        replacement_plan_for_deleting_active(&plans, &active_plan.guid).ok_or_else(|| {
            "The active FPSHEAVEN plan is the only available power plan, so it cannot be deleted"
                .to_owned()
        })?;

    set_active_plan(&replacement.guid)?;
    delete_power_plan(&active_plan.guid)?;

    Ok(Some(active_plan.name))
}

fn replacement_plan_for_deleting_active<'a>(
    plans: &'a [PowerPlan],
    active_guid: &str,
) -> Option<&'a PowerPlan> {
    plans
        .iter()
        .find(|plan| plan.guid != active_guid && !is_fpsheaven_power_plan(plan))
        .or_else(|| plans.iter().find(|plan| plan.guid != active_guid))
}

fn is_fpsheaven_power_plan(plan: &PowerPlan) -> bool {
    contains_ascii_case_insensitive(&plan.name, "fpsheaven")
        || contains_ascii_case_insensitive(&plan.description, "fpsheaven")
}

fn contains_ascii_case_insensitive(text: &str, needle: &str) -> bool {
    text.to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn new_power_plan_guid() -> Result<String, String> {
    let output = run_powershell_script("[guid]::NewGuid().ToString()")?;
    let guid = output.trim().to_ascii_lowercase();

    if parse_guid(&guid).as_deref() == Some(guid.as_str()) {
        Ok(guid)
    } else {
        Err(format!("Windows returned an invalid GUID: {output}"))
    }
}

#[cfg(test)]
fn generate_power_plan_guid() -> String {
    let mut value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
        ^ ((process::id() as u128) << 64);

    value &= !(0xf_u128 << 76);
    value |= 0x4_u128 << 76;
    value &= !(0x3_u128 << 62);
    value |= 0x2_u128 << 62;

    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (value >> 96) as u32,
        ((value >> 80) & 0xffff) as u16,
        ((value >> 64) & 0xffff) as u16,
        ((value >> 48) & 0xffff) as u16,
        value & 0xffff_ffff_ffff
    )
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
        return Err(format!(
            "{error}\nCustom-plan backups were kept in {}",
            temp_dir.display()
        ));
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

    if errors.is_empty() {
        cleanup_backup_dir(&temp_dir);

        let mut message = format!(
            "Restored default Windows power plans and preserved {imported_custom} custom plan(s)"
        );
        if !enabled_extra.is_empty() {
            message.push_str(&format!("; enabled {}", enabled_extra.join(", ")));
        }
        Ok(message)
    } else {
        Err(format!(
            "Restored Windows defaults and re-imported {imported_custom} custom plan(s), but {} step(s) failed\n{}\nCustom-plan backups were kept in {}",
            errors.len(),
            errors.join("\n"),
            temp_dir.display()
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

fn run_powershell_script(script: &str) -> Result<String, String> {
    run_command(
        "powershell.exe",
        &[
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-ExecutionPolicy"),
            OsString::from("Bypass"),
            OsString::from("-Command"),
            OsString::from(script),
        ],
    )
}

fn run_command(program: &str, args: &[OsString]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    hide_window(&mut command);

    let output = command
        .output()
        .map_err(|error| format!("Failed to run {program}: {error}"))?;

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

fn ps_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn ps_single_quote_path(path: &Path) -> String {
    ps_single_quote(&path.to_string_lossy())
}

fn read_power_plan_description(guid: &str, name: &str) -> Result<String, String> {
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
            return Ok(display_power_plan_description(guid, name, raw));
        }
    }

    Ok(default_power_plan_description(guid, name).unwrap_or_default())
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

fn display_power_plan_description(guid: &str, name: &str, raw: &str) -> String {
    let description = display_registry_description(raw);
    if is_registry_resource_reference(&description) {
        default_power_plan_description(guid, name).unwrap_or_default()
    } else {
        description
    }
}

fn is_registry_resource_reference(text: &str) -> bool {
    text.trim_start().starts_with('@')
}

fn default_power_plan_description(guid: &str, name: &str) -> Option<String> {
    DEFAULT_POWER_PLANS
        .iter()
        .find(|plan| guid.eq_ignore_ascii_case(plan.guid) || name.eq_ignore_ascii_case(plan.name))
        .map(|plan| plan.description.to_owned())
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
    fn uses_default_description_for_registry_resource_reference() {
        assert_eq!(
            display_power_plan_description(
                "381b4222-f694-41f0-9685-ff5bb260df2e",
                "Balanced",
                r"@%SystemRoot%\system32\powrprof.dll,-13"
            ),
            "Automatically balances performance with energy consumption on capable hardware."
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

    #[test]
    fn fpsheaven_download_folder_names_are_ascii() {
        assert!(APP_DATA_FOLDER.is_ascii());
        assert!(FPSHEAVEN_DOWNLOADS_FOLDER.is_ascii());
        assert!(FPSHEAVEN_POWER_PLANS_FOLDER.is_ascii());
        assert!(FPSHEAVEN_POWER_PLANS_ZIP_FILE.is_ascii());
    }

    #[test]
    fn uses_local_app_data_when_available() {
        let local_app_data = OsString::from("C:\\Users\\Player\\AppData\\Local");

        assert_eq!(
            fpsheaven_download_base_dir_from(Some(local_app_data.clone())),
            PathBuf::from(local_app_data)
        );
    }

    #[test]
    fn falls_back_to_temp_when_local_app_data_is_missing() {
        assert_eq!(fpsheaven_download_base_dir_from(None), env::temp_dir());
    }

    #[test]
    fn creates_fpsheaven_download_dir_under_unicode_base_path() {
        let root = env::temp_dir().join(format!(
            "pp_ui_unicode_path_test_{}_{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let base = root.join("Ren\u{00e9}e A\u{00e7}\u{00e3}o \u{5c71}\u{7530} O'Neil");

        let path = fpsheaven_download_dir_under(&base).unwrap();

        assert_eq!(
            path,
            base.join(APP_DATA_FOLDER).join(FPSHEAVEN_DOWNLOADS_FOLDER)
        );
        assert!(path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn powershell_quotes_unicode_paths_and_apostrophes() {
        let path = PathBuf::from(concat!(
            "C:\\Users\\Ren\u{00e9}e A\u{00e7}\u{00e3}o \u{5c71}\u{7530} O'Neil",
            "\\AppData\\Local\\Simple Power Plan Manager\\FPSHEAVEN Downloads",
            "\\fpsheaven_powerplans.zip"
        ));

        assert_eq!(
            ps_single_quote_path(&path),
            concat!(
                "'C:\\Users\\Ren\u{00e9}e A\u{00e7}\u{00e3}o \u{5c71}\u{7530} O''Neil",
                "\\AppData\\Local\\Simple Power Plan Manager\\FPSHEAVEN Downloads",
                "\\fpsheaven_powerplans.zip'"
            )
        );
    }

    #[test]
    fn imports_power_plan_from_unicode_fallback_extract_path_as_native_arg() {
        let root = env::temp_dir().join(format!(
            "pp_ui_import_fallback_test_{}_{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let base = root.join("Temp \u{5c71}\u{7530} O'Neil");
        let download_dir = fpsheaven_download_dir_under(&base).unwrap();
        let extract_dir = download_dir.join(FPSHEAVEN_POWER_PLANS_FOLDER);
        let nested_dir = extract_dir.join("nested");
        let plan_path = nested_dir.join("fpsheaven2026_intel.pow");

        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(&plan_path, b"").unwrap();

        let found_path =
            find_fpsheaven_power_plan(&extract_dir, FpsheavenPowerPlanKind::Intel).unwrap();
        let guid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let args = import_power_plan_args(&found_path, Some(guid));

        assert_eq!(found_path, plan_path);
        assert_eq!(args[0], OsString::from("/import"));
        assert_eq!(args[1], plan_path.as_os_str().to_os_string());
        assert_eq!(args[2], OsString::from(guid));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn finds_exact_fpsheaven_power_plan_file() {
        let folder = env::temp_dir().join(format!(
            "pp_ui_fpsheaven_test_{}_{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));

        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("FPSHEAVEN2026_AMD.pow"), b"").unwrap();
        fs::write(folder.join("FPSHEAVEN2026_INTEL.pow"), b"").unwrap();

        let path = find_fpsheaven_power_plan(&folder, FpsheavenPowerPlanKind::Intel).unwrap();

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("FPSHEAVEN2026_INTEL.pow")
        );

        let _ = fs::remove_dir_all(folder);
    }

    #[test]
    fn fpsheaven_file_fallback_only_checks_file_name() {
        let folder = env::temp_dir().join(format!(
            "pp_ui_intel_parent_test_{}_{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));

        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("FPSHEAVEN2026_AMD_CUSTOM.pow"), b"").unwrap();

        assert!(find_fpsheaven_power_plan(&folder, FpsheavenPowerPlanKind::Intel).is_err());
        assert!(find_fpsheaven_power_plan(&folder, FpsheavenPowerPlanKind::Amd).is_ok());

        let _ = fs::remove_dir_all(folder);
    }

    #[test]
    fn detects_fpsheaven_power_plans_by_name_or_description() {
        let named_plan = PowerPlan {
            guid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            name: "FPSHEAVEN2026_AMD".to_owned(),
            description: String::new(),
            active: true,
        };
        let described_plan = PowerPlan {
            guid: "bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            name: "Gaming".to_owned(),
            description: "Created by fpsheaven".to_owned(),
            active: false,
        };

        assert!(is_fpsheaven_power_plan(&named_plan));
        assert!(is_fpsheaven_power_plan(&described_plan));
    }

    #[test]
    fn prefers_non_fpsheaven_replacement_plan() {
        let plans = vec![
            PowerPlan {
                guid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                name: "FPSHEAVEN2026_AMD".to_owned(),
                description: String::new(),
                active: true,
            },
            PowerPlan {
                guid: "bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                name: "FPSHEAVEN2026_INTEL".to_owned(),
                description: String::new(),
                active: false,
            },
            PowerPlan {
                guid: "cccccccc-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                name: "Balanced".to_owned(),
                description: String::new(),
                active: false,
            },
        ];

        let replacement =
            replacement_plan_for_deleting_active(&plans, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                .unwrap();

        assert_eq!(replacement.name, "Balanced");
    }

    #[test]
    fn generates_valid_power_plan_guid() {
        let guid = generate_power_plan_guid();

        assert_eq!(parse_guid(&guid).as_deref(), Some(guid.as_str()));
    }
}
