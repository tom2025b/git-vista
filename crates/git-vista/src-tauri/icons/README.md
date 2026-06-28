Generate the app icons here with:

    cargo tauri icon path/to/source-1024x1024.png

That produces the PNG/ICNS/ICO files referenced by `bundle.icon` in
`tauri.conf.json`. They are required for `tauri build`; `tauri dev` runs without
them.
