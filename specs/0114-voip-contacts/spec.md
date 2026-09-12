# 0114 — Company contacts for translated calls

| | |
|---|---|
| **Status** | 🚧 In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-11 |
| **Depends on** | [0111](../0111-translated-voip/spec.md), [0112](../0112-business-phone-dashboard/spec.md), [0106](../0106-voxtranslate-for-business/spec.md) |

## 1. Context & Problem

Dialling today means typing a number and then choosing the recipient's language from a
list of eighty-four, **every time**, for the same person. Nothing remembers that the
number ending 8000 belongs to Wei in Shenzhen and that Wei speaks Mandarin. Get it wrong
and the call is placed, billed, and useless.

The i18n string `phone.contact` — *"Contact name (optional)"* — has shipped in all five
dashboard locales since 1.50.0 against a field that does not exist, which is the shape of
a feature that was designed and not built.

There is no contact entity anywhere in the product: `rg -ni 'contact'` over `server/src`
finds only the public sales form. And **no many-to-many join involving `projects` exists
in the entire schema** — every `REFERENCES projects(id)` is a 1:N `project_id` column.
This is the first, which is why the shape is worth getting right rather than copying.

Inbound calling (0116) needs the reverse lookup — given the number that rang us, who is
this? — so the index that answers it is designed here rather than bolted on later.

## 2. Goals / Non-Goals

**Goals**
- An organisation keeps an address book; a contact belongs to the org, not to a project.
- A phone number carries its own language, so the dialer stops asking.
- A contact reaches any number of projects, and appears once regardless.
- After calling a number nobody knows, saving it takes one action and keeps what was
  already chosen.
- The lookup inbound will need — org + E.164 → contact — is indexed from the start.

**Non-Goals**
- Import from CSV, Google Contacts or a CRM. A later piece of work, and one that needs a
  deduplication story this spec does not owe.
- Presence, availability or call-me-back. Not telephony data.
- Sharing a contact between organisations. Tenancy is the point.

## 3. Requirements

- **R1 — A contact belongs to the organisation.** As a Business user, I want one address
  book for my company.
  - *Given* any member, *then* they can read the org's contacts; *given* a non-member,
    *then* the org's contacts are a 404, not a 403.
  - *Given* a contact, *then* it carries a name, and optionally a company, a role, notes,
    tags and an email.

- **R2 — A contact has numbers, and a number has a language.** As a caller, I want the
  dialer to know what language to use.
  - *Given* a contact with numbers, *then* each carries its own `language`, label and
    country, and at most one is primary.
  - *Given* the same number twice in one organisation, *then* the second is refused:
    inbound routing cannot be ambiguous about who is calling.

- **R3 — A contact reaches many projects.** As a project lead, I want the people involved
  in my project without duplicating them.
  - *Given* a contact, *then* it can be linked to zero, one or many projects, and linking
    it twice is idempotent.
  - *Given* a project is deleted, *then* the contact survives with one fewer link.

- **R4 — Contacts are searchable.** 
  - *Given* the list, *then* it can be filtered by free text over name, company and number,
    and by project, tag and language.

- **R5 — The dialer knows the person.** As a caller, I want to pick a contact instead of a
  number.
  - *Given* a contact's number is chosen, *then* the recipient's language is preselected
    from that number and remains overridable for this call.
  - *Given* a project is linked to the contact and none is chosen, *then* it is offered.

- **R6 — An unknown number can be kept.** As a caller, I want the person I just called in
  my address book without retyping anything.
  - *Given* a completed call to a number no contact holds, *then* the call page offers to
    save it, pre-filled with the number, its country, the language used for the call and
    the project it was filed under.
  - *Given* the number is already known, *then* nothing is offered.

- **R7 — Deleting a contact does not rewrite history.**
  - *Given* a contact with calls against it, *when* it is deleted, *then* the calls remain
    and stop naming it — a call that happened cannot un-happen, and the financial record
    outlives the address book (the rule `organization_credits_transactions` already
    follows with `ON DELETE SET NULL`).

## 4. Design & Architecture

**Data model** — migration `060_voip_contacts.sql`:

| Table | Shape |
|---|---|
| `voip_contacts` | `id`, `org_id`→organizations CASCADE, `name`, `company`, `role`, `notes`, `tags TEXT[]`, `email`, `created_by`→users SET NULL, timestamps |
| `voip_contact_numbers` | `id`, `contact_id`→contacts CASCADE, **`org_id`** (denormalised), `e164`, `label`, `language`, `country`, `is_primary` |
| `voip_contact_projects` | `(contact_id, project_id)` composite PK, both CASCADE |

**Three decisions worth the ink.**

*`org_id` is denormalised onto `voip_contact_numbers`.* It buys the constraint that
matters — `UNIQUE (org_id, e164)` — and the index inbound will use to answer "who is
ringing" in one statement rather than a join. The alternative, a unique index over a join,
Postgres will not give you.

*The language lives on the NUMBER, not the contact.* A colleague in Barcelona who takes
work calls in English on the office line and Catalan on their mobile is not two people.
Putting it on the contact would force a choice that is wrong half the time.

*`voip_calls.contact_id` is `ON DELETE SET NULL`.* R7. The same rule the credits ledger
already follows for `session_id`: the record of what happened and what it cost must
outlive the convenience data that described it.

**Routes** (all `require_role(MEMBER)`, org id from the path, tenancy in the `WHERE`):

```
GET    …/voip/contacts            list + filter (q, project_id, tag, language, page)
POST   …/voip/contacts            create, with numbers and project links in one body
GET    …/voip/contacts/{id}       one contact with its numbers and projects
PATCH  …/voip/contacts/{id}       edit
DELETE …/voip/contacts/{id}       remove
GET    …/voip/contacts/lookup     ?e164= → the contact holding it, or 404
```

`lookup` exists for R6 and for 0116; it is the reverse index made addressable.

**Dashboard**: `/[lang]/phone/contacts/` (list + editor), a contact picker in the dialer,
and the save-after-call offer on the call page. The section tab bar gains a fourth tab.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec | this file |
| S1 | Migration 060 | `server/migrations/060_voip_contacts.sql` |
| S2 | Domain + routes + tests | `server/src/voip/contacts.rs`, `server/src/voip/routes.rs`, `server/tests/voip_api.rs` |
| S3 | `voip_calls.contact_id` + resolution at dial time | `server/src/voip/service.rs` |
| S4 | Dashboard: contacts page, typed client, i18n ×5 | `dashboard/src/pages/[lang]/phone/contacts.astro` |
| S5 | Dialer: contact picker, language preselect | `dashboard/src/pages/[lang]/phone.astro`, `phone-catalogue.ts` |
| S6 | Call page: save an unknown number | `dashboard/src/pages/[lang]/phone/detail.astro` |

## 6. Testing & Verification

- The same number twice in one org is refused; the same number in two orgs is fine.
- A contact links to two projects and appears once in each project's filter.
- Deleting a project leaves the contact with one fewer link; deleting a contact leaves its
  calls, with `contact_id` null.
- A non-member gets 404 on every contact route.
- `lookup` finds a number by its E.164 and is scoped to the org.
- Pure client logic: preselecting the language from a chosen number, and deciding whether
  a completed call should offer to save.

## 7. Deployment & Operations

Migration 060 is additive and idempotent. No env var. No provider surface — this is the
one piece of the phone product that touches no carrier at all.

## 8. Risks / Open Items

1. `UNIQUE (org_id, e164)` means one organisation cannot hold the same number under two
   people. That is deliberate (0116 must not have to choose), but it will surprise someone
   with a shared office line, and the refusal has to say so clearly.
2. Contacts are PII with no separate retention policy: they live and die with the
   organisation. GDPR erasure of an org already cascades; erasure of one *person* across
   an address book is not modelled and is named here rather than assumed.
