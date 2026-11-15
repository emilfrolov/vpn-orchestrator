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

Единый workflow `CI` в `.github/workflows/rust.yml` запускается для push в `master`, pull request в `master`, тегов `v*.*.*`, ежедневно в 11:39 UTC и вручную через `workflow_dispatch`. Задача `Rust checks` выполняет все проверки выше на Ubuntu без sudo. Повторный запуск для того же PR отменяет предыдущий; сборки с публикацией не прерываются новым push.

Только после успешных проверок задача `Docker image` собирает образ для `linux/amd64`: текущий Dockerfile устанавливает Xray для x86_64. В PR и при ручном запуске из другой ветки образ только собирается. Для `master` и тегов `v*` он публикуется в `ghcr.io/<владелец>/<репозиторий>` через `GITHUB_TOKEN` и подписывается cosign по digest через GitHub OIDC. Репозиторий должен иметь право записи в соответствующий пакет GHCR; дополнительные секреты не требуются.

Ветка `master` обновляет теги `master` и `sha-<commit>`. Релиз `v1.2.3` сохраняет исходный тег `v1.2.3` и добавляет `1.2.3`, `1.2`, `latest` и `sha-<commit>`; prerelease не обновляет `1.2` и `latest`. Ежедневная сборка также добавляет `nightly`, заново выполняя слои Dockerfile и подтягивая базовые образы. Это обновляет внешние инструменты даже без изменений Rust-кода. Actions закреплены по полным SHA с указанием версии в комментарии; при обновлении меняйте обе части и проверяйте workflow через `actionlint`.
