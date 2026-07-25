//! Entrega del texto a la aplicación que tiene el foco.
//!
//! Como la isla nunca se activa (ver `win.rs`), el cursor sigue exactamente donde el usuario
//! lo dejó, así que basta con poner el texto en el portapapeles y sintetizar `Ctrl+V`.
//!
//! El portapapeles se guarda al empezar a dictar y se devuelve al terminar, **no en cada
//! frase**: al pegar varias frases seguidas, restaurar entre una y otra provocaría carreras
//! con el pegado siguiente.

use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use std::{thread, time::Duration};

/// Código de tecla virtual de Windows para la V.
///
/// Se usa `Key::Other` en lugar de `Key::Unicode('v')` a propósito: en Windows, `Unicode`
/// se envía con `KEYEVENTF_UNICODE`, que ignora los modificadores y haría que `Ctrl` no
/// contase, pegando una "v" literal en vez de ejecutar el pegado.
const VK_V: u32 = 0x56;

/// Lo que el usuario tuviera copiado antes de empezar a dictar.
pub fn snapshot() -> Option<String> {
    Clipboard::new().ok()?.get_text().ok()
}

/// Cuánto se espera antes de devolver el portapapeles a su contenido original.
///
/// Generoso a propósito. El Bloc de notas pega al instante, pero los editores web
/// (Claude, Notion, Google Docs) procesan el pegado en JavaScript y tardan bastante más.
/// Restaurar demasiado pronto se los deja sin contenido que leer: parece que Riff "no
/// hace nada" justo en las aplicaciones donde más se dicta.
const RESTORE_DELAY: Duration = Duration::from_millis(1500);

/// Margen entre copiar y enviar Ctrl+V, para que Windows publique el nuevo contenido.
const CLIPBOARD_SETTLE: Duration = Duration::from_millis(90);

/// Devuelve el portapapeles a su contenido original, tras dar margen al último pegado.
pub fn restore(previous: String) {
    thread::spawn(move || {
        thread::sleep(RESTORE_DELAY);
        if let Ok(mut clipboard) = Clipboard::new() {
            let _ = clipboard.set_text(previous);
        }
    });
}

pub fn paste_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Ok(());
    }

    let mut clipboard = Clipboard::new().map_err(|e| format!("portapapeles: {e}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| format!("no se pudo copiar: {e}"))?;
    drop(clipboard);

    // Windows necesita un instante para que el nuevo contenido esté disponible.
    thread::sleep(CLIPBOARD_SETTLE);

    let mut enigo =
        Enigo::new(&Settings::default()).map_err(|e| format!("no se pudo simular teclado: {e}"))?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|e| e.to_string())?;
    let pressed = enigo.key(Key::Other(VK_V), Direction::Click);
    // Soltar Ctrl pase lo que pase: dejarlo pulsado dejaría el teclado inservible.
    let released = enigo.key(Key::Control, Direction::Release);
    pressed.map_err(|e| e.to_string())?;
    released.map_err(|e| e.to_string())?;

    Ok(())
}
