# davfast

Быстрый CLI-клиент WebDAV и мост к **cloud.mail.ru** для Linux (Rust). Оптимизирован под главную боль davfs2 и большинства клиентов — **тысячи файлов небольшого размера**.

```
davfast upload ~/photos http://webdav.example.com/backup/ -c 32
davfast sync ~/data http://127.0.0.1:8080/            # инкрементальное зеркало
davfast mailru upload ./papka /backups                # как в веб-интерфейсе cloud.mail.ru
davfast serve --backend mailru --listen 127.0.0.1:8080  # WebDAV-мост для davfs2
```

## Почему быстрый

| Проблема davfs2 / обычных клиентов | davfast |
|---|---|
| На каждый файл: PROPFIND → PUT → PROPFIND (3 round-trip'а) | Один PROPFIND на каталог: дерево из тысяч файлов сравнивается за несколько запросов |
| Операции идут последовательно | Параллельный конвейер `buffer_unordered` (по умолчанию 16, настраивается `-c 32`, `-c 64`...) |
| Новое TCP-соединение на запрос | Пул keep-alive соединений + TCP_NODELAY + HTTP/2 к апстриму |
| Каталоги создаются по одному | Дерево каталогов создаётся пакетно, уровни параллельно |
| Повторная загрузка неизменённого дерева | Пропуск по размеру (`--skip-existing`) или точно по ETag/хешу (`.davfast-cache.json`) |
| Облако mail.ru по WebDAV медленное | Режим `mailru` повторяет путь веб-интерфейса: PUT на upload-узел + `file/add`, дедупликация по хешу |

Замеры на локальной машине (in-memory сервер, сеть не участвует): 1000 файлов за 0.06 с (конвейер не сериализуется). В реальной сети всё упирается в RTT: при 50 мс однопоточный клиент делает ~20 файлов/с, davfast с `-c 32` — сотни файлов/с. Повторный `sync` неизменённого дерева из 320 файлов занимает 0.013 с.

## Установка одной строкой

```bash
curl -fsSL https://raw.githubusercontent.com/teliy/davfast/main/install.sh | bash
```

Инсталлятор сам проверит систему (сеть, git, curl, компилятор, Rust, место на диске), установит недостающее, соберёт davfast и покажет меню с вариантами:

1. **Установить / обновить** — сборка из исходников, бинарник в `/usr/local/bin` или `~/.local/bin` (пункт 2 — то же с ручным выбором путей).
2. **Настроить монтирование облака** — спросит бэкенд (с пояснениями: `webdav` — прокси к любому WebDAV, `mailru` — прямой доступ к веб-API cloud.mail.ru), URL, логин и токен («пароль для внешних приложений» с id.mail.ru, ввод скрыт), точку монтирования. Затем сохранит креды в `~/.config/davfast/davfast.env` (права 600), создаст systemd-сервис моста с автозапуском и настроит davfs2 (`use_locks 0`, secrets, строка в fstab) — и сразу попробует смонтировать облако.
3. **Только проверить систему** — отчёт о зависимостях без изменений.
4. **Удалить davfast**.

Язык интерфейса выбирается автоматически (русский/английский). Неинтерактивный режим для CI: `curl -fsSL https://raw.githubusercontent.com/teliy/davfast/main/install.sh | bash -s -- --yes`. Повторный запуск однострочника обновляет репозиторий и пересобирает бинарник. Настроить монтирование отдельно, без переустановки: `install.sh --setup-mount`.

## Установка вручную

Требуется Rust 1.75+ (в Debian/Ubuntu: `apt install rustc cargo` или [rustup](https://rustup.rs)).

```bash
git clone https://github.com/teliy/davfast.git
cd davfast
cargo build --release
# бинарник: target/release/davfast
sudo cp target/release/davfast /usr/local/bin/
```

## Команды

| Команда | Описание |
|---|---|
| `ls <url>` | список каталога |
| `mkdir <url>` | создать каталог (с родителями) |
| `rm <url> [-r]` | удалить файл/каталог |
| `upload <local> <url> [-c N] [--skip-existing] [--dry-run]` | загрузить файл или дерево (рекурсивно, параллельно) |
| `download <url> <local> [-c N] [--skip-existing]` | скачать файл или дерево |
| `sync <local> <url> [-c N] [--delete] [--dry-run]` | инкрементальное зеркало local → remote (ETag-кэш) |
| `serve --backend webdav\|mailru` | локальный WebDAV-мост/прокси |
| `mailru ls\|upload\|download\|sync ...` | работа с cloud.mail.ru через веб-API |

Учётные данные: `user:pass@host` прямо в URL, либо переменные окружения `DAVFAST_USER` / `DAVFAST_PASS`.

## Быстрый старт

```bash
# загрузить каталог в 32 потока
davfast upload ~/backup https://webdav.example.com/dav/backup/ -c 32

# посмотреть, что изменилось, ничего не отправляя
davfast sync ~/backup https://webdav.example.com/dav/backup/ --dry-run

# инкрементальное зеркало (+ удалить на сервере то, что удалено локально)
davfast sync ~/backup https://webdav.example.com/dav/backup/ --delete

# докачать всё дерево
davfast download https://webdav.example.com/dav/backup/ ~/restored -c 32
```

Инкрементальный кэш пишется в `<локальный-каталог>/.davfast-cache.json` и исключается из передачи. Он запоминает ETag (или хеш mail.ru) после каждой загрузки — второй `sync` отправляет только реально изменённые файлы, даже если размер совпал.

## cloud.mail.ru

У веб-интерфейса есть собственный REST API, и он заметно быстрее, чем WebDAV-шлюз. Подробный разбор — [docs/analysis-cloud-mail-ru.ru.md](docs/analysis-cloud-mail-ru.ru.md). Коротко: файл уходит PUT-ом на выделенный upload-узел (`cld-uploaderXXX`), затем регистрируется `POST /api/v2/file/add` с хешем; сервер дедуплицирует по хешу — уже известное облаку содержимое «загружается» мгновенно, без передачи байтов.

### Напрямую (быстрый путь, как в вебе)

```bash
export MAILRU_LOGIN='user@mail.ru'
export MAILRU_PASSWORD='пароль-для-внешних-приложений'   # id.mail.ru → Безопасность → Пароли для внешних приложений

davfast mailru ls /
davfast mailru upload ~/papka /backups -c 32
davfast mailru sync  ~/papka /backups        # инкрементально по хешу
davfast mailru download /backups ~/restored -c 32
```

### Мост для davfs2 и любых WebDAV-клиентов

`serve --backend mailru` поднимает локальный WebDAV-сервер, который транслирует PROPFIND/GET/PUT/DELETE/MKCOL/MOVE в веб-API облака:

```bash
davfast serve --backend mailru --listen 127.0.0.1:8080
# в другом терминале:
mkdir -p /mnt/cloud
mount -t davfs http://127.0.0.1:8080/ /mnt/cloud
```

В `/etc/davfs2/davfs2.conf` рекомендуется `use_locks 0` (это же советует и официальная помощь Mail.ru) — мост LOCK/UNLOCK имитирует, но блокировки облаку не нужны.

Есть и обычный прокси-режим для любого WebDAV-сервера — пул соединений и HTTP/2 к апстриму:

```bash
davfast serve --backend webdav --url https://webdav.example.com/dav/ --listen 127.0.0.1:8081
```

Мост можно защитить паролем: `--user me --pass secret` (Basic-аутентификация на локальном порту).

## Переменные окружения

| Переменная | Назначение |
|---|---|
| `DAVFAST_USER`, `DAVFAST_PASS` | учётные данные для WebDAV-команд, если их нет в URL |
| `MAILRU_LOGIN`, `MAILRU_PASSWORD` | аккаунт cloud.mail.ru для `mailru` и `serve --backend mailru` |
| `DAVFAST_MAILRU_AUTH_URL` | переопределить `https://auth.mail.ru/cgi-bin/auth` |
| `DAVFAST_MAILRU_REST_BASE` | переопределить `https://cloud.mail.ru` |
| `DAVFAST_MAILRU_DISPATCHER` | путь dispatcher-метода (по умолчанию `api/v2/dispatcher`) |
| `RUST_LOG=debug` | подробный журнал |

## Статус и ограничения

- **WebDAV-клиент** (`ls/mkdir/rm/upload/download/sync`, бэкенд `webdav`): покрыт интеграционными тестами (собственный in-memory DAV-сервер + отдельный тестовый сервер на Python stdlib, 320–1000 файлов, побайтовая сверка). Используйте его с любым WebDAV-сервером.
- **mailru-бэкенд** (`mailru ...`, `serve --backend mailru`): реализован по открытой документации API и исходникам [kireevmp/mailru-cloud-api](https://github.com/kireevmp/mailru-cloud-api), [mad-gooze/PyMailCloud](https://github.com/mad-gooze/PyMailCloud), бэкенда [rclone mailru](https://rclone.org/mailru/). **В сети не проверялся** (нет тестового аккаунта в песочнице) — при первом запуске с реальным аккаунтом возможны нюансы форматов ответов; эндпоинты переопределяются переменными окружения. Нужен «пароль для внешних приложений», а не основной.
- Файл в памяти при загрузке (целевой сценарий — мелкие файлы); максимум mail.ru — 2 ГБ на файл для бесплатных аккаунтов.
- Символические ссылки при обходе дерева пропускаются.
- `serve` ограничивает тело PUT 1 ГиБ.

## Разработка

```bash
cargo test        # 13 тестов: юнит + e2e (400 файлов: upload/download/sync/delete/кэш) + бенчмарк
cargo run --release -- --help
tests/webdav_test_server.py   # минимальный DAV-сервер на Python для ручных проверок
```

## Лицензия

GNU GPL v3.0 — см. [LICENSE](LICENSE).
