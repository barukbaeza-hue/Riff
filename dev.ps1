# Arranca Riff en modo desarrollo.
# Los cambios en src/ (HTML, CSS, JS) se ven recargando la ventana; los de Rust
# requieren recompilar, así que conviene tocar la interfaz sin parar el proceso.

$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
Set-Location (Join-Path $PSScriptRoot "src-tauri")
cargo run
