//! `flywire`: a patient's medical bills from a Flywire patient portal (the
//! Simplee platform, `<provider>.simpleepay.com`), for the household
//! medical-records system (health-records/v1, documents/v1). Read-only.

pub mod app;
pub mod auth;
pub mod bill;
pub mod cli;
pub mod client;
pub mod graphql;
pub mod portal;
pub mod session;
pub mod site;
