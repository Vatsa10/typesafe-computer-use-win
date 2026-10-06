//! The loop: perception, the decision request, actions and the runner.
//!
//! A port of the Python package, module for module. The Python implementation is the reference and
//! stays runnable throughout; every module here is checked against it on the same machine.

pub mod actions;
pub mod apps;
pub mod catalog;
pub mod config;
pub mod dates;
pub mod decide;
pub mod intent;
pub mod models;
pub mod perception;
pub mod report;
pub mod runner;
pub mod runs_index;
pub mod shortlist;
pub mod sitepick;
pub mod talk;
pub mod timing;
pub mod worldmodel;
pub mod writer;
