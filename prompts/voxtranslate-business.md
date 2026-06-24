# VoxTranslate for Business — Spec Driven + TDD Implementation

## Context
VoxTranslate (voxtranslate.app) is a real-time multilingual voice translation SaaS.
Stack: Rust/Axum backend (Railway), Astro 5 frontend, Supabase (auth + DB + Storage),
Stripe credit billing, WebRTC mesh (max 4 participants), 4 AI engine tiers (Standard/Enhanced/Pro/Premium).

You must implement the "VoxTranslate for Business" layer: Organizations, Members,
Projects, Cloud Recording, Transcripts, Call History — without touching the existing
call room logic, consumer credit system, or individual auth flow.
**Zero regressions on the existing consumer flow.**

## Mandatory approach
1. Before writing any code: write specs (types, API contracts, RLS rules)
2. Write tests (integration + unit) before implementation
3. Implement until tests pass
4. Update the marketing site (website.voxtranslate.app — Astro 5 + PocketBase)
5. Run the full test suite to verify zero regressions

---

## PHASE 1 — Supabase Database Schema

### 1.1 Schema spec

Create the file `supabase/migrations/YYYYMMDDHHMMSS_business_workspace.sql` with:

**Table `organizations`**
```sql
id uuid PK default gen_random_uuid()
name text NOT NULL
slug text UNIQUE NOT NULL  -- e.g. "acme" → future acme.voxtranslate.app
plan text NOT NULL DEFAULT 'business' -- 'business' | 'enterprise'
credits_balance integer NOT NULL DEFAULT 0
settings jsonb NOT NULL DEFAULT '{
  "retention_days": 90,
  "compliance_mode": false,
  "allowed_domains": [],
  "recording_notify_participants": true
}'
owner_id uuid NOT NULL REFERENCES auth.users(id)
created_at timestamptz DEFAULT now()
updated_at timestamptz DEFAULT now()
```

**Table `organization_members`**
```sql
id uuid PK default gen_random_uuid()
org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE
user_id uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE
role text NOT NULL DEFAULT 'member' -- 'owner' | 'admin' | 'member' | 'guest'
invited_by uuid REFERENCES auth.users(id)
joined_at timestamptz DEFAULT now()
UNIQUE(org_id, user_id)
```

**Table `organization_invites`**
```sql
id uuid PK default gen_random_uuid()
org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE
email text NOT NULL
role text NOT NULL DEFAULT 'member'
token text UNIQUE NOT NULL DEFAULT encode(gen_random_bytes(32), 'hex')
invited_by uuid NOT NULL REFERENCES auth.users(id)
expires_at timestamptz NOT NULL DEFAULT now() + interval '48 hours'
accepted_at timestamptz
created_at timestamptz DEFAULT now()
```

**Table `projects`**
```sql
id uuid PK default gen_random_uuid()
org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE
name text NOT NULL
description text
default_languages text[] DEFAULT '{}'
created_by uuid NOT NULL REFERENCES auth.users(id)
archived_at timestamptz
created_at timestamptz DEFAULT now()
updated_at timestamptz DEFAULT now()
```

**Additions to existing `rooms` table** (ALTER TABLE, do not recreate):
```sql
ALTER TABLE rooms ADD COLUMN IF NOT EXISTS org_id uuid REFERENCES organizations(id);
ALTER TABLE rooms ADD COLUMN IF NOT EXISTS project_id uuid REFERENCES projects(id);
ALTER TABLE rooms ADD COLUMN IF NOT EXISTS cloud_recording_enabled boolean DEFAULT false;
ALTER TABLE rooms ADD COLUMN IF NOT EXISTS recording_storage_path text;
ALTER TABLE rooms ADD COLUMN IF NOT EXISTS transcript_status text DEFAULT 'none';
-- 'none' | 'processing' | 'ready' | 'failed'
```

**Table `transcripts`**
```sql
id uuid PK default gen_random_uuid()
room_id uuid NOT NULL REFERENCES rooms(id) ON DELETE CASCADE
org_id uuid NOT NULL REFERENCES organizations(id)
source_language text NOT NULL
segments jsonb NOT NULL DEFAULT '[]'
-- [{speaker_id, speaker_name, text, start_ms, end_ms}]
translations jsonb NOT NULL DEFAULT '{}'
-- {"it": "translated text...", "de": "..."}  ← cache
duration_seconds integer
word_count integer
created_at timestamptz DEFAULT now()
processed_at timestamptz
```

**Table `audit_logs`** (only for orgs with compliance_mode = true)
```sql
id uuid PK default gen_random_uuid()
org_id uuid NOT NULL REFERENCES organizations(id)
actor_id uuid NOT NULL REFERENCES auth.users(id)
action text NOT NULL
-- 'transcript.view' | 'transcript.export' | 'recording.play' | 'member.invite' | etc.
resource_type text NOT NULL
resource_id uuid NOT NULL
metadata jsonb DEFAULT '{}'
ip_address inet
created_at timestamptz DEFAULT now()
```

**Table `organization_credits_transactions`**
```sql
id uuid PK default gen_random_uuid()
org_id uuid NOT NULL REFERENCES organizations(id)
amount integer NOT NULL  -- positive=purchase, negative=consumption
type text NOT NULL
-- 'purchase' | 'recording' | 'transcription' | 'translation' | 'subscription_grant'
description text
room_id uuid REFERENCES rooms(id)
stripe_payment_intent_id text
created_at timestamptz DEFAULT now()
```

### 1.2 RLS Policies

Enable RLS on all new tables. Rules:

- `organizations`: SELECT/UPDATE only if you are a member (any role)
- `organization_members`: SELECT if you are a member of the same org; INSERT only owner/admin; DELETE only owner/admin (or self-leave)
- `organization_invites`: SELECT/INSERT/DELETE only owner/admin of the org
- `projects`: SELECT if member; INSERT if member; UPDATE/DELETE if admin/owner
- `rooms`: existing policies remain; add that if `org_id IS NOT NULL`, SELECT is allowed for org members
- `transcripts`: SELECT only for org members; no direct external access
- `audit_logs`: SELECT only admin/owner; INSERT only via service_role (backend)
- `organization_credits_transactions`: SELECT only admin/owner

Helper function to create:
```sql
CREATE OR REPLACE FUNCTION get_user_org_role(p_org_id uuid, p_user_id uuid)
RETURNS text AS $$
  SELECT role FROM organization_members
  WHERE org_id = p_org_id AND user_id = p_user_id
$$ LANGUAGE sql SECURITY DEFINER STABLE;
```

### 1.3 Migration tests

Write tests in `tests/db/business_schema_test.sql` (or pgTAP if available) verifying:
- All tables exist with correct columns
- RLS is enabled on every table
- A user cannot see data from another org
- The `updated_at` trigger works on `organizations` and `projects`

---

## PHASE 2 — Rust/Axum Backend

### 2.1 Module structure

Create the module `src/business/` with:
```
src/business/
├── mod.rs
├── organizations.rs   -- CRUD organizations
├── members.rs         -- invites, roles, removal
├── projects.rs        -- CRUD projects
├── recording.rs       -- S3 upload, signed URL, lifecycle
├── transcripts.rs     -- processing, translation, export
├── credits.rs         -- deduct_org_credits, transaction log
├── audit.rs           -- log_audit_event (async, non-blocking)
└── routes.rs          -- all endpoints under /api/business/...
```

### 2.2 API endpoints spec

**Organizations**
```
POST   /api/business/organizations
       body: {name, slug, plan}
       → create org, add creator as 'owner', respond with Organization

GET    /api/business/organizations/me
       → list orgs of the authenticated user with their role

GET    /api/business/organizations/:org_id
       → org detail (members only)

PATCH  /api/business/organizations/:org_id
       body: {name?, settings?}
       → owner/admin only

GET    /api/business/organizations/:org_id/credits
       → balance + last 50 transactions (admin/owner only)
```

**Members**
```
GET    /api/business/organizations/:org_id/members
       → list members with role

POST   /api/business/organizations/:org_id/invites
       body: {email, role}
       → create invite, send email with link /join?token=xxx

GET    /api/business/invites/:token
       → invite info (org name, role, expiry) — public but secret token

POST   /api/business/invites/:token/accept
       → authenticated: add user as member, mark invite accepted

DELETE /api/business/organizations/:org_id/members/:user_id
       → removal (admin/owner, or self-leave)

PATCH  /api/business/organizations/:org_id/members/:user_id
       body: {role}
       → role change (owner only)
```

**Projects**
```
GET    /api/business/organizations/:org_id/projects
POST   /api/business/organizations/:org_id/projects
       body: {name, description?, default_languages?}
GET    /api/business/organizations/:org_id/projects/:project_id
PATCH  /api/business/organizations/:org_id/projects/:project_id
DELETE /api/business/organizations/:org_id/projects/:project_id
       → soft delete (archived_at = now())
```

**Rooms / History**
```
GET    /api/business/organizations/:org_id/rooms
       query: ?project_id=&page=&limit=&from=&to=
       → paginated call list with metadata

GET    /api/business/organizations/:org_id/projects/:project_id/rooms
       → calls for a specific project

PATCH  /api/rooms/:room_id/business
       body: {org_id?, project_id?, cloud_recording_enabled?}
       → associate room to org/project (before call starts)
```

**Recording**
```
POST   /api/business/rooms/:room_id/recording/complete
       body: {duration_seconds, file_size_bytes}
       multipart: audio file (WebM/Opus)
       → upload to Supabase Storage bucket 'recordings' (private)
       → path: {org_id}/{room_id}/{timestamp}.webm
       → deduct org credits: 1 credit per minute (round up)
       → update room: recording_storage_path, transcript_status='processing'
       → enqueue async transcription job
       → respond with {recording_id, credits_deducted}

GET    /api/business/rooms/:room_id/recording/url
       → generate Supabase signed URL with 1-hour TTL (org members only)
       → if compliance_mode: log audit 'recording.play'
```

**Transcripts**
```
GET    /api/business/rooms/:room_id/transcript
       → transcript with segments
       → if compliance_mode: log audit 'transcript.view'

POST   /api/business/rooms/:room_id/transcript/translate
       body: {target_language: "it"}
       → if already in translations{} cache: return from cache (0 credits)
       → otherwise: translate with appropriate engine, save to cache
       → deduct credits: 2 credits per 1000 words
       → respond with translated text

GET    /api/business/rooms/:room_id/transcript/export
       query: ?format=txt|pdf&language=it
       → export transcript (original or translated)
       → if compliance_mode: log audit 'transcript.export'
       → signed URL TTL: 15 minutes
```

**Stripe for orgs**
```
POST   /api/business/organizations/:org_id/credits/purchase
       body: {credits_amount}
       → same consumer system but charged to the org
       → transaction log type 'purchase'

POST   /api/business/organizations/:org_id/subscription
       body: {plan: 'business'|'enterprise'}
       → Stripe monthly subscription
       → webhook updates org.plan and grants monthly credits
```

### 2.3 Async transcription job

In `src/business/transcripts.rs`:
```rust
pub async fn process_recording_transcript(
    room_id: Uuid,
    storage_path: &str,
    org_id: Uuid,
) -> Result<()>
```

Flow:
1. Download recording from Supabase Storage (internal signed URL)
2. Send to Deepgram API with diarization enabled (`diarize=true`)
3. Parse response: build segments[] with speaker labels
4. Save to `transcripts` table
5. Update `rooms.transcript_status = 'ready'`
6. On failure: `transcript_status = 'failed'`, log error

Credits for transcription: 5 credits per hour of audio (deduct post-processing).

### 2.4 Backend tests

Create `tests/business/` with integration tests for every endpoint:
- Auth guard: 401 without token
- Permission guard: 403 if insufficient role
- Happy path for every endpoint
- Cross-tenant isolation: user from org A cannot see org B data
- Credits deduction: verify credits are deducted correctly after recording
- Full invite flow: create → accept → verify membership
- Recording upload → transcript job enqueued → status updated

Follow the existing Rust test patterns in the project.

---

## PHASE 3 — Astro 5 Frontend

### 3.1 New pages

```
src/pages/
├── business/
│   ├── index.astro              -- redirect to /business/dashboard if authenticated
│   ├── onboarding.astro         -- create first org
│   ├── dashboard.astro          -- org overview (selector if multi-org)
│   ├── members.astro            -- member management
│   ├── projects/
│   │   ├── index.astro          -- project list
│   │   ├── [id].astro           -- project detail + call list
│   │   └── new.astro            -- create project
│   ├── history.astro            -- org call history
│   ├── credits.astro            -- balance + transactions + purchase
│   └── settings.astro           -- org settings (retention, compliance, etc.)
├── join.astro                   -- accept invite (?token=xxx)
```

### 3.2 Components

```
src/components/business/
├── OrgSwitcher.astro            -- org selector in navbar (if multi-org)
├── MemberList.astro
├── InviteModal.astro
├── ProjectCard.astro
├── CallHistoryTable.astro       -- call list with transcript status
├── TranscriptViewer.astro       -- segments viewer with speaker labels
├── TranscriptTranslator.astro   -- language select + on-demand translation
├── RecordingPlayer.astro        -- player with signed URL
├── OrgCreditsWidget.astro       -- org balance + link to purchase
└── ComplianceBadge.astro        -- badge if compliance_mode is active
```

### 3.3 Navigation

Add to navbar (if user has at least one org):
- "Workspace" link → /business/dashboard
- Org credits badge (when in business context)

The business context is separate from consumer: a user can use VoxTranslate
normally AND have a business workspace. They are not mutually exclusive.

### 3.4 Call Room — updates

In the existing call room component, add the following **without modifying existing logic**:
- Before start: if user has an org → show optional "Associate to project" selector
- If `cloud_recording_enabled`: show "☁️ Recording" badge to all participants
  with a GDPR notice: "This call is being recorded"
- At end of call: if cloud recording is active → call
  `POST /api/business/rooms/:room_id/recording/complete` with audio blob

**Do not modify**: WebRTC logic, translation engines, consumer credit system,
existing components unrelated to the business layer.

---

## PHASE 4 — Marketing Site (website.voxtranslate.app)

Stack: Astro 5 + PocketBase on Railway.

### 4.1 New "For Business" page

Create page `/business` with:

**Hero**
- Headline: "VoxTranslate for Business"
- Subheadline: "Real-time multilingual communication for your team. History, transcripts, projects."
- Primary CTA: "Get started" → voxtranslate.app/business/onboarding
- Secondary CTA: "Contact us" → email/form

**Features section** (3 columns):
- 🏢 Team Workspace — Organize calls by project, keep full history
- 📝 Multilingual Transcripts — Every call transcribed and translatable into 84 languages
- 🔒 Privacy & Compliance — Audit log, configurable retention, EU storage

**B2B pricing section**
```
Business — €49/month
- X credits included/month
- Up to 20 members
- Unlimited history
- Transcripts included
- Email support

Enterprise — €199/month
- More credits included
- Unlimited members
- Compliance mode + audit log
- Configurable data retention
- SLA + priority support
- Contact us for setup
```

**Comparison table** vs Google Meet:
| Feature | VoxTranslate | Google Meet |
|---|---|---|
| Languages | 84 | 6 |
| Translated recording | ✅ | ❌ |
| Call history | ✅ | ❌ |
| Standalone | ✅ | ❌ (requires Workspace) |
| Price | From €49/org | From $14/user |
| Mobile support | ✅ | ❌ (coming later 2026) |

### 4.2 Homepage updates

- Add "For Business" section with link to the dedicated page
- Update navbar with "Business" link
- Update meta description to include the enterprise use case

### 4.3 Blog post draft (PocketBase)

Create a draft post: "VoxTranslate for Business: real-time translation for international teams"
- Intro: language barriers in distributed teams
- Solution: workspace with history and transcripts
- CTA: try for free

---

## PHASE 5 — Final verification

Before considering the work complete:

1. Run the **full existing test suite** → zero regressions
2. Run all new business tests → all green
3. Verify the consumer flow (call without org) works identically
4. Verify RLS: log in as two users from different orgs, confirm data isolation
5. Verify Stripe webhook for org subscription
6. Manual smoke test:
   - Create org → invite member → accept invite
   - Create project → start call associated to project
   - Enable cloud recording → end call → verify S3 upload
   - Request transcript → request translation (IT, DE) → verify cache on second call
   - Export transcript as TXT
   - Verify audit log entries when compliance_mode is active

---

## Constraints and final notes

- **Code language**: everything in English (variables, comments, commits)
- **Commits**: Conventional Commits for each completed phase
  e.g. `feat(business): add organizations schema and RLS policies`
- **No new dependencies** without first checking they don't already exist in the project
- **Org credits vs consumer credits**: they are separate. A consumer user with personal
  credits does NOT use org credits and vice versa. Business calls deduct org credits.
- **Supabase Storage bucket**: create `recordings` bucket with private policy
  (no public access, only service_role + signed URL)
- **Signed URL TTL**: 1 hour for recording playback, 15 minutes for exports
- **Deepgram diarization**: use `diarize=true` parameter — create a dedicated call
  in the transcripts module, do not alter the existing real-time engine calls
- **Do not touch**: `src/engines/`, `src/rooms/` core logic (only ALTER TABLE rooms),
  consumer credit system, auth flow, WebRTC logic

Start from PHASE 1 in plan mode: present the full schema and RLS policies
for approval before proceeding to any implementation.