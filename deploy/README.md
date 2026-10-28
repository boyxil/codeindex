# Семантический слой (M5): отдельный YantrikDB

Принцип: **память агента и индекс кода не живут в одной базе**. Память
(`yantrikdb-fixed`, порт 7439) — belief-revised и общая для всех
клиентов; индекс кода обязан быть детерминированным. Поэтому здесь —
второй контейнер того же образа, со своим `data_dir`, своим портом и
выключенным обслуживанием.

Файлы:

- `docker-compose.yantrik-code.yml` — сервис `yantrikdb-code`, порт
  127.0.0.1:7442, `data-code/`, конфиг `yantrikdb-code.toml`.
- `yantrikdb-code.toml` — `[maintenance] enabled = false`,
  consolidation/decay раз в 10 лет; `strategy = "client_only"` —
  векторы присылает codeindex (Ollama `bge-m3`, 1024d), инстанс только
  хранит их.

## Первый запуск

```bash
# в каталоге деплоя (там, где лежат embedder/models):
docker compose -f docker-compose.yantrik-code.yml up -d

# токен для codeindex (печатается один раз — сохранить):
docker exec -u yantrikdb yantrikdb-code \
  yantrikdb token --data-dir /var/lib/yantrikdb create --db default --label codeindex
```

Токен кладётся в `codeindex.env` рядом (в репозиторий НЕ коммитится):

```sh
CODEINDEX_YANTRIK_URL=http://127.0.0.1:7442
CODEINDEX_YANTRIK_TOKEN=ydb_…
CODEINDEX_YANTRIK_NAMESPACE=codeindex
CODEINDEX_YANTRIK_DOMAIN=code
CODEINDEX_EMBED_URL=http://127.0.0.1:11434
CODEINDEX_EMBED_MODEL=bge-m3
```

## Использование

```bash
set -a; . ./codeindex.env; set +a
codeindex index . --semantic      # инкрементальный индекс + delta-синк
codeindex search . "query" --semantic
codeindex serve . --semantic      # MCP + семантика
```

Рабочая копия деплоя живёт вне репозитория:
`/media/veracrypt3/Docker/yantrikdb/` (там же `data-code/` и
`codeindex.env`).
