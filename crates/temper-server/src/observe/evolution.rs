//! Evolution engine endpoints: trajectories, sentinel checks, and O-P-A-D-I record management.

pub(crate) mod insight_generator;
mod operations;
mod records_detail;
mod records_list;
mod trajectories;

pub(crate) use operations::{
    handle_evolution_analyze, handle_evolution_materialize, handle_evolution_stream,
    handle_feature_requests, handle_intent_evidence, handle_sentinel_check, handle_unmet_intents,
    handle_update_feature_request,
};
pub(crate) use records_detail::{handle_decide, handle_get_evolution_record};
pub(crate) use records_list::{handle_list_evolution_insights, handle_list_evolution_records};
pub(crate) use trajectories::{
    handle_get_ots_trajectories, handle_post_ots_trajectory, handle_trajectories,
    handle_unmet_intent,
};
