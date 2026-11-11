#!/usr/bin/env bash
# ============================================================================
#  davfast installer
#  https://github.com/teliy/davfast
#
#  One-line install (RU/EN auto):
#      curl -fsSL https://raw.githubusercontent.com/teliy/davfast/main/install.sh | bash
#
#  The one-liner also offers an interactive mount setup: backend, WebDAV URL,
#  login + token (hidden input), mount point, systemd service and davfs2.
#
#  Non-interactive / CI:
#      curl -fsSL https://raw.githubusercontent.com/teliy/davfast/main/install.sh | bash -s -- --yes
#
#  License: GPL-3.0-or-later (same as davfast)
# ============================================================================
set -euo pipefail

# ----------------------------------------------------------------- defaults -
REPO_URL="${DAVFAST_REPO_URL:-https://github.com/teliy/davfast.git}"
SRC_DIR="${DAVFAST_SRC_DIR:-$HOME/.local/src/davfast}"
BRANCH="${DAVFAST_BRANCH:-}"      # empty = default branch
BIN_DIR=""                        # "" = auto-resolved later
ASSUME_YES=0
CHECK_ONLY=0
UNINSTALL=0
SETUP_MOUNT=0
SKIP_DEPS=0
CHOOSE_DIRS=0
FORCE_LANG=""
USE_COLOR=1
MIN_RUST_MAJOR=1
MIN_RUST_MINOR=75
NEED_MB=2000                      # free disk space needed for the build, MB
RUST_V=""                         # detected rustc version (set by rustver)

PATH="$HOME/.cargo/bin:$PATH"
export PATH

# ------------------------------------------------------------------ colors --
if [[ -n ${NO_COLOR:-} ]]; then USE_COLOR=0; fi

set_colors() {
  if [[ $USE_COLOR == 1 && -t 1 ]]; then
    C_G=$'\e[1;32m'; C_Y=$'\e[1;33m'; C_R=$'\e[1;31m'; C_B=$'\e[1;36m'; C_D=$'\e[2m'; C_0=$'\e[0m'
  else
    C_G=""; C_Y=""; C_R=""; C_B=""; C_D=""; C_0=""
  fi
}

ok()   { printf '%s%s%s\n' "$C_G" "$*" "$C_0"; }
warn() { printf '%s%s%s\n' "$C_Y" "$*" "$C_0"; }
err()  { printf '%s%s%s\n' "$C_R" "$*" "$C_0" >&2; }
info() { printf '%s%s%s\n' "$C_B" "$*" "$C_0"; }
dim()  { printf '%s%s%s\n' "$C_D" "$*" "$C_0"; }
hr()   { printf '%s\n' "-------------------------------------------------------------------------"; }

# -------------------------------------------------------------------- i18n --
t() {  # t <key> [args...] -> translated, formatted message (adds trailing \n)
  local key="$1"; shift
  local fmt=""
  if [[ "$LANG_SEL" == "ru" ]]; then
    case "$key" in
      banner)            fmt="Установщик davfast — быстрый WebDAV-клиент и мост cloud.mail.ru" ;;
      sys_line)          fmt="Система: %s · архитектура: %s · пакетный менеджер: %s" ;;
      sys_nopkg)         fmt="Система: %s · архитектура: %s · пакетный менеджер: не найден" ;;
      unknown_distro)    fmt="Дистрибутив не распознан (%s); продолжаю без автопостановки пакетов" ;;
      check_start)       fmt="Проверка системы…" ;;
      chk_net_ok)        fmt="Сеть до github.com ................. OK" ;;
      chk_net_fail)      fmt="Сеть до github.com ................. НЕДОСТУПНА" ;;
      chk_git_ok)        fmt="git ................................ OK (%s)" ;;
      chk_git_fail)      fmt="git ................................ ОТСУТСТВУЕТ" ;;
      chk_curl_ok)       fmt="curl ............................... OK" ;;
      chk_curl_fail)     fmt="curl ............................... ОТСУТСТВУЕТ" ;;
      chk_cc_ok)         fmt="Компилятор C ....................... OK (%s)" ;;
      chk_cc_fail)       fmt="Компилятор C ....................... ОТСУТСТВУЕТ (нужен для сборки)" ;;
      chk_rust_ok)       fmt="Rust ............................... OK (%s, нужно >= %s)" ;;
      chk_rust_old)      fmt="Rust ............................... СЛИШКОМ СТАРЫЙ (%s, нужно >= %s)" ;;
      chk_rust_missing)  fmt="Rust ............................... НЕ НАЙДЕН (нужен cargo >= %s)" ;;
      chk_disk_ok)       fmt="Свободно на диске .................. %s МБ в %s" ;;
      chk_disk_low)      fmt="Свободно на диске .................. МАЛО: %s МБ в %s (сборке нужно ~%s МБ)" ;;
      chk_fail_exit)     fmt="Критические проверки не пройдены, установка невозможна." ;;
      deps_needed)       fmt="Отсутствуют инструменты: %s" ;;
      deps_offer)        fmt="Установить недостающее через %s?" ;;
      deps_done)         fmt="Системные зависимости установлены." ;;
      deps_failed)       fmt="Не удалось установить пакеты. Поставьте вручную: %s" ;;
      deps_skipped)      fmt="--skip-deps: пропускаю автопостановку пакетов. Установите вручную: %s" ;;
      deps_still)        fmt="После установки всё ещё отсутствует: %s" ;;
      rust_offer)        fmt="Установить Rust через rustup (рекомендуется, ставится в ~/.cargo)?" ;;
      rust_installing)   fmt="Устанавливаю Rust (rustup, канал stable)…" ;;
      rust_done)         fmt="Rust установлен: %s" ;;
      rust_rustup_fail)  fmt="rustup не сработал. Установите Rust >= %s вручную: https://rustup.rs" ;;
      rust_distro_offer) fmt="Вместо этого поставить Rust из репозитория дистрибутива (может быть старше %s)?" ;;
      rust_still_bad)    fmt="Подходящий Rust не найден — сборка невозможна." ;;
      rust_skipped)      fmt="--skip-deps: Rust не трогаю. Для сборки нужен cargo >= %s" ;;
      disk_continue)     fmt="Продолжить сборку?" ;;
      src_dirty)         fmt="Каталог %s существует, но это не репозиторий davfast." ;;
      src_dirty_offer)   fmt="Переместить его в %s и склонировать заново?" ;;
      src_dirty_abort)   fmt="Прервано: выберите другой --src-dir." ;;
      clone_to)          fmt="Клонирую %s → %s…" ;;
      repo_update)       fmt="Репозиторий уже склонирован в %s — обновляю…" ;;
      repo_reset)        fmt="git pull не сработал — сбрасываю ветку до origin (локальные правки будут потеряны)." ;;
      build_start)       fmt="Сборка (cargo build --release). Обычно 2–5 минут…" ;;
      build_done)        fmt="Сборка завершена за %s с." ;;
      build_failed)      fmt="Сборка не удалась. Частые причины: нет компилятора C, мало места на диске, старый Rust." ;;
      binary_missing)    fmt="Бинарник %s не найден после сборки." ;;
      bin_dir_using)     fmt="Каталог установки: %s" ;;
      bin_dir_head)      fmt="Куда установить бинарник davfast?" ;;
      bin_dir_opt1)      fmt="  1) /usr/local/bin  (общесистемно, нужен %s)" ;;
      bin_dir_opt2)      fmt="  2) %s  (только для текущего пользователя)" ;;
      bin_dir_opt3)      fmt="  3) указать свой путь" ;;
      bin_dir_custom)    fmt="Путь: " ;;
      bin_dir_priv)      fmt="sudo" ;;
      installing)        fmt="Устанавливаю бинарник → %s" ;;
      install_noaccess)  fmt="Нет прав записи в %s, а sudo недоступен." ;;
      install_fallback)  fmt="Ставлю в %s (без root)." ;;
      installed_ok)      fmt="Установлено: %s (%s, коммит %s)" ;;
      path_missing)      fmt="Каталог %s не входит в PATH." ;;
      path_offer)        fmt="Добавить 'export PATH=…' в %s?" ;;
      path_added)        fmt="Добавлено в %s. Откройте новый терминал или: source %s" ;;
      path_hint)         fmt="Добавьте в ~/.bashrc вручную:  export PATH=\"%s:\$PATH\"" ;;
      menu_title)        fmt="Что делаем?" ;;
      menu_install)      fmt="  1) Установить / обновить davfast — сборка из исходников, бинарник в PATH" ;;
      menu_dirs)         fmt="  2) Установить с выбором каталогов — свои пути для бинарника и исходников" ;;
      menu_setup)        fmt="  3) Настроить монтирование облака — бэкенд, логин/токен, точка монтирования, davfs2, автозапуск" ;;
      menu_check)        fmt="  4) Только проверить систему — отчёт о зависимостях, ничего не меняет" ;;
      menu_uninstall)    fmt="  5) Удалить davfast — бинарник и (опционально) исходники" ;;
      menu_exit)         fmt="  0) Выход" ;;
      menu_prompt)       fmt="Выбор [%s]: " ;;
      menu_bad)          fmt="Нет такого пункта меню." ;;
      setup_offer)       fmt="Настроить монтирование облака сейчас (бэкенд, логин, токен, точка монтирования)?" ;;
      setup_title)       fmt="Настройка монтирования облака" ;;
      setup_backend_head) fmt="Какой бэкенд использовать для моста?" ;;
      setup_b_webdav)    fmt="  1) webdav — мост-прокси к любому WebDAV-серверу (рекомендуется; для облака — webdav.cloud.mail.ru)" ;;
      setup_b_webdav_d)  fmt="     нужен логин и «пароль для внешних приложений» (id.mail.ru → Безопасность → Пароли для внешних приложений)" ;;
      setup_b_mailru)    fmt="  2) mailru — прямая работа с веб-API cloud.mail.ru (быстрее на тысячах мелких файлов)" ;;
      setup_b_mailru_d)  fmt="     экспериментальный путь (по открытой документации); нужен «пароль для внешних приложений»" ;;
      setup_b_skip)      fmt="  0) Пропустить настройку" ;;
      setup_url_q)       fmt="URL WebDAV-сервера [%s]: " ;;
      setup_login_q)     fmt="Логин (полный e-mail)" ;;
      setup_token_q)     fmt="Токен / «пароль для внешних приложений» (ввод скрыт): " ;;
      setup_token_empty) fmt="Логин или токен пусты — настройка монтирования прервана." ;;
      setup_url_bad)     fmt="URL должен начинаться с http:// или https://" ;;
      setup_mp_q)        fmt="Точка монтирования [%s]: " ;;
      setup_mp_created)  fmt="Каталог %s готов." ;;
      setup_mp_fail)     fmt="Не удалось создать %s — создайте вручную: sudo mkdir -p %s" ;;
      setup_env_written) fmt="Учётные данные сохранены в %s (права 600; файл хранит токен в открытом виде)." ;;
      setup_unit_written) fmt="Создан сервис моста: %s" ;;
      setup_unit_ok)     fmt="Мост запущен и добавлен в автозапуск (systemctl --user status davfast-serve)." ;;
      setup_unit_fail)   fmt="systemd user-сессия недоступна — мост запущен в фоне (лог: %s)." ;;
      setup_port_timeout) fmt="Мост не поднялся на %s за 10 с — проверьте лог ~/.local/state/davfast-serve.log" ;;
      setup_serve_cmd)   fmt="Запуск моста вручную: %s" ;;
      setup_davfs_missing) fmt="davfs2 не установлен (нужен для монтирования)." ;;
      setup_davfs_offer) fmt="Установить davfs2 через %s?" ;;
      setup_davfs_hint)  fmt="Поставьте davfs2 вручную (%s install davfs2) и повторите пункт 3 меню." ;;
      setup_davfs_conf)  fmt="davfs2 настроен: %s (use_locks 0) и %s (учётка для монтирования, 600)." ;;
      setup_fstab_ok)    fmt="Добавлена строка в /etc/fstab (бэкап: %s). Монтировать: mount %s" ;;
      setup_fstab_line)  fmt="Добавьте в /etc/fstab строку (от root):" ;;
      setup_mount_try)   fmt="Пробую смонтировать %s…" ;;
      setup_mount_ok)    fmt="Смонтировано! Файлы облака доступны в %s" ;;
      setup_mount_fail)  fmt="Автомонтирование не удалось — выполните команды из блоков выше вручную." ;;
      setup_done)        fmt="Настройка монтирования завершена." ;;
      setup_skip)        fmt="Настройка пропущена. Вернуться: однострочник ещё раз → пункт 3 (или install.sh --setup-mount)." ;;
      setup_need_bin)    fmt="Бинарник davfast не найден — сначала установите его (пункт 1 меню)." ;;
      setup_next_head)   fmt="Полезные команды:" ;;
      sum_setup_hint)    fmt="Монтирование можно настроить позже: запустите однострочник ещё раз и выберите пункт «Настроить монтирование» (или install.sh --setup-mount)." ;;
      uninst_found)      fmt="Найден: %s" ;;
      uninst_confirm)    fmt="Удалить перечисленное?" ;;
      uninst_removed)    fmt="Удалено: %s" ;;
      uninst_src_offer)  fmt="Удалить также исходники и сборку в %s?" ;;
      uninst_done)       fmt="Удаление завершено." ;;
      uninst_not_found)  fmt="davfast в типовых местах не найден (нечего удалять)." ;;
      sum_title)         fmt="Установка завершена." ;;
      sum_head)          fmt="Быстрый старт:" ;;
      sum_mount)         fmt="Монтирование облака как диска (davfs2):" ;;
      sum_rerun)         fmt="Обновление — запустите однострочник ещё раз (он обновит и пересоберёт)." ;;
      sum_link)          fmt="Вопросы и проблемы: https://github.com/teliy/davfast/issues" ;;
      nonint_notice)     fmt="TTY недоступен — включаю неинтерактивный режим (все ответы: да, пути по умолчанию)." ;;
      sudo_none)         fmt="sudo не найден: системные пакеты и /usr/local/bin будут недоступны." ;;
      err_line)          fmt="Ошибка в строке %s. Установка прервана." ;;
      done_ok)           fmt="Готово." ;;
      *)                 fmt="<key: %s>" ;;
    esac
  else
    case "$key" in
      banner)            fmt="davfast installer — fast WebDAV client and cloud.mail.ru bridge" ;;
      sys_line)          fmt="System: %s · arch: %s · package manager: %s" ;;
      sys_nopkg)         fmt="System: %s · arch: %s · package manager: none found" ;;
      unknown_distro)    fmt="Unrecognized distribution (%s); continuing without automatic package install" ;;
      check_start)       fmt="Checking system…" ;;
      chk_net_ok)        fmt="Network to github.com .............. OK" ;;
      chk_net_fail)      fmt="Network to github.com .............. UNREACHABLE" ;;
      chk_git_ok)        fmt="git ................................ OK (%s)" ;;
      chk_git_fail)      fmt="git ................................ MISSING" ;;
      chk_curl_ok)       fmt="curl ............................... OK" ;;
      chk_curl_fail)     fmt="curl ............................... MISSING" ;;
      chk_cc_ok)         fmt="C compiler ......................... OK (%s)" ;;
      chk_cc_fail)       fmt="C compiler ......................... MISSING (required to build)" ;;
      chk_rust_ok)       fmt="Rust ............................... OK (%s, need >= %s)" ;;
      chk_rust_old)      fmt="Rust ............................... TOO OLD (%s, need >= %s)" ;;
      chk_rust_missing)  fmt="Rust ............................... NOT FOUND (need cargo >= %s)" ;;
      chk_disk_ok)       fmt="Free disk space .................... %s MB at %s" ;;
      chk_disk_low)      fmt="Free disk space .................... LOW: %s MB at %s (build needs ~%s MB)" ;;
      chk_fail_exit)     fmt="Critical checks failed — cannot proceed." ;;
      deps_needed)       fmt="Missing tools: %s" ;;
      deps_offer)        fmt="Install the missing ones via %s?" ;;
      deps_done)         fmt="System dependencies installed." ;;
      deps_failed)       fmt="Package install failed. Install manually: %s" ;;
      deps_skipped)      fmt="--skip-deps: skipping automatic package install. Install manually: %s" ;;
      deps_still)        fmt="Still missing after install: %s" ;;
      rust_offer)        fmt="Install Rust via rustup (recommended, goes to ~/.cargo)?" ;;
      rust_installing)   fmt="Installing Rust (rustup, stable channel)…" ;;
      rust_done)         fmt="Rust installed: %s" ;;
      rust_rustup_fail)  fmt="rustup failed. Install Rust >= %s manually: https://rustup.rs" ;;
      rust_distro_offer) fmt="Or install Rust from the distro repository (may be older than %s)?" ;;
      rust_still_bad)    fmt="No suitable Rust toolchain found — cannot build." ;;
      rust_skipped)      fmt="--skip-deps: leaving Rust untouched. Building requires cargo >= %s" ;;
      disk_continue)     fmt="Continue with the build?" ;;
      src_dirty)         fmt="Directory %s exists but is not a davfast repository." ;;
      src_dirty_offer)   fmt="Move it to %s and clone fresh?" ;;
      src_dirty_abort)   fmt="Aborted: pick another --src-dir." ;;
      clone_to)          fmt="Cloning %s → %s…" ;;
      repo_update)       fmt="Repository already cloned at %s — updating…" ;;
      repo_reset)        fmt="git pull failed — resetting branch to origin (local changes will be lost)." ;;
      build_start)       fmt="Building (cargo build --release). Usually takes 2–5 minutes…" ;;
      build_done)        fmt="Build finished in %s s." ;;
      build_failed)      fmt="Build failed. Common causes: no C compiler, low disk space, outdated Rust." ;;
      binary_missing)    fmt="Binary %s not found after build." ;;
      bin_dir_using)     fmt="Install directory: %s" ;;
      bin_dir_head)      fmt="Where should davfast be installed?" ;;
      bin_dir_opt1)      fmt="  1) /usr/local/bin  (system-wide, needs %s)" ;;
      bin_dir_opt2)      fmt="  2) %s  (current user only)" ;;
      bin_dir_opt3)      fmt="  3) enter a custom path" ;;
      bin_dir_custom)    fmt="Path: " ;;
      bin_dir_priv)      fmt="sudo" ;;
      installing)        fmt="Installing binary → %s" ;;
      install_noaccess)  fmt="No write access to %s and sudo is unavailable." ;;
      install_fallback)  fmt="Installing to %s instead (no root)." ;;
      installed_ok)      fmt="Installed: %s (%s, commit %s)" ;;
      path_missing)      fmt="Directory %s is not in PATH." ;;
      path_offer)        fmt="Append 'export PATH=…' to %s?" ;;
      path_added)        fmt="Added to %s. Open a new terminal or run: source %s" ;;
      path_hint)         fmt="Add this to ~/.bashrc manually:  export PATH=\"%s:\$PATH\"" ;;
      menu_title)        fmt="What do you want to do?" ;;
      menu_install)      fmt="  1) Install / update davfast — build from source, binary on PATH" ;;
      menu_dirs)         fmt="  2) Install with custom directories — pick paths for the binary and sources" ;;
      menu_setup)        fmt="  3) Set up cloud mounting — backend, login/token, mount point, davfs2, autostart" ;;
      menu_check)        fmt="  4) System checks only — dependency report, changes nothing" ;;
      menu_uninstall)    fmt="  5) Uninstall davfast — binary and (optionally) sources" ;;
      menu_exit)         fmt="  0) Exit" ;;
      menu_prompt)       fmt="Choice [%s]: " ;;
      menu_bad)          fmt="Unknown menu item." ;;
      setup_offer)       fmt="Set up cloud mounting now (backend, login, token, mount point)?" ;;
      setup_title)       fmt="Cloud mount setup" ;;
      setup_backend_head) fmt="Which backend should the bridge use?" ;;
      setup_b_webdav)    fmt="  1) webdav — proxy bridge to any WebDAV server (recommended; for the cloud use webdav.cloud.mail.ru)" ;;
      setup_b_webdav_d)  fmt="     needs a login and an app password (id.mail.ru → Security → App passwords)" ;;
      setup_b_mailru)    fmt="  2) mailru — direct cloud.mail.ru web API (faster for thousands of small files)" ;;
      setup_b_mailru_d)  fmt="     experimental path (based on public docs); needs an app password" ;;
      setup_b_skip)      fmt="  0) Skip setup" ;;
      setup_url_q)       fmt="WebDAV server URL [%s]: " ;;
      setup_login_q)     fmt="Login (full e-mail)" ;;
      setup_token_q)     fmt="Token / app password (hidden input): " ;;
      setup_token_empty) fmt="Login or token is empty — mount setup aborted." ;;
      setup_url_bad)     fmt="URL must start with http:// or https://" ;;
      setup_mp_q)        fmt="Mount point [%s]: " ;;
      setup_mp_created)  fmt="Directory %s is ready." ;;
      setup_mp_fail)     fmt="Could not create %s — create it manually: sudo mkdir -p %s" ;;
      setup_env_written) fmt="Credentials saved to %s (mode 600; the file stores the token in plain text)." ;;
      setup_unit_written) fmt="Bridge service created: %s" ;;
      setup_unit_ok)     fmt="Bridge started and enabled (systemctl --user status davfast-serve)." ;;
      setup_unit_fail)   fmt="No systemd user session — bridge started in the background (log: %s)." ;;
      setup_port_timeout) fmt="Bridge did not come up on %s within 10 s — check ~/.local/state/davfast-serve.log" ;;
      setup_serve_cmd)   fmt="Run the bridge manually: %s" ;;
      setup_davfs_missing) fmt="davfs2 is not installed (required for mounting)." ;;
      setup_davfs_offer) fmt="Install davfs2 via %s?" ;;
      setup_davfs_hint)  fmt="Install davfs2 manually (%s install davfs2), then re-run menu item 3." ;;
      setup_davfs_conf)  fmt="davfs2 configured: %s (use_locks 0) and %s (mount credentials, 600)." ;;
      setup_fstab_ok)    fmt="Line added to /etc/fstab (backup: %s). To mount: mount %s" ;;
      setup_fstab_line)  fmt="Add this line to /etc/fstab (as root):" ;;
      setup_mount_try)   fmt="Trying to mount %s…" ;;
      setup_mount_ok)    fmt="Mounted! Cloud files are available at %s" ;;
      setup_mount_fail)  fmt="Automatic mount failed — run the commands from the blocks above manually." ;;
      setup_done)        fmt="Mount setup finished." ;;
      setup_skip)        fmt="Setup skipped. Return any time: re-run the one-liner → item 3 (or install.sh --setup-mount)." ;;
      setup_need_bin)    fmt="davfast binary not found — install it first (menu item 1)." ;;
      setup_next_head)   fmt="Useful commands:" ;;
      sum_setup_hint)    fmt="You can set up mounting later: re-run the one-liner and pick 'Set up cloud mounting' (or install.sh --setup-mount)." ;;
      uninst_found)      fmt="Found: %s" ;;
      uninst_confirm)    fmt="Delete the listed files?" ;;
      uninst_removed)    fmt="Removed: %s" ;;
      uninst_src_offer)  fmt="Also remove sources and build dir %s?" ;;
      uninst_done)       fmt="Uninstall finished." ;;
      uninst_not_found)  fmt="davfast was not found in standard locations (nothing to remove)." ;;
      sum_title)         fmt="Installation complete." ;;
      sum_head)          fmt="Quick start:" ;;
      sum_mount)         fmt="Mount the cloud as a disk (davfs2):" ;;
      sum_rerun)         fmt="To update later, just re-run the one-liner (it pulls and rebuilds)." ;;
      sum_link)          fmt="Issues and feedback: https://github.com/teliy/davfast/issues" ;;
      nonint_notice)     fmt="No TTY available — switching to non-interactive mode (yes to all, default paths)." ;;
      sudo_none)         fmt="sudo not found: system packages and /usr/local/bin will be unavailable." ;;
      err_line)          fmt="Error at line %s. Installation aborted." ;;
      done_ok)           fmt="Done." ;;
      *)                 fmt="<key: %s>" ;;
    esac
  fi
  # shellcheck disable=SC2059
  printf "$fmt\n" "$@"
}

# ------------------------------------------------------------------ prompts --
no_tty() {
  # /dev/tty may exist but fail to open (ENXIO) when there is no controlling terminal
  if [[ ! -r /dev/tty ]]; then return 0; fi
  if ! { : < /dev/tty; } 2>/dev/null; then return 0; fi
  return 1
}

mark_noninteractive() {
  if (( ! ASSUME_YES )); then
    warn "$(t nonint_notice)"
    ASSUME_YES=1
  fi
}

# confirm "question" [default: y|n] -> 0=yes
confirm() {
  local q="$1" def="${2:-y}" a
  if (( ASSUME_YES )); then
    [[ "$def" == "y" ]]          # non-interactive: default wins (safe for "no"-default questions)
  elif no_tty; then
    mark_noninteractive
    [[ "$def" == "y" ]]
  else
    local yn="[Y/n]"; if [[ "$def" == "n" ]]; then yn="[y/N]"; fi
    printf '%s %s ' "$q" "$yn"
    a=""
    read -r a < /dev/tty || a=""
    a="${a:-$def}"
    [[ "$a" =~ ^([YyДд]|yes|YES|да|Да|ДА)$ ]]
  fi
}

# ask <var> <prompt> [default]
ask() {
  local __var="$1" __prompt="$2" __def="${3:-}" __a=""
  __prompt="${__prompt%$'\n'}"          # strip trailing newline from t()
  if no_tty; then mark_noninteractive; printf -v "$__var" '%s' "$__def"; return 0; fi
  printf '%s' "$__prompt"
  read -r __a < /dev/tty || __a=""
  printf -v "$__var" '%s' "${__a:-$__def}"
}

# ask_hidden <var> <prompt> — secret input, echo suppressed (tokens/passwords)
ask_hidden() {
  local __var="$1" __prompt="$2" __a=""
  __prompt="${__prompt%$'\n'}"
  if no_tty; then mark_noninteractive; printf -v "$__var" '%s' ""; return 0; fi
  printf '%s' "$__prompt"
  read -rs __a < /dev/tty || __a=""
  printf '\n'
  printf -v "$__var" '%s' "$__a"
}

# wait_port <host> <port> — poll for up to ~10 s
wait_port() {
  local i
  # shellcheck disable=SC2034  # loop counter is intentionally unused
  for i in $(seq 1 20); do
    if (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null; then return 0; fi
    sleep 0.5
  done
  return 1
}

# ------------------------------------------------------------------ system --
SUDO=""
PKG=""
OS_PRETTY="?"
ARCH="?"

resolve_sudo() {
  if (( EUID == 0 )); then SUDO=""
  elif command -v sudo >/dev/null 2>&1; then SUDO="sudo"
  elif command -v doas >/dev/null 2>&1; then SUDO="doas"
  else SUDO="none"
  fi
}

detect_os() {
  ARCH="$(uname -m 2>/dev/null || echo '?')"
  if [[ -r /etc/os-release ]]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    OS_PRETTY="${PRETTY_NAME:-$NAME}"
    case "${ID:-}:${ID_LIKE:-}" in
      *debian*|*ubuntu*) PKG="apt" ;;
      *fedora*|*rhel*)   PKG="dnf" ;;
      *arch*)            PKG="pacman" ;;
      *suse*)            PKG="zypper" ;;
      *alpine*)          PKG="apk" ;;
      *)
        PKG=""
        for m in apt-get dnf yum pacman zypper apk; do
          if command -v "$m" >/dev/null 2>&1; then
            case "$m" in apt-get) PKG="apt" ;; dnf|yum) PKG="dnf" ;; pacman) PKG="pacman" ;; zypper) PKG="zypper" ;; apk) PKG="apk" ;; esac
            break
          fi
        done
        ;;
    esac
  else
    OS_PRETTY="$(uname -s 2>/dev/null || echo '?')"
    PKG=""
    for m in apt-get dnf yum pacman zypper apk; do
      if command -v "$m" >/dev/null 2>&1; then
        case "$m" in apt-get) PKG="apt" ;; dnf|yum) PKG="dnf" ;; pacman) PKG="pacman" ;; zypper) PKG="zypper" ;; apk) PKG="apk" ;; esac
        break
      fi
    done
  fi
  if [[ -z "$OS_PRETTY" || "$OS_PRETTY" == "?" ]]; then OS_PRETTY="$(uname -s 2>/dev/null || echo '?')"; fi
}

net_ok() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsI --max-time 8 https://github.com >/dev/null 2>&1
  elif command -v wget >/dev/null 2>&1; then
    wget -q --spider -T 8 https://github.com
  else
    return 0   # no tool to test; curl check will catch it
  fi
}

rustver() {
  command -v rustc >/dev/null 2>&1 || { RUST_V=""; return 1; }
  RUST_V="$(rustc --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' | head -n1 || true)"
  [[ -n "$RUST_V" ]]
}

rust_ok() {
  rustver || return 1
  local maj="${RUST_V%%.*}" min="${RUST_V#*.}"
  min="${min%%.*}"
  if (( maj > MIN_RUST_MAJOR )); then return 0; fi
  if (( maj == MIN_RUST_MAJOR && min >= MIN_RUST_MINOR )); then return 0; fi
  return 1
}

rust_state() {  # echoes: ok | old | missing
  if ! rustver; then echo "missing"; return; fi
  if rust_ok; then echo "ok"; else echo "old"; fi
}

cc_name() {
  if command -v gcc   >/dev/null 2>&1; then echo "gcc"
  elif command -v clang >/dev/null 2>&1; then echo "clang"
  elif command -v cc    >/dev/null 2>&1; then echo "cc"
  else return 1
  fi
}

free_mb() {  # free_mb <dir> -> MB available
  local d="$1"
  while [[ ! -d "$d" && "$d" != "/" && "$d" != "." && -n "$d" ]]; do
    d="$(dirname "$d")"
  done
  df -Pk "$d" 2>/dev/null | awk 'NR==2{print int($4/1024)}'
}

# ----------------------------------------------------------------- packages --
pkg_install() {  # pkg_install "pkg1 pkg2 ..."
  local pkgs="$1"
  if [[ -z "$PKG" ]]; then return 1; fi
  if [[ "$SUDO" == "none" && $EUID != 0 ]]; then warn "$(t sudo_none)"; return 1; fi
  # word splitting of $pkgs is intentional (multiple package names)
  # shellcheck disable=SC2086
  case "$PKG" in
    apt)    $SUDO apt-get update -qq && $SUDO apt-get install -y $pkgs ;;
    dnf)    $SUDO dnf install -y $pkgs ;;
    pacman) $SUDO pacman -Sy --needed --noconfirm $pkgs ;;
    zypper) $SUDO zypper --non-interactive install $pkgs ;;
    apk)    $SUDO apk add $pkgs ;;
    *)      return 1 ;;
  esac
}

build_tools_pkg() {
  case "$PKG" in
    apt)    echo "build-essential" ;;
    dnf)    echo "gcc make" ;;
    pacman) echo "base-devel" ;;
    zypper) echo "gcc make" ;;
    apk)    echo "build-base" ;;
    *)      echo "gcc" ;;
  esac
}

rust_distro_pkg() {
  case "$PKG" in
    apt)    echo "rustc cargo" ;;
    dnf)    echo "rust cargo" ;;
    pacman) echo "rust" ;;
    zypper) echo "rust cargo" ;;
    apk)    echo "rust cargo" ;;
    *)      echo "rust cargo" ;;
  esac
}

ensure_deps() {
  local -a miss=() pkgs=() _ccarr=() still=()
  if ! command -v git >/dev/null 2>&1;  then miss+=("git");  fi
  if ! command -v curl >/dev/null 2>&1; then miss+=("curl"); fi
  if ! cc_name >/dev/null 2>&1;         then miss+=("cc");   fi

  if (( ${#miss[@]} == 0 )); then return 0; fi

  local m
  for m in "${miss[@]}"; do
    case "$m" in
      git)  pkgs+=("git") ;;
      curl) pkgs+=("curl") ;;
      cc)   read -r -a _ccarr <<< "$(build_tools_pkg)"; pkgs+=("${_ccarr[@]}") ;;
    esac
  done
  local pkglist="${pkgs[*]}"

  if (( SKIP_DEPS )); then
    err "$(t deps_skipped "$pkglist")"
    return 1
  fi
  if [[ -z "$PKG" ]]; then
    err "$(t deps_failed "$pkglist")"
    return 1
  fi
  warn "$(t deps_needed "${miss[*]}")"
  if confirm "$(t deps_offer "$PKG")"; then
    info "… $PKG: $pkglist"
    if pkg_install "$pkglist"; then
      ok "$(t deps_done)"
    else
      err "$(t deps_failed "$pkglist")"
      return 1
    fi
  else
    err "$(t deps_skipped "$pkglist")"
    return 1
  fi

  # re-verify
  if ! command -v git >/dev/null 2>&1;  then still+=("git");  fi
  if ! command -v curl >/dev/null 2>&1; then still+=("curl"); fi
  if ! cc_name >/dev/null 2>&1;         then still+=("cc");   fi
  if (( ${#still[@]} > 0 )); then
    err "$(t deps_still "${still[*]}")"
    return 1
  fi
  return 0
}

install_rust() {
  if (( SKIP_DEPS )); then
    err "$(t rust_skipped "1.$MIN_RUST_MINOR")"
    return 1
  fi
  if confirm "$(t rust_offer)"; then
    info "$(t rust_installing)"
    if curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable; then
      export PATH="$HOME/.cargo/bin:$PATH"
      if [[ -f "$HOME/.cargo/env" ]]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
      fi
      if rust_ok; then ok "$(t rust_done "$(rustc --version)")"; return 0; fi
    fi
    warn "$(t rust_rustup_fail "1.$MIN_RUST_MINOR")"
  fi
  # fallback: distro rust
  if [[ -n "$PKG" ]] && confirm "$(t rust_distro_offer "1.$MIN_RUST_MINOR")"; then
    if pkg_install "$(rust_distro_pkg)"; then
      if rust_ok; then ok "$(t rust_done "$(rustc --version)")"; return 0; fi
    fi
  fi
  if rust_ok; then return 0; fi
  err "$(t rust_still_bad)"
  return 1
}

# ------------------------------------------------------------------ checks --
print_banner() {
  hr
  info "  $(t banner)"
  dim "  $REPO_URL"
  hr
  if [[ -n "$PKG" ]]; then
    dim "  $(t sys_line "$OS_PRETTY" "$ARCH" "$PKG")"
  else
    dim "  $(t sys_nopkg "$OS_PRETTY" "$ARCH")"
    warn "  $(t unknown_distro "$OS_PRETTY")"
  fi
  hr
}

run_checks() {  # prints report; returns non-zero only on hard failure (network)
  info "$(t check_start)"
  local rc=0 st mb

  if net_ok; then
    ok "  $(t chk_net_ok)"
  else
    err "  $(t chk_net_fail)"
    rc=1
  fi

  if command -v git >/dev/null 2>&1; then
    ok "  $(t chk_git_ok "$(git --version | awk '{print $3}')")"
  else
    warn "  $(t chk_git_fail)"
  fi

  if command -v curl >/dev/null 2>&1; then
    ok "  $(t chk_curl_ok)"
  else
    warn "  $(t chk_curl_fail)"
  fi

  if cc_name >/dev/null 2>&1; then
    ok "  $(t chk_cc_ok "$(cc_name)")"
  else
    warn "  $(t chk_cc_fail)"
  fi

  rustver || true   # populate RUST_V in the current shell (rust_state runs in a subshell)
  st="$(rust_state)"
  case "$st" in
    ok)      ok   "  $(t chk_rust_ok "$RUST_V" "1.$MIN_RUST_MINOR")" ;;
    old)     warn "  $(t chk_rust_old "$RUST_V" "1.$MIN_RUST_MINOR")" ;;
    missing) warn "  $(t chk_rust_missing "1.$MIN_RUST_MINOR")" ;;
  esac

  mb="$(free_mb "$SRC_DIR")"
  if (( mb >= NEED_MB )); then
    dim "  $(t chk_disk_ok "$mb" "$SRC_DIR")"
  else
    warn "  $(t chk_disk_low "$mb" "$SRC_DIR" "$NEED_MB")"
  fi

  return "$rc"
}

# ------------------------------------------------------------------- repo ---
get_repo() {
  if [[ -d "$SRC_DIR/.git" ]]; then
    info "$(t repo_update "$SRC_DIR")"
    if ! git -C "$SRC_DIR" pull --ff-only --quiet 2>/dev/null; then
      warn "$(t repo_reset)"
      local br
      br="$(git -C "$SRC_DIR" symbolic-ref --short HEAD 2>/dev/null || echo main)"
      git -C "$SRC_DIR" fetch --depth 1 origin "$br"
      git -C "$SRC_DIR" reset --hard "origin/$br"
      git -C "$SRC_DIR" clean -fdq
    fi
    return 0
  fi
  if [[ -e "$SRC_DIR" ]]; then
    warn "$(t src_dirty "$SRC_DIR")"
    local bak
    bak="${SRC_DIR}.bak.$(date +%Y%m%d%H%M%S)"
    if confirm "$(t src_dirty_offer "$bak")"; then
      mv "$SRC_DIR" "$bak"
    else
      err "$(t src_dirty_abort)"
      return 1
    fi
  fi
  mkdir -p "$(dirname "$SRC_DIR")"
  info "$(t clone_to "$REPO_URL" "$SRC_DIR")"
  if [[ -n "$BRANCH" ]]; then
    git clone --depth 1 -b "$BRANCH" "$REPO_URL" "$SRC_DIR"
  else
    git clone --depth 1 "$REPO_URL" "$SRC_DIR"
  fi
}

build_repo() {
  local mb
  mb="$(free_mb "$SRC_DIR")"
  if (( mb < NEED_MB )); then
    warn "$(t chk_disk_low "$mb" "$SRC_DIR" "$NEED_MB")"
    if ! confirm "$(t disk_continue)" y; then return 1; fi
  fi
  info "$(t build_start)"
  local t0=${SECONDS} t1
  if ! ( cd "$SRC_DIR" && cargo build --release ); then
    err "$(t build_failed)"
    return 1
  fi
  t1=$(( SECONDS - t0 ))
  ok "$(t build_done "$t1")"
  if [[ ! -x "$SRC_DIR/target/release/davfast" ]]; then
    err "$(t binary_missing "$SRC_DIR/target/release/davfast")"
    return 1
  fi
}

# ----------------------------------------------------------------- install --
choose_bin_dir() {
  local priv="sudo"
  if [[ "$SUDO" == "none" && $EUID != 0 ]]; then priv="нет sudo / no sudo"; fi
  local home_bin="$HOME/.local/bin"
  info "$(t bin_dir_head)"
  dim "$(t bin_dir_opt1 "$priv")"
  dim "$(t bin_dir_opt2 "$home_bin")"
  dim "$(t bin_dir_opt3)"
  local a d="1"
  if [[ "$SUDO" == "none" && $EUID != 0 ]]; then d="2"; fi
  ask a "$(t menu_prompt "1")" "$d"
  case "$a" in
    1) BIN_DIR="/usr/local/bin" ;;
    2) BIN_DIR="$home_bin" ;;
    3) ask BIN_DIR "$(t bin_dir_custom)" "" ;;
    *) BIN_DIR="/usr/local/bin" ;;
  esac
  if [[ -z "$BIN_DIR" ]]; then BIN_DIR="/usr/local/bin"; fi
}

resolve_bin_dir() {
  if [[ -n "$BIN_DIR" ]]; then return 0; fi
  if (( EUID == 0 )); then BIN_DIR="/usr/local/bin"
  elif [[ "$SUDO" != "none" ]]; then BIN_DIR="/usr/local/bin"
  else BIN_DIR="$HOME/.local/bin"
  fi
}

install_binary() {
  resolve_bin_dir
  if [[ $CHOOSE_DIRS == 1 ]]; then choose_bin_dir; fi
  dim "$(t bin_dir_using "$BIN_DIR")"

  local src_bin="$SRC_DIR/target/release/davfast" dest="$BIN_DIR/davfast"

  if [[ ! -d "$BIN_DIR" ]]; then
    if [[ -w "$(dirname "$BIN_DIR")" ]]; then mkdir -p "$BIN_DIR"
    elif [[ "$SUDO" != "none" ]]; then $SUDO mkdir -p "$BIN_DIR"
    fi
  fi

  if [[ -d "$BIN_DIR" && -w "$BIN_DIR" ]]; then
    info "$(t installing "$dest")"
    install -m 0755 "$src_bin" "$dest"
  elif [[ "$SUDO" != "none" ]]; then
    info "$(t installing "$dest")"
    $SUDO install -m 0755 "$src_bin" "$dest"
  else
    if [[ "$BIN_DIR" == "$HOME/"* ]]; then
      err "$(t install_noaccess "$BIN_DIR")"
      return 1
    fi
    warn "$(t install_fallback "$HOME/.local/bin")"
    BIN_DIR="$HOME/.local/bin"
    mkdir -p "$BIN_DIR"
    install -m 0755 "$src_bin" "$BIN_DIR/davfast"
  fi

  # verify
  local v commit
  v="$("$BIN_DIR/davfast" --version 2>/dev/null | head -n1 || true)"
  commit="$(git -C "$SRC_DIR" rev-parse --short HEAD 2>/dev/null || echo '?')"
  ok "$(t installed_ok "$BIN_DIR/davfast" "${v:-davfast}" "$commit")"

  # PATH
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
      warn "$(t path_missing "$BIN_DIR")"
      local rcfile=""
      case "${SHELL:-}" in
        */zsh) rcfile="$HOME/.zshrc" ;;
        *)     rcfile="$HOME/.bashrc" ;;
      esac
      if [[ -f "$rcfile" ]] && ! grep -qs "davfast PATH" "$rcfile"; then
        if confirm "$(t path_offer "$rcfile")"; then
          # literal $PATH must be written to the rc file
          # shellcheck disable=SC2016
          printf '\n# davfast PATH\nexport PATH="%s:$PATH"\n' "$BIN_DIR" >> "$rcfile"
          ok "$(t path_added "$rcfile" "$rcfile")"
        else
          dim "$(t path_hint "$BIN_DIR")"
        fi
      else
        dim "$(t path_hint "$BIN_DIR")"
      fi
      ;;
  esac
}

print_summary() {
  hr
  ok "$(t sum_title)"
  info "$(t sum_head)"
  cat <<'EOS'
  davfast ls      https://webdav.example.com/dav/
  davfast upload  ~/backup https://webdav.example.com/dav/backup/ -c 32
  davfast sync    ~/backup https://webdav.example.com/dav/backup/
  davfast serve   --backend webdav --url https://webdav.example.com/dav/ --listen 127.0.0.1:8080
EOS
  if [[ "$LANG_SEL" == "ru" ]]; then
    cat <<'EOS'

  cloud.mail.ru (быстрый путь, веб-API):
  export MAILRU_LOGIN='user@mail.ru'
  export MAILRU_PASSWORD='пароль-для-внешних-приложений'
  davfast mailru sync ~/papka /backups -c 32
EOS
  else
    cat <<'EOS'

  cloud.mail.ru (fast path, web API):
  export MAILRU_LOGIN='user@mail.ru'
  export MAILRU_PASSWORD='app password (id.mail.ru -> Security)'
  davfast mailru sync ~/folder /backups -c 32
EOS
  fi
  info "$(t sum_mount)"
  cat <<'EOS'
  davfast serve --backend mailru --listen 127.0.0.1:8080 &
  echo "use_locks 0" | sudo tee -a /etc/davfs2/davfs2.conf
  sudo mount -t davfs http://127.0.0.1:8080/ /mnt/cloud
EOS
  dim "$(t sum_rerun)"
  dim "$(t sum_link)"
  hr
}

# ----------------------------------------------------------- mount setup --
resolve_davfast_bin() {
  command -v davfast 2>/dev/null && return 0
  local c
  for c in /usr/local/bin/davfast "$HOME/.local/bin/davfast" /usr/bin/davfast; do
    if [[ -x "$c" ]]; then echo "$c"; return 0; fi
  done
  return 1
}

setup_mount_flow() {
  local BIN
  BIN="$(resolve_davfast_bin || true)"
  if [[ -z "$BIN" ]]; then
    warn "$(t setup_need_bin)"
    return 1
  fi

  hr
  info "$(t setup_title)"
  hr

  # -- backend choice with explanations -------------------------------------
  info "$(t setup_backend_head)"
  dim "$(t setup_b_webdav)"
  dim "$(t setup_b_webdav_d)"
  dim "$(t setup_b_mailru)"
  dim "$(t setup_b_mailru_d)"
  dim "$(t setup_b_skip)"
  local a backend="" url="" user="" pass=""
  ask a "$(t menu_prompt "1")" "1"
  case "$a" in
    1) backend="webdav" ;;
    2) backend="mailru" ;;
    *) dim "$(t setup_skip)"; return 0 ;;
  esac

  # -- credentials -----------------------------------------------------------
  if [[ "$backend" == "webdav" ]]; then
    ask url "$(t setup_url_q "https://webdav.cloud.mail.ru/")" "https://webdav.cloud.mail.ru/"
    [[ "$url" == http://* || "$url" == https://* ]] || { err "$(t setup_url_bad)"; return 1; }
  fi
  ask user "$(t setup_login_q)" ""
  ask_hidden pass "$(t setup_token_q)"
  if [[ -z "$user" || -z "$pass" ]]; then
    warn "$(t setup_token_empty)"
    return 1
  fi

  # -- mount point -----------------------------------------------------------
  local mp
  ask mp "$(t setup_mp_q "$HOME/cloud")" "$HOME/cloud"
  mp="${mp/#\~/$HOME}"; mp="${mp%/}"
  if [[ -z "$mp" ]]; then mp="$HOME/cloud"; fi
  if [[ ! -d "$mp" ]]; then
    if mkdir -p "$mp" 2>/dev/null; then
      ok "$(t setup_mp_created "$mp")"
    else
      warn "$(t setup_mp_fail "$mp" "$mp")"
    fi
  fi

  # -- env file with credentials (0600) --------------------------------------
  local envdir="$HOME/.config/davfast"
  local envfile="$envdir/davfast.env"
  mkdir -p "$envdir"
  chmod 700 "$envdir" 2>/dev/null || true
  {
    printf '# davfast mount configuration (created by install.sh on %s)\n' "$(date +%Y-%m-%d)"
    printf 'DAVFAST_BACKEND="%s"\n' "$backend"
    printf 'DAVFAST_LISTEN="127.0.0.1:8080"\n'
    if [[ "$backend" == "webdav" ]]; then
      printf 'DAVFAST_URL="%s"\n' "$url"
      printf 'DAVFAST_USER="%s"\n' "$user"
      printf 'DAVFAST_PASS="%s"\n' "$pass"
    else
      printf 'MAILRU_LOGIN="%s"\n' "$user"
      printf 'MAILRU_PASSWORD="%s"\n' "$pass"
    fi
    printf 'DAVFAST_MOUNT="%s"\n' "$mp"
  } > "$envfile"
  chmod 600 "$envfile"
  ok "$(t setup_env_written "$envfile")"

  # -- systemd user unit for the bridge --------------------------------------
  local unitdir="$HOME/.config/systemd/user"
  local unit="$unitdir/davfast-serve.service"
  mkdir -p "$unitdir"
  local serve_cmd
  if [[ "$backend" == "webdav" ]]; then
    # shellcheck disable=SC2016  # ${DAVFAST_URL} must stay literal for systemd
    serve_cmd="$BIN serve --backend webdav --url \${DAVFAST_URL} --listen \${DAVFAST_LISTEN}"
  else
    # shellcheck disable=SC2016
    serve_cmd="$BIN serve --backend mailru --listen \${DAVFAST_LISTEN}"
  fi
  {
    printf '[Unit]\nDescription=davfast WebDAV bridge (%s backend)\nAfter=network-online.target\nWants=network-online.target\n\n' "$backend"
    printf '[Service]\nEnvironmentFile=%s\nExecStart=%s\nRestart=on-failure\nRestartSec=3\n\n' "$envfile" "$serve_cmd"
    printf '[Install]\nWantedBy=default.target\n'
  } > "$unit"
  info "$(t setup_unit_written "$unit")"

  local started=0
  if systemctl --user daemon-reload >/dev/null 2>&1 \
     && systemctl --user enable davfast-serve.service >/dev/null 2>&1 \
     && systemctl --user restart davfast-serve.service >/dev/null 2>&1; then
    ok "$(t setup_unit_ok)"
    started=1
  else
    local log="$HOME/.local/state/davfast-serve.log"
    mkdir -p "$(dirname "$log")"
    (
      set -a
      # shellcheck disable=SC1090
      . "$envfile"
      set +a
      if [[ "$backend" == "webdav" ]]; then
        exec "$BIN" serve --backend webdav --url "$url" --listen 127.0.0.1:8080
      else
        exec "$BIN" serve --backend mailru --listen 127.0.0.1:8080
      fi
    ) >"$log" 2>&1 < /dev/null &
    disown 2>/dev/null || true
    warn "$(t setup_unit_fail "$log")"
    local man_cmd="$BIN serve --backend $backend"
    if [[ "$backend" == "webdav" ]]; then man_cmd+=" --url $url"; fi
    man_cmd+=" --listen 127.0.0.1:8080"
    dim "$(t setup_serve_cmd "$man_cmd")"
    started=1
  fi
  # started is only a marker; both paths continue to the port/mount checks below
  # shellcheck disable=SC2034
  (( started )) || true

  # -- wait for the bridge -----------------------------------------------------
  if ! wait_port 127.0.0.1 8080; then
    warn "$(t setup_port_timeout "127.0.0.1:8080")"
    return 1
  fi

  # -- davfs2 -------------------------------------------------------------------
  local davfs_ok=0
  if command -v mount.davfs >/dev/null 2>&1; then
    davfs_ok=1
  else
    warn "$(t setup_davfs_missing)"
    if [[ -n "$PKG" ]] && confirm "$(t setup_davfs_offer "$PKG")" n; then
      if pkg_install "davfs2" && command -v mount.davfs >/dev/null 2>&1; then
        davfs_ok=1
        ok "$(t done_ok)"
      fi
    fi
    if (( ! davfs_ok )); then
      local hint="apt/dnf/pacman"
      [[ -n "$PKG" ]] && hint="$PKG"
      dim "$(t setup_davfs_hint "$hint")"
    fi
  fi

  mkdir -p "$HOME/.davfs2"
  chmod 700 "$HOME/.davfs2" 2>/dev/null || true
  if ! grep -qs '^use_locks' "$HOME/.davfs2/davfs2.conf" 2>/dev/null; then
    printf 'use_locks 0\n' >> "$HOME/.davfs2/davfs2.conf"
  fi
  if ! grep -qs '127.0.0.1:8080' "$HOME/.davfs2/secrets" 2>/dev/null; then
    printf 'http://127.0.0.1:8080/ davfast davfast\n' >> "$HOME/.davfs2/secrets"
  fi
  chmod 600 "$HOME/.davfs2/davfs2.conf" "$HOME/.davfs2/secrets" 2>/dev/null || true
  ok "$(t setup_davfs_conf "$HOME/.davfs2/davfs2.conf" "$HOME/.davfs2/secrets")"

  # -- fstab (needs root; skipped silently to manual hints otherwise) -----------
  local fstab_ok=0
  local fline
  # shellcheck disable=SC2155
  fline="http://127.0.0.1:8080/ $mp davfs user,noauto,uid=$(id -u),gid=$(id -g) 0 0"
  if [[ $EUID == 0 ]] || { [[ "$SUDO" != "none" ]] && sudo -n true 2>/dev/null; }; then
    if $SUDO grep -Fqs " $mp " /etc/fstab 2>/dev/null; then
      fstab_ok=1
    elif $SUDO cp /etc/fstab /etc/fstab.davfast.bak 2>/dev/null \
      && printf '%s\n' "$fline" | $SUDO tee -a /etc/fstab >/dev/null 2>&1; then
      ok "$(t setup_fstab_ok "/etc/fstab.davfast.bak" "$mp")"
      fstab_ok=1
    fi
  fi
  if (( ! fstab_ok )); then
    dim "$(t setup_fstab_line)"
    dim "  $fline"
  fi

  # -- try to mount ---------------------------------------------------------------
  local mounted=0
  if (( davfs_ok )); then
    info "$(t setup_mount_try "$mp")"
    if (( fstab_ok )); then
      mount "$mp" 2>/dev/null || true
    elif [[ $EUID == 0 ]]; then
      mount -t davfs http://127.0.0.1:8080/ "$mp" 2>/dev/null || true
    elif [[ "$SUDO" != "none" ]] && sudo -n true 2>/dev/null; then
      $SUDO mount -t davfs http://127.0.0.1:8080/ "$mp" 2>/dev/null || true
    fi
    if grep -qs " $mp " /proc/mounts 2>/dev/null; then mounted=1; fi
  fi

  hr
  if (( mounted )); then
    ok "$(t setup_mount_ok "$mp")"
    # shellcheck disable=SC2012  # display listing for the user
    ls -A "$mp" 2>/dev/null | head -n 5 || true
  else
    warn "$(t setup_mount_fail)"
  fi
  info "$(t setup_next_head)"
  dim "  systemctl --user status davfast-serve    # $( [[ "$LANG_SEL" == "ru" ]] && echo 'статус моста' || echo 'bridge status' )"
  if [[ "$LANG_SEL" == "ru" ]]; then
    dim "  mount $mp   # смонтировать (после перезагрузки — то же)"
    dim "  df -h $mp   # проверить"
  else
    dim "  mount $mp   # mount (same after reboot)"
    dim "  df -h $mp   # verify"
  fi
  ok "$(t setup_done)"
}

install_flow() {
  if ! run_checks; then
    err "$(t chk_fail_exit)"
    return 1
  fi
  ensure_deps || return 1
  if ! rust_ok; then
    install_rust || return 1
  fi
  get_repo || return 1
  build_repo || return 1
  install_binary || return 1
  print_summary
  if (( ASSUME_YES )); then
    dim "$(t sum_setup_hint)"
  elif confirm "$(t setup_offer)" y; then
    setup_mount_flow || true
  else
    dim "$(t setup_skip)"
  fi
}

# --------------------------------------------------------------- uninstall --
uninstall_flow() {
  local -a found=()
  local c
  for c in /usr/local/bin/davfast "$HOME/.local/bin/davfast" /usr/bin/davfast /opt/davfast/bin/davfast; do
    if [[ -e "$c" ]]; then found+=("$c"); fi
  done
  if (( ${#found[@]} == 0 )); then
    dim "$(t uninst_not_found)"
  else
    local f
    for f in "${found[@]}"; do
      warn "$(t uninst_found "$f")"
    done
    if confirm "$(t uninst_confirm)" y; then
      for f in "${found[@]}"; do
        if [[ -w "$f" || $EUID == 0 ]]; then rm -f "$f"
        elif [[ "$SUDO" != "none" ]]; then $SUDO rm -f "$f"
        else
          err "$(t install_noaccess "$(dirname "$f")")"
          continue
        fi
        ok "$(t uninst_removed "$f")"
      done
    fi
  fi
  if [[ -d "$SRC_DIR" ]]; then
    if confirm "$(t uninst_src_offer "$SRC_DIR")" n; then
      rm -rf "$SRC_DIR"
      ok "$(t uninst_removed "$SRC_DIR")"
    fi
  fi
  ok "$(t uninst_done)"
}

# -------------------------------------------------------------------- menu --
menu() {
  local a
  while true; do
    print_banner
    info "$(t menu_title)"
    dim "$(t menu_install)"
    dim "$(t menu_dirs)"
    dim "$(t menu_setup)"
    dim "$(t menu_check)"
    dim "$(t menu_uninstall)"
    dim "$(t menu_exit)"
    ask a "$(t menu_prompt "1")" "1"
    case "$a" in
      1) CHOOSE_DIRS=0; if install_flow; then return 0; else echo; fi ;;
      2) CHOOSE_DIRS=1; if install_flow; then return 0; else echo; fi ;;
      3) setup_mount_flow || true; echo; continue ;;
      4) run_checks || true; echo; continue ;;
      5) uninstall_flow; echo; continue ;;
      0) return 0 ;;
      *) warn "$(t menu_bad)"; continue ;;
    esac
  done
}

usage() {
  cat <<'EOT'
davfast installer — https://github.com/teliy/davfast

Usage: install.sh [options]

  -y, --yes, --all   non-interactive: answer yes to everything, use defaults
      --setup-mount  configure mounting only (backend, credentials, mount point,
                     bridge service, davfs2) — davfast must be installed already
      --check-only   run system checks and exit
      --uninstall    remove davfast binary and (optionally) sources
      --bin-dir DIR  install binary to DIR (default: /usr/local/bin, or ~/.local/bin without sudo)
      --src-dir DIR  source directory (default: ~/.local/src/davfast)
      --repo URL     git repository (default: https://github.com/teliy/davfast.git)
      --branch BR    branch to clone/update (default: repository default branch)
      --skip-deps    do not install system packages or rustup automatically
      --lang ru|en   force language (default: auto by $LANG)
      --no-color     disable colored output
  -h, --help         show this help

Environment overrides: DAVFAST_REPO_URL, DAVFAST_SRC_DIR, DAVFAST_BRANCH

One-line install:
  curl -fsSL https://raw.githubusercontent.com/teliy/davfast/main/install.sh | bash
EOT
}

# ------------------------------------------------------------------- main ---
trap 'err "$(t err_line "$LINENO")"' ERR

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -y|--yes|--all) ASSUME_YES=1 ;;
      --check-only)   CHECK_ONLY=1 ;;
      --uninstall)    UNINSTALL=1 ;;
      --setup-mount)  SETUP_MOUNT=1 ;;
      --bin-dir)      BIN_DIR="${2:?--bin-dir needs a value}"; shift ;;
      --src-dir)      SRC_DIR="${2:?--src-dir needs a value}"; shift ;;
      --repo)         REPO_URL="${2:?--repo needs a value}"; shift ;;
      --branch)       BRANCH="${2:?--branch needs a value}"; shift ;;
      --skip-deps)    SKIP_DEPS=1 ;;
      --lang)         FORCE_LANG="${2:?--lang needs a value}"; shift ;;
      --no-color)     USE_COLOR=0 ;;
      -h|--help)      usage; exit 0 ;;
      *)              usage; exit 2 ;;
    esac
    shift
  done
}

main() {
  parse_args "$@"

  # language
  if [[ -n "$FORCE_LANG" ]]; then
    LANG_SEL="$FORCE_LANG"
  else
    local l="${LC_ALL:-${LC_MESSAGES:-${LANG:-en}}}"
    if [[ "$l" =~ ^ru ]]; then LANG_SEL="ru"; else LANG_SEL="en"; fi
  fi
  if [[ "$LANG_SEL" != "ru" && "$LANG_SEL" != "en" ]]; then LANG_SEL="en"; fi

  set_colors
  resolve_sudo
  detect_os

  if (( UNINSTALL )); then
    print_banner
    uninstall_flow
    return 0
  fi

  if (( CHECK_ONLY )); then
    print_banner
    local rc=0
    run_checks || rc=1
    if (( rc == 0 )); then ok "$(t done_ok)"; fi
    exit "$rc"
  fi

  if (( SETUP_MOUNT )); then
    print_banner
    setup_mount_flow
    return 0
  fi

  if no_tty; then mark_noninteractive; fi

  if (( ASSUME_YES )); then
    install_flow
  else
    menu
  fi
}

main "$@"
