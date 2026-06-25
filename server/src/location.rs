//! Opt-in user location (spec: superadmin user map). The frontend asks for GPS
//! consent ("help us improve the service"); on accept it POSTs coordinates here,
//! which we store on the user's row. A PostGIS `geometry` column (maintained from
//! latitude/longitude) lets the Directus native Map layout plot every consenting
//! user, filterable by org / recency / etc. Location is never required and can be
//! withdrawn at any time (DELETE), which clears the coordinates and the consent flag.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::db::Pool;
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(Deserialize)]
pub struct LocationInput {
    pub latitude: f64,
    pub longitude: f64,
    /// Optional GPS accuracy in metres (advisory; stored only via geometry today).
    #[serde(default)]
    pub accuracy: Option<f64>,
}

/// True when the coordinates are finite and within the valid lat/lng ranges.
fn valid_coords(lat: f64, lng: f64) -> bool {
    lat.is_finite()
        && lng.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lng)
}

/// `POST /api/user/location` — record the caller's opt-in location. The browser only
/// reaches this after the user granted both our prompt and the OS geolocation
/// permission, so arrival here is itself the consent signal.
pub async fn update_location(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<LocationInput>,
) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response();
    };
    if !valid_coords(body.latitude, body.longitude) {
        return (StatusCode::BAD_REQUEST, "invalid coordinates").into_response();
    }
    let res = sqlx::query(
        "UPDATE users
            SET latitude = $2, longitude = $3, location_consent = true,
                location_updated_at = now()
          WHERE id = $1",
    )
    .bind(user.user_id)
    .bind(body.latitude)
    .bind(body.longitude)
    .execute(pool)
    .await;
    match res {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("store user location failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed").into_response()
        }
    }
}

/// `DELETE /api/user/location` — withdraw consent and forget the coordinates.
pub async fn clear_location(State(state): State<AppState>, user: AuthUser) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response();
    };
    let res = sqlx::query(
        "UPDATE users
            SET latitude = NULL, longitude = NULL, location_consent = false,
                location_updated_at = now()
          WHERE id = $1",
    )
    .bind(user.user_id)
    .execute(pool)
    .await;
    match res {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("clear user location failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed").into_response()
        }
    }
}

/// Best-effort: enable PostGIS and add a `users.location` geometry column derived from
/// latitude/longitude (kept in sync by Postgres as a generated column), so the Directus
/// Map layout can plot users. NON-FATAL: a database image without PostGIS just logs a
/// warning and the rest of the app (including the plain lat/lng columns) is unaffected.
pub async fn ensure_geometry(pool: &Pool) {
    let steps = [
        "CREATE EXTENSION IF NOT EXISTS postgis",
        "ALTER TABLE users ADD COLUMN IF NOT EXISTS location geometry(Point, 4326) \
         GENERATED ALWAYS AS ( \
             CASE WHEN latitude IS NOT NULL AND longitude IS NOT NULL \
                  THEN ST_SetSRID(ST_MakePoint(longitude, latitude), 4326) \
                  ELSE NULL END \
         ) STORED",
        "CREATE INDEX IF NOT EXISTS idx_users_location ON users USING GIST (location)",
    ];
    for sql in steps {
        if let Err(e) = sqlx::query(sql).execute(pool).await {
            let what: String = sql.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            tracing::warn!(
                "user-map geometry setup skipped at `{what}…` (PostGIS unavailable?): {e}"
            );
            return;
        }
    }
    tracing::info!("user location geometry ready — Directus map enabled");
}

#[cfg(test)]
mod tests {
    use super::valid_coords;

    #[test]
    fn accepts_in_range_coordinates() {
        assert!(valid_coords(0.0, 0.0));
        assert!(valid_coords(45.4642, 9.19)); // Milan
        assert!(valid_coords(-90.0, -180.0)); // corners are inclusive
        assert!(valid_coords(90.0, 180.0));
    }

    #[test]
    fn rejects_out_of_range_or_nonfinite() {
        assert!(!valid_coords(90.1, 0.0));
        assert!(!valid_coords(0.0, 180.1));
        assert!(!valid_coords(-90.1, 0.0));
        assert!(!valid_coords(0.0, -180.1));
        assert!(!valid_coords(f64::NAN, 0.0));
        assert!(!valid_coords(0.0, f64::INFINITY));
    }
}
