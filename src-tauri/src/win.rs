//! Ajustes específicos de Windows sobre la ventana de la isla.
//!
//! Este módulo sostiene el producto entero: sin `WS_EX_NOACTIVATE` la isla roba el foco al
//! aparecer, el cursor de texto del usuario se pierde y el `Ctrl+V` posterior no tiene dónde
//! aterrizar. Se declaran los símbolos de `user32` a mano para no arrastrar la crate
//! `windows`, que es enorme y costaría varios minutos de compilación.

const GWL_EXSTYLE: i32 = -20;

/// La ventana nunca se activa aunque se pulse sobre ella.
const WS_EX_NOACTIVATE: isize = 0x0800_0000;
/// Además la saca del Alt+Tab, que para un overlay es lo correcto.
const WS_EX_TOOLWINDOW: isize = 0x0000_0080;

#[link(name = "user32")]
extern "system" {
    fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: isize, index: i32, new_long: isize) -> isize;
}

/// Marca la ventana como no activable. Debe llamarse una sola vez, al arrancar.
pub fn make_non_activating(hwnd: isize) {
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            current | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        );
    }
}
