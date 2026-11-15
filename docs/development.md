# Разработка

## Структура

```
src/
├── main.rs            — CLI, запуск и graceful shutdown
├── config.rs          — TOML и валидация
├── server.rs          — серверный WG/AWG-интерфейс
├── routing.rs         — policy routing
├── nat.rs             — nftables/iptables
├── util.rs            — сетевые утилиты
├── monitor.rs         — пробы, selector и Prometheus
└── upstream/
    ├── mod.rs         — UpstreamManager и failover
    ├── wireguard.rs   — WireGuard и AmneziaWG
    └── xray.rs        — Xray TUN
```

## Добавление протокола

1. Создайте адаптер в `src/upstream/<type>.rs` с методами `start`, `stop`, `health_check`, `name`, `interface`, `health_check_interval` и `failover_threshold`.
2. Добавьте вариант в `UpstreamKind` и ветки dispatch в `src/upstream/mod.rs`.
3. Добавьте вариант в `UpstreamType` и его создание в `create_upstream`.
4. Добавьте проверку конфигурации и сценарные тесты для запуска, failover и очистки.

## Проверки

```bash
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --locked
python3 tests/regression.py
```

Сценарные тесты подменяют сетевые утилиты и Xray во временной директории и не меняют сеть хоста. Для проверяемого процесса они задают `NO_COLOR=1`, чтобы ANSI-коды не мешали проверять поля логов. Они покрывают failover, запуск без доступного VPN, отключённый NAT, переключение по скорости, откат ошибок маршрутизации и исключение отключённого upstream. Fake Xray с exit 23 проверяет общий backoff при одновременных источниках recovery, доступность stderr, независимость active health-check и сброс после подтверждённого восстановления. Модульные тесты без ожидания реального времени проверяют рост до потолка, границы jitter и насыщение счётчика отказов.

## GitHub Actions

Единый workflow `CI` в `.github/workflows/ci.yml` запускается для push в `master`, pull request в `master`, тегов `v*.*.*`, публикации GitHub Release (`release: published`, включая prerelease), ежедневно в 11:39 UTC и вручную через `workflow_dispatch`. Задача `Rust checks` выполняет все проверки выше на Ubuntu без sudo. Повторный запуск для того же PR отменяет предыдущий; сборки с публикацией не прерываются новым push.

Только после успешных проверок задача `Docker image` собирает корневой `Dockerfile` для `linux/amd64`: он устанавливает Xray для x86_64. В PR и при ручном запуске из другой ветки образ только собирается. Для `master` и тегов `v*` одна сборка публикуется в два реестра: `ghcr.io/<владелец>/<репозиторий>` и `docker.io/donakan/vpn-orchestrator`. Образы в обоих реестрах подписываются cosign по digest через GitHub OIDC.

GHCR использует `GITHUB_TOKEN`; репозиторий должен иметь право записи в соответствующий пакет. Для Docker Hub добавьте repository secrets `DOCKERHUB_USERNAME` (логин пользователя Docker Hub) и `DOCKERHUB_TOKEN` (access token с правом записи в `donakan/vpn-orchestrator`) в GitHub → Settings → Secrets and variables → Actions. В PR эти секреты не используются. Без них публикация завершится ошибкой авторизации.

При публикации GitHub Release с тегом `v*` отдельная задача `Release image instructions` после успешной публикации и подписи добавляет в описание команды `docker pull` для обоих реестров с тегом текущей версии. Остальной текст релиза сохраняется; повторный запуск заменяет только автоматически добавленный блок между HTML-комментариями `vpn-orchestrator:docker`, без дублирования команд. Только эта задача получает `contents: write`; новый секрет не нужен. Архивы образов в Assets не загружаются. Push тега и публикация Release запускают CI независимо: сам push не меняет описание релиза. Уже опубликованные релизы автоматически не обновляются; для нового релиза тег должен включать актуальный workflow.

Ветка `master` обновляет теги `master` и `sha-<commit>`. Релиз `v1.2.3` сохраняет исходный тег `v1.2.3` и добавляет `1.2.3`, `1.2`, `latest` и `sha-<commit>`; prerelease не обновляет `1.2` и `latest`. Ежедневная сборка также добавляет `nightly`, заново выполняя слои Dockerfile и подтягивая базовые образы. Это обновляет внешние инструменты даже без изменений Rust-кода. Actions закреплены по полным SHA с указанием версии в комментарии; при обновлении меняйте обе части и проверяйте workflow через `actionlint`.
