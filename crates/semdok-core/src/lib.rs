//! Semdok Edge core. See docs/ARCHITECTURE.md for the trust boundaries.
#![allow(unused_assignments)]
pub mod db;
pub mod engine;
pub mod gateway;
pub mod history;
pub mod keys;
pub mod models;
pub mod proc;
pub mod secrets;
pub mod storage;
pub mod tunnel;

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random generator unavailable");
    b
}
