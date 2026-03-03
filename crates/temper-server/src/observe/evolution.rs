//! Evolution engine endpoints: trajectories, sentinel checks, and O-P-A-D-I record management.

mod operations;
mod records_detail;
mod records_list;
mod trajectories;

pub(crate) use operations::{
    handle_evolution_stream, handle_feature_requests, handle_sentinel_check, handle_unmet_intents,
    handle_update_feature_request,
};
pub(crate) use records_detail::{get_evolution_record, handle_decide};
pub(crate) use records_list::{list_evolution_insights, list_evolution_records};
pub(crate) use trajectories::{handle_trajectories, handle_unmet_intent};
