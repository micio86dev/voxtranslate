Esta Política de privacidad explica qué datos personales trata VoxTranslate cuando usas nuestro servicio de videollamadas traducidas en tiempo real, por qué los tratamos y qué derechos tienes. Está redactada para cumplir el Reglamento General de Protección de Datos (RGPD) de la UE y leyes similares.

## 1. Quién es el responsable del tratamiento

El Servicio es operado por **Alessandro Micelli**, Puerto del Rosario, Spain ("VoxTranslate", "nosotros"), responsable del tratamiento de los datos personales tratados a través del Servicio. Para cualquier solicitud de privacidad, escribe a privacy@voxtranslate.app.

## 2. Qué datos tratamos

- **Datos de la cuenta** — cuando inicias sesión con Google, recibimos tu nombre, correo electrónico y la URL de la foto de perfil.
- **Audio (transitorio)** — mientras hablas, el audio de tu micrófono se transmite al proveedor que sustenta el motor elegido para esa llamada: un proveedor de traducción de voz de extremo a extremo en los tiers **Standard** y **Premium**. En el tier **Enhanced** tu navegador transmite el audio **directamente** al proveedor mediante un token de acceso de corta duración, por lo que no pasa por nuestros servidores. No almacenamos el audio en bruto.
- **Audio de la pestaña del navegador (extensión de Chrome, transitorio)** — nuestra extensión de Chrome captura el audio que se reproduce en una pestaña que **tú** eliges, y no solo tu micrófono, para que puedas seguir un vídeo, un seminario web o una llamada alojados en otro sitio. La captura solo se activa mientras hay una sesión en curso y únicamente en la pestaña que seleccionaste. Ese audio se transcribe y se traduce sobre la marcha y se te devuelve como subtítulos en directo; no se graba, y una sesión de la extensión no almacena nada en nuestros servidores. Los ajustes de la extensión permanecen en tu navegador.
- **Muestra de voz (opcional)** — en el tier Enhanced puedes grabar un clip breve para que tu voz traducida suene con un timbre parecido al tuyo. El clip se envía a nuestro proveedor de voz, que crea la voz sintética y devuelve un identificador que almacenamos en tu cuenta. Es opcional y el tier puede usarse sin ello.
- **Transcripciones y traducciones** — el texto del habla y del chat junto con sus traducciones. Cuando un usuario con sesión iniciada participa en una llamada, estos se **almacenan** para que los participantes puedan revisar, exportar (PDF/JSON) y corregir con IA la transcripción posteriormente. Las llamadas en las que no participó ningún usuario con sesión iniciada no se almacenan. Las transcripciones almacenadas se eliminan cuando eliminas tu cuenta: tus intervenciones se borran con ella.
- **Mensajes de chat y archivos** — el chat se transmite y traduce entre los participantes. Los archivos que adjuntas se almacenan de forma privada y se comparten con los participantes de la llamada mediante enlaces de corta duración.
- **Datos de uso, analítica y facturación** — saldo de créditos, transacciones, tiempo de conversación por sesión y eventos de uso del producto (qué funciones y nivel de plan usas, y durante cuánto tiempo) utilizados para medir, facturar, proteger y mejorar el Servicio. La analítica se agrega con fines de elaboración de informes.
- **Datos de seguridad** — denuncias de abuso que envías o que se presentan sobre ti (que pueden incluir un breve extracto de la transcripción) y los registros de moderación/bloqueo.
- **Datos técnicos** — metadatos de conexión necesarios para operar el servicio en tiempo real, enrutar los medios, enviar registros operativos y mantener el Servicio seguro.

El vídeo y el audio entre participantes viajan de igual a igual (WebRTC) y no pasan por nuestros servidores ni se graban. Cuando no es posible establecer una conexión directa, los medios se retransmiten a través de un servidor TURN de forma cifrada que el relay no puede leer. Nuestro servidor gestiona el inicio de sesión, la señalización, el flujo de voz a texto en directo, la traducción, el relay del chat y —cuando está habilitado— el almacenamiento de la transcripción. En el tier Enhanced incluso el audio del habla evita nuestro servidor: viaja directamente de tu navegador al proveedor de voz.

## 3. Por qué los tratamos y nuestras bases jurídicas

- Prestar la llamada, la transcripción y la traducción — ejecución de un contrato.
- Tratar el audio necesario para los subtítulos/traducción en directo — contrato; y tu consentimiento dado al registrarte.
- Almacenar transcripciones para tu posterior revisión y exportación — contrato; interés legítimo en facilitar el historial de llamadas.
- Medición, facturación y prevención del fraude — contrato; interés legítimo.
- Analítica del producto para entender y mejorar el Servicio — interés legítimo.
- Seguridad, moderación y gestión de denuncias de abuso — interés legítimo en un servicio seguro; obligación legal.
- Conservar los registros de transacciones exigidos por ley — obligación legal.

La transcripción y la traducción son automatizadas (basadas en IA) y pueden ser inexactas; los resultados de la IA se generan únicamente para prestar el Servicio y no se utilizan para entrenar modelos de terceros.

## 4. Proveedores de servicios (encargados / subencargados del tratamiento)

Compartimos datos personales con los proveedores indicados a continuación estrictamente para operar el Servicio. Algunos están ubicados fuera del EEE; en ese caso nos basamos en garantías adecuadas como las Cláusulas Contractuales Tipo de la UE.

- **Google** — inicio de sesión (OAuth): nombre, correo, foto de perfil; como Google Gemini, traducción de voz en tiempo real en el tier **Premium** (audio transmitido y texto de la transcripción, transitorios); y, solo con tu consentimiento, Google Analytics 4 y Google Ads: eventos de uso y de conversión.
- **Meta** — solo con tu consentimiento, el Meta Pixel: eventos de página y de conversión usados para medir y segmentar la publicidad.
- **Alibaba Cloud (Qwen)** — traducción de voz en tiempo real y voz sintetizada en el tier **Standard**: audio transmitido y texto de la transcripción (transitorios).
- **Groq** — traducción automática de texto — subtítulos, chat y transcripciones — en todos los tiers: texto de la transcripción y del chat (transitorio).
- **Deepgram** — transcripción únicamente de audio subido o grabado, nunca de llamadas en directo y nunca traducción: el audio que subes o grabas (transitorio).
- **Cartesia** — voz a texto y síntesis de voz en el tier **Enhanced**: audio transmitido directamente desde tu navegador (transitorio, sin pasar por nuestros servidores) y, si usas la clonación de voz, el clip que grabas y la voz sintética resultante.
- **Stripe** — procesamiento de pagos: datos de facturación y de pago.
- **Supabase** — base de datos y almacenamiento de archivos: datos de cuenta, uso, facturación y seguridad, transcripciones almacenadas y archivos adjuntos del chat.
- **Cloudflare** — entrega en el edge y relay de medios TURN: metadatos de conexión; los medios retransmitidos permanecen cifrados y el relay no puede leerlos.
- **Resend** — correo transaccional (por ejemplo, invitaciones y avisos de cuenta): dirección de correo del destinatario.
- **Better Stack** — registro operativo y monitorización de disponibilidad: metadatos técnicos/de conexión.
- **Vercel** — alojamiento del frontend: datos técnicos/de conexión.
- **Railway** — alojamiento del backend: datos técnicos/de conexión.

## 5. Cuánto tiempo conservamos los datos

- **Audio:** tratado en tiempo real y no almacenado.
- **Muestra de voz (si usas la clonación de voz):** la voz sintética la conserva nuestro proveedor de voz y su identificador se almacena en tu cuenta hasta que elimines la cuenta.
- **Transcripciones y traducciones:** en las llamadas con un participante con sesión iniciada, se conservan hasta que eliminas la llamada o tu cuenta; las llamadas con solo invitados no se almacenan.
- **Datos de la cuenta:** conservados mientras exista tu cuenta; eliminados cuando la eliminas.
- **Archivos adjuntos del chat:** conservados mientras exista la llamada/cuenta relacionada y servidos mediante enlaces privados de corta duración.
- **Registros de facturación/transacciones:** conservados según exijan las leyes fiscales y contables aplicables.
- **Denuncias de abuso/seguridad y registros de bloqueo:** conservados el tiempo necesario para mantener el Servicio seguro y cumplir obligaciones legales.
- **Registros operativos:** conservados durante un período limitado por seguridad y fiabilidad.

## 6. Cookies y almacenamiento local

Parte del almacenamiento del navegador es estrictamente necesario para que el Servicio funcione. La analítica y la publicidad son opcionales: se cargan **solo después de que las aceptes** en el banner de cookies, nunca antes, y puedes cambiar de opinión en cualquier momento desde **Configuración de cookies**.

**Estrictamente necesarios** — no requieren consentimiento según las normas ePrivacy, porque prestan un servicio que has solicitado:

- un **token de sesión** guardado en tu navegador para que sigas con la sesión iniciada;
- una **preferencia de consentimiento de cookies** que recuerda tu elección en el banner;
- **indicadores de interfaz** menores (por ejemplo, recordar que ya has visto una sugerencia sobre una función).

**Analítica y publicidad** — se cargan solo con tu consentimiento:

- **Google Analytics 4** — medición agregada de qué funciones se usan y durante cuánto tiempo;
- **Google Ads** — medición de conversiones, donde esté activada;
- **Meta Pixel** — mide el efecto de nuestra publicidad y puede usarse para crear públicos publicitarios.

Si rechazas, queda solo el almacenamiento estrictamente necesario indicado arriba y el Servicio funciona igual. Retirar un consentimiento ya dado detiene las recogidas posteriores, y al recargar la página los rastreadores ya cargados desaparecen.

## 7. Tus derechos

Sujeto a la ley aplicable, tienes derecho a acceder, rectificar y suprimir tus datos; a recibirlos en un formato portable; a limitar u oponerte a ciertos tratamientos; a retirar el consentimiento en cualquier momento; y a presentar una reclamación ante una autoridad de control. Puedes ejercer el acceso y la portabilidad con **Descargar mis datos** y la supresión con **Eliminar mi cuenta** en el panel Privacidad y datos dentro de la app, o escribir a privacy@voxtranslate.app. Si estás en España, la autoridad de control principal es la **Agencia Española de Protección de Datos (AEPD, www.aepd.es)**; también puedes contactar con la autoridad de protección de datos de tu propio país de residencia.

## 8. Seguridad

Usamos medidas estándar del sector para proteger los datos personales, incluido el cifrado en tránsito y medios de igual a igual que no pasan por nuestros servidores. No obstante, ningún método de transmisión o almacenamiento es completamente seguro, y no podemos garantizar una seguridad absoluta.

## 9. Menores

El Servicio es para adultos (18+). No tratamos conscientemente datos de menores. Si crees que un menor nos ha facilitado datos, contáctanos y los eliminaremos.

## 10. Datos de usuario de Google (Calendario) y uso limitado

Cuando inicias sesión con Google y usas la función de programación de reuniones de VoxTranslate, accedemos a ciertos datos de tu cuenta de Google: tu perfil básico (nombre, dirección de correo, foto de perfil) para crear e identificar tu cuenta, y tu Google Calendar mediante el ámbito calendar.events. Usamos el acceso al Calendario únicamente para crear, actualizar y eliminar los eventos del calendario para las reuniones que programas a través de VoxTranslate y para añadir a las personas que invitas como asistentes, de modo que Google pueda enviarles invitaciones y recordatorios. Solo creamos y modificamos eventos que tú creas a través de la función de programación de VoxTranslate; nunca leemos, editamos ni eliminamos ninguna otra entrada de tu calendario. Para mantener tus eventos sincronizados, almacenamos un token de actualización de Google cifrado en reposo, más un registro mínimo de cada reunión (título, hora, enlace de sala y los asistentes que eliges). Puedes desconectar Google Calendar en cualquier momento, lo que elimina el token almacenado.

El uso y la transferencia de información recibida de las APIs de Google a cualquier otra aplicación por parte de VoxTranslate se ajustarán a la [Política de Datos de Usuario de los Servicios API de Google](https://developers.google.com/terms/api-services-user-data-policy), incluidos los requisitos de uso limitado. No utilizamos los datos de usuario de Google para publicidad, no los vendemos y no permitimos que personas los lean a menos que tengamos tu consentimiento, sea necesario por seguridad o para cumplir la ley aplicable, o los datos hayan sido agregados y anonimizados.

## 11. Cambios

Podemos actualizar esta Política; revisaremos la versión y la fecha y, para cambios sustanciales, adoptaremos medidas adicionales cuando lo exija la ley.
