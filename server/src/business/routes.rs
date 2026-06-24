//! Route table for the Business API. Merged into the main router in `lib.rs::app`.

use axum::routing::{delete, get, post};
use axum::Router;

use crate::business::{members, organizations, projects};
use crate::AppState;

/// All `/api/business/...` routes (Phase 2 PR-A: orgs, members, invites, projects).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/business/organizations",
            post(organizations::create).get(organizations::list_mine),
        )
        .route(
            "/api/business/organizations/{org_id}",
            get(organizations::get).patch(organizations::patch),
        )
        .route(
            "/api/business/organizations/{org_id}/members",
            get(members::list),
        )
        .route(
            "/api/business/organizations/{org_id}/members/{user_id}",
            delete(members::remove).patch(members::change_role),
        )
        .route(
            "/api/business/organizations/{org_id}/invites",
            post(members::create_invite),
        )
        .route("/api/business/invites/{token}", get(members::get_invite))
        .route(
            "/api/business/invites/{token}/accept",
            post(members::accept_invite),
        )
        .route(
            "/api/business/organizations/{org_id}/projects",
            get(projects::list).post(projects::create),
        )
        .route(
            "/api/business/organizations/{org_id}/projects/{project_id}",
            get(projects::get)
                .patch(projects::patch)
                .delete(projects::delete),
        )
}
