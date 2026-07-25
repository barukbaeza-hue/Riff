// Isla dinámica: solo dibuja. Toda la lógica vive en Rust.
//
// El backend emite tres eventos:
//   riff://state    { state: "listening" | "transcribing" | "done" | "error", message }
//   riff://level    número 0..1 con el pico del bloque de audio, cada ~40 ms
//   riff://preview  texto acumulado que Riff lleva entendido

const BAR_COUNT = 12;

const ISLAND = document.getElementById("island");
const BARS = document.getElementById("bars");
const TITLE = document.getElementById("title");
const PREVIEW = document.getElementById("preview");
const TIMER = document.getElementById("timer");

const TITLES = {
  listening: "Escuchando",
  paused: "En pausa",
  transcribing: "Transcribiendo…",
  done: "Listo",
};

// Historial de niveles que se desplaza hacia la izquierda, como un sismógrafo.
const history = new Array(BAR_COUNT).fill(0);
const elements = [];

for (let i = 0; i < BAR_COUNT; i += 1) {
  const bar = document.createElement("div");
  bar.className = "bar";
  BARS.appendChild(bar);
  elements.push(bar);
}

let smoothed = 0;
let timerId = null;
let resetId = null;
let startedAt = 0;
/** Tiempo dictado antes de la pausa actual: el contador debe seguir, no reiniciarse. */
let accumulated = 0;
let previousState = "idle";

/** Curva de respuesta: la voz normal pica bajo, así que se realza sin saturar. */
function shape(level) {
  return Math.pow(Math.min(level * 2.4, 1), 0.62);
}

function render() {
  for (let i = 0; i < BAR_COUNT; i += 1) {
    // Las barras se atenúan hacia los extremos: da sensación de foco en el centro.
    const distance = Math.abs(i - (BAR_COUNT - 1) / 2) / ((BAR_COUNT - 1) / 2);
    const falloff = 0.5 + 0.5 * (1 - distance * distance);
    const value = Math.max(history[i] * falloff, 0.08);
    elements[i].style.transform = `scaleY(${value.toFixed(3)})`;
  }
}

function pushLevel(level) {
  // Suavizado exponencial asimétrico: sube rápido para no perder ataques de voz,
  // baja lento para que no parpadee.
  const target = shape(level);
  smoothed = target > smoothed ? target : smoothed * 0.72 + target * 0.28;

  history.shift();
  history.push(smoothed);
  render();
}

function resetBars() {
  history.fill(0);
  smoothed = 0;
  render();
}

function formatElapsed(millis) {
  const total = Math.floor(millis / 1000);
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

function runTimer() {
  startedAt = Date.now();
  stopTimer();
  // Un tick por segundo basta y no despierta el CPU más de lo necesario.
  timerId = setInterval(() => {
    TIMER.textContent = formatElapsed(accumulated + (Date.now() - startedAt));
  }, 1000);
}

/** Empieza un dictado nuevo desde cero. */
function startTimer() {
  accumulated = 0;
  TIMER.textContent = "0:00";
  runTimer();
}

/** Congela el contador conservando lo ya dictado. */
function pauseTimer() {
  accumulated += Date.now() - startedAt;
  stopTimer();
  TIMER.textContent = formatElapsed(accumulated);
}

function stopTimer() {
  if (timerId !== null) {
    clearInterval(timerId);
    timerId = null;
  }
}

/** Relleno vertical de la burbuja, repartido arriba y abajo. Debe coincidir con el CSS. */
const BUBBLE_PADDING = 28;
/** Tope antes de que el texto empiece a desplazarse dentro en vez de seguir creciendo. */
const MAX_BUBBLE = 420;

/** Ritmo al que se van descubriendo las palabras nuevas. */
const REVEAL_INTERVAL = 26;

let displayed = "";
let target = "";
let revealId = null;

/** Vuelca el texto en la burbuja y ajusta su altura a lo que ocupe. */
function paint(text) {
  PREVIEW.textContent = text;
  ISLAND.dataset.text = text ? "filled" : "empty";

  // La silueta negra es un div vacío: no puede crecer sola con el texto. Se mide el
  // contenido real y se pasa la altura por variable CSS, que leen a la vez la silueta y
  // la capa de contenido. Sin esto, una quedaría más alta que la otra.
  const content = text ? Math.min(PREVIEW.scrollHeight, MAX_BUBBLE) : 0;
  ISLAND.style.setProperty("--bubble-h", text ? `${content + BUBBLE_PADDING}px` : "0px");

  // Siempre visible lo último dicho.
  PREVIEW.scrollTop = PREVIEW.scrollHeight;
}

function stopReveal() {
  if (revealId !== null) {
    clearInterval(revealId);
    revealId = null;
  }
}

/** Posición del final de la siguiente palabra. */
function nextBoundary(text, from) {
  const space = text.indexOf(" ", from + 1);
  return space === -1 ? text.length : space;
}

function startReveal() {
  if (revealId !== null) return;
  revealId = setInterval(() => {
    if (displayed.length >= target.length) {
      stopReveal();
      return;
    }
    displayed = target.slice(0, nextBoundary(target, displayed.length));
    paint(displayed);
  }, REVEAL_INTERVAL);
}

/**
 * Texto que Riff va entendiendo. Ocupa el lugar del nombre del artista.
 *
 * Whisper devuelve la frase entera de una vez, así que esto no es transcripción palabra a
 * palabra de verdad: es el texto ya recibido, descubierto poco a poco para que se lea como
 * si se estuviera escribiendo. Cuando llega la versión pulida el texto anterior cambia, y
 * entonces se sustituye de golpe en vez de volver a animarlo.
 */
function setPreview(text) {
  const clean = (text || "").trim();

  if (!clean) {
    stopReveal();
    displayed = "";
    target = "";
    paint("");
    return;
  }

  if (clean.startsWith(displayed) && clean.length > displayed.length) {
    target = clean;
    startReveal();
    return;
  }

  // No es una continuación sino una corrección: aparece ya arreglada.
  stopReveal();
  displayed = clean;
  target = clean;
  paint(clean);
}

function setState(state, message) {
  if (resetId !== null) {
    clearTimeout(resetId);
    resetId = null;
  }

  const wasPaused = previousState === "paused";
  previousState = state;

  ISLAND.dataset.state = state;
  TITLE.textContent = state === "error" ? message || "Algo salió mal" : TITLES[state] || "";

  if (state === "listening") {
    resetBars();
    // Al reanudar se continúa el contador; solo un dictado nuevo lo pone a cero.
    if (wasPaused) {
      runTimer();
    } else {
      startTimer();
    }
    return;
  }

  if (state === "paused") {
    pauseTimer();
    resetBars();
    return;
  }

  stopTimer();

  if (state === "done" || state === "error") {
    // El backend ocultará la ventana; volver a idle deja la isla lista para la próxima
    // apertura sin que se vea el estado anterior durante un fotograma.
    resetId = setTimeout(() => {
      ISLAND.dataset.state = "idle";
      resetBars();
      setPreview("");
    }, 3400);
  }
}

render();

const tauri = window.__TAURI__;
if (tauri && tauri.event) {
  tauri.event.listen("riff://level", (event) => pushLevel(event.payload ?? 0));
  tauri.event.listen("riff://preview", (event) => setPreview(event.payload));
  tauri.event.listen("riff://state", (event) => {
    const payload = event.payload || {};
    setState(payload.state || "idle", payload.message);
  });
}
