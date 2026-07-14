Diese Datenschutzerklärung erläutert, welche personenbezogenen Daten VoxTranslate verarbeitet, wenn Sie unseren Dienst für Videoanrufe mit Echtzeit-Übersetzung nutzen, warum wir sie verarbeiten und welche Rechte Sie haben. Sie ist mit Blick auf die EU-Datenschutz-Grundverordnung (DSGVO) und ähnliche Gesetze verfasst.

## 1. Wer ist der Verantwortliche

Der Dienst wird betrieben von **Alessandro Micelli**, Puerto del Rosario, Spain („VoxTranslate“, „wir“, „uns“), dem Verantwortlichen für die über den Dienst verarbeiteten personenbezogenen Daten. Für alle Datenschutzanfragen wenden Sie sich an privacy@voxtranslate.app.

## 2. Welche Daten wir verarbeiten

- **Kontodaten** — wenn Sie sich mit Google anmelden, erhalten wir Ihren Namen, Ihre E-Mail-Adresse und die URL Ihres Profilbilds.
- **Audio (flüchtig)** — während Sie sprechen, wird Ihr Mikrofon-Audio an unseren Speech-to-Text-Anbieter und, im Premium-Tarif, an unseren Echtzeit-Übersetzungsanbieter gestreamt, um Live-Untertitel und -Übersetzungen zu erzeugen. Rohes Audio speichern wir nicht.
- **Transkripte und Übersetzungen** — der Text von Sprache und Chat zusammen mit seinen Übersetzungen. Wenn ein angemeldeter Nutzer an einem Anruf teilnimmt, werden diese **gespeichert**, damit die Teilnehmer das Transkript anschließend durchsehen, exportieren (PDF/JSON) und per KI korrigieren können. Anrufe, an denen kein angemeldeter Nutzer teilgenommen hat, werden nicht gespeichert. Gespeicherte Transkripte werden gelöscht, wenn Sie Ihr Konto löschen — Ihre Äußerungen werden damit entfernt.
- **Chat-Nachrichten und Dateien** — der Chat wird zwischen den Teilnehmern weitergeleitet und übersetzt. Von Ihnen angehängte Dateien werden privat gespeichert und über kurzlebige Links mit den Teilnehmern des Anrufs geteilt.
- **Nutzungs-, Analyse- und Abrechnungsdaten** — Guthabenstand, Transaktionen, Sprechzeit pro Sitzung und Produktnutzungsereignisse (welche Funktionen und welche Tarifstufe Sie nutzen und wie lange), die zur Messung, Abrechnung, Absicherung und Verbesserung des Dienstes verwendet werden. Analysedaten werden für die Berichterstattung aggregiert.
- **Sicherheitsdaten** — Missbrauchsmeldungen, die Sie einreichen oder die über Sie eingereicht werden (ggf. mit einem kurzen Transkriptauszug), sowie Moderations-/Sperrdatensätze.
- **Technische Daten** — Verbindungsmetadaten, die erforderlich sind, um den Echtzeitdienst zu betreiben, Medien zu routen, Betriebsprotokolle zu übermitteln und den Dienst sicher zu halten.

Video und Audio zwischen den Teilnehmern werden Peer-to-Peer (WebRTC) übertragen und weder über unsere Server geleitet noch von ihnen aufgezeichnet. Wenn keine direkte Verbindung hergestellt werden kann, werden Medien in verschlüsselter Form über einen TURN-Server weitergeleitet, die das Relay nicht lesen kann. Unser Server übernimmt die Anmeldung, das Signaling, den Live-Speech-to-Text-Stream, die Übersetzung, die Chat-Weiterleitung und — wo aktiviert — die Speicherung von Transkripten.

## 3. Warum wir sie verarbeiten und unsere Rechtsgrundlagen

- Bereitstellung von Anruf, Transkription und Übersetzung — Vertragserfüllung.
- Verarbeitung des für Live-Untertitel/Übersetzung erforderlichen Audios — Vertrag; und Ihre bei der Registrierung erteilte Einwilligung.
- Speicherung von Transkripten zur späteren Durchsicht und zum Export durch Sie — Vertrag; berechtigte Interessen an der Bereitstellung des Anrufverlaufs.
- Messung, Abrechnung und Betrugsprävention — Vertrag; berechtigte Interessen.
- Produktanalyse zum Verständnis und zur Verbesserung des Dienstes — berechtigte Interessen.
- Sicherheit, Moderation und Bearbeitung von Missbrauchsmeldungen — berechtigte Interessen an einem sicheren Dienst; rechtliche Verpflichtung.
- Aufbewahrung gesetzlich vorgeschriebener Transaktionsaufzeichnungen — rechtliche Verpflichtung.

Transkription und Übersetzung erfolgen automatisiert (KI-basiert) und können fehlerhaft sein; KI-Ausgaben werden ausschließlich zur Bereitstellung des Dienstes erzeugt und nicht zum Training von Drittanbietermodellen verwendet.

## 4. Dienstleister (Auftragsverarbeiter / Unterauftragsverarbeiter)

Wir geben personenbezogene Daten ausschließlich zum Betrieb des Dienstes an die unten genannten Anbieter weiter. Einige befinden sich außerhalb des EWR; in diesem Fall stützen wir uns auf geeignete Garantien wie die EU-Standardvertragsklauseln.

- **Google** — Anmeldung (OAuth): Name, E-Mail, Profilbild.
- **Deepgram** — Speech-to-Text: gestreamtes Audio (flüchtig).
- **Groq** — maschinelle Übersetzung (Standard-Tarif): Transkripttext (flüchtig).
- **OpenAI** — Echtzeit-Übersetzung (Premium-Tarif): gestreamtes Audio und Transkripttext (flüchtig).
- **Stripe** — Zahlungsabwicklung: Rechnungs- und Zahlungsdaten.
- **Supabase** — Datenbank und Dateispeicherung: Konto-, Nutzungs-, Abrechnungs- und Sicherheitsdaten, gespeicherte Transkripte und Chat-Dateianhänge.
- **Cloudflare** — Edge-Auslieferung und TURN-Medien-Relay: Verbindungsmetadaten; weitergeleitete Medien bleiben verschlüsselt und sind für das Relay nicht lesbar.
- **Resend** — transaktionale E-Mails (zum Beispiel Einladungen und Kontomitteilungen): E-Mail-Adresse des Empfängers.
- **Better Stack** — Betriebsprotokollierung und Verfügbarkeitsüberwachung: technische Daten/Verbindungsmetadaten.
- **Vercel** — Frontend-Hosting: technische/Verbindungsdaten.
- **Railway** — Backend-Hosting: technische/Verbindungsdaten.

## 5. Wie lange wir Daten aufbewahren

- **Audio:** in Echtzeit verarbeitet und nicht gespeichert.
- **Transkripte und Übersetzungen:** bei Anrufen mit einem angemeldeten Teilnehmer aufbewahrt, bis Sie den Anruf oder Ihr Konto löschen; Anrufe nur mit Gästen werden nicht gespeichert.
- **Kontodaten:** aufbewahrt, solange Ihr Konto besteht; gelöscht, wenn Sie Ihr Konto löschen.
- **Chat-Dateianhänge:** aufbewahrt, solange der zugehörige Anruf bzw. das Konto besteht, und über kurzlebige private Links bereitgestellt.
- **Abrechnungs-/Transaktionsaufzeichnungen:** aufbewahrt, soweit die geltenden Steuer- und Buchführungsgesetze dies verlangen.
- **Sicherheits-/Missbrauchsmeldungen und Sperrdatensätze:** aufbewahrt, solange dies erforderlich ist, um den Dienst sicher zu halten und rechtliche Verpflichtungen zu erfüllen.
- **Betriebsprotokolle:** für einen begrenzten Zeitraum zur Sicherheit und Zuverlässigkeit aufbewahrt.

## 6. Cookies und lokaler Speicher

Wir verwenden ausschließlich unbedingt erforderlichen Browser-Speicher — **keine** Cookies von Drittanbietern für Werbung oder seitenübergreifendes Tracking und keine Werbe- oder Analyse-Cookies:

- ein **Sitzungs-Token**, das in Ihrem Browser gespeichert wird, damit Sie angemeldet bleiben;
- eine **Cookie-Einwilligungseinstellung**, die Ihre Wahl im Cookie-Banner speichert;
- geringfügige **Oberflächen-Flags** (zum Beispiel das Speichern, dass Sie einen Funktionshinweis bereits gesehen haben).

Da diese unbedingt erforderlich sind, um einen von Ihnen angeforderten Dienst bereitzustellen, bedürfen sie nach den ePrivacy-Vorschriften keiner Einwilligung.

## 7. Ihre Rechte

Vorbehaltlich des geltenden Rechts haben Sie das Recht, auf Ihre Daten zuzugreifen, sie zu berichtigen und zu löschen; sie in einem übertragbaren Format zu erhalten; bestimmte Verarbeitungen einzuschränken oder ihnen zu widersprechen; Ihre Einwilligung jederzeit zu widerrufen; und eine Beschwerde bei einer Aufsichtsbehörde einzureichen. Zugriff und Übertragbarkeit können Sie über **Meine Daten herunterladen** und die Löschung über **Mein Konto löschen** im Bereich „Datenschutz & Daten“ in der App ausüben, oder schreiben Sie an privacy@voxtranslate.app. Wenn Sie sich in Spanien befinden, ist die federführende Aufsichtsbehörde die **Agencia Española de Protección de Datos (AEPD, www.aepd.es)**; Sie können sich auch an die Datenschutzbehörde in Ihrem eigenen Wohnsitzland wenden.

## 8. Sicherheit

Wir setzen branchenübliche Maßnahmen zum Schutz personenbezogener Daten ein, einschließlich Verschlüsselung bei der Übertragung und Peer-to-Peer-Medien, die nicht über unsere Server laufen. Keine Übertragungs- oder Speichermethode ist jedoch vollkommen sicher, und wir können keine absolute Sicherheit garantieren.

## 9. Kinder

Der Dienst ist für Erwachsene (18+) bestimmt. Wir verarbeiten wissentlich keine Daten von Kindern. Wenn Sie glauben, dass ein Kind uns Daten übermittelt hat, kontaktieren Sie uns, und wir werden sie löschen.

## 10. Google-Nutzerdaten (Kalender) und eingeschränkte Verwendung

Wenn Sie sich mit Google anmelden und die Terminplanungsfunktion von VoxTranslate nutzen, greifen wir auf bestimmte Daten aus Ihrem Google-Konto zu: Ihr Basisprofil (Name, E-Mail-Adresse, Profilbild) zur Kontoerstellung und -identifizierung sowie Ihren Google Kalender über den calendar.events-Bereich. Wir verwenden den Kalenderzugriff ausschließlich dazu, die Kalendertermine für die von Ihnen über VoxTranslate geplanten Meetings zu erstellen, zu aktualisieren und zu löschen und die von Ihnen eingeladenen Personen als Teilnehmer hinzuzufügen, damit Google ihnen Einladungen und Erinnerungen senden kann. Wir erstellen und ändern nur Termine, die Sie über die Planungsfunktion von VoxTranslate erstellt haben; wir lesen, bearbeiten oder löschen niemals andere Kalendereinträge. Zur Synchronisierung Ihrer Termine speichern wir ein verschlüsseltes Google Refresh-Token sowie einen minimalen Datensatz jedes Meetings (Titel, Uhrzeit, Raumlink und die von Ihnen gewählten Eingeladenen). Sie können Google Kalender jederzeit trennen, wodurch das gespeicherte Token gelöscht wird.

Die Verwendung und Weitergabe von Informationen, die VoxTranslate von Google APIs erhalten hat, an andere Apps erfolgt in Übereinstimmung mit der [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy), einschließlich der Anforderungen zur eingeschränkten Verwendung. Wir verwenden Google-Nutzerdaten nicht für Werbung, verkaufen sie nicht und erlauben Menschen keinen Zugriff darauf, außer mit Ihrer Einwilligung, aus Sicherheitsgründen, zur Einhaltung geltenden Rechts oder wenn die Daten aggregiert und anonymisiert wurden.

## 11. Änderungen

Wir können diese Erklärung aktualisieren; wir werden Version und Datum anpassen und bei wesentlichen Änderungen zusätzliche Schritte unternehmen, soweit gesetzlich erforderlich.
