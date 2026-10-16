<!--
Copyright (C) 2026 PocketHLE Emulator Project
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# PocketHLE

<p align="center">
  <img src="./frontends/pocket-desktop/assets/pockethle-logo.png" width="25%" alt="PocketHLE logo" />
</p>

<p align="center">
  A high-level Windows Mobile / Pocket PC / Gizmondo emulator for Linux, Windows and Android.
</p>

<p align="center">
  <a href="https://discord.gg/jDvzctQdFu">
    <img src="https://img.shields.io/badge/Discord-Join%20our%20Community-5865F2?style=for-the-badge&logo=discord&logoColor=white" alt="Join the PocketHLE Discord community" />
  </a>
</p>

<p align="center">
  <strong>Join our Discord for development updates, compatibility discussions, support and community chat.</strong>
</p>

[Русская версия → `README.ru.md`](README.ru.md)

---

> [!NOTE]
> PocketHLE targets native ARM Windows CE / Windows Mobile executables packaged in `.CAB` archives. It is experimental software: compatibility, performance and input behavior vary by game and host platform.

> [!WARNING]
> PocketHLE is developed for research and educational purposes. It does not include Microsoft system files, firmware or game data. Use only legally obtained game copies and archives.

## Info

PocketHLE is an early-stage high-level emulator for Pocket PC 2002/2003 and Windows Mobile 5/6 games. Instead of emulating a complete Windows CE device, it loads the original game executable, runs its ARM code through a CPU backend, and provides clean-room host-side implementations of the Windows CE APIs the game expects.

The project is inspired by [touchHLE](https://github.com/touchHLE/touchHLE) and [EKA2L1](https://github.com/EKA2L1/EKA2L1), with a launcher and library workflow influenced by [j2me-loader](https://github.com/nikita36078/j2me-loader).

## Games Tested

| Asphalt 4 Elite Racing | Call of Duty 2 |
| :---: | :---: |
| ![Asphalt 4 gameplay](./proof/games/asphalt-4-elite-racing.jpg) | ![Call of Duty 2](./proof/games/call-of-duty-2.jpg) |

| Zuma Deluxe | Bejeweled |
| :---: | :---: |
| ![Zuma Deluxe](./proof/games/zuma-deluxe.jpg) | ![Bejeweled](./proof/games/bejeweled.jpg) |

Here is a version of PocketHLE that’s tailored for the Gizmondo [PocketHLE by guizmox](https://github.com/guizmox/PocketHLE)
