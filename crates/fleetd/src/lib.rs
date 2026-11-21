//! `fleetd`: el demonio de fleet. Lleva las tareas con el motor y sirve la API HTTP.
#![forbid(unsafe_code)]

pub mod api;
pub mod args;
pub mod engine;

pub use engine::{Engine, EngineConfig, EngineError, Event, OutputStream};
