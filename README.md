# Tonearm

Лёгкий нативный десктопный клиент для стримингового сервиса Spotify на Rust и [Slint](https://slint.dev), без Chromium/Electron/webview.
Цель: меньше 100 МБ RAM в простое и быстрый холодный старт. Платформы: Arch Linux (Wayland/X11) и Windows 10/11.

> **Дисклеймер.** Проект неофициальный и никак не связан со Spotify AB. Воспроизведение работает через
> [librespot](https://github.com/librespot-org/librespot), то есть через обратно-разработанный протокол.
> Его использование может нарушать условия использования сервиса. Используйте на свой риск.
> Нужен аккаунт **Spotify Premium** (ограничение librespot).

## Статус

Готово: этап 1 (каркас), этап 2 (вход через браузер, автологин, выход), этап 3 (воспроизведение, пауза, перемотка, громкость), этап 4 (медиатека: мои плейлисты, Любимые треки, страницы плейлиста, альбома и артиста, кэш обложек), этап 5 (поиск треков, альбомов, исполнителей и плейлистов; ссылки Spotify открываются из поля поиска по Enter), этап 6 (очередь: следующий/предыдущий, перемешивание, повтор, «В очередь» по правому клику, панель очереди), этап 7 (медиаклавиши и виджеты плеера рабочего стола через MPRIS / SMTC, уведомление о смене трека, горячие клавиши) этап 8 (Spotify Connect: перенос воспроизведения на телефон, колонку или другой компьютер и обратно, управление ими из деки) и этап 9 (экран настроек: качество, нормализация, gapless, аудиовыход, уведомления, размер кэша; «В любимые» по правому клику; установщик MSI для Windows). Интерфейс в концепции «Пульт», см. [docs/DESIGN.md](docs/DESIGN.md). План: [docs/PLAN.md](docs/PLAN.md).

## Сборка

Нужен Rust stable (≥ 1.88, ставится через `rustup`).

### Arch Linux

```sh
sudo pacman -S --needed base-devel rust fontconfig libxkbcommon libxkbcommon-x11 wayland alsa-lib
cargo build --release
./target/release/tonearm
```

Пакет: `cd packaging/arch && makepkg -si`.

### Debian/Ubuntu (для CI и разработки)

```sh
sudo apt install libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-0 libwayland-dev libasound2-dev
```

### Windows 10/11

Установите Rust с MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`) и Visual Studio Build Tools (C++).

```powershell
cargo build --release
.\target\release\tonearm.exe
```

В release-сборке нет консольного окна, иконка встраивается в exe.

## Вход

Кнопка «Войти через браузер» открывает страницу Spotify; пароль вводится только там. Приложение ждёт
возврата на `http://127.0.0.1:8898/login`, поэтому порт 8898 должен быть свободен. После входа
librespot выдаёт многоразовые учётные данные, они сохраняются в системном keyring и используются для автологина.
«Выйти» удаляет их.

Где хранится вход:
- **Windows**: Credential Manager.
- **Linux**: служба секретов (Secret Service), если она запущена (GNOME Keyring, KWallet, KeePassXC). Ставить её не нужно:
  без неё вход сохраняется в зашифрованный файл `~/.local/share/tonearm/secrets/` (права 0600, ключ привязан к
  `/etc/machine-id` и пользователю). Скопированный на другую машину или в бэкап файл бесполезен, но программы,
  запущенные от вашего пользователя, могут его прочитать, как и разблокированный keyring. Если служба секретов
  появится позже, вход сам переедет в неё, а файл удалится.

Хранится не пароль, а многоразовый токен этого устройства; отозвать его можно на сайте Spotify («Выйти везде»).

## Звук

Вывод через ALSA (cpal/rodio из librespot). С PipeWire нужен `pipewire-alsa`, с PulseAudio — `pulseaudio-alsa`
(на большинстве десктопов уже стоит). Если аудиоустройства нет, приложение не падает: покажет сообщение,
а после подключения наушников достаточно снова нажать «Играть». Качество потока: 320 кбит/с.

Для проверки интерфейса без аккаунта (только debug-сборка): `TONEARM_UI_PREVIEW=<экран> cargo run`, где экран: `home`, `library`, `playlist`, `search`, `search-tracks`, `queue`, `menu`, `devices`, `settings`, `login`, `waiting`, `offline`.

## Настройки

Пример конфига: [config.example.toml](config.example.toml). Токены хранятся только в системном keyring
(Secret Service на Linux, Credential Manager на Windows) и никогда не пишутся в файлы и логи.

Рендерер по умолчанию программный (около 20 МБ RSS). OpenGL-рендерер можно попробовать так:
`SLINT_BACKEND=winit-femtovg tonearm`.

Логи: `RUST_LOG=debug tonearm`.

## Горячие клавиши

| Клавиша | Действие |
|---|---|
| Пробел | играть / пауза |
| ← / → | перемотка на 5 с |
| Ctrl+← / Ctrl+→ | предыдущий / следующий трек |
| ↑ / ↓ | громкость |
| Q | панель очереди |
| / или Ctrl+F | поиск |
| Esc | очистить поиск, назад со страницы плейлиста |

Медиаклавиши клавиатуры, виджет плеера GNOME/KDE и `playerctl` работают через MPRIS (имя `org.mpris.MediaPlayer2.tonearm`),
на Windows — через системную панель мультимедиа (SMTC). Уведомление о новом треке показывается, только когда окно не в фокусе.

## Структура

```
crates/core    сессия, авторизация, librespot, Web API, модели, кэш
crates/player  очередь, shuffle/repeat, громкость, seek, события
crates/app     UI на Slint, общается с core/player только через каналы
packaging/     PKGBUILD и .desktop (Arch), заметки по Windows
assets/        иконка (svg, png, ico)
```

## Лицензия

MIT
