This Privacy Policy explains what personal data VoxTranslate processes when you use our real-time translated video-calling service, why we process it, and the rights you have. It is written to comply with the EU General Data Protection Regulation (GDPR) and similar laws.

## 1. Who is the data controller

The Service is operated by **Alessandro Micelli**, Puerto del Rosario, Spain ("VoxTranslate", "we", "us"), the controller for personal data processed through the Service. For any privacy request, contact privacy@voxtranslate.app.

## 2. What data we process

- **Account data** — when you sign in with Google, we receive your name, email address, and profile picture URL.
- **Audio (transient)** — while you speak, your microphone audio is streamed to the providers that power the engine chosen for that call: a speech-to-text provider on the Standard tier, and an end-to-end speech-translation provider on the Pro and Premium tiers. On the **Enhanced** tier your browser streams the audio **directly** to the provider using a short-lived access token, so it does not pass through our servers at all. We do not store raw audio.
- **Voice sample (optional)** — on the Enhanced tier you may record a short clip so your translated speech can be spoken in a voice resembling your own. The clip is sent to our voice provider, which creates the synthetic voice and returns an identifier that we store on your account. The feature is optional and can be used without it.
- **Transcripts & translations** — the text of speech and chat together with its translations. When a signed-in user takes part in a call, these are **stored** so participants can review, export (PDF/JSON), and AI-correct the transcript afterwards. Calls in which no signed-in user took part are not stored. Stored transcripts are deleted when you delete your account — your utterances are removed with it.
- **Chat messages & files** — chat is relayed and translated between participants. Files you attach are stored privately and shared with the call's participants through short-lived links.
- **Usage, analytics & billing data** — credit balance, transactions, per-session speaking time, and product-usage events (which features and plan tier you use, and for how long) used to meter, bill, secure, and improve the Service. Analytics are aggregated for reporting.
- **Safety data** — abuse reports you submit or that are submitted about you (which may include a short transcript excerpt), and moderation/ban records.
- **Technical data** — connection metadata needed to operate the real-time service, route media, ship operational logs, and keep the Service secure.

Video and audio between participants are sent peer-to-peer (WebRTC) and are not routed through or recorded by our servers. When a direct connection cannot be established, media is relayed through a TURN server in encrypted form that the relay cannot read. Our server handles sign-in, signaling, the live speech-to-text stream, translation, chat relay, and — where enabled — transcript storage. On the Enhanced tier even the speech audio bypasses our server, travelling directly from your browser to the speech provider. On the Enhanced tier even the speech audio bypasses our server, travelling directly from your browser to the speech provider.

## 3. Why we process it and our legal bases

- Provide the call, transcription and translation — performance of a contract.
- Process the audio needed for live captions/translation — contract; and your consent given at sign-up.
- Store transcripts for your later review and export — contract; legitimate interests in providing call history.
- Metering, billing and fraud prevention — contract; legitimate interests.
- Product analytics to understand and improve the Service — legitimate interests.
- Safety, moderation and handling abuse reports — legitimate interests in a safe service; legal obligation.
- Keeping legally required transaction records — legal obligation.

Transcription and translation are automated (AI-based) and may be inaccurate; AI outputs are generated solely to provide the Service and are not used to train third-party models.

## 4. Service providers (processors / sub-processors)

We share personal data with the providers below strictly to operate the Service. Some are located outside the EEA; where that is the case we rely on appropriate safeguards such as the EU Standard Contractual Clauses.

- **Google** — sign-in (OAuth): name, email, profile picture; as Google Gemini, real-time speech translation on the **Premium** tier (streamed audio and transcript text, transient); and, with your consent only, Google Analytics 4 and Google Ads: usage and conversion events.
- **Meta** — with your consent only, the Meta Pixel: page and conversion events used to measure and target advertising.
- **Deepgram** — speech-to-text (Standard tier): streamed audio (transient).
- **Groq** — machine translation (Standard and Enhanced tiers): transcript text (transient).
- **OpenAI** — real-time speech translation (**Pro** tier): streamed audio and transcript text (transient).
- **Cartesia** — speech-to-text and speech synthesis on the **Enhanced** tier: audio streamed directly from your browser (transient, not routed through our servers) and, if you use voice cloning, the voice clip you record plus the resulting synthetic voice.
- **Stripe** — payment processing: billing details and payment data.
- **Supabase** — database and file storage: account, usage, billing and safety data, stored transcripts, and chat file attachments.
- **Cloudflare** — edge delivery and TURN media relay: connection metadata; relayed media stays encrypted and is not readable by the relay.
- **Resend** — transactional email (for example invitations and account notices): recipient email address.
- **Better Stack** — operational logging and uptime monitoring: technical/connection metadata.
- **Vercel** — frontend hosting: technical/connection data.
- **Railway** — backend hosting: technical/connection data.

## 5. How long we keep data

- **Audio:** processed in real time and not stored.
- **Voice sample (if you use voice cloning):** the synthetic voice is held by our voice provider and its identifier is stored on your account until you delete your account.
- **Voice sample (if you use voice cloning):** the synthetic voice is held by our voice provider and its identifier is stored on your account until you delete your account.
- **Transcripts & translations:** for calls with a signed-in participant, kept until you delete the call or your account; calls with only guests are not stored.
- **Account data:** kept while your account exists; deleted when you delete your account.
- **Chat file attachments:** kept while the related call/account exists and served through short-lived private links.
- **Billing/transaction records:** retained as required by applicable tax and accounting laws.
- **Safety/abuse reports and ban records:** retained for as long as needed to keep the Service safe and to comply with legal obligations.
- **Operational logs:** retained for a limited period for security and reliability.

## 6. Cookies and local storage

Some browser storage is strictly necessary to run the Service. Analytics and advertising are optional: they load **only after you accept them** on the cookie banner, never before, and you can change your mind at any time from **Cookie settings**.

**Strictly necessary** — no consent required under the ePrivacy rules, because they provide a service you asked for:

- a **session token** kept in your browser so you stay signed in;
- a **cookie-consent preference** remembering your choice on the cookie banner;
- minor **interface flags** (for example, remembering that you have already seen a feature hint).

**Analytics and advertising** — loaded only with your consent:

- **Google Analytics 4** — aggregated measurement of which features are used and for how long;
- **Google Ads** — conversion measurement, where enabled;
- **Meta Pixel** — measures the effect of our advertising and may be used to build advertising audiences.

Declining leaves only the strictly necessary storage listed above; the Service works exactly the same. Withdrawing a consent you had given stops further collection, and reloading the page drops the trackers already loaded into it.

## 7. Your rights

Subject to applicable law, you have the right to access, rectify, and erase your data; to receive it in a portable format; to restrict or object to certain processing; to withdraw consent at any time; and to lodge a complaint with a supervisory authority. You can exercise access and portability with **Download my data** and erasure with **Delete my account** in the Privacy & data panel inside the app, or email privacy@voxtranslate.app. If you are in Spain, the lead supervisory authority is the **Agencia Española de Protección de Datos (AEPD, www.aepd.es)**; you may also contact the data protection authority in your own country of residence.

## 8. Security

We use industry-standard measures to protect personal data, including encryption in transit and peer-to-peer media that does not transit our servers. No method of transmission or storage is completely secure, however, and we cannot guarantee absolute security.

## 9. Children

The Service is for adults (18+). We do not knowingly process data of children. If you believe a child has provided us data, contact us and we will delete it.

## 10. Google user data (Calendar) and Limited Use

When you sign in with Google and use VoxTranslate's meeting-scheduling feature, we access certain data from your Google Account: your basic profile (name, email address, profile picture) to create and identify your account, and your Google Calendar via the calendar.events scope. We use Calendar access solely to create, update, and delete the calendar events for the meetings you schedule through VoxTranslate, and to add the people you invite as attendees so Google can send them invitations and reminders. We only create and modify events that you create through VoxTranslate's scheduling feature; we never read, edit, or delete any of your other calendar entries. To keep your events in sync we store a Google refresh token, encrypted at rest, plus a minimal record of each meeting (title, time, room link, and the invitees you choose). You can disconnect Google Calendar at any time, which deletes the stored token.

VoxTranslate's use and transfer of information received from Google APIs to any other app will adhere to the [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy), including the Limited Use requirements. We do not use Google user data for advertising, we do not sell it, and we do not allow humans to read it unless we have your consent, it is necessary for security or to comply with applicable law, or the data has been aggregated and anonymised.

## 11. Changes

We may update this Policy; we will revise the version and date and, for material changes, take additional steps where required by law.
