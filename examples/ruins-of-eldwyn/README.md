# Ruins of Eldwyn

Минимальный top-down 3D RPG для Windows CE / Pocket PC 2002, ARMv4T, рассчитанный на запуск через PocketHLE.

## Что уже есть

- ARM PE executable `cab/eldwyn.exe` и готовый `RuinsOfEldwyn.cab`.
- fixed-point Q16.16 математика и ручные ARM runtime helpers без floating-point операций в игровом коде.
- GLES 1.x renderer: палитровые RGBA8 textures, vertex-colour shading, fog, depth buffer, alpha-tested billboards.
- 24×24 островная карта с травой, водой, стенами, деревьями, воротами, сундуком, elder NPC, slime/skeleton enemies.
- квестовый flow, HP/level/potions, sword swing, pickup, dialogue, death/win states и PC speaker/waveOut sound hooks.

## Сборка

```sh
./build.sh
```

Скрипт использует clang ARMv4T, lld и локальный PE wrapper `tools/pelink.py`; SDK Windows CE не требуется. Результат: `build/eldwyn.exe`.

CAB пересобирается из executable отдельной командой упаковки; текущий проверенный архив находится в `RuinsOfEldwyn.cab`.

## Запуск

```sh
pockethle run build/eldwyn.exe --cpu unicorn
```

Для автоматической smoke-проверки PocketHLE поддерживает ограничение кадров и dump framebuffer, например `--max-frames 120 --dump-frames-to /tmp/eldwyn-frames`.

## Управление

- D-pad / стрелки — движение
- A — удар, разговор, открыть сундук, подтвердить
- B — potion

## Изменения PocketHLE

В самом PocketHLE добавлена поддержка загрузки OES paletted texture uploads через `glTexImage2D`: `pocket-winceapi` теперь читает palette + index payload вместо нулевой длины, а `pocket-gles` декодирует форматы `GL_PALETTE4_*` и `GL_PALETTE8_*` в RGBA. Это нужно игре для 32×32 PS1-style texture atlas при малом VRAM и полном отсутствии runtime image decoder в guest.

Изменения находятся в:

- `crates/pocket-winceapi/src/gles.rs`
- `crates/pocket-gles/src/texture.rs`

Проверка: все 114 тестов `pocket-gles` и все 99 тестов `pocket-winceapi` проходят.
