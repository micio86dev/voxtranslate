//! The organisation's address book (spec 0114).
//!
//! The one part of the phone product that touches no carrier at all. It exists because
//! dialling meant typing a number and choosing the recipient's language out of eighty-four
//! EVERY time, for the same person — and getting it wrong places a call, bills it, and
//! leaves two people unable to understand each other.
//!
//! Three shapes carry the design, and each is argued for in `migrations/060_voip_contacts.sql`:
//! a contact belongs to the ORGANISATION, a language belongs to a NUMBER rather than to a
//! person, and `voip_contact_projects` is the first many-to-many involving `projects` in
//! this schema.

// Every handler returns `Result<Response, Response>` — the Business API convention, and
// what `routes.rs` and `business/mod.rs` allow for the same reason.
#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{db_err, not_found, require_pool, require_role, MEMBER};
use crate::middleware::AuthUser;
use crate::telephony::E164;
use crate::AppState;

use crate::voip::routes::refuse;

/// One telephone number on a contact, as the client sends it.
#[derive(Debug, Deserialize)]
pub struct NumberBody {
    pub e164: String,
    #[serde(default)]
    pub label: Option<String>,
    /// The language THIS number speaks. See the migration for why it is not on the person.
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub is_primary: bool,
}

#[derive(Debug, Deserialize)]
pub struct ContactBody {
    pub name: String,
    #[serde(default)]
    pub company: Option<String>,
    pub role: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub numbers: Vec<NumberBody>,
    #[serde(default)]
    pub project_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, FromRow)]
struct NumberRow {
    id: Uuid,
    e164: String,
    label: Option<String>,
    language: Option<String>,
    country: Option<String>,
    is_primary: bool,
}

#[derive(Debug, Serialize, FromRow)]
struct ProjectRow {
    id: Uuid,
    name: String,
}

#[derive(Debug, Serialize, FromRow)]
struct ContactRow {
    id: Uuid,
    name: String,
    company: Option<String>,
    role: Option<String>,
    notes: Option<String>,
    tags: Vec<String>,
    email: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ContactQuery {
    /// Free text over name, company and number — the three things people remember.
    q: Option<String>,
    project_id: Option<Uuid>,
    tag: Option<String>,
    language: Option<String>,
    page: Option<i64>,
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct LookupQuery {
    e164: String,
}

/// Normalise every number in a body, refusing the whole request if one is not dialable.
///
/// Refused as a body-level error rather than per number: a half-saved contact whose mobile
/// silently vanished is worse than a refusal naming the problem.
fn normalise(numbers: &[NumberBody]) -> Result<Vec<(E164, &NumberBody)>, Response> {
    numbers
        .iter()
        .map(|n| {
            E164::parse(&n.e164)
                .map(|parsed| (parsed, n))
                .map_err(|_| refuse(StatusCode::BAD_REQUEST, "number_not_e164"))
        })
        .collect()
}

/// `POST …/voip/contacts`
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<ContactBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(refuse(StatusCode::BAD_REQUEST, "name_required"));
    }
    let numbers = normalise(&body.numbers)?;

    let mut tx = pool.begin().await.map_err(db_err)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO voip_contacts (org_id, name, company, role, notes, tags, email, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(org_id)
    .bind(name)
    .bind(body.company.as_deref())
    .bind(body.role.as_deref())
    .bind(body.notes.as_deref())
    .bind(&body.tags)
    .bind(body.email.as_deref())
    .bind(user.user_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)?;

    insert_numbers(&mut tx, org_id, id, &numbers).await?;
    link_projects(&mut tx, org_id, id, &body.project_ids).await?;
    tx.commit().await.map_err(db_err)?;

    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

/// Insert a contact's numbers, mapping the org-wide uniqueness violation onto a code.
async fn insert_numbers(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org_id: Uuid,
    contact_id: Uuid,
    numbers: &[(E164, &NumberBody)],
) -> Result<(), Response> {
    // At most one primary, decided here rather than left to the partial unique index to
    // reject: a caller who marks two is making a mistake, not attacking us.
    let mut primary_taken = false;
    for (parsed, body) in numbers {
        let is_primary = body.is_primary && !primary_taken;
        primary_taken |= is_primary;
        let result = sqlx::query(
            "INSERT INTO voip_contact_numbers
                (contact_id, org_id, e164, label, language, country, is_primary)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(contact_id)
        .bind(org_id)
        .bind(parsed.as_str())
        .bind(body.label.as_deref())
        .bind(body.language.as_deref())
        .bind(parsed.region())
        .bind(is_primary)
        .execute(&mut **tx)
        .await;

        if let Err(e) = result {
            // One number, one person, per organisation — so inbound never has to choose
            // whose call this is. The constraint is in the database; this turns it into
            // something a customer can act on.
            if let sqlx::Error::Database(db) = &e {
                if db.is_unique_violation() {
                    return Err(refuse(StatusCode::CONFLICT, "number_already_known"));
                }
            }
            return Err(db_err(e));
        }
    }
    Ok(())
}

/// Link a contact to projects, refusing any that belong to another organisation.
async fn link_projects(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org_id: Uuid,
    contact_id: Uuid,
    project_ids: &[Uuid],
) -> Result<(), Response> {
    for project_id in project_ids {
        // Tenancy in the WHERE, as everywhere else: a project id from another org must not
        // become a link, and must not say whether it exists.
        let linked = sqlx::query(
            "INSERT INTO voip_contact_projects (contact_id, project_id)
             SELECT $1, id FROM projects WHERE id = $2 AND org_id = $3
             ON CONFLICT DO NOTHING",
        )
        .bind(contact_id)
        .bind(project_id)
        .bind(org_id)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;

        // Zero rows means either "already linked" (idempotent, fine) or "not this org's
        // project". Distinguishing them costs a second statement and tells an attacker
        // which ids exist, so the check is whether the project is ours at all.
        if linked.rows_affected() == 0 {
            let ours: Option<bool> =
                sqlx::query_scalar("SELECT true FROM projects WHERE id = $1 AND org_id = $2")
                    .bind(project_id)
                    .bind(org_id)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(db_err)?;
            if ours.is_none() {
                return Err(refuse(StatusCode::BAD_REQUEST, "project_not_in_org"));
            }
        }
    }
    Ok(())
}

/// `GET …/voip/contacts`
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<ContactQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let page = q.page.unwrap_or(1).max(1);
    let text =
        q.q.as_deref()
            .map(|s| format!("%{}%", s.trim().to_lowercase()));

    // One statement, one row per contact. A contact on two projects must appear ONCE in
    // each project's filter, not once per link — which is what a naive join would give.
    let rows: Vec<ContactRow> = sqlx::query_as(
        "SELECT DISTINCT c.id, c.name, c.company, c.role, c.notes, c.tags, c.email
         FROM voip_contacts c
         LEFT JOIN voip_contact_numbers n ON n.contact_id = c.id
         WHERE c.org_id = $1
           AND ($2::text IS NULL
                OR lower(c.name) LIKE $2
                OR lower(coalesce(c.company, '')) LIKE $2
                OR n.e164 LIKE $2)
           AND ($3::uuid IS NULL OR EXISTS (
                 SELECT 1 FROM voip_contact_projects p
                 WHERE p.contact_id = c.id AND p.project_id = $3))
           AND ($4::text IS NULL OR $4 = ANY (c.tags))
           AND ($5::text IS NULL OR EXISTS (
                 SELECT 1 FROM voip_contact_numbers l
                 WHERE l.contact_id = c.id AND l.language = $5))
         ORDER BY c.name
         LIMIT $6 OFFSET $7",
    )
    .bind(org_id)
    .bind(text.as_deref())
    .bind(q.project_id)
    .bind(q.tag.as_deref())
    .bind(q.language.as_deref())
    .bind(limit)
    .bind((page - 1) * limit)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({ "contacts": rows, "page": page, "limit": limit })).into_response())
}

/// `GET …/voip/contacts/{id}`
pub async fn detail(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, contact_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let contact: Option<ContactRow> = sqlx::query_as(
        "SELECT id, name, company, role, notes, tags, email
         FROM voip_contacts WHERE id = $1 AND org_id = $2",
    )
    .bind(contact_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let Some(contact) = contact else {
        return Err(not_found("contact not found"));
    };

    let numbers: Vec<NumberRow> = sqlx::query_as(
        "SELECT id, e164, label, language, country, is_primary
         FROM voip_contact_numbers WHERE contact_id = $1
         ORDER BY is_primary DESC, created_at",
    )
    .bind(contact_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    let projects: Vec<ProjectRow> = sqlx::query_as(
        "SELECT p.id, p.name FROM projects p
         JOIN voip_contact_projects l ON l.project_id = p.id
         WHERE l.contact_id = $1 ORDER BY p.name",
    )
    .bind(contact_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    let mut out = serde_json::to_value(&contact)
        .map_err(|_| refuse(StatusCode::INTERNAL_SERVER_ERROR, "storage_error"))?;
    out["numbers"] = serde_json::to_value(&numbers).unwrap_or(json!([]));
    out["projects"] = serde_json::to_value(&projects).unwrap_or(json!([]));
    Ok(Json(out).into_response())
}

/// `PATCH …/voip/contacts/{id}` — replaces the contact's numbers and project links.
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, contact_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ContactBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(refuse(StatusCode::BAD_REQUEST, "name_required"));
    }
    let numbers = normalise(&body.numbers)?;

    let mut tx = pool.begin().await.map_err(db_err)?;
    let updated = sqlx::query(
        "UPDATE voip_contacts
            SET name = $3, company = $4, role = $5, notes = $6, tags = $7, email = $8,
                updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(contact_id)
    .bind(org_id)
    .bind(name)
    .bind(body.company.as_deref())
    .bind(body.role.as_deref())
    .bind(body.notes.as_deref())
    .bind(&body.tags)
    .bind(body.email.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    if updated.rows_affected() == 0 {
        return Err(not_found("contact not found"));
    }

    // Replace rather than diff. The numbers and the links are small sets the client always
    // sends whole, and a diff would need identity for rows the UI does not track.
    sqlx::query("DELETE FROM voip_contact_numbers WHERE contact_id = $1")
        .bind(contact_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    sqlx::query("DELETE FROM voip_contact_projects WHERE contact_id = $1")
        .bind(contact_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    insert_numbers(&mut tx, org_id, contact_id, &numbers).await?;
    link_projects(&mut tx, org_id, contact_id, &body.project_ids).await?;
    tx.commit().await.map_err(db_err)?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE …/voip/contacts/{id}`
///
/// The calls stay. `voip_calls.contact_id` is `ON DELETE SET NULL`, so removing somebody
/// from the address book does not remove the record of having called them or what it cost.
pub async fn remove(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, contact_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let deleted = sqlx::query("DELETE FROM voip_contacts WHERE id = $1 AND org_id = $2")
        .bind(contact_id)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(db_err)?;
    if deleted.rows_affected() == 0 {
        return Err(not_found("contact not found"));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET …/voip/contacts/lookup?e164=` — who holds this number?
///
/// The reverse index made addressable. Inbound (spec 0116) answers "who is ringing?" with
/// it, and the dashboard uses it to decide whether to offer to save a number after a call.
/// An unknown number is a 404 rather than an error: not knowing somebody is normal.
pub async fn lookup(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<LookupQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let dest =
        E164::parse(&q.e164).map_err(|_| refuse(StatusCode::BAD_REQUEST, "number_not_e164"))?;

    let row: Option<(Uuid, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT c.id, c.name, c.company, n.language
         FROM voip_contact_numbers n
         JOIN voip_contacts c ON c.id = n.contact_id
         WHERE n.org_id = $1 AND n.e164 = $2",
    )
    .bind(org_id)
    .bind(dest.as_str())
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    match row {
        Some((id, name, company, language)) => Ok(Json(json!({
            "id": id,
            "name": name,
            "company": company,
            // The language of THAT number, which is the whole reason this lookup exists.
            "language": language,
        }))
        .into_response()),
        None => Err(not_found("no contact holds that number")),
    }
}
