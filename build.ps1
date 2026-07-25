# Genera el instalador de Riff.
# Requiere tauri-cli:  cargo install tauri-cli --version "^2"
# El resultado queda en src-tauri/target/release/bundle/nsis/

$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
Set-Location (Join-Path $PSScriptRoot "src-tauri")
cargo tauri build
