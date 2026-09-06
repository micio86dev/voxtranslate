//! Minimal Google Calendar API v3 client (spec: scheduled meetings, Phase 1b).
//!
//! We create/update/delete events on the organizer's calendar with `sendUpdates=all`
//! so Google emails native invites and the event lands in every attendee's calendar.
//! Events are tagged with `extendedProperties.private` (vox_meeting_id, vox_room_code,
//! vox_project_id) so the dashboard calendar view — which reads back from Calendar as
//! the source of truth — can recognize VoxTranslate meetings and overlay app data.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://www.googleapis.com/calendar/v3";

/// What to write to a Calendar event. Times are RFC3339 strings + an IANA timezone.
pub struct EventInput {
    pub summary: String,
    pub description: Option<String>,
    /// Event location. We put the room join URL here so Calendar renders it as a
    /// clickable link on the event itself, not only buried in the description.
    pub location: Option<String>,
    pub start_rfc3339: String,
    pub end_rfc3339: String,
    pub timezone: String,
    /// Attendee emails (the organizer's own email is implicit).
    pub attendee_emails: Vec<String>,
    /// `extendedProperties.private` key/values (our app metadata).
    pub private_props: HashMap<String, String>,
    /// RRULE strings for a recurring series (e.g. `["RRULE:FREQ=WEEKLY;COUNT=10"]`).
    /// `None` for a one-off event.
    pub recurrence: Option<Vec<String>>,
}

#[derive(Serialize)]
struct EventTime {
    #[serde(rename = "dateTime")]
    date_time: String,
    #[serde(rename = "timeZone")]
    time_zone: String,
}

#[derive(Serialize)]
struct Attendee {
    email: String,
}

#[derive(Serialize)]
struct ExtendedProperties {
    private: HashMap<String, String>,
}

#[derive(Serialize)]
struct EventBody {
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    start: EventTime,
    end: EventTime,
    attendees: Vec<Attendee>,
    #[serde(rename = "extendedProperties")]
    extended_properties: ExtendedProperties,
    #[serde(skip_serializing_if = "Option::is_none")]
    recurrence: Option<Vec<String>>,
}

/// A created/updated event, as far as we care about it.
#[derive(Debug, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    #[serde(rename = "htmlLink", default)]
    pub html_link: Option<String>,
}

/// Build an event description that always carries the room join link. An empty or
/// whitespace-only description yields the bare join line, so the link is never lost.
pub fn description_with_join_link(description: Option<&str>, join_url: &str) -> String {
    match description.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!("{d}\n\nJoin: {join_url}"),
        None => format!("Join: {join_url}"),
    }
}

impl EventInput {
    fn to_body(&self) -> EventBody {
        EventBody {
            summary: self.summary.clone(),
            description: self.description.clone(),
            location: self.location.clone(),
            start: EventTime {
                date_time: self.start_rfc3339.clone(),
                time_zone: self.timezone.clone(),
            },
            end: EventTime {
                date_time: self.end_rfc3339.clone(),
                time_zone: self.timezone.clone(),
            },
            attendees: self
                .attendee_emails
                .iter()
                .map(|e| Attendee { email: e.clone() })
                .collect(),
            extended_properties: ExtendedProperties {
                private: self.private_props.clone(),
            },
            recurrence: self.recurrence.clone(),
        }
    }
}

async fn handle_event_response(resp: reqwest::Response) -> Result<CalendarEvent, String> {
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("calendar api {status}: {body}"));
    }
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Create an event on `calendar_id` (use `"primary"`). Sends invites to attendees.
pub async fn create_event(
    http: &reqwest::Client,
    access_token: &str,
    calendar_id: &str,
    input: &EventInput,
) -> Result<CalendarEvent, String> {
    let url = format!("{API_BASE}/calendars/{calendar_id}/events?sendUpdates=all");
    let resp = http
        .post(&url)
        .bearer_auth(access_token)
        .json(&input.to_body())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    handle_event_response(resp).await
}

/// Update an existing event (full replace via PATCH semantics on the fields we send).
pub async fn update_event(
    http: &reqwest::Client,
    access_token: &str,
    calendar_id: &str,
    event_id: &str,
    input: &EventInput,
) -> Result<CalendarEvent, String> {
    let url = format!("{API_BASE}/calendars/{calendar_id}/events/{event_id}?sendUpdates=all");
    let resp = http
        .patch(&url)
        .bearer_auth(access_token)
        .json(&input.to_body())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    handle_event_response(resp).await
}

/// Delete an event. A 410 (already gone) is treated as success — idempotent cancel.
pub async fn delete_event(
    http: &reqwest::Client,
    access_token: &str,
    calendar_id: &str,
    event_id: &str,
) -> Result<(), String> {
    let url = format!("{API_BASE}/calendars/{calendar_id}/events/{event_id}?sendUpdates=all");
    let resp = http
        .delete(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 410 {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(format!("calendar api {status}: {body}"))
}

/// An event as read back from Calendar (source of truth) for the dashboard view.
#[derive(Debug, Deserialize)]
pub struct ListedEvent {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub start: Option<EventTimeRead>,
    #[serde(default)]
    pub end: Option<EventTimeRead>,
    #[serde(rename = "htmlLink", default)]
    pub html_link: Option<String>,
    #[serde(rename = "extendedProperties", default)]
    pub extended_properties: Option<ExtendedPropertiesRead>,
}

#[derive(Debug, Deserialize)]
pub struct EventTimeRead {
    #[serde(rename = "dateTime", default)]
    pub date_time: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExtendedPropertiesRead {
    #[serde(default)]
    pub private: HashMap<String, String>,
}

#[derive(Deserialize)]
struct EventsList {
    #[serde(default)]
    items: Vec<ListedEvent>,
}

/// List events on `calendar_id` between `time_min`/`time_max` (RFC3339), expanding
/// recurring events. Used by the dashboard calendar view to read the source of truth.
pub async fn list_events(
    http: &reqwest::Client,
    access_token: &str,
    calendar_id: &str,
    time_min: &str,
    time_max: &str,
) -> Result<Vec<ListedEvent>, String> {
    let url = format!("{API_BASE}/calendars/{calendar_id}/events");
    let resp = http
        .get(&url)
        .bearer_auth(access_token)
        .query(&[
            ("timeMin", time_min),
            ("timeMax", time_max),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("maxResults", "250"),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("calendar api {status}: {body}"));
    }
    let list: EventsList = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    Ok(list.items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> EventInput {
        EventInput {
            summary: "Sync".into(),
            description: None,
            location: None,
            start_rfc3339: "2026-01-01T10:00:00Z".into(),
            end_rfc3339: "2026-01-01T10:30:00Z".into(),
            timezone: "UTC".into(),
            attendee_emails: vec![],
            private_props: HashMap::new(),
            recurrence: None,
        }
    }

    #[test]
    fn join_link_is_appended_to_an_existing_description() {
        let out = description_with_join_link(Some("Agenda: roadmap"), "https://vox.app/?room=abc");
        assert_eq!(out, "Agenda: roadmap\n\nJoin: https://vox.app/?room=abc");
    }

    #[test]
    fn join_link_stands_alone_when_there_is_no_description() {
        assert_eq!(
            description_with_join_link(None, "https://vox.app/?room=abc"),
            "Join: https://vox.app/?room=abc"
        );
        assert_eq!(
            description_with_join_link(Some("   \n "), "https://vox.app/?room=abc"),
            "Join: https://vox.app/?room=abc"
        );
    }

    #[test]
    fn location_is_serialized_when_set_and_omitted_when_not() {
        let body = serde_json::to_value(input().to_body()).unwrap();
        assert!(body.get("location").is_none());

        let mut with_location = input();
        with_location.location = Some("https://vox.app/?room=abc".into());
        let body = serde_json::to_value(with_location.to_body()).unwrap();
        assert_eq!(body["location"], "https://vox.app/?room=abc");
    }
}
