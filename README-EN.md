<div align="center">

<img src="assets/images/app_logo.png" width="120" alt="EdgeCube logo" />

# EdgeCube

**Run a Minecraft server on your Android device.**

[English](README.md) · [简体中文](README.zh-CN.md)

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL%20v3-blue.svg)](LICENSE)
[![Flutter](https://img.shields.io/badge/Flutter-Dart%20%3E%3D%203.12.2-02569B?logo=flutter&logoColor=white)](https://flutter.dev)
[![Rust](https://img.shields.io/badge/Rust-2024%20Edition-dea582?logo=rust&logoColor=black)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/Platform-Android%207.0%2B-3DDC84?logo=android&logoColor=white)](https://www.android.com)
[![Version](https://img.shields.io/badge/version-1.0.6-orange.svg)](https://github.com/venti1112/EdgeCube)

[Website](https://edgecubemc.com) · [Docs](https://edgecubemc.com/#docs) · [Issues](https://github.com/venti1112/EdgeCube/issues)

</div>

---

## Overview

EdgeCube is an open-source Android app for creating, configuring, launching and operating Minecraft servers — entirely on your phone, no PC or root required.

It ships with a full runtime management system (JRE 17/21/25, PHP, rootfs), a built-in terminal, file manager, mod/plugin browser, and a set of network services that turn your device into a server you can reach from anywhere.

## Features

**Servers & runtimes**

- One-click server creation wizard: pick a type → version → loader → download.
- Supported: Vanilla, Paper, Purpur, Spigot, CraftBukkit, Forge, NeoForge, Fabric, Leaf, Leaves, Velocity, BungeeCord, PowerNukkitX, PocketMine-MP, Allay.
- Two launch modes, switchable at runtime:
  - **Native** — exec the bundled JRE (17/21/25) or the packaged PHP CLI directly.
  - **proot container** — run inside an imported Linux rootfs for maximum compatibility.
- Runtime environment packages (`.ecpkg`) can be downloaded, imported, updated and verified in-app; `.ecpkg` files are file-associated, so you can open them straight from a file manager.
- Automatic Minecraft EULA confirmation on first start, plus crash analysis: when a server or tunnel dies, logs are matched against a bundled rule set (`assets/rules/server_errors.json`) and you get a cause + fix suggestion.

**Reachability**

- UPnP port mapping, DDNS, STUN NAT1 hole punching.
- FRP tunnels with multiple providers: CHMLFRP, MeFrp, MSL FRP, OpenFRP, SakuraFRP.

**Content**

- Browse and download mods / plugins / modpacks from Modrinth, CurseForge, Spiget, Hangar and Poggit.
- Modpack import: CurseForge, Modrinth, MultiMC, MCBBS/HMCL.
- Download queue, hash verification and icon caching.

**Operations**

- Incremental + scheduled backups to local storage, FTP, SFTP or WebDAV.
- Keep-alive: foreground service, battery-optimization exemption, WakeLock/WifiLock, status overlay, screen-on lock, home-screen widget (start/stop, status, CPU/RAM, online players).
- Server properties editors, world settings, player management, system monitor.

**Experience**

- UI built with [flutter_miuix](https://pub.dev/packages/flutter_miuix) (HyperOS-style).
- i18n: 简体中文 (source) / English.
- Themes: dark, seed color, dynamic color (Monet).

## Tech stack

```
┌──────────────────────────────────────────────────────────┐
│ Flutter UI (lib/)                                         │
│   Scope tree → 5 tabs: Servers · Console · Manage ·      │
│   Files · Settings                                        │
├──────────────────────────────────────────────────────────┤
│ Dart services — 21 Method/EventChannels                   │
│   server · shell · ftp · ssh · proot · runtime ·         │
│   archive · ecpkg · forge · tunnel · monitor · …         │
├──────────────────────────────────────────────────────────┤
│ Android native (Kotlin + C, minSdk 24)                    │
│   ServerLauncher · RuntimeInstaller · ProotBootstrap ·   │
│   KeepAliveManager · WidgetUpdater · signature verifier  │
├──────────────────────────────────────────────────────────┤
│ Rust JNI (cargo-ndk → libedgecube_pty.so / _tools.so)    │
│   edgecube-pty   : PTY spawn / framing / broadcast        │
│   edgecube-tools : archive + FTP (unftp) + SSH (russh)   │
└──────────────────────────────────────────────────────────┘
```

## Requirements

- Flutter SDK (Dart `>=3.12.2 <4.0.0`)
- Android SDK with NDK 30.0.16248370, CMake 4.1.2, build-tools 37.0.0
- Rust toolchain + [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk)
- A physical device or emulator running **Android 7.0+** (`arm64-v8a`, `armeabi-v7a` or `x86_64`)

## Getting started

```bash
git clone https://github.com/venti1112/EdgeCube.git
cd EdgeCube
flutter pub get
flutter run
```

### Release build

```bash
# Rust is compiled automatically by the `cargoBuild` Gradle task
# (hooked into `preBuild`), so a normal Flutter build is enough:
flutter build apk --release

# CurseForge is optional — without the key the platform is hidden in the UI
flutter build apk --release --dart-define=CURSEFORGE_API_KEY=<your-key>
```

> Release signing reads `android/key.properties` (`storeFile`, `storePassword`, `keyAlias`, `keyPassword`). Debug builds use the same signature.

### Tests & analysis

```bash
flutter analyze          # flutter_lints
flutter test             # i18n key parity, PTY framing, mod metadata, world service, dialogs
cd rust && cargo test    # edgecube-pty + edgecube-tools (archive / ftp / ssh)
```

## Configuration

Runtime configuration lives as JSON under `<app documents>/config/` (`lib/config/`), one file per feature — `network.json`, `ssh.json`, `ftp.json`, `mcp.json`, `backup.json`, `ddns.json`, `stun.json`, `instances.json`, `locale.json`, …

Server working directories live under `<shared storage>/EdgeCube/instances/<id>/` and can be relocated from Settings.

| Service | Default port | Config file |
| --- | --- | --- |
| FTP | 2121 | `config/ftp.json` |
| SSH / SFTP | 2222 | `config/ssh.json` |
| MCP | 8765 | `config/mcp.json` |

### Signing `.ecpkg` packages

Runtime / rootfs packages are signed with ECDSA-P256 + SHA-256 using the APK release key, and verified in-app before import:

```bash
pip install cryptography
python tools/sign_package.py --key-properties android/key.properties --ecpkg runtime.ecpkg
python tools/sign_package.py --key-properties android/key.properties --rootfs-tar rootfs.tar.zst
```

## Project structure

```
EdgeCube/
├── lib/                  # Dart sources
│   ├── main.dart         # bootstrap + Scope tree
│   ├── home_shell.dart   # bottom tabs
│   ├── instance/         # creation wizard, download, import
│   ├── server/           # launch, proot, properties, DDNS/UPnP, monitor, crash analysis
│   ├── files/  mods/     # file manager, mod/plugin/modpack sources
│   ├── ftp/ ssh/ shell/ mcp/ frp/ stun/ tunnel/ backup/
│   ├── config/           # JSON config stores
│   ├── i18n/  theme/  logging/  pages/  widgets/
├── android/              # Kotlin + C (CMake) + prebuilt binaries
├── rust/                 # workspace: edgecube-pty, edgecube-tools
├── assets/               # images, i18n JSON, markdown, crash rules
├── test/                 # Flutter tests
└── tools/                # sign_package.py
```

## Contributing

Issues and pull requests are welcome at [github.com/venti1112/EdgeCube](https://github.com/venti1112/EdgeCube).

- Bug reports → [Issues](https://github.com/venti1112/EdgeCube/issues)
- Docs → [edgecubemc.com/#docs](https://edgecubemc.com/#docs)

Please keep code comments and commit messages in Chinese, matching the existing codebase.

## License

[AGPL-3.0](LICENSE) — © venti1112
