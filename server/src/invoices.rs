//! Invoices (spec 0109) — the local index of billing documents, for both
//! consumers and organizations.
//!
//! Stripe is the **issuer**: it owns the numbering sequence, the VAT lines and
//! the PDF. This module owns only what Stripe cannot give us cheaply — an
//! authorised, groupable list per owner, without a round-trip on every page
//! load — plus the mapping from a raw Stripe Invoice object to our row.
//!
//! ## Why a trait
//!
//! [`InvoiceProvider`] exists because the *issuer* is expected to change while
//! the API and both UIs stay put. Phase 2 of spec 0109 adds Italian electronic
//! invoicing (FatturaPA XML transmitted through the SDI), which Stripe does not
//! do at all and which needs a certified intermediary. When that lands it
//! becomes a second implementation behind this trait, not a rewrite of the
//! handlers.
//!
//! ## Invariants
//!
//! - `stripe_invoice_id` is UNIQUE, so [`upsert`] is safe to call on every
//!   webhook delivery — Stripe retries, and `invoice.payment_succeeded` and
//!   `checkout.session.completed` can describe the same invoice.
//! - Exactly one of `user_id` / `org_id` is set (enforced by a CHECK).
//! - The stored `invoice_pdf_url` is a cache, never what we hand the browser:
//!   Stripe's PDF links expire. Downloads re-resolve through
//!   [`InvoiceProvider::pdf_url`].

use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

use crate::config::BillingConfig;
use crate::db::Pool;

/// Who an invoice belongs to. Modelled as an enum rather than two nullable
/// arguments so a caller cannot pass both or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    User(Uuid),
    Org(Uuid),
}

/// One invoice as the API returns it. Money stays in integer cents — the
/// formatting decision belongs to the UI, which knows the locale.
#[derive(Debug, Clone, FromRow, Serialize, PartialEq, Eq)]
pub struct Invoice {
    pub id: Uuid,
    pub number: Option<String>,
    pub issued_at: DateTime<Utc>,
    pub period_start: Option<DateTime<Utc>>,
    pub period_end: Option<DateTime<Utc>>,
    pub subtotal_cents: i64,
    pub tax_cents: i64,
    pub total_cents: i64,
    pub currency: String,
    pub status: String,
    pub hosted_invoice_url: Option<String>,
}

/// Invoices for one calendar month, newest month first. The goal is "download
/// last month's invoices", so the month is the unit the API hands over rather
/// than something each client re-derives.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InvoiceMonth {
    /// `YYYY-MM`, in UTC.
    pub month: String,
    pub invoices: Vec<Invoice>,
}

/// The fields we lift out of a Stripe Invoice object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeInvoice {
    pub stripe_invoice_id: String,
    pub number: Option<String>,
    pub issued_at: DateTime<Utc>,
    pub period_start: Option<DateTime<Utc>>,
    pub period_end: Option<DateTime<Utc>>,
    pub subtotal_cents: i64,
    pub tax_cents: i64,
    pub total_cents: i64,
    pub currency: String,
    pub status: String,
    pub hosted_invoice_url: Option<String>,
    pub invoice_pdf_url: Option<String>,
}

/// The source of billing documents. Stripe today; an SDI intermediary alongside
/// it in Phase 2 (see the module docs).
#[allow(async_fn_in_trait)]
pub trait InvoiceProvider {
    /// Fetch one invoice's current downloadable PDF URL. Separate from the
    /// stored copy because the issuer's links expire.
    async fn pdf_url(&self, stripe_invoice_id: &str) -> Result<Option<String>, String>;

    /// Fetch an owner's recent invoices straight from the issuer. Used to repair
    /// a local index that missed a webhook, not on the read path.
    async fn fetch_recent(&self, customer_id: &str) -> Result<Vec<StripeInvoice>, String>;
}

/// [`InvoiceProvider`] backed by the Stripe REST API.
pub struct StripeInvoices<'a> {
    pub http: &'a reqwest::Client,
    pub cfg: &'a BillingConfig,
}

impl InvoiceProvider for StripeInvoices<'_> {
    async fn pdf_url(&self, stripe_invoice_id: &str) -> Result<Option<String>, String> {
        let inv =
            crate::stripe_handler::get_invoice(self.http, self.cfg, stripe_invoice_id).await?;
        // `invoice_pdf` is the direct download; the hosted page is the fallback
        // for an invoice Stripe has not finalised into a PDF yet.
        Ok(inv["invoice_pdf"]
            .as_str()
            .or_else(|| inv["hosted_invoice_url"].as_str())
            .map(str::to_string))
    }

    async fn fetch_recent(&self, customer_id: &str) -> Result<Vec<StripeInvoice>, String> {
        let raw =
            crate::stripe_handler::list_invoices(self.http, self.cfg, customer_id, 100).await?;
        Ok(raw.iter().filter_map(parse_stripe_invoice).collect())
    }
}

/// Map a raw Stripe Invoice object onto [`StripeInvoice`].
///
/// Returns `None` only when there is no invoice id — everything else has a
/// defensible default, and dropping a whole invoice because one optional field
/// moved between Stripe API versions would be worse than showing it with a zero.
pub fn parse_stripe_invoice(inv: &Value) -> Option<StripeInvoice> {
    let id = inv["id"].as_str()?.to_string();

    // Prefer the moment Stripe finalised the invoice — that is its fiscal date.
    // `created` is the fallback for a draft that has not been finalised yet.
    let issued_at = ts(inv, "status_transitions/finalized_at")
        .or_else(|| ts(inv, "status_transitions/paid_at"))
        .or_else(|| ts(inv, "created"))
        .unwrap_or_else(Utc::now);

    Some(StripeInvoice {
        stripe_invoice_id: id,
        number: inv["number"].as_str().map(str::to_string),
        issued_at,
        period_start: ts(inv, "period_start").or_else(|| ts(inv, "lines/data/0/period/start")),
        period_end: ts(inv, "period_end").or_else(|| ts(inv, "lines/data/0/period/end")),
        subtotal_cents: cents(inv, "subtotal"),
        // Newer API versions moved the tax total to `total_taxes`; keep both.
        tax_cents: match inv.get("tax").and_then(Value::as_i64) {
            Some(v) => v,
            None => cents(inv, "total_taxes/0/amount"),
        },
        total_cents: cents(inv, "total"),
        currency: inv["currency"].as_str().unwrap_or("usd").to_string(),
        status: inv["status"].as_str().unwrap_or("open").to_string(),
        hosted_invoice_url: inv["hosted_invoice_url"].as_str().map(str::to_string),
        invoice_pdf_url: inv["invoice_pdf"].as_str().map(str::to_string),
    })
}

/// Unix-seconds field at a `/`-separated JSON path → UTC datetime.
fn ts(v: &Value, path: &str) -> Option<DateTime<Utc>> {
    v.pointer(&format!("/{path}"))
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .and_then(|n| DateTime::from_timestamp(n, 0))
}

/// Integer-cents field at a `/`-separated JSON path, defaulting to 0.
fn cents(v: &Value, path: &str) -> i64 {
    v.pointer(&format!("/{path}"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// Record (or refresh) an invoice for `owner`.
///
/// Idempotent on `stripe_invoice_id`: a replayed webhook updates the mutable
/// fields (status can go `open` → `paid`, and a draft gains a number) and never
/// inserts a second row. Ownership columns are deliberately NOT part of the
/// update — an invoice does not change hands.
pub async fn upsert(pool: &Pool, owner: Owner, inv: &StripeInvoice) -> Result<(), sqlx::Error> {
    let (user_id, org_id) = match owner {
        Owner::User(id) => (Some(id), None),
        Owner::Org(id) => (None, Some(id)),
    };
    sqlx::query(
        "INSERT INTO invoices (
             user_id, org_id, stripe_invoice_id, number, issued_at,
             period_start, period_end, subtotal_cents, tax_cents, total_cents,
             currency, status, hosted_invoice_url, invoice_pdf_url
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT (stripe_invoice_id) DO UPDATE SET
             number             = COALESCE(EXCLUDED.number, invoices.number),
             issued_at          = EXCLUDED.issued_at,
             period_start       = COALESCE(EXCLUDED.period_start, invoices.period_start),
             period_end         = COALESCE(EXCLUDED.period_end, invoices.period_end),
             subtotal_cents     = EXCLUDED.subtotal_cents,
             tax_cents          = EXCLUDED.tax_cents,
             total_cents        = EXCLUDED.total_cents,
             currency           = EXCLUDED.currency,
             status             = EXCLUDED.status,
             hosted_invoice_url = COALESCE(EXCLUDED.hosted_invoice_url, invoices.hosted_invoice_url),
             invoice_pdf_url    = COALESCE(EXCLUDED.invoice_pdf_url, invoices.invoice_pdf_url),
             updated_at         = now()",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(&inv.stripe_invoice_id)
    .bind(&inv.number)
    .bind(inv.issued_at)
    .bind(inv.period_start)
    .bind(inv.period_end)
    .bind(inv.subtotal_cents)
    .bind(inv.tax_cents)
    .bind(inv.total_cents)
    .bind(&inv.currency)
    .bind(&inv.status)
    .bind(&inv.hosted_invoice_url)
    .bind(&inv.invoice_pdf_url)
    .execute(pool)
    .await?;
    Ok(())
}

/// Every invoice for `owner`, newest first. Drafts are excluded: an unfinalised
/// invoice has no number and is not a document anyone can book.
pub async fn list(pool: &Pool, owner: Owner) -> Result<Vec<Invoice>, sqlx::Error> {
    let (user_id, org_id) = match owner {
        Owner::User(id) => (Some(id), None),
        Owner::Org(id) => (None, Some(id)),
    };
    sqlx::query_as(
        "SELECT id, number, issued_at, period_start, period_end,
                subtotal_cents, tax_cents, total_cents, currency, status,
                hosted_invoice_url
         FROM invoices
         WHERE (user_id = $1 OR org_id = $2) AND status <> 'draft'
         ORDER BY issued_at DESC",
    )
    .bind(user_id)
    .bind(org_id)
    .fetch_all(pool)
    .await
}

/// Resolve an invoice's Stripe id, but only if it really belongs to `owner`.
///
/// `None` covers both "no such invoice" and "not yours" on purpose: the handler
/// answers 404 either way, so invoice ids cannot be probed for existence.
pub async fn stripe_id_for_owner(
    pool: &Pool,
    owner: Owner,
    invoice_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    let (user_id, org_id) = match owner {
        Owner::User(id) => (Some(id), None),
        Owner::Org(id) => (None, Some(id)),
    };
    sqlx::query_scalar(
        "SELECT stripe_invoice_id FROM invoices
         WHERE id = $1 AND (user_id = $2 OR org_id = $3)",
    )
    .bind(invoice_id)
    .bind(user_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
}

/// Group an `issued_at`-descending list into calendar months, preserving order.
///
/// Input order is the contract — [`list`] already sorts, so this walks once and
/// never re-sorts.
pub fn group_by_month(invoices: Vec<Invoice>) -> Vec<InvoiceMonth> {
    let mut out: Vec<InvoiceMonth> = Vec::new();
    for inv in invoices {
        let month = format!("{:04}-{:02}", inv.issued_at.year(), inv.issued_at.month());
        match out.last_mut() {
            Some(last) if last.month == month => last.invoices.push(inv),
            _ => out.push(InvoiceMonth {
                month,
                invoices: vec![inv],
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn invoice_at(iso: &str) -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            number: Some("INV-1".into()),
            issued_at: iso.parse::<DateTime<Utc>>().expect("valid RFC3339"),
            period_start: None,
            period_end: None,
            subtotal_cents: 1000,
            tax_cents: 220,
            total_cents: 1220,
            currency: "eur".into(),
            status: "paid".into(),
            hosted_invoice_url: None,
        }
    }

    // R5: the whole point is "give me last month's invoices", so months must come
    // out as distinct groups in the order they were fed in.
    #[test]
    fn groups_into_months_newest_first() {
        let invoices = vec![
            invoice_at("2026-08-03T10:00:00Z"),
            invoice_at("2026-07-28T10:00:00Z"),
            invoice_at("2026-07-01T10:00:00Z"),
            invoice_at("2026-05-14T10:00:00Z"),
        ];
        let months = group_by_month(invoices);

        let shape: Vec<(&str, usize)> = months
            .iter()
            .map(|m| (m.month.as_str(), m.invoices.len()))
            .collect();
        assert_eq!(shape, vec![("2026-08", 1), ("2026-07", 2), ("2026-05", 1)]);
    }

    // A month boundary must split even when the two invoices are hours apart.
    #[test]
    fn splits_across_a_month_boundary() {
        let months = group_by_month(vec![
            invoice_at("2026-08-01T00:30:00Z"),
            invoice_at("2026-07-31T23:30:00Z"),
        ]);
        assert_eq!(months.len(), 2);
    }

    #[test]
    fn grouping_an_empty_list_yields_no_months() {
        assert!(group_by_month(vec![]).is_empty());
    }

    // A non-contiguous list (same month reappearing later) must NOT be silently
    // merged — that would mean `list` stopped sorting and we want to see it.
    #[test]
    fn does_not_merge_non_adjacent_months() {
        let months = group_by_month(vec![
            invoice_at("2026-08-03T10:00:00Z"),
            invoice_at("2026-07-28T10:00:00Z"),
            invoice_at("2026-08-01T10:00:00Z"),
        ]);
        assert_eq!(months.len(), 3);
    }

    #[test]
    fn parses_a_finalized_stripe_invoice() {
        let raw = json!({
            "id": "in_123",
            "number": "VOX-0001",
            "created": 1_750_000_000,
            "status_transitions": { "finalized_at": 1_750_000_100 },
            "period_start": 1_749_000_000,
            "period_end": 1_751_000_000,
            "subtotal": 1000,
            "tax": 220,
            "total": 1220,
            "currency": "eur",
            "status": "paid",
            "hosted_invoice_url": "https://stripe.test/i/1",
            "invoice_pdf": "https://stripe.test/i/1.pdf"
        });
        let inv = parse_stripe_invoice(&raw).expect("id present");

        assert_eq!(inv.stripe_invoice_id, "in_123");
        assert_eq!(inv.number.as_deref(), Some("VOX-0001"));
        // The fiscal date is when Stripe finalised it, not when it was created.
        assert_eq!(inv.issued_at.timestamp(), 1_750_000_100);
        assert_eq!(inv.total_cents, 1220);
        assert_eq!(inv.tax_cents, 220);
        assert_eq!(inv.currency, "eur");
        assert_eq!(
            inv.invoice_pdf_url.as_deref(),
            Some("https://stripe.test/i/1.pdf")
        );
    }

    // Newer Stripe API versions dropped the flat `tax` field for `total_taxes[]`.
    #[test]
    fn reads_tax_from_the_newer_total_taxes_shape() {
        let raw = json!({
            "id": "in_456",
            "created": 1_750_000_000,
            "total_taxes": [ { "amount": 440 } ],
            "total": 2440,
            "currency": "eur",
            "status": "paid"
        });
        let inv = parse_stripe_invoice(&raw).expect("id present");
        assert_eq!(inv.tax_cents, 440);
        assert_eq!(inv.total_cents, 2440);
    }

    // Missing optional fields must degrade, not drop the invoice: a document we
    // show with a zero total is recoverable, one we silently discarded is not.
    #[test]
    fn tolerates_a_sparse_invoice_but_requires_an_id() {
        let sparse = parse_stripe_invoice(&json!({ "id": "in_789" })).expect("id present");
        assert_eq!(sparse.total_cents, 0);
        assert_eq!(sparse.currency, "usd");
        assert_eq!(sparse.status, "open");
        assert!(sparse.number.is_none());

        assert!(parse_stripe_invoice(&json!({ "total": 100 })).is_none());
    }
}
