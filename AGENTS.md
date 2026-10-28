# codeindex — рабочий стол проекта

Свой индекс кода для ИИ-агентов (Rust-only v1): ПОЛНОСТЬЮ СВОЯ реализация
на Rust — собственный лексер + токенный сканер элементов; никаких
парсинг-зависимостей (tree-sitter/stack-graphs не используются).
Хранилище SQLite, MCP-поверхность. Мотивация и полный план — в PLAN.md.

## Запуск и тесты

```bash
PATH="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"
cargo test                      # юнит-тесты на фикстурах
cargo run -- scan <путь>        # M0: инвентарь символов
# M1+: cargo run -- index <путь> && cargo run -- search <query>
```

## MCP-сервер (M4): подключение

`codeindex serve <репо>` — JSON-RPC 2.0 по stdio (по строке на сообщение).
stdout — только протокол, логи в stderr. На старте сервер делает
инкрементальный `index`; по умолчанию повторяет его перед каждым tool
call (свежесть важнее скорости; `--no-refresh` отключает).

Инструменты: `search_symbols`, `get_definition`, `get_callers`,
`get_references`, `call_tree`, `blast_radius`, `stats`. `get_references`
— лексические вхождения по индексированным файлам (definition/call/
mention); не-вызовы честно помечены `mention`, не «resolved».

Собрать бинарник: `cargo build --release` → `target/release/codeindex`
(с учётом `CARGO_TARGET_DIR`, если задан).

zcode — `~/.zcode/cli/config.json`, ключ `servers` (формат проверен
по живому конфигу):

```json
{ "servers": { "codeindex": {
    "command": "/path/to/codeindex",
    "args": ["serve", "/media/veracrypt3/Developer/codeindex"]
} } }
```

opencode — `~/.config/opencode/cli.json` (или `opencode.json`), ключ
`mcp`:

```json
{ "mcp": { "codeindex": {
    "type": "local",
    "command": ["/path/to/codeindex", "serve", "/path/to/repo"],
    "enabled": true
} } }
```

## Состояние (обновлять после каждой вехи!)

- **M0 готов** (2026-10-28): сканер символов (свой лексер + токенный
  сканер). Коммит: см. git log.
- **M1 готов** (2026-10-28): SQLite-хранилище `.codeindex/index.db` —
  meta(schema_version), files(path, hash, mtime),
  symbols(file_id, name, kind, line); edges — в M2/M3 (таблица не
  создаётся, пока нечем честно заполнить). Инкрементальный
  `index <path>`: ре-парс только при расхождении SHA-256, mtime не
  доверяется, пути относительные, чужая schema_version → отказ.
  19/19 тестов, clippy чисто; само-индексация 4 файла/66 символов,
  второй прогон 0 parsed, touch без правки не перепарсивает.
- Следующая веха: **M2** — запросы по хранилищу: `search_symbols`,
  `get_definition`, `get_callers` (candidate-рёбра по имени).
- **M2 готов** (2026-10-28): CLI `search` / `definition` / `callers`
  (короткие имена; MCP-тулы M4 — search_symbols/get_definition/
  get_callers). Таблица edges(src_file_id, src_line, callee, tier):
  экстрактор кандидатов — прямой вызов, метод, последний сегмент
  пути, турбофиш `foo::<T>(`; гварды: `fn/struct/enum/union` перед
  именем = определение, ключевые слова + Fn-семейство исключены.
  Везде честный tier `candidate`. Ловушка: лексер отдаёт `::` двумя
  токенами `:` — матчить парами. Инкремент расширен: meta
  `extractor_version`, при смене экстрактора полный ре-парс
  (проверено на M1-базе). 26/26 тестов, clippy чисто; само-индекс
  84 символа, `callers lex` = 6 реальных мест.
- Следующая веха: **M3** — свой резолвер имён (модульное дерево +
  use-пути + учёт impl/trait) → resolved-рёбра, `call_tree`,
  `blast_radius`.
- **M3 готов** (2026-10-28): свой резолвер имён. `src/scopes.rs` —
  inline-модули со спанами, impl/trait-спаны, use-деревья (группы,
  алиасы, glob, self/super/crate, ведущий `::`). `src/resolve.rs` —
  модульное дерево из раскладки файлов (корни: каталоги с lib.rs/
  main.rs; один корень → ns `crate`), резолв: plain (items модуля →
  use-биндинги), path (квалификатор: тип → impl-таблица, модуль,
  `crate::`/`self::`/`super::`, алиас крейта из имени каталога),
  `Self::` внутри impl. Честность: метод-вызовы — всегда
  `candidate(method-call)` (нужен вывод типов, его нет), glob —
  `candidate(glob-import)`, внешние крейты/неоднозначности —
  unresolved/candidate с причиной. `src/query.rs` — `call_tree` /
  `blast_radius`: подъём от места вызова к объемлющей fn (символы
  получили `end_line`; попутно фикс: `impl<T> Pair<T>` больше не
  называется `T`). Схема v2 (+module у files; modules/impls/traits/
  imports; resolved_file_id/line/reason у edges), extractor v3,
  резолюция пересчитывается целиком при каждом `index` (дешёво в
  памяти). 42/42 теста, clippy чисто, мутации роняют тесты;
  само-индекс: 178 символов / 1541 ребро (186 resolved, 1093
  candidate, 262 unresolved), `call_tree lex` доходит до main.rs:82;
  повторная индексация 0 parsed.
- **M4 готов** (2026-10-28): MCP-сервер stdio (JSON-RPC 2.0 по
  строкам): `initialize`/`ping`/`tools/list`/`tools/call`, 7
  инструментов; уведомления без ответа, неизвестный метод — -32601,
  неизвестный тул — -32602, сбой тула — result с `isError` (не
  протокольная ошибка). `stats` — счётчики по kind и «реестр
  честности» (reasons). `get_references` читает индексированные файлы
  на запросе: индекс даёт набор файлов, лексер — вхождения
  (definition/call/mention). Живая stdio-сессия — интеграционный тест
  `tests/mcp_stdio.rs` (спавнит бинарник, говорит JSON-RPC). 52/52 +
  1 интеграционный, clippy чисто; живой прогон на своём репо: 9
  файлов / 213 символов / 1875 рёбер (230 resolved, 1311 candidate,
  334 unresolved). Попутный фикс: `blast_radius` терял второе место
  вызова из той же функции.
- Следующая веха: **M5 (опционально)** — бэкенд YantrikDB +
  эмбеддинги сигнатур для семантического поиска символов.

## Важно

- Не уходить от честных пометок рёбер (resolved/candidate/unresolved) —
  это принцип проекта, не деталь.
- НЕ добавлять парсинг-зависимости: лексер и сканер — свои (решение
  2026-10-28). Функционал не резать — расширять свою реализацию.
- Лексер: жизненные ловушки покрыты тестами (raw/byte-строки, вложенные
  блочные комментарии, 'a vs 'a', float vs range, экранированные
  кавычки) — при правках лексера сначала добавь ломающий тест.
- Русские пояснения — сюда и в PLAN.md; код и комментарии — английские.
