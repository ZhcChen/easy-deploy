#![recursion_limit = "256"]

pub mod application_config;
pub mod apps;
pub mod artifact_storage;
pub mod auth;
pub mod catalog;
pub mod deploy;
pub mod events;
pub mod health;
pub mod host_metrics;
pub mod maintenance;
pub mod migrations;
pub mod node_credentials;
pub mod nodes;
pub mod platform;
pub mod runtimefs;
pub mod secret_config;
pub mod settings;
pub mod tasks;
pub mod text;
pub mod web;

pub use settings::Settings;
pub use web::{AppState, AppStateServices, build_router};
