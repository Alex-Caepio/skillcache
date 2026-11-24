//! Native host, sandbox, provider CLI launcher, usage, and bounded subprocess helpers.
#![allow(dead_code)]

pub mod activity;
pub mod capture;
pub mod compiler_launch;
pub mod process;
pub mod provider;
pub mod sandbox;
pub mod strict_json;
pub mod usage;
pub mod util;

pub mod store {
    pub fn hexid() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    pub fn sha(bytes: &[u8]) -> String {
        crate::custody::sha256(bytes)
    }
}
