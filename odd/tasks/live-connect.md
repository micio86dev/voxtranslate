# Live Connect (spec 0121)
Objective: two-phone realtime bidirectional voice translation (B2C owner-pays + guest; B2B permanent QR per desk, project-bound, searchable). Branch: `feature/live-connect` (off `develop`; dashboard = own feature branch in the submodule). Delivery strategy: `ask-on-risk`. Test runners: server `cargo test` (DB-gated: pgvector Postgres on :55432, confirm tests ran, not skipped), client `npm run test:unit` + Playwright, dashboard `npm run test:unit`. Plan: `~/.claude/plans/pasted-content-id-c16e-voxtranslate-linked-planet.md`. Engram mirror `odd/live-connect/tasks`: PENDING (project resolution was ambiguous from the session cwd).

## Specs
Quoted verbatim from the request (L1). `S#` is what tasks and tests reference.

- **S1 (existing feature untouched).** "The existing feature must remain available and retain its current behavior." / "Keep the existing "Parla con chiunque" entry point unchanged."
- **S2 (two phones, concurrent).** "Two people. Two separate phones. Two microphones. Two independent audio input streams. Two independent translation output streams. Simultaneous bidirectional communication. No manual turn-taking. Both participants can speak naturally." / "Both directions must operate independently and concurrently."
- **S3 (UX entry and flow).** "Add a separate Live Connect entry point to the VoxTranslate home screen." Live Connect provides: "Create a conversation. Join an existing conversation. Display connection status. Display participant status. Language selection. Microphone permission handling. Audio output selection where supported. Mute/unmute. End conversation. Connection recovery. Clear indication of the active translation direction. Appropriate loading and error states. Remaining credits or usage information when relevant." / "The interface must be mobile-first and responsive." / "Use existing design systems, components, styles, localization, and frontend conventions."
- **S4 (guest).** "The guest experience must be extremely simple and require no account creation." The guest must be able to: "Join without creating an account. Select their language. Grant microphone permission. Start communicating immediately."
- **S5 (B2C session and billing).** "The application generates: A temporary session identifier. A secure invitation URL. A QR code pointing to the invitation URL." / "The authenticated session creator is responsible for the AI usage charges." / "The guest must not need to purchase credits or create a VoxTranslate account."
- **S6 (B2B permanent QR).** "**A permanent QR code must never represent an already active conversation.**" / "Every scan must create a new temporary Live Connect session." Permanent QR codes must be: "Secure. Revocable. Regenerable. Printable. Downloadable as PNG, SVG, and PDF. Independent of the actual session lifecycle." / "Store secure token hashes where appropriate. Never expose account credentials or sensitive internal identifiers in QR codes." / "Support multiple locations and service desks belonging to the same business account."
- **S7 (language identification).** "Do not blindly translate every recognized sentence." / "JEV must not be assumed to be an audio language detector. If used, it should operate on appropriate recognized text and only when necessary. Avoid unnecessary AI calls that increase latency and cost." Scenarios: "The local participant speaks. The remote participant speaks. Both participants speak simultaneously. The microphone captures the phone's own translated playback. Background voices are present. The language detector is uncertain."
- **S8 (echo).** "The experience must be tested with: Two phones without headphones. Two phones with Bluetooth headphones. One phone using headphones and the other using speakers. Different physical distances. Noisy environments. Do not claim that every hardware configuration is supported unless verified." Use native browser capabilities (echo cancellation, noise suppression, automatic gain control). "Standard Bluetooth headphones and earbuds must work where supported, but headphones are optional."
- **S9 (real-time architecture).** "Do not introduce unnecessary infrastructure." Must support: "Independent audio streams. Bidirectional communication. Concurrent participants. Session lifecycle management. Connection state. Reconnection. Timeouts. Participant disconnection. Session termination. Error propagation. Usage metering." / "The system must remain compatible with the existing VoxTranslate deployment architecture." / "Document the reasons behind important architectural decisions."
- **S10 (reuse AI + billing).** "Do not create a parallel billing system." / "Do not create new subscription tiers or new plans specifically for Live Connect." / "Live Connect must integrate with the existing plans and credit system." B2C: "The authenticated session owner pays." B2B: "The business account pays through its existing wallet." Requirements: "Atomic credit consumption. No duplicate charges caused by retries. Accurate usage attribution. Real-time balance updates where appropriate. Usage history. Clear insufficient-credit behavior. Protection against concurrent sessions overspending the same balance." / "Never trust client-side usage calculations. The backend must be authoritative."
- **S11 (B2B project).** "Do not introduce an unrelated or parallel project system." / "A B2B Live Connect session must be associated with a project." / "A project association must be persisted and remain available after the session ends. Do not rely only on frontend state." / "Validate that the selected project belongs to the correct organization and that the current user has permission to access it."
- **S12 (transcripts).** Provide: "Original speech transcription. Translated transcription. Speaker identification. Language information. Timestamps. Session duration. Project association. Searchable conversation history. AI-generated summaries. Relevant conversation insights." / "Preserve the original language and translated content separately. Do not overwrite original transcripts with translated text." / "Speaker attribution must distinguish the two participants consistently."
- **S13 (search).** "Reuse the existing Meet search and conversation intelligence implementation." / "Do not build a second independent search engine." Respect "Organization isolation. Project permissions. User permissions. Data retention rules. Existing privacy and security policies." Filters: "Project. Date range. Language. Session. Speaker. Search terms." / "Do not invent AI search capabilities that the current architecture cannot safely support. Document any missing capabilities and implement them using the existing platform patterns."
- **S14 (B2B dashboard).** "Extend the existing B2B dashboard. Do not create a separate application unless the current architecture explicitly requires it." Session management, QR and location management, project and conversation management, wallet and usage (balance, live usage, history, by project, by location/service desk, by date range). "The dashboard must use authoritative backend data."
- **S15 (Directus).** "Directus is intended for platform superadmins and authorized internal operators." Account/organization, session, QR, usage/billing and conversation administration. "Do not expose sensitive conversation content to every Directus operator by default. Use role-based permissions and audit logs for privileged access. Never store or expose raw authentication tokens unnecessarily." / "Directus must not become a second business application."
- **S16 (data model).** "Avoid unnecessary tables and duplicated data." Must support: "B2C and B2B ownership. Organization isolation. Project associations. Multiple locations. Multiple concurrent sessions. Guest participants. Session expiration. Billing attribution. Conversation search. Future analytics." / "Use migrations compatible with the existing deployment workflow."
- **S17 (privacy and retention).** "Do not assume that every conversation should be permanently retained." / "Where audio recording is not explicitly required, do not introduce permanent raw audio storage." / "Ensure compliance with applicable privacy requirements, including GDPR."
- **S18 (tests and quality).** Backend, frontend, e2e and real-time audio tests as listed in the request; "Measure: Speech-to-translation latency. Translation-to-audio latency. End-to-end perceived latency. Audio interruptions. Reconnection behavior. Duplicate translations. Echo handling. Concurrent speaker behavior." "Use realistic test environments and document what is simulated versus verified with real devices." / "Maintain or improve existing test coverage. Do not weaken tests to make the build pass."
- **S19 (documentation).** Product requirements, architecture overview, ADRs, data model, API contracts, real-time communication, session lifecycle, billing behavior, B2B project integration, Directus administration, security model, privacy and retention, testing strategy, deployment and migration, operational troubleshooting. "Keep documentation synchronized with actual implementation."
- **S20 (engineering rules).** "Do not break the existing "Parla con chiunque" feature. Do not introduce new subscription tiers. Do not introduce a parallel wallet or billing system. Do not create a separate B2B project model. Do not create a second independent conversation search system if Meet already provides the required capabilities. Do not assume that Directus and the B2B dashboard share the same authorization model. Do not store raw audio permanently without an explicit product requirement. Do not trust frontend billing or authorization decisions. Do not sacrifice security or data integrity for implementation speed." / "Do not push directly to protected branches or bypass existing CI requirements."
- **S21 (definition of done).** The list under "18. DEFINITION OF DONE" in L1, in full.

Assumptions (not asked by the user, from the approved plan): WS-only transport (no WebRTC); desk QR token is HMAC(desk_id, version), never stored; B2C transcripts opt-in; 90-day default retention for Business-plan live sessions; 120-min session cap and 3 concurrent sessions per owner; Enhanced (Cartesia) excluded in v1.

## Tasks
Route: inline (parent writes, following this logbook) unless noted. Check off only after outcome and checks were observed.

- [x] T1 · S19,S9,S20 · inline · `specs/0121-live-connect/spec.md` (decisions as "Key decisions"), `docs/live-connect.md`, spec index row · commit: `b4542212`
- [x] T2 · S16,S17 · inline · one migration `065_live_connect.sql` (not three: one cohesive additive unit) + `tests/live_schema.rs` (own file instead of extending `business_schema.rs`) · 8/8 pass in 0.46 s on pgvector Postgres :55432 db `voxtranslate_voip_test` (not skipped) · commit: `0bcd996a`
- [x] T3 · S4,S6 · inline · `server/src/live/{mod,config,tokens}.rs` + `.env.example`; 21 unit tests incl. guest-token/account-JWT cross-rejection with a reused secret; `cargo clippy --lib --tests -D warnings` clean · commit: `1eab5f84`
- [x] T4 · S5,S11 · inline · live sessions REST (create/join/get/end) in `live/{sessions,routes,state}.rs` + engine `free_capacity()` + room-binding takeover fix (RED observed: 200 instead of 409, then GREEN) · `live_rest` 13/13, `business_calls` 10/10 · commits: `10436f1d` (fix), `3f131122` (REST), `447bbdb8` (test isolation)
- [x] T5 · S2,S9 · inline · `/ws/live` (`live/ws.rs`), sweeper (`live/sweeper.rs`), `SessionState` frame, capacity gate, server-side mute, idle/ping/frame/rate caps, reconnect supersede, persistence policy · `live_ws` 11/11 stable x3 (≈10.8 s, real mock traffic); full suite 74 binaries, 1987 passed, 0 failed; clippy `-D warnings` clean · commit: `cbfa125c`. NOT in T5 (deviations, see L5): EchoLedger, per-language mock extension.
- [x] T6 · S10 · inline · `live/billing.rs` (`charge_tick`, `Charge`, `LiveMeter`), `billing::deduct_usage_tx` extracted from `deduct_usage` (behavior preserved), owner balance sink in `LiveState`, `EndCause` end path, org gate at create and at Start · `live_billing` 6/6 (mutation check: disabling the duplicate guard turns 2 tests red), `live_ws` 14/14 stable, clippy `-D warnings` clean, full suite 75 binaries green except one pre-existing flake · commit: `3634e04e` · **Risk: item 1 (money ledger writes, migration 065), item 4 (row-locked concurrent debits).** Independent verification DONE (see L7; commits `4724451c`, `05f17173`); native review BLOCKED: `gentle-ai review status` stops with `managed_assets_outdated`, whose continuation is `gentle-ai sync --agent claude-code` (rewrites the managed Claude assets under the user's home) — not run without the user's go-ahead.
- [x] T7 · S6,S14 · inline · `live/desks.rs`: admin CRUD + revoke/regenerate/QR download (QR = derived HMAC, never stored; revoke/regenerate also close waiting sessions), public scan (new session every time, identical answer for forged/unknown/revoked/replaced QR, wallet state never disclosed, per-IP + per-desk rate limits, `LIVE_DESK_MAX_WAITING`), staff queue, answer (exactly one colleague, unique seat), `my-desks` · `live_desks` 9/9 incl. a real WS conversation under the org/project; clippy `-D warnings` clean · commit: `b6b823b6`. Dashboard-facing data endpoints (sessions list/detail/usage) are still to do (T7b).
- [ ] T8 · S12,S13,S17 · inline · finalize→`transcripts`+embeddings, purge-rule fix, `kind`/language/date filters, retention extension, session erasure, org-billed summary · commit: —
- [x] T9 · S1,S3 · delegated (context backstop) · `scripts/qr.ts` extracted verbatim from `app.ts` (webinar QR unchanged, unit-tested), `#hero-live` + avatar entry (`#hero-talk` untouched), 5-entry hero grid, `en.json` + 83 locales (77 `live*` keys, generated by 6 parallel translators and validated by `apply-translations.py`: key set, placeholders, no reformatting; I re-verified parity and placeholders for all 83) · commits: `fac770e3` (qr), `0a120f82` (i18n)
- [x] T10 · S2,S3,S4,S8 · delegated · `pages/live.astro` (host), `pages/lc/[code].astro` (guest, `prerender=false`, noindex, no-referrer), `scripts/live/*` (api, pure session-machine, conversation, view, host/guest page glue, session store, messages, language picker) · client: 116 files / 2358 tests green (scripts/live 94.9% lines), `astro check` 0 errors, `npm run build` ok, thresholds met; the writer also drove real Chrome against `astro dev` with a scripted fake `/ws/live` and fake media devices (join, mute, reconnect with the same token, end, 320 px, RTL, axe clean) — NOT Playwright e2e against the server, NOT real devices · commit: `c2ba6c0c`. Desk-scan page and staff surfaces are T12.
- [ ] T11 · S14 · delegated candidate (submodule, disjoint from T10) · dashboard live area, desk console, QR assets, usage, 5 locales, tests · commit: —
- [ ] T12 · S6 · inline · main-app staff "Desks" list · commit: —
- [ ] T13 · S15 · inline · `directus/setup-live-connect.mjs`, roles, flows, `/api/admin/live/*` · commit: —
- [ ] T14 · S8,S18,S19,S21 · inline · hardening: pentest, k6, latency metrics, device matrix doc, runbook, deploy notes · commit: —

## Log
- **L1 (2026-11-19, user, original request, verbatim).**

````text
# VOXTRANSLATE — LIVE CONNECT
## Full Technical Audit, Architecture, Implementation Plan & Autonomous Development

You are working on the existing VoxTranslate platform.

Your mission is to analyze, architect, implement, test, and integrate a new major feature called **Live Connect**, while preserving and reusing as much of the existing platform as possible.

This is not a greenfield project.

**The first priority is to understand the existing codebase, architecture, business logic, infrastructure, and user experiences before making any implementation decisions.**

Do not assume that a feature is missing simply because it is not immediately visible. Search the entire codebase, trace existing implementations, inspect APIs, database models, services, frontend flows, integrations, and tests.

The final result must be a production-quality feature integrated into VoxTranslate, not a standalone prototype.

---

# 1. EXECUTION STRATEGY

## Phase A — Comprehensive audit (Opus, Plan mode)

Start in Plan mode.

Do not modify application code during the initial audit.

Inspect all relevant repositories and modules available in the current workspace.

You must investigate:

- Backend architecture and existing services.
- Existing STT, TTS, translation, language detection, and streaming implementations.
- Existing real-time communication infrastructure.
- Existing meeting/conversation functionality.
- Existing wallet, credits, billing, and usage accounting.
- Authentication, authorization, organizations, and user roles.
- Existing B2B account and project management.
- Existing conversation history, transcripts, indexing, and search.
- Existing frontend applications and shared components.
- Existing backoffice and Directus configuration/extensions.
- Existing tests, CI/CD, development conventions, and documentation.

Do not duplicate existing capabilities.

For every relevant capability, classify it as:

1. Reusable without changes.
2. Reusable with extensions.
3. Requires a new implementation.
4. Requires refactoring before reuse.

Provide concrete evidence: repository paths, classes, functions, API endpoints, database tables, components, and existing tests.

Do not invent implementation details or assume undocumented behavior.

## Phase B — Architecture and implementation plan (Opus, Plan mode)

After the audit, produce a complete implementation plan.

The plan must include:

- Architecture and component boundaries.
- Backend changes.
- Frontend changes.
- Backoffice changes.
- Directus superadmin functionality.
- B2B dashboard functionality.
- Database migrations and data models.
- API contracts.
- Real-time communication design.
- Security and authorization.
- Billing and wallet integration.
- Conversation persistence and search.
- Testing strategy.
- Deployment and migration considerations.
- Rollout and backward compatibility.

Identify dependencies between tasks and organize implementation into independently testable milestones.

Create or update the appropriate project documentation, architecture decisions (ADR), and implementation plan, following existing repository conventions.

**Do not start implementation until the audit and plan are complete.**

## Phase C — Autonomous implementation (Sonnet 5.5)

Once the plan has been reviewed and execution begins, implement the complete feature.

Work through all milestones without stopping after scaffolding, initial architecture, or a basic MVP.

Do not repeatedly ask for confirmation for routine technical decisions.

Use engineering judgment, following the existing project conventions and documented architecture.

If a genuine blocker or architectural contradiction is discovered:

- Document the issue.
- Explain its impact.
- Propose the safest solution.
- Continue with independent work wherever possible.

Do not silently remove existing functionality or make unrelated breaking changes.

---

# 2. PRODUCT DEFINITION

Live Connect is a new real-time, bidirectional voice translation experience.

It complements the existing VoxTranslate functionality currently called:

**"Parla con chiunque"**

The existing feature must remain available and retain its current behavior.

## Existing functionality: Parla con chiunque

This is a turn-based conversation using a single phone.

- Both people use the same phone.
- The phone's microphone captures both speakers.
- One person speaks.
- The application processes and translates the speech.
- The other person waits for the translation.
- They then exchange turns.

This functionality is already implemented.

Audit it carefully and reuse its underlying capabilities where appropriate, but do not change its interaction model.

## New functionality: Live Connect

Live Connect provides a different experience:

- Two people.
- Two separate phones.
- Two microphones.
- Two independent audio input streams.
- Two independent translation output streams.
- Simultaneous bidirectional communication.
- No manual turn-taking.
- Both participants can speak naturally.

Example:

An Italian tourist wants to communicate with a Spanish-speaking person.

1. The Italian speaker talks into their phone.
2. Their speech is recognized and translated into Spanish.
3. The Spanish participant hears the Spanish translation on their device.
4. The Spanish participant responds in Spanish.
5. Their speech is translated into Italian.
6. The Italian participant hears the Italian translation.

Both directions must operate independently and concurrently.

The target experience is a natural conversation with minimal latency.

Standard Bluetooth headphones and earbuds must work where supported, but headphones are optional.

The feature must also work using the phone's built-in microphone and speakers, subject to browser and device limitations.

---

# 3. APPLICATION NAVIGATION AND USER EXPERIENCE

Add a separate Live Connect entry point to the VoxTranslate home screen.

Keep the existing "Parla con chiunque" entry point unchanged.

The two features must have distinct UX flows.

Live Connect should provide:

- Create a conversation.
- Join an existing conversation.
- Display connection status.
- Display participant status.
- Language selection.
- Microphone permission handling.
- Audio output selection where supported.
- Mute/unmute.
- End conversation.
- Connection recovery.
- Clear indication of the active translation direction.
- Appropriate loading and error states.
- Remaining credits or usage information when relevant.

The interface must be mobile-first and responsive.

The guest experience must be extremely simple and require no account creation.

Use existing design systems, components, styles, localization, and frontend conventions.

Do not introduce a new visual design system if one already exists.

---

# 4. LIVE CONNECT SESSION CREATION AND GUEST ACCESS

There are two main access scenarios.

## 4.1 Personal B2C sessions

An authenticated VoxTranslate user creates a Live Connect session.

The application generates:

- A temporary session identifier.
- A secure invitation URL.
- A QR code pointing to the invitation URL.

The other participant scans the QR code or opens the link.

The guest must be able to:

- Join without creating an account.
- Select their language.
- Grant microphone permission.
- Start communicating immediately.

The authenticated session creator is responsible for the AI usage charges.

The guest must not need to purchase credits or create a VoxTranslate account.

## 4.2 Business B2B sessions

Businesses must be able to create permanent QR codes associated with specific locations or service desks.

Examples:

- Hotel reception.
- Concierge.
- Car rental counter.
- Restaurant.
- Shop.
- Customer support desk.

A permanent QR code identifies:

- The business account.
- The associated project, when applicable.
- The specific location or service desk.
- The configuration associated with that entry point.

**A permanent QR code must never represent an already active conversation.**

Every scan must create a new temporary Live Connect session.

Permanent QR codes must be:

- Secure.
- Revocable.
- Regenerable.
- Printable.
- Downloadable as PNG, SVG, and PDF.
- Independent of the actual session lifecycle.

Store secure token hashes where appropriate. Never expose account credentials or sensitive internal identifiers in QR codes.

Support multiple locations and service desks belonging to the same business account.

---

# 5. LANGUAGE DETECTION AND AUDIO PROCESSING

This is one of the most critical engineering areas.

Each device must process its own microphone input independently.

For each direction, the system must implement the necessary pipeline using the existing VoxTranslate services wherever possible:

1. Audio capture.
2. Voice activity detection.
3. Speech recognition.
4. Language identification.
5. Translation.
6. Speech synthesis.
7. Audio delivery to the remote participant.

Investigate the existing STT/TTS providers and determine their actual streaming capabilities.

Do not assume that existing implementations already support incremental streaming.

Where supported, prefer incremental transcription, translation, and speech generation to minimize perceived latency.

## Language identification

Each participant has a selected target language.

The application must identify the language spoken in each audio stream and determine whether the utterance should be translated.

Important scenarios:

- The local participant speaks.
- The remote participant speaks.
- Both participants speak simultaneously.
- The microphone captures the phone's own translated playback.
- Background voices are present.
- The language detector is uncertain.

Do not blindly translate every recognized sentence.

Implement a robust language decision strategy using:

- Existing provider language detection capabilities.
- Confidence scores where available.
- Conversation language configuration.
- Context from previous utterances.
- JEV API where it provides meaningful additional value.

JEV must not be assumed to be an audio language detector.

If used, it should operate on appropriate recognized text and only when necessary.

Avoid unnecessary AI calls that increase latency and cost.

## Echo and audio feedback

Pay particular attention to acoustic feedback.

The phone may capture the translated speech being played through its own speaker.

Investigate:

- Browser echo cancellation.
- Noise suppression.
- Automatic gain control.
- Audio output routing.
- Playback-aware microphone handling.
- Duplicate transcription prevention.
- Audio buffering.
- VAD thresholds.

Use native browser capabilities where appropriate.

The experience must be tested with:

- Two phones without headphones.
- Two phones with Bluetooth headphones.
- One phone using headphones and the other using speakers.
- Different physical distances.
- Noisy environments.

Do not claim that every hardware configuration is supported unless verified.

---

# 6. REAL-TIME COMMUNICATION ARCHITECTURE

Audit existing real-time infrastructure before selecting a transport.

Investigate existing WebSocket, WebRTC, SSE, queues, streaming services, and audio-provider integrations.

Choose the most appropriate architecture based on actual requirements.

Evaluate WebRTC where useful for audio transport, and WebSockets for signaling, session events, and coordination.

Do not introduce unnecessary infrastructure.

The architecture must support:

- Independent audio streams.
- Bidirectional communication.
- Concurrent participants.
- Session lifecycle management.
- Connection state.
- Reconnection.
- Timeouts.
- Participant disconnection.
- Session termination.
- Error propagation.
- Usage metering.

The system must remain compatible with the existing VoxTranslate deployment architecture.

Document the reasons behind important architectural decisions.

---

# 7. REUSE EXISTING AI AND BILLING SERVICES

Reuse the existing VoxTranslate infrastructure for:

- STT.
- Translation.
- TTS.
- Language detection.
- JEV integration.
- AI provider configuration.
- Usage accounting.
- Wallets and credits.
- Authentication.
- Organization management.

Do not create a parallel billing system.

Do not create new subscription tiers or new plans specifically for Live Connect.

VoxTranslate already has three existing tiers.

Live Connect must integrate with the existing plans and credit system.

The feature is available to consumer and business users, subject to the existing account entitlements and usage rules.

## Billing rules

For B2C:

- The authenticated session owner pays.
- The guest is not charged.
- All usage must be attributed to the correct owner.

For B2B:

- The business account pays through its existing wallet.
- All sessions belonging to that business must be correctly attributed.
- Multiple service desks can share the same business wallet.

Implement reliable usage accounting.

Requirements:

- Atomic credit consumption.
- No duplicate charges caused by retries.
- Accurate usage attribution.
- Real-time balance updates where appropriate.
- Usage history.
- Clear insufficient-credit behavior.
- Protection against concurrent sessions overspending the same balance.

Never trust client-side usage calculations.

The backend must be authoritative.

---

# 8. B2B PROJECT INTEGRATION — CRITICAL REQUIREMENT

Live Connect must integrate with the existing VoxTranslate B2B project model.

Do not introduce an unrelated or parallel project system.

First, audit how existing meetings are associated with projects.

Investigate the current implementation of:

- Project creation and management.
- Meeting/project relationships.
- Conversation persistence.
- Transcripts.
- Meeting summaries.
- Search.
- AI-generated insights.
- Knowledge retrieval.
- Existing project-level permissions.

**Live Connect must reuse the same project association and conversation intelligence capabilities already available for meetings wherever possible.**

## Project association

A B2B Live Connect session must be associated with a project.

Depending on the existing architecture, this association may happen through:

- The permanent QR code configuration.
- The session creation interface.
- The business dashboard.

A project association must be persisted and remain available after the session ends.

Do not rely only on frontend state.

Validate that the selected project belongs to the correct organization and that the current user has permission to access it.

## Conversation history

For B2B sessions, investigate and implement the same conversation history capabilities available for existing meetings.

Where supported by the current platform, Live Connect conversations should provide:

- Original speech transcription.
- Translated transcription.
- Speaker identification.
- Language information.
- Timestamps.
- Session duration.
- Project association.
- Searchable conversation history.
- AI-generated summaries.
- Relevant conversation insights.

Preserve the original language and translated content separately.

Do not overwrite original transcripts with translated text.

Speaker attribution must distinguish the two participants consistently.

## Search and information retrieval

This is a major product requirement.

Business users must be able to search previous Live Connect conversations and retrieve information about what was discussed.

Examples:

- "What did the customer ask about the rental deposit?"
- "Did the guest mention an arrival time?"
- "What problems were reported by customers this week?"
- "Find conversations where customers asked about parking."
- "What did this customer say about their reservation?"
- "Summarize the recurring questions from this project's conversations."

Reuse the existing Meet search and conversation intelligence implementation.

If meetings already support semantic search, RAG, vector embeddings, AI summaries, or project knowledge retrieval, extend those capabilities to Live Connect.

Do not build a second independent search engine.

The search must respect:

- Organization isolation.
- Project permissions.
- User permissions.
- Data retention rules.
- Existing privacy and security policies.

Where appropriate, support filtering by:

- Project.
- Date range.
- Language.
- Session.
- Speaker.
- Search terms.

The result should provide useful references to the original conversation, including timestamps where supported.

Do not invent AI search capabilities that the current architecture cannot safely support. Document any missing capabilities and implement them using the existing platform patterns.

---

# 9. B2B DASHBOARD

Extend the existing B2B dashboard.

Do not create a separate application unless the current architecture explicitly requires it.

Add a dedicated Live Connect management area.

Expected capabilities:

## Session management

- View active sessions.
- View past sessions.
- View session details.
- See connected participants.
- See source and target languages.
- See duration.
- See associated project.
- See session status.

## QR and location management

- Create permanent QR codes.
- Associate QR codes with projects.
- Associate QR codes with locations or service desks.
- Rename locations.
- Disable and revoke QR codes.
- Regenerate QR codes.
- Download printable QR assets.
- Configure default language preferences where appropriate.

## Project and conversation management

- Browse Live Connect conversations by project.
- Open individual conversations.
- View original and translated transcripts.
- Search conversations.
- Ask questions about past conversations using the existing AI search/Meet intelligence capabilities.
- View AI-generated summaries where supported.
- Access relevant timestamps and conversation references.

## Wallet and usage

Display:

- Current business wallet balance.
- Live usage during active sessions.
- Historical usage.
- Consumption by project.
- Consumption by location/service desk.
- Consumption by date range.

Real-time updates should use existing application infrastructure where possible.

The dashboard must use authoritative backend data.

---

# 10. DIRECTUS SUPERADMIN BACKOFFICE

This is a separate and important administrative scope.

Audit the existing Directus implementation and extend it for complete Live Connect administration.

Do not assume that the B2B dashboard and Directus have the same audience or permissions.

Directus is intended for platform superadmins and authorized internal operators.

Implement the necessary collections, relationships, permissions, interfaces, and extensions following existing conventions.

Expected administrative capabilities:

## Account and organization administration

- List and inspect business accounts.
- Inspect Live Connect entitlements.
- Inspect associated projects.
- Inspect locations and permanent QR configurations.
- Inspect wallet attribution and usage.
- Investigate session-related issues.

## Session administration

- List Live Connect sessions across the platform.
- Filter by business, user, project, date, status, and language.
- Inspect session metadata.
- Inspect participants.
- Inspect duration and usage.
- Inspect connection errors and relevant diagnostic information.
- Inspect billing attribution.

## QR administration

- Inspect permanent QR configurations.
- Identify their associated organization and project.
- Inspect active/revoked status.
- Revoke compromised QR configurations.
- Regenerate credentials where authorized.

## Usage and billing administration

- Inspect credit consumption.
- Inspect wallet transactions.
- Reconcile session usage against billing records.
- Identify failed or inconsistent billing operations.
- Investigate duplicate or missing charges.

## Conversation administration

Provide authorized superadmins with the appropriate tools to investigate conversation records and search-related issues.

Respect existing privacy policies and access controls.

Do not expose sensitive conversation content to every Directus operator by default.

Use role-based permissions and audit logs for privileged access.

Never store or expose raw authentication tokens unnecessarily.

Directus must not become a second business application. Keep customer-facing workflows in the existing B2B dashboard and use Directus for platform-level administration.

---

# 11. DATA MODEL AND PERSISTENCE

Analyze the existing schema before designing new tables or collections.

Reuse existing entities where appropriate.

Potential conceptual entities include:

- Live Connect sessions.
- Participants.
- Permanent QR configurations.
- Temporary invitation tokens.
- Session events.
- Audio/translation usage records.
- Conversation transcripts.
- Transcript segments.
- Project associations.
- Session diagnostics.

These are conceptual requirements, not instructions to create every entity independently.

Avoid unnecessary tables and duplicated data.

The data model must support:

- B2C and B2B ownership.
- Organization isolation.
- Project associations.
- Multiple locations.
- Multiple concurrent sessions.
- Guest participants.
- Session expiration.
- Billing attribution.
- Conversation search.
- Future analytics.

Preserve referential integrity.

Implement database indexes based on actual query patterns.

Use migrations compatible with the existing deployment workflow.

---

# 12. PRIVACY, SECURITY AND DATA RETENTION

Treat conversations as potentially sensitive data.

Audit the existing VoxTranslate privacy and retention policies.

Implement appropriate controls for:

- Guest access.
- Session authorization.
- QR security.
- Token expiration.
- Organization isolation.
- Project permissions.
- Transcript access.
- Administrative access.
- Data deletion.
- Data retention.
- Encryption in transit.
- Secure storage of sensitive information.

Do not assume that every conversation should be permanently retained.

Determine the correct retention behavior based on existing product policies and configuration.

Where audio recording is not explicitly required, do not introduce permanent raw audio storage.

Persist only the data needed for the supported features and business requirements.

Ensure compliance with applicable privacy requirements, including GDPR.

---

# 13. FRONTEND APPLICATIONS

Audit all existing frontends before making changes.

Identify which frontend applications need modifications and which existing shared components can be reused.

Potential areas include:

- Main VoxTranslate application.
- Mobile-responsive application experience.
- Guest access interface.
- B2B dashboard.
- Existing meeting/conversation interface.
- Directus extensions and interfaces.

Follow existing frontend architecture and framework conventions.

Do not introduce unnecessary frameworks or dependencies.

The guest interface must be lightweight and optimized for mobile browsers.

Implement:

- Permission handling.
- Device compatibility checks.
- Audio device handling where supported.
- Responsive layout.
- Accessible controls.
- Loading and connection states.
- Localization.
- Error recovery.

Ensure that Live Connect works without requiring a native application installation.

---

# 14. TESTING AND QUALITY REQUIREMENTS

This is production software.

Do not consider the feature complete simply because the happy path works.

Follow the existing testing conventions and extend them.

## Backend tests

Cover:

- Session creation.
- Session authorization.
- Guest access.
- QR validation.
- Session expiration.
- Participant management.
- Organization isolation.
- Project association.
- Billing attribution.
- Atomic wallet consumption.
- Duplicate event protection.
- Transcript persistence.
- Search permissions.
- Session lifecycle.

## Frontend tests

Cover:

- Main entry points.
- Session creation.
- QR/link joining.
- Guest onboarding.
- Language selection.
- Microphone permissions.
- Connection state.
- Reconnection.
- Session termination.
- Error handling.

Add end-to-end tests for critical user journeys.

## Real-time audio tests

Test actual concurrent communication.

Measure:

- Speech-to-translation latency.
- Translation-to-audio latency.
- End-to-end perceived latency.
- Audio interruptions.
- Reconnection behavior.
- Duplicate translations.
- Echo handling.
- Concurrent speaker behavior.

Use realistic test environments and document what is simulated versus verified with real devices.

## Quality gates

Follow the project's existing engineering standards.

At minimum:

- Strict TypeScript where applicable.
- No unnecessary `any`.
- SOLID principles.
- Clear separation of concerns.
- Reusable services.
- Input validation.
- Secure authorization.
- Unit tests for business logic.
- Integration tests for service boundaries.
- Frontend E2E tests for critical flows.
- No regressions in existing functionality.
- Successful type checking.
- Successful builds.
- CI-compatible test execution.

Maintain or improve existing test coverage. Do not weaken tests to make the build pass.

---

# 15. DOCUMENTATION

Create or update the appropriate documentation following existing repository conventions.

Required documentation should cover:

- Product requirements.
- Architecture overview.
- ADRs.
- Data model.
- API contracts.
- Real-time communication.
- Session lifecycle.
- Billing behavior.
- B2B project integration.
- Directus administration.
- Security model.
- Privacy and retention.
- Testing strategy.
- Deployment and migration.
- Operational troubleshooting.

Include diagrams where useful.

Keep documentation synchronized with actual implementation.

---

# 16. IMPLEMENTATION MILESTONES

Organize the implementation into logical, independently testable milestones.

Suggested order:

**M0 — Existing platform audit**
- Complete codebase investigation.
- Reuse matrix.
- Dependency analysis.
- Existing Meet intelligence investigation.
- Frontend and backend architecture review.

**M1 — Architecture and foundation**
- Final architecture.
- ADRs.
- Database model.
- API contracts.
- Session lifecycle.
- Security model.

**M2 — Live Connect backend**
- Session management.
- Guest access.
- Signaling.
- Real-time communication.
- Independent audio streams.
- AI pipeline integration.

**M3 — Live Connect frontend**
- Main application entry point.
- Session creation.
- Guest joining.
- Mobile experience.
- Audio controls.
- Connection handling.

**M4 — Conversation intelligence**
- Transcript persistence.
- Original and translated text.
- Speaker attribution.
- Project association.
- Reuse of existing Meet search and AI capabilities.

**M5 — Wallet and billing**
- Existing credit system integration.
- B2C billing.
- B2B billing.
- Atomic usage accounting.
- Real-time balance updates.

**M6 — B2B dashboard**
- Session management.
- Permanent QR management.
- Project association.
- Conversation history.
- Search and AI retrieval.
- Usage analytics.

**M7 — Directus superadmin**
- Administrative collections.
- Interfaces.
- Permissions.
- Session inspection.
- Billing investigation.
- QR administration.
- Auditing.

**M8 — Hardening and release**
- Security.
- Performance.
- Real-device testing.
- Regression tests.
- Deployment.
- Documentation.
- Production readiness.

Adjust this order if the audit identifies better dependencies.

Do not skip milestones or mark incomplete work as complete.

---

# 17. IMPORTANT ENGINEERING RULES

1. Analyze before implementing.
2. Reuse existing services before creating new ones.
3. Do not duplicate Meet functionality.
4. Do not break the existing "Parla con chiunque" feature.
5. Do not introduce new subscription tiers.
6. Do not introduce a parallel wallet or billing system.
7. Do not create a separate B2B project model.
8. Do not create a second independent conversation search system if Meet already provides the required capabilities.
9. Do not assume that Directus and the B2B dashboard share the same authorization model.
10. Do not store raw audio permanently without an explicit product requirement.
11. Do not trust frontend billing or authorization decisions.
12. Do not sacrifice security or data integrity for implementation speed.
13. Avoid unnecessary infrastructure and dependencies.
14. Prefer incremental, backward-compatible changes.
15. Keep the implementation maintainable and production-ready.

Use the existing Git workflow and repository conventions.

Work in appropriate feature branches and keep changes logically organized.

Do not push directly to protected branches or bypass existing CI requirements.

---

# 18. DEFINITION OF DONE

Live Connect is complete only when:

- It is available as a separate feature in the VoxTranslate home screen.
- Existing "Parla con chiunque" continues to work unchanged.
- Two devices can join the same session.
- Both participants can speak independently.
- Both directions can be translated concurrently.
- Guest access works without registration.
- Temporary QR/link invitations work securely.
- Permanent B2B QR configurations work.
- Existing STT, translation, TTS, and AI services are reused appropriately.
- Language detection and audio feedback are handled robustly.
- B2C and B2B billing work through the existing wallet system.
- B2B sessions can be associated with existing projects.
- Original and translated transcripts are persisted according to policy.
- Existing Meet conversation intelligence and search capabilities are reused.
- B2B users can search conversations and retrieve information about what was said.
- The B2B dashboard provides session, QR, project, search, and usage management.
- Directus provides the necessary superadmin administration capabilities.
- Authorization, privacy, and organization isolation are enforced.
- Critical automated tests pass.
- Real-device audio behavior has been tested and documented.
- Documentation reflects the actual implementation.
- Production deployment and migration requirements are documented.

---

# FINAL INSTRUCTION

Begin with **Phase A: Comprehensive Audit**.

Stay in Plan mode.

Do not write application code during the initial investigation.

Be particularly thorough when examining the existing Meet implementation, conversation intelligence, project relationships, transcript search, AI retrieval, wallet, and Directus administration.

The objective is to build Live Connect as a natural extension of the existing VoxTranslate ecosystem.

**Do not build what already exists. Extend what works. Improve what is necessary. Implement what is genuinely missing.**

Once the complete audit and implementation plan are ready, present the plan with concrete file-level changes, dependencies, risks, milestones, and verification criteria.

After the plan is reviewed, implementation will continue with Sonnet 5.5, following the approved plan through completion.
````

- **L2 (2026-11-19, user).** The Scheda SSD checkout was corrupted; the user chose copy + GitHub recovery ("Puoi spostare l'intera cartella di VoxTranslate sul Desktop e lavorare da lì"). Working copy is `~/Desktop/VoxTranslate/app`. No unpushed work expected by the user; the SSD's `recording.rs` imports `crate::r2`/`recording_store`, never committed anywhere (not on any remote branch or in the readable copy); left alone.
- **L3 (2026-11-19, user).** Staff side of a desk scan: "Entrambe" (dashboard Desk console AND main-app Desks list).
- **L4 (2026-11-19).** Plan approved as written (see plan file). Open defaults the user may still change: B2C transcripts opt-in; 90-day default retention for Business live sessions; Premium selectable for signed-in owners; dashboard deps `qrcode` + `jspdf`; 120 min session cap and 3 concurrent sessions per owner.
- **L5 (2026-11-19, implementation decisions, not user input).**
  1. **EchoLedger deferred (S7/S8).** The plan listed a server-side echo ledger in M2. Building it needs a per-session shared registry and interception of every outgoing frame of the *other* peer, for a defence whose value is unknown until real devices are measured. Client-side guarded mode (M3) is the first layer; the ledger is revisited in T14 with measurements. Recorded so it is not silently dropped.
  2. **Purge rule needed no change.** The plan said to fix `finalize_session` for guest-only B2B sessions. The rule deletes a `call_sessions` row only when no *signed-in* participant took part; in Live Connect the owner side (host/staff) is always a signed-in member, so only a desk session nobody answered (no content) can be purged. M4 item removed.
  3. **Presence needs no in-memory registry.** The database is the truth for "has it ended" (3 s check per socket) and the sweeper reads room presence from `RoomManager`. Valid because the server is single-replica (documented); the sweeper is scoped per session in tests because they share one DB.
  4. **Interpretation of S11 ("must be associated with a project"):** a REST-created B2B session requires `project_id` (`project_required`), validated against the org and not archived. Desk creation (T7) will require a project too, although `live_desks.project_id` is nullable in the schema (ON DELETE SET NULL when a project is deleted).
  5. **Billing is a placeholder until T6.** `/ws/live` charges the owner through the existing `spawn_meter` (not idempotent per tick). The feature stays off (`LIVE_CONNECT_ENABLED=0`) until T6 replaces it.
  6. **Pre-existing test flaw found:** `room_binding_is_inherited_by_new_call` used a fixed room, so it only passed on a fresh DB once the takeover guard existed; fixed with a per-run room.
  7. **Environment:** local DB is the repo's own `docker compose up -d --wait postgres` (pgvector, :55432, db `voxtranslate_voip_test`); build needs the rustup 1.97 toolchain first on PATH and `MACOSX_DEPLOYMENT_TARGET=26.0` (memory note). `server/.cargo/config.toml` copied from the SSD is untracked and must never be committed.
- **L6 (2026-11-19, observations, not user input).** Pre-existing flake seen once in 4 runs: `voip::regulatory::tests::a_recent_creating_group_claim_is_left_alone` (global `reclaim_stale_groups` sweep on a DB shared by parallel test binaries). Unrelated to Live Connect; left untouched. Local env note: `~/Desktop/VoxTranslate/app/server/.cargo/config.toml` is an untracked copy from the SSD and is excluded from review inventories (`--untracked-scope=exclude`).
- **L7 (2026-11-19, independent verification of T4–T6, high-risk items 1 and 4).** Verifier (fresh agent, read-only, own probes) reported 6 defects; one correction batch + one limited recheck, per the bounded-correction rule.
  - **Fixed with durable RED→GREEN tests (all mutation-checked):** D1 a Start/Stop shorter than one interval was free (0 ticks for 4.8 s of speech) → opening charge at Start with a per-Start key; D2 mute lost on reconnect → `LiveState.muted` per (session, peer); D3 owner's account UUID sent to the guest in `room_joined`/`peer_joined` → room peers carry `user_id: None`; D5 `live-*` rooms could be pre-bound by a client / persistence could inherit a binding → `bind` rejects `live-*`, persistence off when the call row cannot be written; D6 per-host session cap now covers B2B, bidi characters stripped from names, uncharged org credits carried (`put_back`).
  - **N1 (found by the recheck, introduced by my D1 fix):** skipping a tick while muted let a client mute around every tick and translate for free (0 charges while the engine heard audio). Fixed again: billing follows **audio frames actually forwarded** per interval (`activity` counter), not mute state; test reproduces the exploit (0 charged on the old logic) and passes on the fix.
  - **Open, tracked:** D4 erasure leaves guest names/speech (`call_sessions`, `session_participants`, `transcript_events`) because the FK points from `live_sessions` to `call_sessions` (T8: delete the call sessions listed in `room_session` events in the erase path and in retention). N3 (nit): live participants no longer count as "online" for friend-ring suppression because room peers carry no `user_id` — accepted for now. N2 accepted: each Start/reconnect prepays one interval. Not stripped by `clean_name`: U+061C, U+200B, U+2028/9.
  - **Not verified (by design / environment):** real two-device and Bluetooth behaviour, multi-replica sweeper, Directus interplay, desks/QR/search/retention/client (later tasks). Native `gentle-ai` review not run (blocked on `gentle-ai sync`, awaiting the user).
  - Cleanup owed: the verifier left probe crates under `…/scratchpad/verifier/` (scratch, safe to delete) and rows in the local test DB.
- **L8 (2026-11-19, client delegation outcome and open points, not user input).**
  1. **Probable pre-existing bug in "Parla con chiunque" (S1 — NOT touched, needs a decision):** `client/src/scripts/talk/conversation.ts` stores `setTimeout`/`clearTimeout` on the instance and calls `this.setTimeoutImpl(...)`. In browsers that throws `Illegal invocation` (it did in the new Live engine until fixed with arrow wrappers; unit tests under Node cannot see it). Talk's reconnect scheduling very likely has the same fault; the writer did not reproduce it on `/talk`. Left alone because the brief says Talk must stay unchanged; the fix is two lines.
  2. **Mute vs billing:** client mute sends `mute_audio` then `stop`, so the server pauses that seat's meter; unmute re-sends `start` (a new opening charge). Consistent with L7, slightly conservative for the user.
  3. **Output device selection works on Chromium only** (`pcm-playback.ts` keeps its `AudioContext` private; reached through a narrow cast). A proper `setOutputDevice` on `PcmPlayback` is the clean follow-up. iOS/Safari: control hidden.
  4. **Visual baselines will fail:** `e2e/visual.spec.ts-snapshots/home-*.png` include the hero, which gained a button. Regenerating them needs Playwright with `--update-snapshots` (T14).
  5. **Copy needing human/legal sign-off:** the guest notice ("Audio is never recorded… the person who invited you can choose to keep a text transcript") and, for B2B desks, a different notice (the transcript is kept and an org can read it). Translators flagged low confidence for: kk, nn, ig, mt, ckb, km, si, az, cy, eu, ht, ps, ml, ta, uz, ga, my, sw, zu, so, lb, am, ha, yo (a native skim is advisable before release).
  6. **iOS reload:** after a page reload the AudioContext may stay locked until a tap (no user gesture on rejoin).
  7. **Not built yet on the client:** the customer-facing desk scan page (`/lc/d/[token]`), the staff "Desks" list in the main app, and the dashboard Desk console (T12/T11).
