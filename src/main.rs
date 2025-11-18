#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::collections::VecDeque;
use std::fmt::{Display, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::{Event, Level, Subscriber};
use tracing_appender::rolling::Rotation;
use tracing_subscriber::layer::Context as TracingContext;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, reload};

use crate::player_data::ExportSettings;

mod admin;
mod app;
mod capture;
mod good;
mod monitor;
mod player_data;
mod update;
mod wish;

const APP_ID: &str = "Irminsul";

#[derive(Clone, Copy, Debug)]
pub enum ConfirmationType {
    Initial,
    Update,
}

#[derive(Clone, Debug)]
pub enum State {
    Starting,
    CheckingForUpdate,
    WaitingForUpdateConfirmation(String),
    Updating,
    Updated,
    CheckingForData,
    WaitingForDownloadConfirmation(ConfirmationType),
    Downloading,
    Main,
}

#[derive(Debug)]
pub enum Message {
    UpdateAcknowledged,
    UpdateCanceled,
    DownloadAcknowledged,
    StartCapture,
    StopCapture,
    ExportGenshinOptimizer(ExportSettings, oneshot::Sender<Result<String>>),
}

#[derive(Clone, Debug)]
pub struct DataUpdated {
    achievements_updated: Option<String>,
    characters_updated: Option<String>,
    items_updated: Option<String>,
}

impl DataUpdated {
    pub fn new() -> Self {
        Self {
            achievements_updated: None,
            characters_updated: None,
            items_updated: None,
        }
    }
}

impl Default for DataUpdated {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct AppState {
    state: State,
    capturing: bool,
    updated: DataUpdated,
}

impl AppState {
    fn new() -> Self {
        AppState {
            state: State::Starting,
            capturing: false,
            updated: DataUpdated::new(),
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, default_value_t = false)]
    no_admin: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, Default)]
pub enum TracingLevel {
    #[default]
    Default,
    VerboseInfo,
    VerboseDebug,
    VerboseTrace,
}

impl Display for TracingLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TracingLevel::Default => write!(f, "Default"),
            TracingLevel::VerboseInfo => write!(f, "Verbose Info"),
            TracingLevel::VerboseDebug => write!(f, "Verbose Debug"),
            TracingLevel::VerboseTrace => write!(f, "Verbose Trace"),
        }
    }
}

impl TracingLevel {
    fn get_filter(&self) -> &'static str {
        match self {
            TracingLevel::Default => {
                if cfg!(debug_assertions) {
                    "info"
                } else {
                    "warn,irminsul=info"
                }
            }
            TracingLevel::VerboseInfo => "info",
            TracingLevel::VerboseDebug => "debug",
            TracingLevel::VerboseTrace => "trace",
        }
    }
}

struct ReloadHandle(reload::Handle<EnvFilter, tracing_subscriber::Registry>);

impl ReloadHandle {
    pub fn set_filter(&mut self, filter: &str) {
        if let Err(e) = self.0.reload(filter) {
            tracing::warn!("Failed to set tracing filter to \"{filter}\": {e}");
        }
        tracing::info!("Set tracing filter to \"{filter}\"");
    }
}

fn main() -> eframe::Result {
    let log_buffer = LogLL::new(500, 6);
    let (_guard, reload_handle) = tracing_init(log_buffer.clone()).unwrap();

    let args = Args::parse();

    if !args.no_admin {
        #[cfg(windows)]
        admin::ensure_admin();
    }

    let background_image_size = [1600., 1000.];

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(background_image_size.map(|v| v * 0.5))
            .with_resizable(false)
            .with_decorations(false)
            .with_icon(
                // NOTE: Adding an icon is optional
                eframe::icon_data::from_png_bytes(&include_bytes!("../assets/icon-256.png")[..])
                    .expect("Failed to load icon"),
            ),
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "Irminsul",
        native_options,
        Box::new(|cc| {
            Ok(Box::new(app::IrminsulApp::new(
                cc,
                reload_handle,
                log_buffer.clone(),
            )))
        }),
    )
}

fn log_dir() -> Result<PathBuf> {
    let mut dir = eframe::storage_dir(APP_ID).context("Storage dir not found")?;
    dir.push("log");
    Ok(dir)
}

fn open_log_dir() -> Result<()> {
    let dir = log_dir()?;
    open::that(dir)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Log {
    pub level: Level,
    pub message: String,
}

pub struct LogLL {
    buffer: Mutex<VecDeque<Log>>,
    min_size: usize,
    capacity: usize,
}

impl LogLL {
    pub fn new(capacity: usize, min_size: usize) -> Arc<Self> {
        Arc::new(Self {
            buffer: Mutex::new(VecDeque::with_capacity(capacity)),
            min_size,
            capacity,
        })
    }

    pub fn push(&self, entry: Log) {
        let mut buffer = self.buffer.lock().unwrap();
        if buffer.len() == self.capacity {
            buffer.pop_front();
        }
        buffer.push_back(entry);
    }

    pub fn get_entries(&self) -> Vec<Log> {
        let mut copy: Vec<Log> = self
            .buffer
            .lock()
            .unwrap()
            .iter()
            // .filter(|l| l.level == Level::ERROR)
            .cloned()
            .collect();
        if copy.len() < self.min_size {
            copy.resize(
                self.min_size,
                Log {
                    level: Level::ERROR,
                    message: "".to_string(),
                },
            );
        }
        return copy;
    }
}

pub struct GuiLayer {
    log_buffer: Arc<LogLL>,
}

impl GuiLayer {
    pub fn new(log_buffer: Arc<LogLL>) -> Self {
        Self { log_buffer }
    }
}

impl<S> Layer<S> for GuiLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: TracingContext<'_, S>) {
        if event.metadata().level() >= &Level::INFO {
            let mut visitor = CustomVisitor::new();
            event.record(&mut visitor);

            let entry = Log {
                level: *event.metadata().level(),
                message: visitor.message,
            };
            self.log_buffer.push(entry);
        }
    }
}

struct CustomVisitor {
    message: String,
}

impl CustomVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
        }
    }
}

impl tracing::field::Visit for CustomVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            write!(&mut self.message, "{:?}", value).unwrap();
        } else {
            if !self.message.is_empty() {
                self.message.push_str(", ");
            }
            write!(&mut self.message, "{}: {:?}", field.name(), value).unwrap();
        }
    }
}

fn tracing_init(
    log_buffer: Arc<LogLL>,
) -> Result<(tracing_appender::non_blocking::WorkerGuard, ReloadHandle)> {
    let appender = tracing_appender::rolling::Builder::new()
        .filename_prefix("log")
        .rotation(Rotation::DAILY)
        .max_log_files(7)
        .build(log_dir()?)?;
    let (non_blocking_appender, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::new(TracingLevel::default().get_filter());
    let (filter, reload_handle) = reload::Layer::new(filter);
    let writer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking_appender)
        .with_ansi(false);
    tracing_subscriber::registry()
        .with(filter)
        .with(writer)
        .with(GuiLayer::new(log_buffer))
        .init();
    tracing::info!("Tracing initialized and logging to file.");

    Ok((guard, ReloadHandle(reload_handle)))
}
