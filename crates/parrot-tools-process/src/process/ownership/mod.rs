//! Scoped process ownership, inventories and monitoring.
//!
//! These services do not select a sandbox or authorize filesystem access.

pub mod domain;
pub mod execution;
pub mod inventory;
pub mod managed;
pub mod monitor;
pub mod monitor_report;
pub mod owner;
pub mod protocol;

#[cfg(test)]
#[path = "../../../tests/process/ownership/faulted_execution.rs"]
mod faulted_execution;
