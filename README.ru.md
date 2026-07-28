# PocketHLE

> Высокоуровневый эмулятор (HLE) игр для Windows Mobile / Pocket PC.
> Архитектура и стиль кода вдохновлены проектами
> [touchHLE](https://github.com/touchHLE/touchHLE) (iPhone OS) и
> [EKA2L1](https://github.com/EKA2L1/EKA2L1) (Symbian).
> Интерфейс лаунчера сделан в стиле
> [j2me-loader](https://github.com/nikita36078/j2me-loader).

PocketHLE не пытается эмулировать целое ядро Windows CE. Вместо этого, как
и `touchHLE`, эмулятор загружает реальный игровой `.exe`, запускает ARM-код
в эмуляторе процессора и реализует системные DLL (`coredll`, `aygshell`,
`gx`, `hss`...) на стороне хоста. Игра «думает», что она работает на
реальном Pocket PC.

Первая целевая ROM — небольшая физическая игра **JumpyBall** (ARM PE32,
Windows CE 5 GUI). Реализованные API соответствуют тому, что вызывает
именно эта игра.

> **Статус:** ARM и MIPS PE загружаются через Unicorn, вызовы системных DLL
> перехватываются, а GAPI-фреймбуфер можно сохранять в кадры. Полный Windows CE
> ещё не эмулируется: для каждой игры могут понадобиться дополнительные HLE API
> и сценарий нажатий.

Английская версия → [`README.md`](README.md).

## Что собирается

| Платформа | Артефакт                              | Бэкенд CPU      |
|-----------|---------------------------------------|-----------------|
| Linux     | `pockethle`, `pockethle-gui` (egui)   | stub / ARM / MIPS Unicorn |
| Windows   | `pockethle.exe`, `pockethle-gui.exe`  | stub / ARM / MIPS Unicorn |
| Android   | APK (arm64-v8a, armeabi-v7a)          | stub / ARM / MIPS Unicorn |

CI собирает артефакты для всех трёх платформ — как у touchHLE.

## Сборка на Linux

```bash
sudo apt install -y cmake build-essential pkg-config libclang-dev \
                    libgtk-3-dev libxkbcommon-dev \
                    libwayland-dev libx11-dev libxcb1-dev \
                    libxrandr-dev libxinerama-dev libxi-dev \
                    libxcursor-dev libxdamage-dev libxext-dev libxfixes-dev
rustup default stable      # 1.85+

# Базовая сборка (CLI + десктопный GUI, без настоящего CPU-бэкенда):
cargo build --release --workspace

# Полноценный билд с Unicorn Engine (~3 минуты в первый раз):
cargo build --release -p pocket-cli      --features unicorn
cargo build --release -p pocket-desktop  --features unicorn

cargo test --workspace
```

Бинарники появятся в `target/release/`:

- `pockethle` — командная строка (`pe-info`, `unpack-cab`, `inspect-cab`, `run`...).
- `pockethle-gui` — десктопный GUI (egui) с библиотекой игр и настройками.

## Сборка на Windows

PocketHLE собирается «из коробки» на Windows с MSVC-toolchain (так же
распространяется и сам touchHLE).

```powershell
# 1. Установите rustup и затем:
rustup default stable-x86_64-pc-windows-msvc

# 2. Сборка CLI и десктопного GUI (быстро, без unicorn):
cargo build --release -p pocket-cli
cargo build --release -p pocket-desktop

# 3. (Опционально) С Unicorn Engine — нужен cmake в PATH и MSVC C/C++.
cargo build --release -p pocket-cli      --features unicorn
cargo build --release -p pocket-desktop  --features unicorn
```

Результат — `target\release\pockethle.exe` и
`target\release\pockethle-gui.exe`. Двойной клик на `pockethle-gui.exe`
открывает окно лаунчера: импортируйте `.CAB`, выберите игру в библиотеке
и нажмите Run.

## Сборка для Android

Каталог: [`frontends/pocket-android`](frontends/pocket-android). Нужны:

- Android Studio Iguana (или AGP 8.4+)
- Android NDK r26+
- [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk) (`cargo install cargo-ndk`)

```bash
# 1. Кросс-компиляция JNI-моста под обе ABI:
cargo ndk \
    -t arm64-v8a \
    -t armeabi-v7a \
    -o frontends/pocket-android/app/src/main/jniLibs \
    build --release -p pocket-android-jni

# 2. Сборка APK:
cd frontends/pocket-android
./gradlew assembleRelease
```

APK окажется в
`frontends/pocket-android/app/build/outputs/apk/release/`.

Интерфейс Android-приложения сделан по образцу
[j2me-loader](https://github.com/nikita36078/j2me-loader): RecyclerView с
карточками игр (Run / Settings / Remove), плавающая кнопка импорта `.CAB`
через системный файлпикер, общий экран настроек (CPU-бэкенд по умолчанию,
уровень логов) и экран настроек на конкретную игру (CPU-бэкенд, лимит
слайсов диспатчера, halt-on-unimplemented). Запуск открывает
`SurfaceView`-экран `GameActivity`, в котором отображается фреймбуфер
эмулятора.

## Структура библиотеки игр

Десктопный GUI и Android-лаунчер используют общую библиотеку, которой
заведует крейт [`pocket-library`](crates/pocket-library):

```
<library-root>/
├── library.json          # индекс импортированных игр
├── config.json           # CPU-бэкенд по умолчанию, log verbosity, ...
└── games/
    └── <sanitized-id>/
        ├── game.json     # имя, исходный CAB, настройки игры
        ├── source.cab    # оригинальный архив
        └── extracted/
            └── ... PE / data files ...
```

На Linux/Windows по умолчанию это
`~/.local/share/PocketHLE/library`. На Android —
`getExternalFilesDir(null)/library` внутри песочницы приложения.

## Запуск JumpyBall

# Просмотр содержимого CAB:
pockethle inspect-cab ~/JumpyBallPPC.cab

# Или распаковка вручную и запуск через Unicorn:
pockethle unpack-cab ~/JumpyBallPPC.cab /tmp/jumpy
pockethle -v run /tmp/jumpy/JUMPYB~1.002 \
    --cpu unicorn --max-slices 200 --instructions-per-slice 100000

Для MIPS-игры используйте `--cpu mips`. Для ARM — `--cpu unicorn`. Чтобы
передать игре последовательность нажатий, повторяйте `--tap X,Y` или используйте
отдельную программу для ИИ/vision-агента:

```bash
python3 tools/ai-tap-sequence.py /path/to/game.exe \
    --cpu mips --tap 120,210 --tap 120,250 \
    --dump-frames-to /tmp/pockethle-frames --max-frames 3
```

`tools/ai-tap-sequence.py` не угадывает кнопки сам: другая ИИ-программа
выбирает координаты по скриншоту, а этот запускной помощник передаёт их игре
по порядку и сохраняет кадры для проверки.

В выводе вы увидите строчки вида
`unimplemented call -> COREDLL.dll!Rectangle` — это API, которые ещё нужно
реализовать в `crates/pocket-winceapi/src/coredll.rs`. Каждый «недостающий»
API — это маленький pull request на пару десятков строк.

## Дальнейшие планы

1. CRT-пролог: `__chkstk`, `_setjmp`, `longjmp`, `_except_handler3`.
2. Создание окна: `RegisterClassW`, `CreateWindowExW`, `SHFullScreen`.
3. Загрузка ресурсов: `FindResourceW`, `LoadResource`, `CreateFileW`,
   `ReadFile`.
4. GDI: софтверный растеризатор (`BitBlt`, `Rectangle`, `FillRect`).
5. GAPI: вывод фреймбуфера в окно desktop GUI (egui) и `SurfaceView` (Android).
6. Звук: реальное воспроизведение через SDL2 / OpenSL ES.
7. Ввод: клавиатура и тач → `WM_KEYDOWN` / `WM_LBUTTONDOWN`.

## Лицензия

Двойная лицензия: [Apache-2.0](LICENSE-APACHE) **ИЛИ** [MIT](LICENSE-MIT).
