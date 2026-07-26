La presente Informativa sulla privacy spiega quali dati personali tratta VoxTranslate quando utilizzi il nostro servizio di videochiamate tradotte in tempo reale, perché li trattiamo e quali diritti hai. È redatta per conformarsi al Regolamento generale sulla protezione dei dati (GDPR) dell'UE e a leggi analoghe.

## 1. Chi è il titolare del trattamento

Il Servizio è gestito da **Alessandro Micelli**, Puerto del Rosario, Spain ("VoxTranslate", "noi"), titolare del trattamento dei dati personali trattati tramite il Servizio. Per qualsiasi richiesta sulla privacy, scrivi a privacy@voxtranslate.app.

## 2. Quali dati trattiamo

- **Dati dell'account** — accedendo con Google riceviamo nome, indirizzo email e URL dell'immagine del profilo.
- **Audio (transitorio)** — mentre parli, l'audio del microfono è inviato in streaming ai fornitori che alimentano il motore scelto per quella chiamata: un fornitore di speech-to-text nel tier Standard e un fornitore di traduzione vocale end-to-end nei tier Pro e Premium. Nel tier **Enhanced** il tuo browser invia l'audio **direttamente** al fornitore tramite un token di accesso di breve durata, quindi non passa affatto dai nostri server. Non conserviamo l'audio grezzo.
- **Campione vocale (facoltativo)** — nel tier Enhanced puoi registrare un breve clip perché la tua voce tradotta sia pronunciata con un timbro simile al tuo. Il clip è inviato al nostro fornitore vocale, che crea la voce sintetica e restituisce un identificativo che conserviamo sul tuo account. La funzione è facoltativa e il tier si può usare senza di essa.
- **Trascrizioni e traduzioni** — il testo del parlato e della chat insieme alle relative traduzioni. Quando un utente registrato partecipa a una chiamata, questi vengono **conservati** affinché i partecipanti possano successivamente rivedere, esportare (PDF/JSON) e correggere con l'IA la trascrizione. Le chiamate a cui non ha partecipato alcun utente registrato non vengono conservate. Le trascrizioni conservate vengono eliminate quando elimini il tuo account: i tuoi interventi vengono rimossi insieme ad esso.
- **Messaggi di chat e file** — la chat è trasmessa e tradotta tra i partecipanti. I file che alleghi sono archiviati in modo privato e condivisi con i partecipanti alla chiamata tramite link di breve durata.
- **Dati di utilizzo, analitici e di fatturazione** — saldo crediti, transazioni, tempo di conversazione per sessione ed eventi di utilizzo del prodotto (quali funzioni e quale livello di piano utilizzi, e per quanto tempo) usati per conteggiare, fatturare, proteggere e migliorare il Servizio. I dati analitici sono aggregati a fini di reportistica.
- **Dati di sicurezza** — segnalazioni di abuso inviate da te o riguardanti te (che possono includere un breve estratto della trascrizione) e i record di moderazione/ban.
- **Dati tecnici** — metadati di connessione necessari per far funzionare il servizio in tempo reale, instradare i media, inviare i log operativi e mantenere il Servizio sicuro.

Il video e l'audio tra i partecipanti viaggiano peer-to-peer (WebRTC) e non transitano né vengono registrati dai nostri server. Quando non è possibile stabilire una connessione diretta, i media vengono inoltrati tramite un server TURN in forma cifrata che il relay non è in grado di leggere. Il nostro server gestisce l'accesso, il signaling, lo stream di speech-to-text dal vivo, la traduzione, il relay della chat e — dove abilitata — la conservazione delle trascrizioni. Nel tier Enhanced anche l'audio del parlato non passa dal nostro server: viaggia direttamente dal tuo browser al fornitore vocale.

## 3. Perché li trattiamo e basi giuridiche

- Fornire la chiamata, la trascrizione e la traduzione — esecuzione di un contratto.
- Trattare l'audio necessario a sottotitoli/traduzione dal vivo — contratto; e il tuo consenso prestato alla registrazione.
- Conservare le trascrizioni per la tua successiva revisione ed esportazione — contratto; legittimo interesse a fornire lo storico delle chiamate.
- Conteggio, fatturazione e prevenzione frodi — contratto; legittimo interesse.
- Analisi di prodotto per comprendere e migliorare il Servizio — legittimo interesse.
- Sicurezza, moderazione e gestione delle segnalazioni di abuso — legittimo interesse a un servizio sicuro; obbligo di legge.
- Conservazione dei registri delle transazioni richiesti per legge — obbligo di legge.

La trascrizione e la traduzione sono automatizzate (basate sull'IA) e possono essere imprecise; gli output dell'IA sono generati esclusivamente per fornire il Servizio e non sono usati per addestrare modelli di terze parti.

## 4. Fornitori di servizi (responsabili / sub-responsabili)

Condividiamo i dati personali con i fornitori indicati di seguito esclusivamente per far funzionare il Servizio. Alcuni si trovano fuori dallo SEE; in tal caso ci basiamo su garanzie adeguate come le Clausole contrattuali standard dell'UE.

- **Google** — accesso (OAuth): nome, email, immagine del profilo; in quanto Google Gemini, traduzione vocale in tempo reale nel tier **Premium** (audio in streaming e testo della trascrizione, transitori); e, solo con il tuo consenso, Google Analytics 4 e Google Ads: eventi di utilizzo e di conversione.
- **Meta** — solo con il tuo consenso, il Meta Pixel: eventi di pagina e di conversione usati per misurare e indirizzare la pubblicità.
- **Deepgram** — speech-to-text (tier Standard): audio in streaming (transitorio).
- **Groq** — traduzione automatica (tier Standard ed Enhanced): testo della trascrizione (transitorio).
- **OpenAI** — traduzione vocale in tempo reale (tier **Pro**): audio in streaming e testo della trascrizione (transitori).
- **Cartesia** — speech-to-text e sintesi vocale nel tier **Enhanced**: audio inviato in streaming direttamente dal tuo browser (transitorio, non instradato dai nostri server) e, se usi la clonazione vocale, il clip che registri e la voce sintetica risultante.
- **Stripe** — elaborazione dei pagamenti: dati di fatturazione e di pagamento.
- **Supabase** — database e archiviazione file: dati di account, utilizzo, fatturazione e sicurezza, trascrizioni conservate e file allegati alla chat.
- **Cloudflare** — distribuzione edge e relay media TURN: metadati di connessione; i media inoltrati restano cifrati e non sono leggibili dal relay.
- **Resend** — email transazionali (ad esempio inviti e avvisi sull'account): indirizzo email del destinatario.
- **Better Stack** — logging operativo e monitoraggio dell'uptime: metadati tecnici/di connessione.
- **Vercel** — hosting del frontend: dati tecnici/di connessione.
- **Railway** — hosting del backend: dati tecnici/di connessione.

## 5. Per quanto tempo conserviamo i dati

- **Audio:** trattato in tempo reale e non conservato.
- **Campione vocale (se usi la clonazione vocale):** la voce sintetica è detenuta dal nostro fornitore vocale e il suo identificativo è conservato sul tuo account fino a quando elimini l’account.
- **Trascrizioni e traduzioni:** per le chiamate con un partecipante registrato, conservate finché non elimini la chiamata o il tuo account; le chiamate con soli ospiti non vengono conservate.
- **Dati dell'account:** conservati finché l'account esiste; eliminati quando elimini l'account.
- **File allegati alla chat:** conservati finché esiste la chiamata/l'account correlato e serviti tramite link privati di breve durata.
- **Registri di fatturazione/transazioni:** conservati come richiesto dalle leggi fiscali e contabili applicabili.
- **Segnalazioni di abuso e record di ban:** conservati per il tempo necessario a mantenere il Servizio sicuro e ad adempiere agli obblighi di legge.
- **Log operativi:** conservati per un periodo limitato a fini di sicurezza e affidabilità.

## 6. Cookie e archiviazione locale

Una parte dell'archiviazione del browser è strettamente necessaria per far funzionare il Servizio. Analytics e pubblicità sono facoltativi: si caricano **solo dopo che li hai accettati** sul banner dei cookie, mai prima, e puoi cambiare idea in qualsiasi momento da **Impostazioni cookie**.

**Strettamente necessari** — non richiedono consenso ai sensi delle norme ePrivacy, perché forniscono un servizio da te richiesto:

- un **token di sessione** conservato nel tuo browser per mantenerti connesso;
- una **preferenza di consenso ai cookie** che ricorda la tua scelta sul banner;
- piccoli **flag di interfaccia** (ad esempio, ricordare che hai già visto un suggerimento su una funzione).

**Analytics e pubblicità** — caricati solo con il tuo consenso:

- **Google Analytics 4** — misurazione aggregata di quali funzioni vengono usate e per quanto tempo;
- **Google Ads** — misurazione delle conversioni, dove attivata;
- **Meta Pixel** — misura l'effetto della nostra pubblicità e può essere usato per creare pubblici pubblicitari.

Se rifiuti resta solo l'archiviazione strettamente necessaria elencata sopra, e il Servizio funziona esattamente allo stesso modo. Revocare un consenso già dato interrompe le raccolte successive, e ricaricando la pagina i tracker già caricati vengono rimossi.

## 7. I tuoi diritti

Nei limiti della legge applicabile, hai diritto di accedere, rettificare ed eliminare i tuoi dati; di riceverli in formato portabile; di limitare o opporti a determinati trattamenti; di revocare il consenso in qualsiasi momento; e di proporre reclamo a un'autorità di controllo. Puoi esercitare l'accesso e la portabilità con **Scarica i miei dati** e la cancellazione con **Elimina il mio account** nel pannello Privacy e dati dell'app, oppure scrivere a privacy@voxtranslate.app. Se ti trovi in Spagna, l'autorità di controllo capofila è la **Agencia Española de Protección de Datos (AEPD, www.aepd.es)**; puoi anche contattare l'autorità per la protezione dei dati del tuo paese di residenza.

## 8. Sicurezza

Adottiamo misure standard del settore per proteggere i dati personali, inclusa la cifratura in transito e media peer-to-peer che non transitano dai nostri server. Nessun metodo di trasmissione o archiviazione è però del tutto sicuro e non possiamo garantire una sicurezza assoluta.

## 9. Minori

Il Servizio è destinato agli adulti (18+). Non trattiamo consapevolmente dati di minori. Se ritieni che un minore ci abbia fornito dati, contattaci e li elimineremo.

## 10. Dati utente Google (Calendar) e uso limitato

Quando accedi con Google e utilizzi la funzione di pianificazione riunioni di VoxTranslate, accediamo a determinati dati del tuo account Google: il tuo profilo di base (nome, indirizzo email, foto profilo) per creare e identificare il tuo account, e il tuo Google Calendar tramite lo scope calendar.events. Utilizziamo l'accesso al Calendar esclusivamente per creare, aggiornare ed eliminare gli eventi del calendario per le riunioni che pianifichi tramite VoxTranslate e per aggiungere le persone che inviti come partecipanti, in modo che Google possa inviare loro inviti e promemoria. Creiamo e modifichiamo solo gli eventi che tu crei tramite la funzione di pianificazione di VoxTranslate; non leggiamo mai, modifichiamo né eliminiamo nessun'altra voce del tuo calendario. Per mantenere i tuoi eventi sincronizzati, conserviamo un token di aggiornamento Google cifrato a riposo, oltre a un registro minimo di ogni riunione (titolo, orario, link alla stanza e i partecipanti che scegli). Puoi disconnettere Google Calendar in qualsiasi momento, il che eliminerà il token memorizzato.

L'uso e il trasferimento da parte di VoxTranslate delle informazioni ricevute dalle API di Google a qualsiasi altra app avverranno in conformità con la [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy), inclusi i requisiti di uso limitato. Non utilizziamo i dati utente Google per scopi pubblicitari, non li vendiamo e non permettiamo a persone di leggerli a meno che non abbiamo il tuo consenso, sia necessario per motivi di sicurezza o per rispettare la legge applicabile, oppure i dati siano stati aggregati e anonimizzati.

## 11. Modifiche

Possiamo aggiornare questa Informativa; ne rivedremo la versione e la data e, per modifiche sostanziali, adotteremo ulteriori misure ove richiesto dalla legge.
