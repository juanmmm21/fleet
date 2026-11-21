//! `fleetd`: el demonio de fleet. Lleva las tareas con el motor y sirve la API HTTP.
#![forbid(unsafe_code)]

pub mod api;
pub mod args;
pub mod engine;
pub mod guard;
pub mod tailnet;
pub mod web;

pub use engine::{Engine, EngineConfig, EngineError, Event, OutputStream};
