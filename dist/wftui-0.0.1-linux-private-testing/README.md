# WFTUI — CLASSIFIED DEVELOPMENT BUILD

## ADVANCED ARTIFICIAL INTELLIGENCE. SECRETIVE SOFTWARE FROM THE FUTURE.

**AUTHORIZED TESTER: JOSEPHUR**  
**STATUS: IN DEVELOPMENT**  
**DISTRIBUTION: PRIVATE TESTING ONLY — NOT FOR REDISTRIBUTION**

You have been entrusted with a highly secretive, AI-produced terminal client for WindowsForum.com, forged with advanced artificial intelligence and delivered suspiciously ahead of its own timeline.

This is experimental software from the future. The future still has bugs.

**Do not redistribute, re-upload, mirror, publish, or forward this package.** No public releases, package repositories, or “my friend wanted a copy” temporal anomalies. Private testing only unless Mike explicitly authorizes otherwise.

(The classification and time-travel claims are theatrical. The development status and redistribution restriction are real.)

## Activate the prototype

Extract the ZIP, open a terminal in this folder, and run:

```sh
chmod +x wftui bin/wftui-*
./wftui
```

Follow the displayed WindowsForum sign-in link in your browser. Press `?` for keyboard help and Ctrl+C to exit (if text is selected, the first Ctrl+C copies it).

The launcher automatically selects the included x64 or ARM64 executable. No installation, root access, Rust toolchain, or separate application libraries are required. Keep the launcher and `bin/` directory together. Alternatively, run the matching binary directly.

## Earth-system compatibility

- Linux with an x86-64/AMD64 or ARM64/AArch64 CPU and a reasonably modern kernel.
- Both executables are statically linked with musl; no matching glibc version is required.
- Not a universal executable for every Linux CPU, old kernel, or restricted environment. 32-bit x86 and ARM are not included.
- Internet access, a working DNS configuration, and the system's trusted CA certificates are needed for HTTPS. Minimal containers may need their distribution's `ca-certificates` package installed.
- A UTF-8 terminal is recommended. Image quality depends on terminal graphics support. Character-block fallback looks pixelated.
- If your terminal supports Sixel, try `WFTUI_GRAPHICS=sixel ./wftui`. To disable images, use `WFTUI_NO_IMAGES=1 ./wftui`.
- Login tokens and drafts are saved in your user configuration directory. Do not share these files with bug reports. To isolate this test, run `WFTUI_CONFIG_DIR="$PWD/test-config" ./wftui`.

## Transmit field observations

Report your distribution, `uname -m`, terminal name/version, reproduction steps, expected result, and actual result. Screenshots are useful; remove private messages and account information first.

Build: 0.0.1, based on fdefd31, with a local Windows-only import correction and synchronized lockfile package versions. Image support enabled. Unsigned private development artifacts.

Checksum verification: run `sha256sum -c SHA256SUMS.txt` from this folder.

**THE FUTURE THANKS YOU FOR YOUR DISCRETION.**
