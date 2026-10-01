# scripts/ agent rules

## Installers

- `install.sh` and `install.ps1` install the latest GitHub Release of
  `daftpunkwav/wave-code`: a bare binary, SHA-256 checked against the
  published checksum, into `~/.cargo/bin` or `WAVECODE_INSTALL_DIR`.
- A change to asset names or verification updates both scripts.
- Unix targets stay in `install.sh`. `install.ps1` targets
  `x86_64-pc-windows-msvc`.
- Both scripts resolve the release tag once and download the binary
  and the checksum from that same tag.
- Do not embed tokens, credentials, or a private download URL.

## conhost sweep

- `conhost-sweep.ps1` may stop a process only when the image is
  `conhost.exe`, the command line contains `--headless`, and the
  parent process is gone.
- Leave every other process running.
