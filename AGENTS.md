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

## Семантика (M5): отдельный YantrikDB, НЕ память

Жёсткое правило: индекс кода не пишется в инстанс памяти
(`yantrikdb-fixed`, 7439) — та база belief-revised (затухание,
консолидация, конфликты) и общая для всех клиентов, а `recall` в обёртке
даже не передаёт namespace. Для кода — отдельный контейнер того же
образа: порт 7442, `data-code/`, namespace `codeindex`, domain `code`,
`[maintenance] enabled = false`. Файлы — `deploy/` в репо; рабочая
копия — в `/media/veracrypt3/Docker/yantrikdb/` (там же `codeindex.env`
с токеном, в git не попадает).

Как устроено: SQLite остаётся истиной; `index --semantic` зеркалит
строки объявлений (док-блок + строка объявления) в инстанс через
`/v1/remember/batch` с `idempotency_key` = хэш содержимого. Текст
начинается маркером `[codeindex key=… | file | kind | name | line]`,
поэтому hit самодостаточен и гидрируется из него, а не из скора сервера.
Синк дельта-овый: список namespace → досылаем отсутствующее, забываем
устаревшее (чужие записи не трогаем). Поиск — `search --semantic` /
MCP `search_symbols` с `mode: "semantic"`.

Честность качества (замерено на этом репо): английские технические
запросы ранжируются точно (`resolve use statements` → resolve_path 0.46);
кросс-языковые русские короткие запросы слабее (русский «разбирает
исходник на токены» → `lex` лишь 41-й при 384d MiniLM) — поэтому для
русского надёжнее лексический поиск или английские термины. Скоров
не скрываем: выдача показывает score.

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
- **M5 готов** (2026-10-28): семантический слой на ОТДЕЛЬНОМ инстансе
  YantrikDB (не память): `deploy/` (compose + toml, обслуживание off),
  контейнер `yantrikdb-code` на 7442 с `data-code/`. `src/yantrik.rs` —
  свой мини-HTTP клиент (plain http, chunked, без новых зависимостей),
  маркеры в тексте записи, delta-sync (list → batch remember 500
  через `/v1/remember/batch` с idempotency_key → forget stale),
  `recall` с namespace/domain. Символы получили `signature` (док-блок +
  строка объявления) — схема v3, extractor v5. CLI: `index --semantic`,
  `search --semantic`; MCP: `serve --semantic`, `search_symbols
  {mode:"semantic"}`, stats показывает semantic-блок. 58 тестов +
  интеграционный, clippy чисто; живой прогон: 266 записей, повторный
  синк 0 pushed за 54 мс, семантика отвечает (EN точно). Замеренная
  слабость: русские кросс-языковые запросы ранжируются хуже английских
  — см. раздел «Семантика».
- Следующая веха: **не назначена** (v1 закрыт: M0–M5).
- **Семантика на bge-m3** (2026-10-29): инстанс `yantrikdb-code`
  переведён в `strategy = "client_only"` (хранилище, не эмбеддер);
  векторы считает Ollama `bge-m3` (1024d), присылаются с каждой записью
  и с каждым recall (`query_embedding`). Посчитанные векторы кэшируются
  в SQLite (`embeddings(symbol_key, model, dim, vector)`, схема v5) —
  ре-синк не переэмбеддивает неизменённые символы, а смена модели
  инвалидирует кэш. Синк идемпотентен при пропаже Ollama/инстанса:
  лексические тулы продолжают работать, семантический режим честно
  сообщает ошибку. Замер (этот репо): «функция, которая разбирает
  исходник на токены» → `lex` rank 2 (на 384d MiniLM — вне top-60),
  «обновление индекса…» → `index` rank 1; 400 записей синка —
  1.5 с, повтор — 16 мс. Env: `CODEINDEX_EMBED_URL`,
  `CODEINDEX_EMBED_MODEL`. 89 тестов + 1 интеграционный.
- **Резолвер, третий проход** (2026-10-29): типы получателей —
  `bindings` (параметры `name: Type` и `let name: Type`) в отдельной
  таблице; `x.foo()` резолвится через impl-таблицу типа (а `dyn Trait`
  — через методы трейта); внешние типы (`String`, `Vec`) остаются
  candidate; shadowing — побеждает последняя аннотация до вызова.
  Константы/статики/`type`-алиасы — символы (contextual keywords
  принимаются только в item-позиции, `const N: usize` в генериках не
  ловится). Тела `macro_rules!`/`macro` не индексируются (символы,
  импорты, спаны). Не-UTF-8 читается lossy (хэш считается по сырым
  байтам), нечитаемые файлы пропускаются. Кросс-крейтовый резолв в
  workspace по имени пакета. `callee_tree` (вниз) + CLI `callees`,
  MCP-тул девятый. Glob (`*`, `?`) в `search` — регистрозависимый.
  `outputSchema` у всех тулов — с реальными свойствами. Схема v4,
  extractor v8. Fuzz-тест лексера нашёл два дефекта: не-ASCII
  char-литерал (`'Θ'`) и escape по байтам — оба исправлены. 86 тестов
  + 1 интеграционный.
- **Резолвер, второй проход** (2026-10-29): варианты enum — символы
  `Enum::Variant` (kind `variant`, экстрактор v7; бар `{ A, B(u8),
  C {..} }` разбирается с учётом скобок и дискриминантов); путь-резолвер
  переписан с цепочками ре-экспортов (`pub use`, глубина ≤8 против
  циклов) и квалифицированными детьми (`Enum::Variant`,
  `Trait::method`); импортированный вариант резолвится как конструктор.
  `end_line` теперь считается для всех item'ов с телом (get_symbol
  печатает enum/struct/trait целиком). MCP: каждый тул отдаёт
  `structuredContent` (+ `outputSchema: {type: object}`), текст остаётся
  для старых клиентов. 74 теста + 1 интеграционный.
- **Пост-v1 харденинг готов** (2026-10-28): надёжность — файлы не в
  UTF-8 и нечитаемые пропускаются с предупреждением (`stats.skipped`),
  симлинкнутые каталоги не обходятся (циклы!), `busy_timeout = 5000`
  (MCP-сервер и CLI пишут в одну базу), SIGPIPE → тихий выход при
  `| head`. Резолвер: `self.method()` резолвится внутри impl
  (`CallKind::MethodSelf`, call_kind `self_method`) — на само-индексе
  +85 resolved. Лексер: мультибайтные символы вне строк больше не дают
  byte-split Punct (паника на `text()`); регрессионный тест включён.
  CLI: `symbol` (тело), `impact --since`, `stats`, `--json`, фильтры
  `--kind`/`--path` в `search`. MCP: восьмой тул `get_symbol`, фильтры
  в `search_symbols`. Семантика: гибридный поиск (точные совпадения по
  имени помечаются `lex`), батчи ограничены размером payload 48 КиБ —
  сервер режет тела >64 КиБ (HTTP 413). Инфраструктура: README,
  LICENSE (Apache-2.0), CI (fmt + clippy -D warnings + tests),
  `cargo fmt` применён. 69 тестов + 1 интеграционный.

## Важно

- Не уходить от честных пометок рёбер (resolved/candidate/unresolved) —
  это принцип проекта, не деталь.
- НЕ добавлять парсинг-зависимости: лексер и сканер — свои (решение
  2026-10-28). Функционал не резать — расширять свою реализацию.
- Лексер: жизненные ловушки покрыты тестами (raw/byte-строки, вложенные
  блочные комментарии, 'a vs 'a', float vs range, экранированные
  кавычки) — при правках лексера сначала добавь ломающий тест.
- Русские пояснения — сюда и в PLAN.md; код и комментарии — английские.
