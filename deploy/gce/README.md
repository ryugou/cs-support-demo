# GCE deployment

**この構成は Cloud Run へ移行済みの旧構成（参考用）です。現行のデプロイ手順は
リポジトリルート `CLAUDE.md` を参照してください。**

This repository was deployed into the existing `llm-memory-extention` stack on
the shared `llm-memory` GCE VM.

- public domain: `cs-support-136-110-78-245.nip.io`
- internal port: `8080`
- bind address: `BIND_ADDR=0.0.0.0:8080`
- secret source: existing Secret Manager injection performed by
  `~/llm-memory-extention/deploy/gce/run.sh`
- required env: `VEGAPUNK_BEARER_TOKEN`

旧構成では、restart は共有 wrapper 経由でのみ行っていた:
`~/llm-memory-extention/deploy/gce/run.sh up -d --build cs-support-mcp`
