//! Route table for the Business API. Merged into the main router in `lib.rs::app`.

use axum::routing::{delete, get, patch, post};
use axum::Router;

use crate::business::{
    analytics, audit, billing, calls, meetings, members, organizations, projects, recording,
    search, storyboard, teams, transcripts,
};
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
        .route(
            "/api/business/organizations/{org_id}/projects/{project_id}/storyboard",
            get(storyboard::get).post(storyboard::generate),
        )
        // ---- Teams: people-groups with flexible membership (Phase 2) ----
        .route(
            "/api/business/organizations/{org_id}/teams",
            get(teams::list).post(teams::create),
        )
        .route(
            "/api/business/organizations/{org_id}/teams/{team_id}",
            patch(teams::patch).delete(teams::delete),
        )
        .route(
            "/api/business/organizations/{org_id}/teams/{team_id}/members",
            get(teams::list_members).post(teams::add_member),
        )
        .route(
            "/api/business/organizations/{org_id}/teams/{team_id}/members/{user_id}",
            delete(teams::remove_member),
        )
        // ---- Scheduled meetings (Google Calendar) ----
        .route(
            "/api/business/organizations/{org_id}/meetings",
            get(meetings::list).post(meetings::create),
        )
        .route(
            "/api/business/organizations/{org_id}/meetings/{meeting_id}",
            get(meetings::get).patch(meetings::update),
        )
        .route(
            "/api/business/organizations/{org_id}/meetings/{meeting_id}/cancel",
            post(meetings::cancel),
        )
        // ---- Calls: binding, history (PR-B) ----
        .route(
            "/api/rooms/{room}/business",
            get(calls::get_binding).patch(calls::bind),
        )
        .route(
            "/api/business/organizations/{org_id}/rooms",
            get(calls::list_org_rooms),
        )
        .route(
            "/api/business/organizations/{org_id}/analytics",
            get(analytics::summary),
        )
        .route(
            "/api/business/organizations/{org_id}/audit",
            get(audit::list),
        )
        .route(
            "/api/business/organizations/{org_id}/members/{user_id}/analytics",
            get(analytics::member_summary),
        )
        .route(
            "/api/business/organizations/{org_id}/projects/{project_id}/rooms",
            get(calls::list_project_rooms),
        )
        // ---- Semantic transcript search (per-project, role-scoped) ----
        .route(
            "/api/business/organizations/{org_id}/search",
            get(search::list),
        )
        // ---- Recording (PR-B): direct-to-storage upload, so no large body flows
        //      through the server (a long-call video is ~1 GB). ----
        .route(
            "/api/business/rooms/{session_id}/recording/upload-url",
            post(recording::upload_url),
        )
        .route(
            "/api/business/rooms/{session_id}/recording/complete",
            post(recording::complete),
        )
        .route(
            "/api/business/rooms/{session_id}/recording/url",
            get(recording::url),
        )
        // ---- Transcript: read, translate, export (PR-B) ----
        .route(
            "/api/business/rooms/{session_id}/transcript",
            get(transcripts::get_transcript),
        )
        .route(
            "/api/business/rooms/{session_id}/transcript/translate",
            post(transcripts::translate),
        )
        .route(
            "/api/business/rooms/{session_id}/transcript/export",
            get(transcripts::export),
        )
        // ---- Org billing: subscriptions, portal, top-up, webhook (PR-C) ----
        .route(
            "/api/business/organizations/{org_id}/subscription",
            post(billing::subscribe).get(billing::subscription),
        )
        .route(
            "/api/business/organizations/{org_id}/subscription/portal",
            post(billing::portal),
        )
        .route(
            "/api/business/organizations/{org_id}/credits/purchase",
            post(billing::purchase),
        )
        .route(
            "/api/business/organizations/{org_id}/credits",
            get(billing::credits),
        )
        .route("/api/business/stripe/webhook", post(billing::webhook))
}
