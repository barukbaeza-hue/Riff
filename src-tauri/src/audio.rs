//! Captura de micrófono con cpal, en un hilo propio, segmentada por pausas naturales.
//!
//! El `Stream` de cpal no es `Send` en Windows (WASAPI ata el stream al hilo que lo crea),
//! así que no puede guardarse en el estado compartido de Tauri. La solución es un hilo
//! dedicado que posee el stream durante toda la grabación y se comunica por canales.
//!
//! Capturar desde Rust en vez de con `getUserMedia` evita el diálogo de permiso de WebView2.
//!
//! ## Por qué se corta en los silencios
//!
//! Para que el texto aparezca mientras hablas hay que trocear el audio, pero **no se puede
//! trocear cada N segundos**: cortarías palabras por la mitad y Whisper perdería el contexto
//! que necesita para puntuar. Cortando en las pausas al respirar, cada trozo es una frase
//! entera, así que los `¿` `¡` y las comas salen bien porque el modelo la oyó completa.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Frecuencia que espera Whisper. Enviar más es malgastar ancho de banda.
const TARGET_SAMPLE_RATE: u32 = 16_000;
/// Cada cuánto se informa del nivel a la interfaz. 40 ms ≈ 25 fps.
const LEVEL_INTERVAL: Duration = Duration::from_millis(40);
/// Por debajo de este pico se considera que el bloque es silencio, a efectos de decidir
/// dónde termina una frase.
const SILENCE_PEAK: f32 = 0.022;

/// Umbral, más exigente, para aceptar que algo es voz de verdad y no ruido de la mesa.
const VOICE_PEAK: f32 = 0.045;

/// Cuánto debe mantenerse el sonido por encima de `VOICE_PEAK` para contar como voz.
///
/// Este es el filtro que descarta el teclado: pulsar una tecla es un chasquido de 10-30 ms,
/// mientras que la sílaba más breve dura bastante más de 90 ms. Sin esto, Whisper recibe
/// audio con solo ruido y se inventa texto (el clásico "Gracias." o "Sí.").
const SUSTAINED_VOICE: Duration = Duration::from_millis(90);

/// Voz acumulada mínima para que merezca la pena gastar una petición en el segmento.
const MIN_VOICED: Duration = Duration::from_millis(350);

/// Pausa necesaria para dar una frase por terminada.
///
/// Es el mayor coste fijo de latencia: hasta que no pasa este tiempo no se empieza a
/// transcribir. Bajarlo acelera la sensación de tiempo real, pero si se acorta demasiado
/// se parte la frase cada vez que uno se para a pensar. 450 ms es el punto donde deja de
/// notarse la espera sin trocear el habla normal.
const SILENCE_TO_CUT: Duration = Duration::from_millis(450);
/// Por debajo de esto no merece la pena gastar una petición: es un ruido, no una frase.
const MIN_SEGMENT: Duration = Duration::from_millis(600);
/// Si alguien habla sin respirar, se corta igualmente para no acumular latencia.
const MAX_SEGMENT: Duration = Duration::from_secs(18);
/// Margen que se conserva tras la última voz, para no comerse la consonante final.
const TAIL_MARGIN: Duration = Duration::from_millis(150);
/// Frecuencia con la que se revisa si hay una frase lista. Añade su propio retardo medio,
/// así que conviene tenerlo bajo: comprobar un contador cada 40 ms no cuesta nada.
const POLL: Duration = Duration::from_millis(40);

pub enum Cmd {
    Start,
    /// Detiene el micrófono conservando lo dicho hasta ahora. Cierra la frase en curso.
    Pause,
    /// Vuelve a abrir el micrófono para seguir dictando.
    Resume,
    /// Cierra la grabación. Responde cuando ha emitido la última frase pendiente.
    Stop(Sender<Result<(), String>>),
}

/// Estado que comparten el callback de audio (que solo acumula) y el hilo de control
/// (que decide cuándo cortar). El callback es tiempo real: no debe hacer nada pesado.
struct Capture {
    samples: Vec<f32>,
    /// Muestras consecutivas de silencio al final del búfer.
    trailing_silence: usize,
    /// Muestras consecutivas por encima de `VOICE_PEAK`. Un chasquido de tecla nunca
    /// acumula suficientes; una sílaba sí.
    loud_run: usize,
    /// Total de muestras que han llegado a considerarse voz en este segmento.
    voiced: usize,
    has_voice: bool,
}

impl Capture {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
            trailing_silence: 0,
            loud_run: 0,
            voiced: 0,
            has_voice: false,
        }
    }

    fn reset_segment(&mut self) {
        self.samples.clear();
        self.trailing_silence = 0;
        self.loud_run = 0;
        self.voiced = 0;
        self.has_voice = false;
    }
}

/// Lanza el hilo de audio y devuelve el extremo por el que se le dan órdenes.
///
/// - `on_level`: pico normalizado (0.0–1.0) de cada bloque, para la waveform.
/// - `on_segment`: un WAV listo para transcribir, cada vez que se cierra una frase.
pub fn spawn(
    on_level: Arc<dyn Fn(f32) + Send + Sync>,
    on_segment: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) -> Sender<Cmd> {
    let (tx, rx) = mpsc::channel::<Cmd>();

    std::thread::spawn(move || {
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Start => record_session(&rx, &on_level, &on_segment),
                Cmd::Stop(reply) => {
                    let _ = reply.send(Err("no había ninguna grabación".to_string()));
                }
                // Sin sesión abierta no hay nada que pausar ni reanudar.
                Cmd::Pause | Cmd::Resume => {}
            }
        }
    });

    tx
}

fn record_session(
    rx: &Receiver<Cmd>,
    on_level: &Arc<dyn Fn(f32) + Send + Sync>,
    on_segment: &Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    let capture = Arc::new(Mutex::new(Capture::new()));

    let (stream, sample_rate) = match start_stream(capture.clone(), on_level.clone()) {
        Ok(started) => started,
        Err(error) => {
            // Aunque falle el micrófono hay que atender el Stop que llegará,
            // o la interfaz se queda esperando una respuesta que nunca llega.
            loop {
                match rx.recv() {
                    Ok(Cmd::Stop(reply)) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }
    };

    let mut paused = false;

    loop {
        match rx.recv_timeout(POLL) {
            Ok(Cmd::Stop(reply)) => {
                // El stream debe morir antes de leer el búfer: mientras viva, su callback
                // sigue escribiendo desde el hilo de audio de WASAPI.
                drop(stream);
                flush_pending(&capture, sample_rate, on_segment);
                let _ = reply.send(Ok(()));
                return;
            }
            Ok(Cmd::Pause) => {
                if !paused {
                    let _ = stream.pause();
                    paused = true;
                    // Al pausar se cierra la frase en curso: así el texto dicho hasta aquí
                    // aparece en la isla en vez de quedarse esperando en el búfer.
                    flush_pending(&capture, sample_rate, on_segment);
                }
            }
            Ok(Cmd::Resume) => {
                if paused {
                    let _ = stream.play();
                    paused = false;
                }
            }
            Ok(Cmd::Start) => {}
            Err(RecvTimeoutError::Timeout) => {
                if !paused {
                    take_ready_segment(&capture, sample_rate, on_segment);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Extrae una frase si ya hay pausa suficiente (o si se alcanzó el máximo).
fn take_ready_segment(
    capture: &Arc<Mutex<Capture>>,
    sample_rate: u32,
    on_segment: &Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    let rate = sample_rate as f32;
    let silence_needed = (SILENCE_TO_CUT.as_secs_f32() * rate) as usize;
    let min_len = (MIN_SEGMENT.as_secs_f32() * rate) as usize;
    let max_len = (MAX_SEGMENT.as_secs_f32() * rate) as usize;
    let margin = (TAIL_MARGIN.as_secs_f32() * rate) as usize;

    let min_voiced = (MIN_VOICED.as_secs_f32() * rate) as usize;

    let segment = {
        let Ok(mut capture) = capture.lock() else {
            return;
        };

        if !capture.has_voice || capture.samples.len() < min_len {
            return;
        }

        let by_pause = capture.trailing_silence >= silence_needed;
        let by_length = capture.samples.len() >= max_len;
        if !by_pause && !by_length {
            return;
        }

        // Se descarta la pausa final, dejando un pequeño margen: enviar 700 ms de silencio
        // a Groq no aporta nada y consume cuota.
        let keep = if by_pause {
            capture
                .samples
                .len()
                .saturating_sub(capture.trailing_silence)
                + margin
        } else {
            capture.samples.len()
        };
        let keep = keep.min(capture.samples.len());

        let mut taken: Vec<f32> = capture.samples.drain(..keep).collect();
        let voiced = capture.voiced;
        // Lo que queda es cola de silencio: se descarta para empezar la frase limpia.
        capture.reset_segment();

        // Aunque el búfer sea largo, si dentro apenas hubo voz esto era ruido de fondo o
        // teclado. Mandarlo a Whisper solo produciría texto inventado.
        if taken.len() < min_len || voiced < min_voiced {
            taken.clear();
        }
        taken
    };

    emit(segment, sample_rate, on_segment);
}

/// Envía lo que quede en el búfer aunque no haya pausa final. Se usa al pausar y al cerrar.
fn flush_pending(
    capture: &Arc<Mutex<Capture>>,
    sample_rate: u32,
    on_segment: &Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    let min_voiced = (MIN_VOICED.as_secs_f32() * sample_rate as f32) as usize;

    let segment = {
        let Ok(mut capture) = capture.lock() else {
            return;
        };
        let enough = capture.has_voice && capture.voiced >= min_voiced;
        let samples = std::mem::take(&mut capture.samples);
        // En ambos casos se deja el segmento limpio: al reanudar se empieza de cero.
        capture.reset_segment();
        if !enough {
            return;
        }
        samples
    };

    emit(segment, sample_rate, on_segment);
}

fn emit(samples: Vec<f32>, sample_rate: u32, on_segment: &Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
    if samples.is_empty() {
        return;
    }
    match encode_wav(&samples, sample_rate) {
        Ok(wav) => on_segment(wav),
        Err(error) => eprintln!("[riff] segmento descartado: {error}"),
    }
}

type StartedStream = (cpal::Stream, u32);

fn start_stream(
    capture: Arc<Mutex<Capture>>,
    on_level: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<StartedStream, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("no se encontró ningún micrófono")?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("no se pudo abrir el micrófono: {e}"))?;

    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.config();

    // El tipo se anota a propósito: se usa en tres ramas distintas del match y sin la
    // anotación la inferencia no tiene de dónde deducirlo.
    let on_error = |error: cpal::StreamError| eprintln!("[riff] error de audio: {error}");

    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            let mut collect =
                collector(capture, on_level, channels, sample_rate, |sample: f32| sample);
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| collect(data),
                on_error,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let mut collect = collector(capture, on_level, channels, sample_rate, |sample: i16| {
                sample as f32 / i16::MAX as f32
            });
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| collect(data),
                on_error,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let mut collect = collector(capture, on_level, channels, sample_rate, |sample: u16| {
                (sample as f32 / u16::MAX as f32) * 2.0 - 1.0
            });
            device.build_input_stream(
                &config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| collect(data),
                on_error,
                None,
            )
        }
        other => return Err(format!("formato de audio no soportado: {other:?}")),
    }
    .map_err(|e| format!("no se pudo iniciar la captura: {e}"))?;

    stream
        .play()
        .map_err(|e| format!("no se pudo grabar: {e}"))?;

    Ok((stream, sample_rate))
}

/// Callback de tiempo real: mezcla a mono, acumula y mide el silencio. Nada más.
fn collector<T: Copy + 'static>(
    capture: Arc<Mutex<Capture>>,
    on_level: Arc<dyn Fn(f32) + Send + Sync>,
    channels: usize,
    sample_rate: u32,
    to_f32: impl Fn(T) -> f32 + Send + 'static,
) -> impl FnMut(&[T]) + Send + 'static {
    let mut last_report = Instant::now();

    move |data: &[T]| {
        let channels = channels.max(1);
        let mut peak = 0.0f32;
        let mut mono = Vec::with_capacity(data.len() / channels + 1);

        for frame in data.chunks(channels) {
            let sample = frame.iter().map(|&s| to_f32(s)).sum::<f32>() / channels as f32;
            peak = peak.max(sample.abs());
            mono.push(sample);
        }

        if let Ok(mut capture) = capture.lock() {
            let added = mono.len();
            let sustained = (SUSTAINED_VOICE.as_secs_f32() * sample_rate as f32) as usize;
            capture.samples.append(&mut mono);

            // Silencio: sirve para saber dónde termina la frase.
            if peak < SILENCE_PEAK {
                capture.trailing_silence += added;
            } else {
                capture.trailing_silence = 0;
            }

            // Voz: mucho más exigente. Solo cuenta si se mantiene en el tiempo, que es lo
            // que distingue una sílaba de un golpe de tecla.
            if peak >= VOICE_PEAK {
                capture.loud_run += added;
                if capture.loud_run >= sustained {
                    capture.has_voice = true;
                    capture.voiced += added;
                }
            } else {
                capture.loud_run = 0;
            }
        }

        if last_report.elapsed() >= LEVEL_INTERVAL {
            last_report = Instant::now();
            on_level(peak.min(1.0));
        }
    }
}

/// Remuestrea a 16 kHz mono, normaliza con ganancia limitada y escribe un WAV en memoria.
fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, String> {
    if samples.is_empty() {
        return Err("segmento vacío".to_string());
    }

    let peak = samples.iter().fold(0.0f32, |max, &s| max.max(s.abs()));
    if peak < VOICE_PEAK {
        return Err("sin voz".to_string());
    }

    let resampled = resample(samples, sample_rate, TARGET_SAMPLE_RATE);

    // El micrófono de un portátil graba bajo y Whisper agradece una señal sana, pero la
    // ganancia se limita a 4x: amplificar más solo realza el ruido de la mesa y el teclado.
    let gain = (0.95 / peak).min(4.0);

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).map_err(|e| format!("wav: {e}"))?;
        for sample in resampled {
            let value = (sample * gain).clamp(-1.0, 1.0);
            writer
                .write_sample((value * i16::MAX as f32) as i16)
                .map_err(|e| format!("wav: {e}"))?;
        }
        writer.finalize().map_err(|e| format!("wav: {e}"))?;
    }

    Ok(cursor.into_inner())
}

/// Remuestreo lineal. Es suficiente para voz y no arrastra ninguna dependencia extra.
fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return samples.to_vec();
    }

    let ratio = to as f64 / from as f64;
    let out_len = ((samples.len() as f64) * ratio).round() as usize;
    let last = samples.len() - 1;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let position = i as f64 / ratio;
        let index = position.floor() as usize;
        if index >= last {
            out.push(samples[last]);
            continue;
        }
        let fraction = (position - index as f64) as f32;
        out.push(samples[index] * (1.0 - fraction) + samples[index + 1] * fraction);
    }

    out
}
