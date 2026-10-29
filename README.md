# codeindex

Локальный индекс кода для ИИ-агентов: символы, рёбра вызовов, разрешение
имён, семантический поиск — и всё это **своей реализацией на Rust**, без
tree-sitter, stack-graphs и прочих парсинг-зависимостей.

Мотивация: `repowise` покрывает здоровье кода, историю и семантику, но не
отвечает на структурные вопросы — «кто вызывает эту функцию», «что
сломается, если я её изменю», «где эта функция определена». Готовые
решения (Benzi) проприетарные, в бете и требуют логина. Здесь — свой
лексер, свой резолвер, SQLite и MCP.

## Что умеет

| Слой | Возможность |
|---|---|
| M0 | Рукописный Rust-лексер + токенный сканер: `fn/struct/enum/trait/impl/mod/union/const/static/type`, варианты enum, док-блоки |
| M1 | SQLite-хранилище `.codeindex/index.db`, инкрементальная индексация по SHA-256 |
| M2 | Места вызовов: прямые, методы, пути, турбофиш; запросы `search`/`definition`/`callers` |
| M3 | Свой резолвер имён: модульное дерево, `use`-пути, ре-экспорты, impl-таблица, `Self::`, `self.method()`, `Trait::method`, варианты enum, типы получателей из параметров и `let`-аннотаций, кросс-крейтовый резолв; `call_tree`, `callee_tree`, `trace_path`, `blast_radius`, иерархия типов/трейтов |
| M4 | MCP-сервер (stdio, JSON-RPC 2.0) с пятнадцатью инструментами; ответы несут `structuredContent` + точные `outputSchema` |
| M5 | Семантический поиск на **отдельном** инстансе YantrikDB (не память агента); векторы — Ollama `bge-m3` (1024d), кэш в SQLite |

## Честность рёбер

Каждое ребро вызова помечено:

- `resolved` — цель известна точно (файл и строка);
- `candidate` — форма вызова допускает такую цель, но доказательств нет
  (метод-вызов без типа получателя, glob-импорт, неоднозначность);
- `unresolved` — цель не найдена, и указана причина
  (`method-call`, `glob-import`, `qualifier-not-found`,
  `not-found-in-scope`, `no-such-fn-in-impl`, `not-a-function`,
  `not-found-in-module`, `ambiguous`).

Ассоциированные константы и статики индексируются квалифицированно
(`Store::MAX`), включая резолв через `use` — как варианты enum.

Кроме того, у `unresolved`-рёбер, чей путь начинается не с локального
модуля, записывается **внешний крейт** (`std` в `std::mem::drop`,
`serde` в `use serde::Serialize`). Это метка, а не резолюция: tier
остаётся `unresolved`, но в выводе такой вызов честно помечен
`external: std` — «вне индекса», а не «не смогли разобрать».

Ни один «кандидат» не выдаётся за факт — это принцип проекта, а не
деталь реализации.

## Быстрый старт

```bash
cargo build --release
./target/release/codeindex index .            # построить/обновить индекс
./target/release/codeindex search . lex        # подстрока; "lex*" — glob
./target/release/codeindex definition . Store
./target/release/codeindex callers . lex      # с tier и resolved-целью
./target/release/codeindex call-tree . lex --depth 3
./target/release/codeindex callees . lex --depth 3
./target/release/codeindex hierarchy . Store
./target/release/codeindex trace . main lex
./target/release/codeindex profile . lex
./target/release/codeindex external . run_tool
./target/release/codeindex skim . run_tool
./target/release/codeindex blast-radius . lex
./target/release/codeindex symbol . lex       # исходник символа
./target/release/codeindex references . lex
./target/release/codeindex stats .
./target/release/codeindex impact . --since HEAD~1   # что задевает diff
```

Любая команда принимает `--json`.

## MCP (для zcode / opencode / любых клиентов)

```bash
codeindex serve /path/to/repo            # stdio, JSON-RPC 2.0
codeindex serve /path/to/repo --no-refresh  # не переиндексировать перед вызовом
```

Инструменты: `search_symbols` (в т.ч. `mode: "semantic"`), `get_definition`,
`get_callers`, `get_references`, `get_symbol`, `call_tree`, `callee_tree`,
`trace_path`, `get_hierarchy`, `profile`, `external_calls`, `skim_source`,
`argument_provenance`, `blast_radius`, `stats`.

zcode (`~/.zcode/cli/config.json`, ключ `servers`):

```json
{ "servers": { "codeindex": {
    "command": "/path/to/codeindex",
    "args": ["serve", "/path/to/repo"]
} } }
```

opencode (`~/.config/opencode/cli.json`, ключ `mcp`):

```json
{ "mcp": { "codeindex": {
    "type": "local",
    "command": ["/path/to/codeindex", "serve", "/path/to/repo"],
    "enabled": true
} } }
```

## Семантический поиск (M5)

Отдельный инстанс YantrikDB — **никогда** не база памяти агента: память
belief-revised (затухание, консолидация, конфликты), а индекс кода обязан
быть детерминированным. Готовые файлы — `deploy/` (compose + конфиг
инстанса с отключённым обслуживанием).

```bash
set -a; . ./codeindex.env; set +a      # CODEINDEX_YANTRIK_URL/TOKEN/NAMESPACE
codeindex index . --semantic
codeindex search . "resolve use statements" --semantic
```

Инстанс мультирепозиторный: namespace по умолчанию — имя каталога
репозитория (переопределяется `CODEINDEX_YANTRIK_NAMESPACE`), поэтому
несколько проектов живут в одном инстансе, не затирая друг друга.

Инстанс работает в режиме `client_only` — он хранилище, а не эмбеддер:
векторы считает Ollama (`bge-m3`, 1024d) и присылает их с каждой записью
и с каждым запросом; посчитанные векторы кэшируются в SQLite
(`embeddings`), поэтому ре-синк не переэмбеддивает неизменённые символы.

Векторы проверяются: нулевой или нечисловой ответ эмбеддера — это сбой
раннера, а не «почти ноль», поэтому codeindex перезагружает модель
(`keep_alive: 0`) и повторяет запрос один раз, а если не помогло —
возвращает ошибку. Кэш с невалидными векторами пересчитывается.

```bash
# окружение (см. deploy/README.md)
CODEINDEX_EMBED_URL=http://127.0.0.1:11434   # Ollama
CODEINDEX_EMBED_MODEL=bge-m3
```

SQLite остаётся источником истины: в инстанс зеркалятся объявления
символов (док-блок + сигнатура) с маркером
`[codeindex key=… | file | kind | name | line]`, поэтому хит
восстанавливается из нашего же маркера. Синхронизация дельтовая:
`idempotency_key` = хэш содержимого, повторная индексация дублей не создаёт.
Семантический поиск гибридный: к semantic-выдаче добавляются точные
совпадения по имени (метка `lex`).

Замеренное качество (bge-m3, этот репозиторий): «функция, которая
разбирает исходник на токены» → `lex` rank 2; «обновление индекса только
для изменённых файлов» → `index` rank 1; «кто вызывает функцию и что
сломается» → `Impact`/`call_tree`. На прежней 384d MiniLM первый запрос
не попадал даже в top-60.

## Архитектура

```
src/lexer.rs    рукописный лексер (вложенные комментарии, raw/byte-строки,
                'a vs 'a', float vs range)
src/lib.rs      сканер элементов и вызовов, инкрементальный index()
src/scopes.rs   inline-модули, impl/trait-спаны, use-деревья
src/resolve.rs  резолвер имён (модульное дерево из раскладки файлов)
src/store.rs    SQLite: files/symbols/edges/modules/impls/traits/imports
src/query.rs    call_tree, blast_radius, references, impact, source
src/mcp.rs      MCP-сервер (stdio)
src/yantrik.rs  семантический слой (свой мини-HTTP клиент)
```

Схема БД версионирована: чужая версия отвергается с просьбой удалить
`.codeindex`. `extractor_version` в meta заставляет один раз
перепарсить всё дерево при смене экстрактора.

## Известные приближения (осознанные)

- Метод-вызовы резолвятся только когда тип получателя известен из
  сигнатуры или `let`-аннотации (плюс `self.foo()`); настоящего вывода
  типов нет — `x.foo()`, где `x` получен как результат вызова,
  остаётся `candidate`.
- Дерево модулей строится из раскладки файлов, а не из `mod`; файлы,
  лежащие вне объявленных модулей, всё равно индексируются.
- Варианты enum индексируются как `Enum::Variant` (kind `variant`);
  `Trait::method(x)` и цепочки ре-экспортов (`pub use`) разрешаются.
- Макросы не раскрываются (как и у tree-sitter); тела `macro_rules!`
  не индексируются, чтобы не рождать несуществующие символы.
- Крейты workspace разрешаются друг через друга по имени пакета
  (каталога); внешние зависимости не индексируются.
- Файлы не в UTF-8 читаются lossy (символы заменяются), нечитаемые
  пропускаются с предупреждением; симлинкнутые каталоги не обходятся.

## Разработка

```bash
cargo test          # юнит-тесты на фикстурах + живая MCP-сессия
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Лицензия

Apache-2.0 (см. [LICENSE](LICENSE)).
