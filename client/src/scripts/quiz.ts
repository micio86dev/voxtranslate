// Trivia quiz mini-game (spec 0047, localized 0048). Multiple players answer at
// once, so it's HOST-authoritative: the starter owns the canonical state and drives
// the phases; others send their answer and the host tallies. State flows over the
// game-agnostic relay (spec 0046), tagged `game:'quiz'` so it coexists with TTT.
//
// The question pack is built-in AND pre-translated into the 8 UI languages — the host
// broadcasts only the question's *index* in the pack, and each client renders it in
// ITS OWN language (no LLM/credits, instant). The correct option sits at the same
// index in every language.

interface PackItem {
  answer: number;
  q: Record<string, string>; // by language (en required = fallback)
  options: Record<string, string[]>; // by language; number/symbol options give only `en`
}

const PACK: PackItem[] = [
  {
    answer: 1,
    q: {
      en: 'Which is the largest planet in the Solar System?',
      it: 'Qual è il pianeta più grande del Sistema Solare?',
      es: '¿Cuál es el planeta más grande del Sistema Solar?',
      fr: 'Quelle est la plus grande planète du système solaire ?',
      de: 'Welcher ist der größte Planet im Sonnensystem?',
      pt: 'Qual é o maior planeta do Sistema Solar?',
      ja: '太陽系で最も大きい惑星はどれ？',
      zh: '太阳系中最大的行星是哪个？',
    },
    options: {
      en: ['Earth', 'Jupiter', 'Saturn', 'Mars'],
      it: ['Terra', 'Giove', 'Saturno', 'Marte'],
      es: ['Tierra', 'Júpiter', 'Saturno', 'Marte'],
      fr: ['Terre', 'Jupiter', 'Saturne', 'Mars'],
      de: ['Erde', 'Jupiter', 'Saturn', 'Mars'],
      pt: ['Terra', 'Júpiter', 'Saturno', 'Marte'],
      ja: ['地球', '木星', '土星', '火星'],
      zh: ['地球', '木星', '土星', '火星'],
    },
  },
  {
    answer: 2,
    q: {
      en: 'What is the capital of Japan?',
      it: 'Qual è la capitale del Giappone?',
      es: '¿Cuál es la capital de Japón?',
      fr: 'Quelle est la capitale du Japon ?',
      de: 'Was ist die Hauptstadt von Japan?',
      pt: 'Qual é a capital do Japão?',
      ja: '日本の首都はどこ？',
      zh: '日本的首都是哪里？',
    },
    options: {
      en: ['Seoul', 'Beijing', 'Tokyo', 'Bangkok'],
      it: ['Seul', 'Pechino', 'Tokyo', 'Bangkok'],
      es: ['Seúl', 'Pekín', 'Tokio', 'Bangkok'],
      fr: ['Séoul', 'Pékin', 'Tokyo', 'Bangkok'],
      de: ['Seoul', 'Peking', 'Tokio', 'Bangkok'],
      pt: ['Seul', 'Pequim', 'Tóquio', 'Bangkok'],
      ja: ['ソウル', '北京', '東京', 'バンコク'],
      zh: ['首尔', '北京', '东京', '曼谷'],
    },
  },
  {
    answer: 3,
    q: {
      en: 'Which is the largest ocean?',
      it: "Qual è l'oceano più grande?",
      es: '¿Cuál es el océano más grande?',
      fr: 'Quel est le plus grand océan ?',
      de: 'Welcher ist der größte Ozean?',
      pt: 'Qual é o maior oceano?',
      ja: '最も大きい海洋はどれ？',
      zh: '最大的海洋是哪个？',
    },
    options: {
      en: ['Atlantic', 'Indian', 'Arctic', 'Pacific'],
      it: ['Atlantico', 'Indiano', 'Artico', 'Pacifico'],
      es: ['Atlántico', 'Índico', 'Ártico', 'Pacífico'],
      fr: ['Atlantique', 'Indien', 'Arctique', 'Pacifique'],
      de: ['Atlantik', 'Indik', 'Arktik', 'Pazifik'],
      pt: ['Atlântico', 'Índico', 'Ártico', 'Pacífico'],
      ja: ['大西洋', 'インド洋', '北極海', '太平洋'],
      zh: ['大西洋', '印度洋', '北冰洋', '太平洋'],
    },
  },
  {
    answer: 2, // 7 — number options are language-neutral
    q: {
      en: 'How many continents are there on Earth?',
      it: 'Quanti continenti ci sono sulla Terra?',
      es: '¿Cuántos continentes hay en la Tierra?',
      fr: 'Combien de continents y a-t-il sur Terre ?',
      de: 'Wie viele Kontinente gibt es auf der Erde?',
      pt: 'Quantos continentes há na Terra?',
      ja: '地球には大陸がいくつある？',
      zh: '地球上有多少个大洲？',
    },
    options: { en: ['5', '6', '7', '8'] },
  },
  {
    answer: 2, // 2
    q: {
      en: 'What is the smallest prime number?',
      it: 'Qual è il numero primo più piccolo?',
      es: '¿Cuál es el número primo más pequeño?',
      fr: 'Quel est le plus petit nombre premier ?',
      de: 'Was ist die kleinste Primzahl?',
      pt: 'Qual é o menor número primo?',
      ja: '最小の素数はいくつ？',
      zh: '最小的质数是多少？',
    },
    options: { en: ['0', '1', '2', '3'] },
  },
  {
    answer: 2, // Au — chemical symbols are language-neutral
    q: {
      en: 'What is the chemical symbol for gold?',
      it: "Qual è il simbolo chimico dell'oro?",
      es: '¿Cuál es el símbolo químico del oro?',
      fr: "Quel est le symbole chimique de l'or ?",
      de: 'Was ist das chemische Symbol für Gold?',
      pt: 'Qual é o símbolo químico do ouro?',
      ja: '金の元素記号は？',
      zh: '黄金的化学符号是什么？',
    },
    options: { en: ['Go', 'Gd', 'Au', 'Ag'] },
  },
  // ---- Pack expansion (spec 0049): 6 → 40. Number/symbol options are
  // language-neutral (only `en`); word options carry all 8 languages. ----
  { answer: 1, q: { en: 'How many legs does a spider have?', it: 'Quante zampe ha un ragno?', es: '¿Cuántas patas tiene una araña?', fr: 'Combien de pattes a une araignée ?', de: 'Wie viele Beine hat eine Spinne?', pt: 'Quantas patas tem uma aranha?', ja: 'クモの脚は何本？', zh: '蜘蛛有几条腿？' }, options: { en: ['6', '8', '10', '12'] } },
  { answer: 2, q: { en: 'How many sides does a hexagon have?', it: 'Quanti lati ha un esagono?', es: '¿Cuántos lados tiene un hexágono?', fr: 'Combien de côtés a un hexagone ?', de: 'Wie viele Seiten hat ein Sechseck?', pt: 'Quantos lados tem um hexágono?', ja: '六角形の辺はいくつ？', zh: '六边形有几条边？' }, options: { en: ['4', '5', '6', '8'] } },
  { answer: 2, q: { en: 'How many players from one football team are on the pitch?', it: 'Quanti giocatori di una squadra di calcio sono in campo?', es: '¿Cuántos jugadores de un equipo de fútbol hay en el campo?', fr: "Combien de joueurs d'une équipe de football sont sur le terrain ?", de: 'Wie viele Spieler einer Fußballmannschaft stehen auf dem Feld?', pt: 'Quantos jogadores de um time de futebol estão em campo?', ja: 'サッカー1チームは何人がフィールドに出る？', zh: '一支足球队有几名球员在场上？' }, options: { en: ['9', '10', '11', '12'] } },
  { answer: 1, q: { en: 'How many bones are in the adult human body?', it: 'Quante ossa ha il corpo umano adulto?', es: '¿Cuántos huesos tiene el cuerpo humano adulto?', fr: "Combien d'os compte le corps humain adulte ?", de: 'Wie viele Knochen hat der erwachsene menschliche Körper?', pt: 'Quantos ossos tem o corpo humano adulto?', ja: '成人の体の骨は何本？', zh: '成年人体内有多少块骨头？' }, options: { en: ['186', '206', '226', '246'] } },
  { answer: 1, q: { en: 'How many minutes are in one hour?', it: "Quanti minuti ci sono in un'ora?", es: '¿Cuántos minutos hay en una hora?', fr: 'Combien de minutes y a-t-il dans une heure ?', de: 'Wie viele Minuten hat eine Stunde?', pt: 'Quantos minutos há em uma hora?', ja: '1時間は何分？', zh: '一小时有多少分钟？' }, options: { en: ['30', '60', '90', '100'] } },
  { answer: 1, q: { en: 'How many planets are in the Solar System?', it: 'Quanti pianeti ci sono nel Sistema Solare?', es: '¿Cuántos planetas hay en el Sistema Solar?', fr: 'Combien de planètes compte le système solaire ?', de: 'Wie viele Planeten hat das Sonnensystem?', pt: 'Quantos planetas há no Sistema Solar?', ja: '太陽系の惑星はいくつ？', zh: '太阳系有多少颗行星？' }, options: { en: ['7', '8', '9', '10'] } },
  { answer: 0, q: { en: 'How many strings does a violin have?', it: 'Quante corde ha un violino?', es: '¿Cuántas cuerdas tiene un violín?', fr: 'Combien de cordes a un violon ?', de: 'Wie viele Saiten hat eine Geige?', pt: 'Quantas cordas tem um violino?', ja: 'バイオリンの弦は何本？', zh: '小提琴有几根弦？' }, options: { en: ['4', '5', '6', '7'] } },
  { answer: 2, q: { en: 'How many keys does a standard piano have?', it: 'Quanti tasti ha un pianoforte standard?', es: '¿Cuántas teclas tiene un piano estándar?', fr: 'Combien de touches a un piano standard ?', de: 'Wie viele Tasten hat ein Standardklavier?', pt: 'Quantas teclas tem um piano padrão?', ja: '標準的なピアノの鍵盤は何鍵？', zh: '标准钢琴有多少个琴键？' }, options: { en: ['76', '84', '88', '96'] } },
  { answer: 2, q: { en: 'In which year did World War II end?', it: 'In che anno finì la Seconda guerra mondiale?', es: '¿En qué año terminó la Segunda Guerra Mundial?', fr: "En quelle année s'est terminée la Seconde Guerre mondiale ?", de: 'In welchem Jahr endete der Zweite Weltkrieg?', pt: 'Em que ano terminou a Segunda Guerra Mundial?', ja: '第二次世界大戦が終わったのは何年？', zh: '第二次世界大战在哪一年结束？' }, options: { en: ['1939', '1942', '1945', '1948'] } },
  { answer: 1, q: { en: 'In which year did humans first land on the Moon?', it: "In che anno l'uomo sbarcò per la prima volta sulla Luna?", es: '¿En qué año llegó el ser humano por primera vez a la Luna?', fr: "En quelle année l'homme a-t-il marché pour la première fois sur la Lune ?", de: 'In welchem Jahr betrat der Mensch erstmals den Mond?', pt: 'Em que ano o ser humano pisou pela primeira vez na Lua?', ja: '人類が初めて月に着陸したのは何年？', zh: '人类首次登月是在哪一年？' }, options: { en: ['1965', '1969', '1972', '1980'] } },
  { answer: 1, q: { en: 'In which year did the Berlin Wall fall?', it: 'In che anno cadde il muro di Berlino?', es: '¿En qué año cayó el Muro de Berlín?', fr: 'En quelle année est tombé le mur de Berlin ?', de: 'In welchem Jahr fiel die Berliner Mauer?', pt: 'Em que ano caiu o Muro de Berlim?', ja: 'ベルリンの壁が崩壊したのは何年？', zh: '柏林墙在哪一年倒塌？' }, options: { en: ['1985', '1989', '1991', '1995'] } },
  { answer: 1, q: { en: 'In which year did the Titanic sink?', it: 'In che anno affondò il Titanic?', es: '¿En qué año se hundió el Titanic?', fr: 'En quelle année a coulé le Titanic ?', de: 'In welchem Jahr sank die Titanic?', pt: 'Em que ano o Titanic afundou?', ja: 'タイタニック号が沈没したのは何年？', zh: '泰坦尼克号在哪一年沉没？' }, options: { en: ['1905', '1912', '1918', '1923'] } },
  { answer: 1, q: { en: 'What is the chemical symbol for iron?', it: 'Qual è il simbolo chimico del ferro?', es: '¿Cuál es el símbolo químico del hierro?', fr: 'Quel est le symbole chimique du fer ?', de: 'Was ist das chemische Symbol für Eisen?', pt: 'Qual é o símbolo químico do ferro?', ja: '鉄の元素記号は？', zh: '铁的化学符号是什么？' }, options: { en: ['Ir', 'Fe', 'Fr', 'In'] } },
  { answer: 2, q: { en: 'What is the chemical symbol for sodium?', it: 'Qual è il simbolo chimico del sodio?', es: '¿Cuál es el símbolo químico del sodio?', fr: 'Quel est le symbole chimique du sodium ?', de: 'Was ist das chemische Symbol für Natrium?', pt: 'Qual é o símbolo químico do sódio?', ja: 'ナトリウムの元素記号は？', zh: '钠的化学符号是什么？' }, options: { en: ['So', 'Sd', 'Na', 'Nm'] } },
  { answer: 0, q: { en: 'What is the chemical symbol for oxygen?', it: "Qual è il simbolo chimico dell'ossigeno?", es: '¿Cuál es el símbolo químico del oxígeno?', fr: "Quel est le symbole chimique de l'oxygène ?", de: 'Was ist das chemische Symbol für Sauerstoff?', pt: 'Qual é o símbolo químico do oxigênio?', ja: '酸素の元素記号は？', zh: '氧的化学符号是什么？' }, options: { en: ['O', 'Ox', 'Og', 'Oy'] } },
  { answer: 1, q: { en: 'At what temperature (°C) does water boil at sea level?', it: "A che temperatura (°C) bolle l'acqua al livello del mare?", es: '¿A qué temperatura (°C) hierve el agua al nivel del mar?', fr: "À quelle température (°C) l'eau bout-elle au niveau de la mer ?", de: 'Bei welcher Temperatur (°C) kocht Wasser auf Meereshöhe?', pt: 'A que temperatura (°C) a água ferve ao nível do mar?', ja: '海面で水が沸騰する温度（°C）は？', zh: '在海平面水的沸点是多少（°C）？' }, options: { en: ['90', '100', '110', '120'] } },
  { answer: 1, q: { en: 'At what temperature (°C) does water freeze?', it: "A che temperatura (°C) congela l'acqua?", es: '¿A qué temperatura (°C) se congela el agua?', fr: "À quelle température (°C) l'eau gèle-t-elle ?", de: 'Bei welcher Temperatur (°C) gefriert Wasser?', pt: 'A que temperatura (°C) a água congela?', ja: '水が凍る温度（°C）は？', zh: '水在多少摄氏度结冰？' }, options: { en: ['-10', '0', '10', '32'] } },
  { answer: 2, q: { en: 'What is 7 × 8?', it: 'Quanto fa 7 × 8?', es: '¿Cuánto es 7 × 8?', fr: 'Combien font 7 × 8 ?', de: 'Was ist 7 × 8?', pt: 'Quanto é 7 × 8?', ja: '7 × 8 は？', zh: '7 × 8 等于多少？' }, options: { en: ['48', '54', '56', '64'] } },
  { answer: 1, q: { en: 'What is 15% of 200?', it: 'Quanto è il 15% di 200?', es: '¿Cuánto es el 15% de 200?', fr: 'Combien font 15 % de 200 ?', de: 'Was sind 15 % von 200?', pt: 'Quanto é 15% de 200?', ja: '200 の 15% は？', zh: '200 的 15% 是多少？' }, options: { en: ['15', '30', '45', '60'] } },
  { answer: 1, q: { en: 'What is the square root of 144?', it: 'Qual è la radice quadrata di 144?', es: '¿Cuál es la raíz cuadrada de 144?', fr: 'Quelle est la racine carrée de 144 ?', de: 'Was ist die Quadratwurzel von 144?', pt: 'Qual é a raiz quadrada de 144?', ja: '144 の平方根は？', zh: '144 的平方根是多少？' }, options: { en: ['11', '12', '13', '14'] } },
  { answer: 2, q: { en: 'What is 2 to the power of 5?', it: 'Quanto fa 2 elevato alla 5?', es: '¿Cuánto es 2 elevado a la 5?', fr: 'Combien font 2 puissance 5 ?', de: 'Was ist 2 hoch 5?', pt: 'Quanto é 2 elevado a 5?', ja: '2 の 5 乗は？', zh: '2 的 5 次方是多少？' }, options: { en: ['16', '25', '32', '64'] } },
  { answer: 2, q: { en: 'How many degrees are in a right angle?', it: 'Quanti gradi misura un angolo retto?', es: '¿Cuántos grados tiene un ángulo recto?', fr: 'Combien de degrés mesure un angle droit ?', de: 'Wie viele Grad hat ein rechter Winkel?', pt: 'Quantos graus tem um ângulo reto?', ja: '直角は何度？', zh: '直角是多少度？' }, options: { en: ['45', '60', '90', '180'] } },
  { answer: 0, q: { en: 'How many sides does a triangle have?', it: 'Quanti lati ha un triangolo?', es: '¿Cuántos lados tiene un triángulo?', fr: 'Combien de côtés a un triangle ?', de: 'Wie viele Seiten hat ein Dreieck?', pt: 'Quantos lados tem um triângulo?', ja: '三角形の辺はいくつ？', zh: '三角形有几条边？' }, options: { en: ['3', '4', '5', '6'] } },
  { answer: 2, q: { en: 'How many hearts does an octopus have?', it: 'Quanti cuori ha un polpo?', es: '¿Cuántos corazones tiene un pulpo?', fr: 'Combien de cœurs a une pieuvre ?', de: 'Wie viele Herzen hat ein Oktopus?', pt: 'Quantos corações tem um polvo?', ja: 'タコの心臓はいくつ？', zh: '章鱼有几颗心脏？' }, options: { en: ['1', '2', '3', '4'] } },
  { answer: 2, q: { en: 'How many teeth does an adult human usually have?', it: 'Quanti denti ha di solito un adulto?', es: '¿Cuántos dientes suele tener un adulto?', fr: 'Combien de dents un adulte a-t-il généralement ?', de: 'Wie viele Zähne hat ein Erwachsener in der Regel?', pt: 'Quantos dentes um adulto costuma ter?', ja: '成人の歯は通常何本？', zh: '成年人通常有多少颗牙齿？' }, options: { en: ['28', '30', '32', '36'] } },
  { answer: 1, q: { en: 'How many colors are on the Italian flag?', it: 'Quanti colori ha la bandiera italiana?', es: '¿Cuántos colores tiene la bandera italiana?', fr: 'Combien de couleurs a le drapeau italien ?', de: 'Wie viele Farben hat die italienische Flagge?', pt: 'Quantas cores tem a bandeira italiana?', ja: 'イタリア国旗の色はいくつ？', zh: '意大利国旗有几种颜色？' }, options: { en: ['2', '3', '4', '5'] } },
  { answer: 1, q: { en: 'Which is the largest country by area?', it: 'Qual è il paese più grande per superficie?', es: '¿Cuál es el país más grande por superficie?', fr: 'Quel est le plus grand pays par superficie ?', de: 'Welches ist das flächengrößte Land?', pt: 'Qual é o maior país em área?', ja: '面積が最も大きい国はどれ？', zh: '面积最大的国家是哪个？' }, options: { en: ['Canada', 'Russia', 'China', 'USA'], it: ['Canada', 'Russia', 'Cina', 'USA'], es: ['Canadá', 'Rusia', 'China', 'EE. UU.'], fr: ['Canada', 'Russie', 'Chine', 'États-Unis'], de: ['Kanada', 'Russland', 'China', 'USA'], pt: ['Canadá', 'Rússia', 'China', 'EUA'], ja: ['カナダ', 'ロシア', '中国', 'アメリカ'], zh: ['加拿大', '俄罗斯', '中国', '美国'] } },
  { answer: 1, q: { en: 'Which is the fastest land animal?', it: "Qual è l'animale terrestre più veloce?", es: '¿Cuál es el animal terrestre más rápido?', fr: "Quel est l'animal terrestre le plus rapide ?", de: 'Welches ist das schnellste Landtier?', pt: 'Qual é o animal terrestre mais rápido?', ja: '陸上で最も速い動物はどれ？', zh: '陆地上最快的动物是哪个？' }, options: { en: ['Lion', 'Cheetah', 'Horse', 'Leopard'], it: ['Leone', 'Ghepardo', 'Cavallo', 'Leopardo'], es: ['León', 'Guepardo', 'Caballo', 'Leopardo'], fr: ['Lion', 'Guépard', 'Cheval', 'Léopard'], de: ['Löwe', 'Gepard', 'Pferd', 'Leopard'], pt: ['Leão', 'Guepardo', 'Cavalo', 'Leopardo'], ja: ['ライオン', 'チーター', '馬', 'ヒョウ'], zh: ['狮子', '猎豹', '马', '豹'] } },
  { answer: 1, q: { en: 'Which planet is known as the Red Planet?', it: 'Quale pianeta è noto come il Pianeta Rosso?', es: '¿Qué planeta se conoce como el Planeta Rojo?', fr: 'Quelle planète est appelée la planète rouge ?', de: 'Welcher Planet ist als der Rote Planet bekannt?', pt: 'Qual planeta é conhecido como o Planeta Vermelho?', ja: '「赤い惑星」と呼ばれるのはどれ？', zh: '哪颗行星被称为“红色星球”？' }, options: { en: ['Venus', 'Mars', 'Jupiter', 'Mercury'], it: ['Venere', 'Marte', 'Giove', 'Mercurio'], es: ['Venus', 'Marte', 'Júpiter', 'Mercurio'], fr: ['Vénus', 'Mars', 'Jupiter', 'Mercure'], de: ['Venus', 'Mars', 'Jupiter', 'Merkur'], pt: ['Vênus', 'Marte', 'Júpiter', 'Mercúrio'], ja: ['金星', '火星', '木星', '水星'], zh: ['金星', '火星', '木星', '水星'] } },
  { answer: 1, q: { en: 'What color do you get by mixing blue and yellow?', it: 'Che colore ottieni mescolando blu e giallo?', es: '¿Qué color obtienes al mezclar azul y amarillo?', fr: 'Quelle couleur obtient-on en mélangeant le bleu et le jaune ?', de: 'Welche Farbe entsteht beim Mischen von Blau und Gelb?', pt: 'Que cor se obtém misturando azul e amarelo?', ja: '青と黄を混ぜると何色？', zh: '蓝色和黄色混合得到什么颜色？' }, options: { en: ['Orange', 'Green', 'Purple', 'Brown'], it: ['Arancione', 'Verde', 'Viola', 'Marrone'], es: ['Naranja', 'Verde', 'Morado', 'Marrón'], fr: ['Orange', 'Vert', 'Violet', 'Marron'], de: ['Orange', 'Grün', 'Lila', 'Braun'], pt: ['Laranja', 'Verde', 'Roxo', 'Marrom'], ja: ['オレンジ', '緑', '紫', '茶色'], zh: ['橙色', '绿色', '紫色', '棕色'] } },
  { answer: 2, q: { en: 'What is the largest organ of the human body?', it: "Qual è l'organo più grande del corpo umano?", es: '¿Cuál es el órgano más grande del cuerpo humano?', fr: 'Quel est le plus grand organe du corps humain ?', de: 'Was ist das größte Organ des menschlichen Körpers?', pt: 'Qual é o maior órgão do corpo humano?', ja: '人体で最も大きい臓器は？', zh: '人体最大的器官是什么？' }, options: { en: ['Liver', 'Brain', 'Skin', 'Heart'], it: ['Fegato', 'Cervello', 'Pelle', 'Cuore'], es: ['Hígado', 'Cerebro', 'Piel', 'Corazón'], fr: ['Foie', 'Cerveau', 'Peau', 'Cœur'], de: ['Leber', 'Gehirn', 'Haut', 'Herz'], pt: ['Fígado', 'Cérebro', 'Pele', 'Coração'], ja: ['肝臓', '脳', '皮膚', '心臓'], zh: ['肝脏', '大脑', '皮肤', '心脏'] } },
  { answer: 1, q: { en: 'Which gas do humans need to breathe?', it: 'Quale gas ci serve per respirare?', es: '¿Qué gas necesitamos para respirar?', fr: "Quel gaz l'homme a-t-il besoin de respirer ?", de: 'Welches Gas brauchen Menschen zum Atmen?', pt: 'De qual gás precisamos para respirar?', ja: '人間が呼吸に必要な気体は？', zh: '人类呼吸需要哪种气体？' }, options: { en: ['Nitrogen', 'Oxygen', 'Hydrogen', 'Helium'], it: ['Azoto', 'Ossigeno', 'Idrogeno', 'Elio'], es: ['Nitrógeno', 'Oxígeno', 'Hidrógeno', 'Helio'], fr: ['Azote', 'Oxygène', 'Hydrogène', 'Hélium'], de: ['Stickstoff', 'Sauerstoff', 'Wasserstoff', 'Helium'], pt: ['Nitrogênio', 'Oxigênio', 'Hidrogênio', 'Hélio'], ja: ['窒素', '酸素', '水素', 'ヘリウム'], zh: ['氮气', '氧气', '氢气', '氦气'] } },
  { answer: 0, q: { en: 'What is the capital of France?', it: 'Qual è la capitale della Francia?', es: '¿Cuál es la capital de Francia?', fr: 'Quelle est la capitale de la France ?', de: 'Was ist die Hauptstadt von Frankreich?', pt: 'Qual é a capital da França?', ja: 'フランスの首都はどこ？', zh: '法国的首都是哪里？' }, options: { en: ['Paris', 'Lyon', 'Marseille', 'Nice'], it: ['Parigi', 'Lione', 'Marsiglia', 'Nizza'], es: ['París', 'Lyon', 'Marsella', 'Niza'], fr: ['Paris', 'Lyon', 'Marseille', 'Nice'], de: ['Paris', 'Lyon', 'Marseille', 'Nizza'], pt: ['Paris', 'Lyon', 'Marselha', 'Nice'], ja: ['パリ', 'リヨン', 'マルセイユ', 'ニース'], zh: ['巴黎', '里昂', '马赛', '尼斯'] } },
  { answer: 1, q: { en: 'What is the largest mammal?', it: 'Qual è il mammifero più grande?', es: '¿Cuál es el mamífero más grande?', fr: 'Quel est le plus grand mammifère ?', de: 'Was ist das größte Säugetier?', pt: 'Qual é o maior mamífero?', ja: '最も大きい哺乳類は？', zh: '最大的哺乳动物是什么？' }, options: { en: ['Elephant', 'Blue whale', 'Giraffe', 'Hippopotamus'], it: ['Elefante', 'Balenottera azzurra', 'Giraffa', 'Ippopotamo'], es: ['Elefante', 'Ballena azul', 'Jirafa', 'Hipopótamo'], fr: ['Éléphant', 'Baleine bleue', 'Girafe', 'Hippopotame'], de: ['Elefant', 'Blauwal', 'Giraffe', 'Nilpferd'], pt: ['Elefante', 'Baleia-azul', 'Girafa', 'Hipopótamo'], ja: ['ゾウ', 'シロナガスクジラ', 'キリン', 'カバ'], zh: ['大象', '蓝鲸', '长颈鹿', '河马'] } },
];
const ROUND_QS = 8; // questions per game (pool of 40)

function pick<T>(d: Record<string, T>, lang: string): T {
  return d[lang] ?? d.en;
}
function shuffle<T>(a: T[]): T[] {
  const r = a.slice();
  for (let i = r.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [r[i], r[j]] = [r[j], r[i]];
  }
  return r;
}

interface Player { name: string; score: number }
export interface QuizState {
  game: 'quiz';
  t: 'state';
  phase: 'question' | 'reveal' | 'done';
  hostId: string;
  players: Record<string, Player>;
  qIndex: number;
  total: number;
  packIndex: number; // which PACK question — each client renders it in its own language
  answeredIds: string[]; // who answered THIS question (no choices leaked)
  correct?: number; // reveal only
  choices?: Record<string, number>; // reveal only
}
interface AnswerMsg { game: 'quiz'; t: 'answer'; q: number; choice: number; by: string; name: string }

export class Quiz {
  private state: QuizState | null = null;
  private myChoice: number | null = null;
  private round: number[] = []; // host-only: chosen PACK indices
  private pending: Record<string, number> = {}; // host-only: answers for the current question
  private optionEls: HTMLButtonElement[] = [];

  constructor(
    private root: HTMLElement,
    private myId: string,
    private nameOf: (id: string) => string,
    private myLang: () => string,
    private send: (state: unknown) => void,
    private t: (k: string) => string,
  ) {
    const opts = root.querySelector('#quiz-options') as HTMLElement;
    for (let i = 0; i < 4; i++) {
      const b = document.createElement('button');
      b.className = 'quiz-option';
      b.addEventListener('click', () => this.choose(i));
      opts.appendChild(b);
      this.optionEls.push(b);
    }
    (root.querySelector('#quiz-action') as HTMLButtonElement).addEventListener('click', () =>
      this.onAction(),
    );
    this.render();
  }

  private isHost(): boolean {
    return !!this.state && this.state.hostId === this.myId;
  }

  // ---- actions ----------------------------------------------------------------

  private choose(i: number): void {
    const s = this.state;
    if (!s || s.phase !== 'question' || this.myChoice !== null) return;
    this.myChoice = i;
    this.render();
    const a: AnswerMsg = { game: 'quiz', t: 'answer', q: s.qIndex, choice: i, by: this.myId, name: this.nameOf(this.myId) };
    if (this.isHost()) this.recordAnswer(a);
    else this.send(a);
  }

  private onAction(): void {
    const s = this.state;
    if (!s) return this.startNew();
    if (!this.isHost()) {
      if (s.phase === 'done') this.startNew(); // take over a finished/stuck quiz
      return;
    }
    if (s.phase === 'question') this.reveal();
    else if (s.phase === 'reveal') this.next();
    else this.startNew();
  }

  private startNew(): void {
    this.round = shuffle(PACK.map((_, i) => i)).slice(0, Math.min(ROUND_QS, PACK.length));
    this.pending = {};
    this.myChoice = null;
    this.setState({
      game: 'quiz',
      t: 'state',
      phase: 'question',
      hostId: this.myId,
      players: { [this.myId]: { name: this.nameOf(this.myId), score: 0 } },
      qIndex: 0,
      total: this.round.length,
      packIndex: this.round[0],
      answeredIds: [],
    });
  }

  private recordAnswer(a: AnswerMsg): void {
    const s = this.state;
    if (!s || !this.isHost() || s.phase !== 'question' || a.q !== s.qIndex) return;
    if (this.pending[a.by] !== undefined) return; // first answer wins
    this.pending[a.by] = a.choice;
    if (!s.players[a.by]) s.players[a.by] = { name: a.name, score: 0 };
    this.setState({ ...s, answeredIds: Object.keys(this.pending) });
  }

  private reveal(): void {
    const s = this.state;
    if (!s) return;
    const correct = PACK[s.packIndex].answer;
    const players = { ...s.players };
    for (const [id, choice] of Object.entries(this.pending)) {
      if (choice === correct) players[id] = { ...players[id], score: (players[id]?.score ?? 0) + 1 };
    }
    this.setState({ ...s, phase: 'reveal', correct, choices: { ...this.pending }, players });
  }

  private next(): void {
    const s = this.state;
    if (!s) return;
    const qIndex = s.qIndex + 1;
    this.myChoice = null;
    this.pending = {};
    if (qIndex >= s.total) {
      this.setState({ ...s, phase: 'done', answeredIds: [], correct: undefined, choices: undefined });
      return;
    }
    this.setState({
      ...s,
      phase: 'question',
      qIndex,
      packIndex: this.round[qIndex],
      answeredIds: [],
      correct: undefined,
      choices: undefined,
    });
  }

  private setState(s: QuizState | null): void {
    this.state = s;
    this.render();
    this.send(s ?? null);
  }

  // ---- remote -----------------------------------------------------------------

  applyRemote(msg: unknown): void {
    if (!msg || typeof msg !== 'object') {
      this.state = null;
      this.render();
      return;
    }
    const m = msg as { t?: string; phase?: string; qIndex?: number };
    if (m.t === 'answer') {
      if (this.isHost()) this.recordAnswer(msg as AnswerMsg);
      return; // non-host ignores others' answers (secret until reveal)
    }
    if (this.state && (this.state.qIndex !== m.qIndex || this.state.phase !== m.phase)) {
      if (m.phase === 'question') this.myChoice = null;
    }
    this.state = msg as QuizState;
    this.render();
  }

  reset(): void {
    this.state = null;
    this.myChoice = null;
    this.pending = {};
    this.round = [];
    this.render();
  }

  // ---- rendering --------------------------------------------------------------

  private render(): void {
    const s = this.state;
    const statusEl = this.root.querySelector('#quiz-status') as HTMLElement;
    const qEl = this.root.querySelector('#quiz-question') as HTMLElement;
    const action = this.root.querySelector('#quiz-action') as HTMLButtonElement;
    const optsWrap = this.root.querySelector('#quiz-options') as HTMLElement;

    if (!s) {
      statusEl.textContent = '';
      qEl.textContent = '';
      optsWrap.hidden = true;
      action.textContent = this.t('quizStart');
      action.hidden = false;
      return;
    }

    if (s.phase === 'done') {
      optsWrap.hidden = true;
      qEl.textContent = '🏆';
      statusEl.innerHTML = this.leaderboard(s);
      action.textContent = this.t('quizNew');
      action.hidden = false;
      return;
    }

    const lang = this.myLang();
    const pq = PACK[s.packIndex];
    const options = pick(pq.options, lang);
    optsWrap.hidden = false;
    qEl.textContent = `${s.qIndex + 1}/${s.total} · ${pick(pq.q, lang)}`;
    const revealing = s.phase === 'reveal';
    this.optionEls.forEach((b, i) => {
      b.textContent = options[i] ?? '';
      b.hidden = i >= options.length;
      b.classList.toggle('correct', revealing && s.correct === i);
      b.classList.toggle('chosen', this.myChoice === i);
      b.classList.toggle('wrong', revealing && this.myChoice === i && s.correct !== i);
      b.disabled = revealing || this.myChoice !== null;
    });

    if (revealing) {
      statusEl.textContent = this.scoreLine(s);
      action.hidden = !this.isHost();
      action.textContent = s.qIndex + 1 >= s.total ? this.t('quizFinish') : this.t('quizNext');
    } else {
      statusEl.textContent =
        this.myChoice !== null
          ? this.t('quizWaiting')
          : this.t('quizAnswered').replace('{n}', String(s.answeredIds.length));
      action.hidden = !this.isHost();
      action.textContent = this.t('quizReveal');
    }
  }

  private scoreLine(s: QuizState): string {
    return Object.values(s.players)
      .sort((a, b) => b.score - a.score)
      .map((p) => `${p.name}: ${p.score}`)
      .join(' · ');
  }

  private leaderboard(s: QuizState): string {
    return Object.values(s.players)
      .sort((a, b) => b.score - a.score)
      .map((p, i) => `<div class="quiz-rank"><span>${i + 1}. ${escapeText(p.name)}</span><strong>${p.score}</strong></div>`)
      .join('');
  }
}

function escapeText(s: string): string {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}
