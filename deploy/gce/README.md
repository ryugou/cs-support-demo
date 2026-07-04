# GCE deployment

This repository is deployed into the existing `llm-memory-extention` stack on
the shared `llm-memory` GCE VM.

- public domain: `cs-support-136-110-78-245.nip.io`
- internal port: `8080`
- bind address: `BIND_ADDR=0.0.0.0:8080`
- secret source: existing Secret Manager injection performed by
  `~/llm-memory-extention/deploy/gce/run.sh`
- required env: `VEGAPUNK_BEARER_TOKEN`

Restart with the shared wrapper only:

```sh
~/llm-memory-extention/deploy/gce/run.sh up -d --build cs-support-mcp
```
