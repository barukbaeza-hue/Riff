//! Cliente de Groq: transcripción y pulido.
//!
//! Replica la arquitectura de dos etapas de Wispr Flow. Whisper acierta las palabras pero es
//! irregular con la puntuación española (sobre todo con los signos de apertura `¿` y `¡`);
//! la segunda pasada por un LLM es lo que hace que el resultado se sienta escrito y no
//! dictado.

use serde::Deserialize;
use std::time::Duration;

const TRANSCRIBE_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
const CHAT_URL: &str = "https://api.groq.com/openai/v1/chat/completions";

pub const ASR_MODEL: &str = "whisper-large-v3";

/// Modelos de pulido, en orden de preferencia.
///
/// Va primero el 20b **por velocidad**: corre a ~1000 tokens/s frente a los ~500 del 120b,
/// y colocar bien comas y signos de apertura es una tarea sencilla que no necesita un
/// modelo grande. El 120b queda de red de seguridad.
///
/// (Groq deprecó `llama-3.3-70b-versatile` en junio de 2026 para las cuentas gratuitas.)
pub const POLISH_MODELS: [&str; 2] = ["openai/gpt-oss-20b", "openai/gpt-oss-120b"];

/// Whisper imita el estilo del `prompt`, así que se le siembra un texto que contiene
/// justo la puntuación que queremos que reproduzca.
const ASR_PROMPT: &str = "Hola, ¿cómo estás? ¡Qué gusto verte! Bueno… vamos a ver: esto es \
una prueba de dictado; sí, exactamente. Entonces, ¿lo dejamos así?";

const POLISH_SYSTEM: &str = "Eres el corrector de un sistema de dictado por voz en español. \
Recibes la transcripción automática de una grabación y devuelves EXCLUSIVAMENTE el texto \
corregido: sin comillas, sin encabezados, sin explicaciones y sin comentarios de ningún tipo.

Reglas:
- Corrige la puntuación: comas, puntos, dos puntos, punto y coma y puntos suspensivos donde \
corresponda.
- Abre SIEMPRE las preguntas con ¿ y las exclamaciones con ¡.
- Corrige tildes, mayúsculas de inicio de frase y nombres propios.
- Elimina muletillas y titubeos (eh, em, este, mmm, «o sea» repetido) y las repeticiones \
involuntarias.
- Separa en párrafos solo si el dictado es claramente largo.
- NO resumas, NO parafrasees, NO traduzcas y NO añadas nada que el hablante no haya dicho. \
Respeta sus palabras y su registro.
- Si el texto ya está correcto, devuélvelo tal cual.
- Si la transcripción está vacía o es ininteligible, devuelve una cadena vacía.";

/// Tiempo máximo de la transcripción. Groq va a ~216x tiempo real; si tarda más que esto,
/// el problema es la red.
const ASR_TIMEOUT: Duration = Duration::from_secs(30);
/// El pulido es un lujo: si no llega rápido, se pega la transcripción cruda.
const POLISH_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())
}

/// Sube el WAV a Groq y devuelve la transcripción cruda.
pub async fn transcribe(api_key: &str, wav: Vec<u8>, language: &str) -> Result<String, String> {
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;

    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", ASR_MODEL)
        .text("language", language.to_string())
        .text("response_format", "json")
        .text("temperature", "0")
        .text("prompt", ASR_PROMPT);

    let response = client(ASR_TIMEOUT)?
        .post(TRANSCRIBE_URL)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("no se pudo contactar con Groq: {e}"))?;

    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;

    if !status.is_success() {
        return Err(explain_error(status, &body));
    }

    let parsed: TranscriptionResponse =
        serde_json::from_str(&body).map_err(|e| format!("respuesta inesperada de Groq: {e}"))?;

    let text = parsed.text.trim().to_string();
    if is_hallucination(&text) {
        return Ok(String::new());
    }

    Ok(text)
}

/// Whisper se entrenó con subtítulos de vídeo, así que cuando recibe audio sin voz clara
/// tiende a "recordar" coletillas de ese material en vez de devolver una cadena vacía.
///
/// Solo se filtran frases que nadie dictaría a propósito. Cosas ambiguas como un "Gracias"
/// suelto se dejan pasar: de ese ruido ya se encarga el detector de voz sostenida.
fn is_hallucination(text: &str) -> bool {
    let lowered = text.to_lowercase();
    const NOISE: [&str; 6] = [
        "amara.org",
        "subtítulos realizados por",
        "subtítulos por",
        "suscríbete al canal",
        "gracias por ver el video",
        "gracias por ver el vídeo",
    ];
    NOISE.iter().any(|marker| lowered.contains(marker))
}

/// Segunda etapa. Nunca falla hacia fuera: si algo va mal, se devuelve el texto original,
/// porque perder un dictado por culpa del pulido sería inaceptable.
pub async fn polish(api_key: &str, text: &str) -> String {
    for model in POLISH_MODELS {
        match try_polish(api_key, text, model).await {
            Ok(polished) if !polished.trim().is_empty() => return polished,
            Ok(_) => return text.to_string(),
            Err(error) => {
                eprintln!("[riff] pulido con {model} falló: {error}");
                // Se prueba el siguiente de la cadena.
            }
        }
    }
    text.to_string()
}

async fn try_polish(api_key: &str, text: &str, model: &str) -> Result<String, String> {
    let payload = serde_json::json!({
        "model": model,
        "temperature": 0,
        "max_tokens": 2048,
        "messages": [
            { "role": "system", "content": POLISH_SYSTEM },
            { "role": "user", "content": text }
        ]
    });

    let response = client(POLISH_TIMEOUT)?
        .post(CHAT_URL)
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;

    if !status.is_success() {
        return Err(explain_error(status, &body));
    }

    let parsed: ChatResponse = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let content = parsed
        .choices
        .first()
        .map(|choice| choice.message.content.trim().to_string())
        .unwrap_or_default();

    Ok(strip_wrapping_quotes(&content))
}

/// Algunos modelos devuelven el texto entrecomillado pese a pedir lo contrario.
fn strip_wrapping_quotes(text: &str) -> String {
    let trimmed = text.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('«')
                .and_then(|rest| rest.strip_suffix('»'))
        });
    unquoted.unwrap_or(trimmed).trim().to_string()
}

/// Mensajes en español y accionables, en vez del JSON crudo de la API.
fn explain_error(status: reqwest::StatusCode, body: &str) -> String {
    match status.as_u16() {
        401 => "API key de Groq inválida. Revísala en Ajustes.".to_string(),
        413 => "La grabación es demasiado larga.".to_string(),
        429 => "Límite de Groq alcanzado. Espera un momento.".to_string(),
        500..=599 => "Groq no está respondiendo. Inténtalo de nuevo.".to_string(),
        _ => {
            let detail: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
            let message = detail
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .unwrap_or(body);
            format!("Groq respondió {status}: {message}")
        }
    }
}
