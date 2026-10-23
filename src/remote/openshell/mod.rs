//! The OpenShell transport: one sandbox per project, reached through the
//! `openshell` CLI's exec, created, started, stopped and deleted by workmux.

pub mod image;
pub mod policy;
