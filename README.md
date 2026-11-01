# Readio

Настольная читалка EPUB, FB2 и TXT с синтезом речи. Книги, настройки, закладки и позиция хранятся в SQLite в каталоге данных приложения. В настройках можно импортировать библиотеку Flutter, указав её каталог `Readio` с `index.json`.

## Разработка и сборка

Установите Rust 1.99.1, Node.js 22 и [системные зависимости Tauri v2](https://v2.tauri.app/start/prerequisites/). Затем выполните:

```sh
npm --prefix frontend ci
npm --prefix frontend run tauri -- dev
```

Проверка: `cargo test --workspace`, `npm --prefix frontend run typecheck`. Для сборки установщика Windows x64:

```sh
npm --prefix frontend run tauri -- build --target x86_64-pc-windows-msvc
```

На macOS используйте `npm --prefix frontend run tauri -- build` без параметра `--target`.

## Сборки в GitHub Actions

Workflow `CI` запускается при push и pull request. Его также можно запустить вручную: **Actions → CI → Run workflow**, затем выбрать ветку. После успешного запуска файлы доступны в разделе **Artifacts** на странице этого запуска:

- `readio-windows11-x64-installer` — установщик NSIS для Windows x64.
- `readio-macos-apple-silicon` — DMG и ZIP с приложением для Mac на Apple Silicon.

Сборка macOS имеет ad-hoc подпись и не нотариально заверена Apple. При первом запуске Gatekeeper может потребовать разрешить открытие приложения в настройках «Конфиденциальность и безопасность». Установщик Windows также не подписан сертификатом издателя. Workflow `CI` не публикует GitHub Releases.

## Публикация релиза

Workflow `Release` собирает установщики и публикует их в GitHub Releases при отправке тега вида `v*`. Для уже существующего тега откройте **Actions → Release → Run workflow**, выберите ветку с workflow и укажите тег, например `v0.1.0`. Сборка использует код из указанного тега; тег должен уже существовать и содержать файлы приложения и сборки.

Workflow использует автоматически предоставляемый GitHub Actions `GITHUB_TOKEN` с разрешением `contents: write`; отдельные секреты и `.env` не требуются. Проверки кода запускает отдельный workflow `CI`. macOS-сборка имеет только ad-hoc подпись без нотариального заверения, Windows-установщик не подписан сертификатом издателя.

## Синтез речи

Голоса Edge работают через интернет. Для локальной речи установите Piper и русскую ONNX-модель вместе с её файлом `.onnx.json`. Поместите исполняемый файл в `piper/` внутри каталога данных Readio либо добавьте его в `PATH`. Модели поместите в `models/piper/` внутри того же каталога. Переменные `READIO_PIPER_BIN` и `READIO_PIPER_MODELS` позволяют указать другие пути к исполняемому файлу и каталогу моделей. Перезапустите Readio после установки.
