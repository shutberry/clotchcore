pub mod adapters;
pub mod definitions;
pub mod detection;
pub mod event_mapping;
pub mod registry;

pub use adapters::*;
pub use definitions::*;
pub use detection::*;
pub use event_mapping::*;
pub use registry::*;

pub use definitions::{
    APEX_LIVEAPI_PORT, COD_EXECUTABLES, CS2_GSI_PORT, DOTA2_GSI_PORT, EFOOTBALL_EXECUTABLES,
    FC26_EXECUTABLES, FC27_EXECUTABLES, ROCKET_LEAGUE_PORT,
};
pub use detection::install_apex_liveapi_config;

pub mod presets {
    pub use super::registry::*;
}
