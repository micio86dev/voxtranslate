La présente Politique de confidentialité explique quelles données personnelles VoxTranslate traite lorsque vous utilisez notre service d'appels vidéo traduits en temps réel, pourquoi nous les traitons et quels sont vos droits. Elle est rédigée pour se conformer au Règlement général sur la protection des données (RGPD) de l'UE et aux lois similaires.

## 1. Qui est le responsable du traitement

Le Service est exploité par **Alessandro Micelli**, Puerto del Rosario, Spain (« VoxTranslate », « nous »), responsable du traitement des données personnelles traitées via le Service. Pour toute demande relative à la confidentialité, contactez privacy@voxtranslate.app.

## 2. Quelles données nous traitons

- **Données de compte** — lorsque vous vous connectez avec Google, nous recevons votre nom, votre adresse e-mail et l'URL de votre photo de profil.
- **Audio (transitoire)** — pendant que vous parlez, l'audio de votre micro est transmis en streaming aux fournisseurs qui alimentent le moteur choisi pour cet appel : un fournisseur de reconnaissance vocale sur l'offre Standard, et un fournisseur de traduction vocale de bout en bout sur les offres Pro et Premium. Sur l'offre **Enhanced**, votre navigateur transmet l'audio **directement** au fournisseur au moyen d'un jeton d'accès de courte durée : il ne passe donc pas par nos serveurs. Nous ne conservons pas l'audio brut.
- **Échantillon de voix (facultatif)** — sur l'offre Enhanced, vous pouvez enregistrer un court extrait pour que votre parole traduite soit prononcée dans une voix proche de la vôtre. L'extrait est envoyé à notre fournisseur vocal, qui crée la voix synthétique et renvoie un identifiant que nous conservons sur votre compte. La fonction est facultative et l'offre s'utilise sans elle.
- **Transcriptions et traductions** — le texte de la parole et du chat ainsi que ses traductions. Lorsqu'un utilisateur connecté participe à un appel, ceux-ci sont **conservés** afin que les participants puissent ensuite consulter, exporter (PDF/JSON) et corriger par IA la transcription. Les appels auxquels aucun utilisateur connecté n'a participé ne sont pas conservés. Les transcriptions conservées sont supprimées lorsque vous supprimez votre compte — vos prises de parole sont effacées avec lui.
- **Messages de chat et fichiers** — le chat est relayé et traduit entre les participants. Les fichiers que vous joignez sont stockés de manière privée et partagés avec les participants à l'appel via des liens à durée de vie limitée.
- **Données d'utilisation, d'analyse et de facturation** — solde de crédits, transactions, temps de parole par session et événements d'utilisation du produit (quelles fonctionnalités et quel niveau d'offre vous utilisez, et pendant combien de temps) servant à mesurer, facturer, sécuriser et améliorer le Service. Les données d'analyse sont agrégées à des fins de reporting.
- **Données de sécurité** — signalements d'abus que vous soumettez ou qui vous concernent (pouvant inclure un court extrait de transcription) et les enregistrements de modération/bannissement.
- **Données techniques** — métadonnées de connexion nécessaires au fonctionnement du service en temps réel, à l'acheminement des médias, à la transmission des journaux d'exploitation et au maintien de la sécurité du Service.

La vidéo et l'audio entre participants sont transmis en pair-à-pair (WebRTC) et ne passent pas par nos serveurs ni n'y sont enregistrés. Lorsqu'une connexion directe ne peut être établie, les médias sont relayés via un serveur TURN sous une forme chiffrée que le relais ne peut pas lire. Notre serveur gère la connexion, la signalisation, le flux de reconnaissance vocale en direct, la traduction, le relais du chat et — lorsqu'elle est activée — la conservation des transcriptions. Sur l'offre Enhanced, même l'audio de la parole contourne notre serveur : il va directement de votre navigateur au fournisseur vocal.

## 3. Pourquoi nous les traitons et nos bases juridiques

- Fournir l'appel, la transcription et la traduction — exécution d'un contrat.
- Traiter l'audio nécessaire aux sous-titres/à la traduction en direct — contrat ; et votre consentement donné à l'inscription.
- Conserver les transcriptions pour votre consultation et votre export ultérieurs — contrat ; intérêts légitimes à fournir un historique des appels.
- Mesure, facturation et prévention de la fraude — contrat ; intérêts légitimes.
- Analyses produit pour comprendre et améliorer le Service — intérêts légitimes.
- Sécurité, modération et traitement des signalements d'abus — intérêts légitimes à un service sûr ; obligation légale.
- Conservation des enregistrements de transactions exigés par la loi — obligation légale.

La transcription et la traduction sont automatisées (basées sur l'IA) et peuvent être inexactes ; les résultats de l'IA sont générés uniquement pour fournir le Service et ne sont pas utilisés pour entraîner des modèles tiers.

## 4. Prestataires de services (sous-traitants / sous-traitants ultérieurs)

Nous partageons des données personnelles avec les prestataires ci-dessous, strictement pour faire fonctionner le Service. Certains sont situés en dehors de l'EEE ; dans ce cas, nous nous appuyons sur des garanties appropriées telles que les Clauses contractuelles types de l'UE.

- **Google** — connexion (OAuth) : nom, e-mail, photo de profil ; et, en tant que Google Gemini, traduction vocale en temps réel sur l'offre **Premium** : audio en streaming et texte de la transcription (transitoire).
- **Deepgram** — reconnaissance vocale (offre Standard) : audio en streaming (transitoire).
- **Groq** — traduction automatique (offres Standard et Enhanced) : texte de la transcription (transitoire).
- **OpenAI** — traduction vocale en temps réel (offre **Pro**) : audio en streaming et texte de la transcription (transitoire).
- **Cartesia** — reconnaissance vocale et synthèse vocale sur l'offre **Enhanced** : audio transmis directement depuis votre navigateur (transitoire, sans passer par nos serveurs) et, si vous utilisez le clonage vocal, l'extrait que vous enregistrez ainsi que la voix synthétique obtenue.
- **Stripe** — traitement des paiements : informations de facturation et données de paiement.
- **Supabase** — base de données et stockage de fichiers : données de compte, d'utilisation, de facturation et de sécurité, transcriptions conservées et fichiers joints au chat.
- **Cloudflare** — diffusion en périphérie et relais de médias TURN : métadonnées de connexion ; les médias relayés restent chiffrés et ne sont pas lisibles par le relais.
- **Resend** — e-mails transactionnels (par exemple invitations et notifications de compte) : adresse e-mail du destinataire.
- **Better Stack** — journalisation d'exploitation et surveillance de la disponibilité : métadonnées techniques/de connexion.
- **Vercel** — hébergement du frontend : données techniques/de connexion.
- **Railway** — hébergement du backend : données techniques/de connexion.

## 5. Durée de conservation des données

- **Audio :** traité en temps réel et non conservé.
- **Échantillon de voix (si vous utilisez le clonage vocal) :** la voix synthétique est détenue par notre fournisseur vocal et son identifiant est conservé sur votre compte jusqu'à la suppression de celui-ci.
- **Transcriptions et traductions :** pour les appels comportant un participant connecté, conservées jusqu'à ce que vous supprimiez l'appel ou votre compte ; les appels comportant uniquement des invités ne sont pas conservés.
- **Données de compte :** conservées tant que votre compte existe ; supprimées lorsque vous supprimez votre compte.
- **Fichiers joints au chat :** conservés tant que l'appel/le compte associé existe et servis via des liens privés à durée de vie limitée.
- **Enregistrements de facturation/transactions :** conservés conformément aux lois fiscales et comptables applicables.
- **Signalements d'abus et enregistrements de bannissement :** conservés aussi longtemps que nécessaire pour assurer la sécurité du Service et respecter les obligations légales.
- **Journaux d'exploitation :** conservés pendant une période limitée à des fins de sécurité et de fiabilité.

## 6. Cookies et stockage local

Nous utilisons uniquement un stockage de navigateur strictement nécessaire — **aucun** cookie publicitaire tiers ni de suivi intersites, et aucun cookie publicitaire ou d'analyse :

- un **jeton de session** conservé dans votre navigateur pour vous maintenir connecté ;
- une **préférence de consentement aux cookies** mémorisant votre choix sur la bannière de cookies ;
- de petits **indicateurs d'interface** (par exemple, mémoriser que vous avez déjà vu une astuce de fonctionnalité).

Comme ils sont strictement nécessaires pour fournir un service que vous avez demandé, ils ne requièrent pas de consentement au titre des règles ePrivacy.

## 7. Vos droits

Sous réserve du droit applicable, vous avez le droit d'accéder à vos données, de les rectifier et de les effacer ; de les recevoir dans un format portable ; de limiter certains traitements ou de vous y opposer ; de retirer votre consentement à tout moment ; et d'introduire une réclamation auprès d'une autorité de contrôle. Vous pouvez exercer l'accès et la portabilité avec **Télécharger mes données** et l'effacement avec **Supprimer mon compte** dans le panneau Confidentialité et données de l'application, ou écrire à privacy@voxtranslate.app. Si vous êtes en Espagne, l'autorité de contrôle chef de file est l'**Agencia Española de Protección de Datos (AEPD, www.aepd.es)** ; vous pouvez également contacter l'autorité de protection des données de votre propre pays de résidence.

## 8. Sécurité

Nous utilisons des mesures conformes aux standards du secteur pour protéger les données personnelles, notamment le chiffrement en transit et des médias en pair-à-pair qui ne transitent pas par nos serveurs. Aucune méthode de transmission ou de stockage n'est toutefois totalement sûre, et nous ne pouvons garantir une sécurité absolue.

## 9. Enfants

Le Service est destiné aux adultes (18+). Nous ne traitons pas sciemment de données d'enfants. Si vous pensez qu'un enfant nous a fourni des données, contactez-nous et nous les supprimerons.

## 10. Données utilisateur Google (Agenda) et utilisation limitée

Lorsque vous vous connectez avec Google et utilisez la fonction de planification de réunions de VoxTranslate, nous accédons à certaines données de votre compte Google : votre profil de base (nom, adresse e-mail, photo de profil) pour créer et identifier votre compte, et votre Google Agenda via le périmètre calendar.events. Nous utilisons l'accès à l'Agenda uniquement pour créer, mettre à jour et supprimer les événements du calendrier pour les réunions que vous planifiez via VoxTranslate et pour ajouter les personnes que vous invitez en tant que participants afin que Google puisse leur envoyer des invitations et des rappels. Nous créons et modifions uniquement les événements que vous créez via la fonctionnalité de planification de VoxTranslate ; nous ne lisons, modifions ni supprimons jamais vos autres entrées de calendrier. Pour maintenir vos événements synchronisés, nous stockons un jeton d'actualisation Google chiffré au repos, ainsi qu'un enregistrement minimal de chaque réunion (titre, heure, lien de salle et les invités que vous choisissez). Vous pouvez déconnecter Google Agenda à tout moment, ce qui supprime le jeton stocké.

L'utilisation et le transfert des informations reçues des API Google vers toute autre application par VoxTranslate seront conformes à la [Politique de données utilisateur des services API Google](https://developers.google.com/terms/api-services-user-data-policy), y compris les exigences d'utilisation limitée. Nous n'utilisons pas les données utilisateur Google à des fins publicitaires, nous ne les vendons pas et nous ne permettons pas à des personnes de les lire à moins que nous ayons votre consentement, que cela soit nécessaire pour des raisons de sécurité ou pour se conformer à la loi applicable, ou que les données aient été agrégées et anonymisées.

## 11. Modifications

Nous pouvons mettre à jour cette Politique ; nous réviserons la version et la date et, pour les modifications substantielles, prendrons des mesures supplémentaires lorsque la loi l'exige.
