//! ClickUp integration shared by every module that reads from ClickUp. Keys live in
//! [`crate::keys`].

pub mod client;
pub mod mapping;
#[cfg(test)]
pub mod mock;

pub use client::{ClickUpClient, ClickUpError};
