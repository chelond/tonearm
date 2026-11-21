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

### Свой Client ID для Web API (если «Spotify ограничил запросы»)

Плейлисты, медиатека и поиск идут через Web API. По умолчанию токен для него берётся из сессии librespot,
а она работает под общим Client ID официального клиента, которым пользуются все программы на librespot.
Поэтому Spotify часто отвечает 429 («ограничил запросы»). Решение: зарегистрировать своё приложение.

1. Откройте <https://developer.spotify.com/dashboard> → **Create app**.
2. Redirect URI: `http://127.0.0.1:8898/login`, API: **Web API**. Сохраните и скопируйте **Client ID**
   (Client Secret не нужен, вход по PKCE).
3. В разделе **User Management** приложения добавьте email своего аккаунта Spotify (в Development Mode
   приложение работает только для пользователей из этого списка).
4. Впишите Client ID в `config.toml` (Windows: `%APPDATA%\tonearm\config\config.toml`,
   Linux: `~/.config/tonearm/config.toml`):
   ```toml
   [spotify]
   client_id = "ваш_client_id"
   ```
5. Перезапустите Tonearm. Один раз откроется браузер: разрешите доступ. Refresh-токен ляжет в keyring,
   дальше без браузера. «Выйти» удаляет и его.

Воспроизведение по-прежнему идёт через librespot, Client ID влияет только на Web API.

## Звук

Вывод через ALSA (cpal/rodio из librespot). С PipeWire нужен `pipewire-alsa`, с PulseAudio — `pulseaudio-alsa`
(на большинстве десктопов уже стоит). Если аудиоустройства нет, приложение не падает: покажет сообщение,
а после подключения наушников достаточно снова нажать «Играть». Качество потока: 320 кбит/с.

Для проверки интерфейса без аккаунта (только debug-сборка): `TONEARM_UI_PREVIEW=<экран> cargo run`, где экран: `home`, `library`, `playlist`, `search`, `search-tracks`, `queue`, `menu`, `devices`, `settings`, `login`, `waiting`, `offline`.

## Плеер Spotify Soloist

Некоторым аккаунтам (чаще новым) Spotify не выдаёт ключи для встроенного плеера на librespot
(librespot issue #1649), и треки не играют. Тогда включите **Настройки → Плеер → Soloist**.
Soloist — официальный фоновый плеер Spotify; нужен Premium.

1. Нажмите «Получить ключ»: откроется developer.spotify.com/dashboard/soloist. Нажмите там
   «Copy» у своего ключа. Tonearm заберёт его из буфера обмена сам (или вставьте в поле и Enter).
   Ключ хранится в системном хранилище паролей, в файлы не пишется.
2. Tonearm скачивает Soloist с серверов Spotify (в архив Tonearm он не входит: распространять
   его нельзя), запускает в фоне и входит в него вашим аккаунтом сам, через Spotify Connect.
   Если автоматический вход не прошёл, один раз выберите устройство «Tonearm» в Spotify на телефоне.
3. Сборки Soloist живут 90 дней; Tonearm сам скачивает новую, когда текущей больше 60 дней.

Soloist закрывается вместе с Tonearm. Его журнал: `soloist/soloist.log` в папке данных Tonearm.

## Настройки

Пример конфига: [config.example.toml](config.example.toml). Токены хранятся только в системном keyring
(Secret Service на Linux, Credential Manager на Windows) и никогда не пишутся в файлы и логи.

Рендерер по умолчанию программный (около 20 МБ RSS). OpenGL-рендерер можно попробовать так:
`SLINT_BACKEND=winit-femtovg tonearm`.

Логи: `RUST_LOG=debug tonearm`.

## Темы оформления

В Настройках → Оформление восемь готовых тем (тёмные и светлые). Свои темы это файлы `.toml` в папке тем
(`~/.config/tonearm/themes`, `%APPDATA%\tonearm\config\themes`); кнопка «Создать из текущей» кладёт туда
копию выбранной темы. В файле достаточно указать только те цвета, которые меняются, остальное берётся из
`base = "dark"` или `"light"`. Сохранённые изменения видны в приложении сразу. Пример со всеми цветами:
[crates/app/themes/graphite.toml](crates/app/themes/graphite.toml).

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
