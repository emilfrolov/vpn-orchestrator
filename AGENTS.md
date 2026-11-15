# AGENTS.md

Rust-демон: policy routing трафика с серверного WG/AWG-интерфейса через upstream-VPN (WireGuard, AmneziaWG, Xray TUN) с failover.

## Проверки

Запускать перед завершением любой задачи, затрагивающей код:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --locked
python3 tests/regression.py
```

## Запреты

- Не добавлять зависимости без явной просьбы пользователя.
- Только IPv4: конфиг отклоняет IPv6-подсети, не «дорабатывать» поддержку IPv6.
- Сценарные тесты подменяют сетевые утилиты во временной директории и не меняют сеть хоста — новые тесты обязаны сохранять это свойство.
- Не запускать собранный бинарник и реальные `ip`/`nft`/`wg`/`awg`/`xray` команды на хосте: демон требует root и меняет маршрутизацию.
- Не запускать `tests/regression.py` под sudo.

## Куда смотреть

- `docs/development.md` — структура кода, добавление upstream-протокола.
- `docs/configuration.md` — все секции TOML-конфига.
- `README.md` — схема работы и маршрутизация трафика.

## Стиль

- Rust 2021, tokio; ошибки — `anyhow::Result`, без паник; логи — `tracing`.
- Комментарии и документация — на русском, как в остальном проекте.
- Изменения должны проходить fmt и clippy с `-D warnings`.