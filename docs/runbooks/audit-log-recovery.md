# 監査ログ（WormAuditLog）の復旧手順

| 項目    | 内容 |
| ----- | ---- |
| 目的    | `WormAuditLog` の起動時整合性検証（`verify_chain`）が失敗してサービスが起動しない状態から、安全に復旧する手順を定める |
| 読者    | 障害対応を行う運用者 |
| 正本の範囲 | 復旧作業の判断基準と手順。poisoned になったプロセスの振る舞い（ヘルス応答・自己終了）の正本ではない |
| 関連文書  | [`../superpowers/specs/2026-10-06-poisoned-audit-instance-design.md`](../superpowers/specs/2026-10-06-poisoned-audit-instance-design.md)（poisoned になったプロセスの振る舞いの正本、本手順が前提にする判断基準 §5.1 の出典）、`server/src/harness/audit.rs`（ハッシュ連鎖・`verify_chain` の実装） |

## 1. この手順が必要になる場面

`WormAuditLog::open` は起動時に既存ログ全行のハッシュ連鎖を検証する（`verify_chain`、fail closed）。検証に失敗すると `main.rs` / `homesec_advisor.rs` の起動処理が `Err` を返し、**プロセスが起動しない**。Cloud Run 上ではこれが再起動の無限ループとして観測される。

監査ログが poisoned になったプロセスが自分で終了する挙動（ヘルス応答 503、graceful shutdown 後の自己終了）そのものは、本手順の対象ではない。再起動すれば `verify_chain` が再実行され、ファイルが健全なら自動的に復旧する。**本手順が必要なのは、再起動後も `verify_chain` が失敗し続け、起動できない場合だけ**である。

## 2. 症状の見分け方

起動ログ（`main()` が `Err` を返して終了する際の標準エラー出力。Cloud Run では Cloud Logging の severity `ERROR` に載る）に、次のいずれかの文言が出る（`server/src/harness/audit.rs`）。

外側の context（どのファイルの検証に失敗したか）:

```
audit log {path} failed integrity check
```

内側のエラー（`verify_chain` が返す、失敗した行番号 `line_no` と理由）:

| 文言 | 意味 |
| --- | --- |
| `line {line_no}: read failed` | その行の読み取り自体に失敗した（I/O エラー） |
| `line {line_no}: empty line in append-only log` | その行が空（空白のみ） |
| `line {line_no}: not valid json` | その行が JSON として parse できない |
| `line {line_no}: not a json object` | JSON だが object ではない |
| `line {line_no}: missing prev_hash` | JSON object だが `prev_hash` キーが無い |
| `line {line_no}: missing hash` | JSON object だが `hash` キーが無い |
| `line {line_no}: hash chain is broken (prev_hash mismatch)` | `prev_hash` が直前行の `hash` と一致しない |
| `line {line_no}: hash mismatch (tampered or corrupted)` | 再計算した hash が記録された `hash` と一致しない（改竄・破損） |

### 2.1 対象行の特定（最初に必ず行う）

1. ログに出た `line_no` を控える。
2. 対象ファイルの末尾が改行で終わっているかと、`wc -l`（改行の数）の値を確認する（手順は §4 手順2）。
3. `line_no` が最終行かどうかを、末尾の状態に応じて次のとおり判定する。`verify_chain` は改行で終わらない最終断片も 1 行として数えるが、`wc -l` は改行しか数えないため、末尾の状態で対応が 1 つずれる。
   - **末尾が改行で終わっていない**: `line_no` = `wc -l` + 1 のときだけ最終行（不完全な断片そのもの）。
   - **末尾が改行で終わっている**: `line_no` = `wc -l` のときだけ最終行。ただしこの場合は、下の §2.2 のとおり復旧の対象にならない。
4. 上記に当てはまらない場合は、最終行より前の行で検証が失敗している。

### 2.2 判断基準（design doc §5.1 準拠）

- **最終行より前の行での失敗（上記 3 で不一致）**: 改竄または破損の疑いとして扱う。**ファイルに一切手を加えず**、§6「やってはいけないこと」に従って調査へ回す。本手順の §4（取り除き・書き戻し）は適用しない。
- **最終行での失敗、かつ `not valid json`、かつ末尾が改行で終わっていない**: 書き込みの途中で失敗した不完全な断片であり、ハッシュ連鎖に組み込まれていない。§4 の手順で取り除いてよい。
- **最終行での失敗、かつ末尾が改行で終わっている**: この失敗経路からは生じない。`append` は JSON の本文を書いてから改行を書くので、途中で失敗して残るのは「改行で終わらない非空の断片」だけである。改行で終わっているのに読めない行や、空行（`empty line in append-only log`）は、改竄または破損の疑いとして扱い、**取り除いてはいけない**。調査へ回す。
- **最終行での失敗、かつ `missing prev_hash` / `missing hash` / `hash chain is broken (prev_hash mismatch)` / `hash mismatch (tampered or corrupted)` / `not a json object`**: 行自体は JSON として読める（構造が壊れているのは連鎖側）。これは「不完全な行」ではなく改竄・破損の疑いであり、**最終行であっても取り除いてはいけない**。調査へ回す。

要するに、**取り除いてよいのは「改行で終わらない、JSON として読めない非空の最終行」だけ**である。

## 3. 作業前の確認

### 3.1 対象サービスが起動に失敗していることの確認

```sh
gcloud run services describe cs-support-mcp --project sivira-cs-support --region asia-northeast1 --format='value(status.conditions)'
```

`homesec-advisor` の場合は `cs-support-mcp` を `homesec-advisor` に置き換える。

起動時エラーのログは次で確認する（`<service-name>` は `cs-support-mcp` または `homesec-advisor`）。

```sh
gcloud logging read 'resource.type="cloud_run_revision" resource.labels.service_name="<service-name>" severity>=ERROR' --project sivira-cs-support --limit 50 --format='value(timestamp,textPayload)'
```

`failed integrity check` を含む行があれば、本手順の対象である。

### 3.2 監査ログの保存先の特定

コンテナ内パスは config から読み取れる（これは推測ではなく確定値）。

| サービス | config | コンテナ内パス |
| --- | --- | --- |
| `cs-support-mcp` | `server/config.cloudrun.toml` の `[harness] audit_log_path` | `/data/audit/audit.jsonl` |
| `homesec-advisor`（advisor 本体） | `server/config.homesec.toml` の `[harness] audit_log_path` | `/data/audit/audit.jsonl` |
| `homesec-advisor`（CS 連携用 support_harness） | `server/config.homesec.toml` の `[advisor] support_audit_log_path` | `/data/audit/audit-support.jsonl` |

`cs-support-mcp` と `homesec-advisor` は別 Cloud Run service（別コンテナ・別ファイルシステム）なので、コンテナ内パスの文字列が同じでも実体は衝突しない。

**GCS バケット名・マウント設定はリポジトリから読み取れない。** 次のコマンドで実際の値を調べる（`<service-name>` は上表のサービス名）。

```sh
gcloud run services describe <service-name> --project sivira-cs-support --region asia-northeast1 --format=yaml
```

出力の `spec.template.spec.volumes`（`type: cloud-storage` の項目にバケット名が出る）と、対応する `volumeMounts`（`mountPath: /data` を指す項目）を確認する。コンテナ内パス `/data/audit/audit.jsonl` は、マウントしたバケットの `audit/audit.jsonl` オブジェクトに対応する。

## 4. 手順

以下は §2.2 の判断基準で「最終行、かつ取り除いてよい」と確定した場合にのみ実行する。作業は空の作業用ディレクトリで行う（ダウンロードしたファイルと手元のスクリプトを混在させない）。`<bucket>` は §3.2 で確認した値。CS 連携用（`audit-support.jsonl`）を扱う場合はオブジェクト名を読み替える。

### 手順1: 世代番号の控えと取得

最初にオブジェクトの世代番号（generation）を控え、**その世代を指定して**ダウンロードする。これにより、以降の手順の基準となる世代が固定され、作業中の別プロセスによる更新を手順6で検出できる。

```sh
mkdir -p /tmp/audit-recovery
```

```sh
gcloud storage objects describe gs://<bucket>/audit/audit.jsonl --format='value(generation)'
```

出力された数値を `<generation>` として控える（以降の手順 3・6 でも同じ値を使う。取り直さない）。

```sh
gcloud storage cp 'gs://<bucket>/audit/audit.jsonl#<generation>' /tmp/audit-recovery/audit.jsonl
```

以降のコマンドは `/tmp/audit-recovery` 内で実行する（`cd /tmp/audit-recovery` としてから、以降の相対パスのコマンドをそのまま実行する）。

### 手順2: 末尾の状態、総行数、最終行の確認

先に、末尾が改行で終わっているかを確認する（終わっていない = 不完全な書き込みの直接証拠）。

```sh
tail -c 1 ./audit.jsonl | od -c
```

出力が `\n` なら改行で終わっている。**この場合はここで作業を止め、§2.2 の判断基準に従って調査へ回す（本手順を続けない）。** 途中で失敗した書き込みは改行で終わらないので、改行で終わっているのに検証が失敗するファイルは、この手順の対象ではない。

それ以外（最終バイトが `}` など）なら、最終行が途中で切れている。続けて総行数を確認する。

```sh
wc -l ./audit.jsonl
```

`wc -l` の値（以下 `<wc>`）を控え、`line_no` = `<wc>` + 1 であることを確認する（§2.1 の対応）。

合わない場合はここで作業を止め、§2.2 の判断基準に従って調査へ回す（本手順を続けない）。

取り除く対象（最終行の断片）を、手順7の記録用にバイト列のままファイルへ保存し、長さと 16 進表現を控える。画面表示や手でのコピーは制御文字や末尾の空白を落とすため、記録の正本はこのファイルと 16 進表現にする。

```sh
tail -n 1 ./audit.jsonl > ./removed-final-fragment.bin
wc -c ./removed-final-fragment.bin
xxd ./removed-final-fragment.bin
```

`wc -c` の値（取り除く長さ）と `xxd` の出力を控える。`./removed-final-fragment.bin` は手順7で Issue に添付する。

### 手順3: 元ファイルの保存（上書きしない）

取り除く前に、元ファイルを**別名**で**同じ保存先**（同じバケット）へ保存する。コピー元は手順1で控えた世代を指定する（保存する内容が、手順1で取得したものと同一であることを保証するため）。`<issue>` は本障害の GitHub Issue 番号、`<date>` は作業日（`YYYY-MM-DD`）、`<generation>` は手順1で控えた値。

退避先の名前に元オブジェクトの世代番号を含めるため、同じ日に同じ Issue で作業をやり直しても、世代が異なれば別の名前になる。さらに `--if-generation-match=0`（宛先オブジェクトが存在しないときだけ作成する）を付けるため、同名の退避オブジェクトが既にあればコピーは失敗し、既存の退避オブジェクトを上書きしない。コピーが失敗した場合は、既存の退避オブジェクトを削除・上書きせず、作業を止めて調査へ回す。

```sh
gcloud storage cp --if-generation-match=0 'gs://<bucket>/audit/audit.jsonl#<generation>' gs://<bucket>/audit/audit.jsonl.corrupt-<date>-issue<issue>-gen<generation>
```

### 手順4: 不完全な最終行の取り除き

取り除くのは**最終行だけ**である。`<wc>` は手順2で控えた値。最終行より前の完全な行（ハッシュ連鎖に組み込まれている行）は 1 行も削らない。

手順2で末尾が改行で終わっていないことを確認済みなので、`head -n <wc>` が、改行で終わる完全な行だけを残す（改行で終わらない断片は含まれない）。

```sh
head -n <wc> ./audit.jsonl > ./audit.jsonl.fixed
```

### 手順5: 取り除き結果の確認

確認は、取り除いた後に完全な行が残るかどうかで分かれる。

**完全な行が 1 行も残らない場合**（`line_no` が 1。最初の監査イベントの書き込み中に失敗し、不完全な断片だけが残っていた）: 取り除いた結果は空ファイルになる。空ファイルは起動時検証で「イベント 0 件の正しい連鎖」として受理されるので、この場合は次の 1 点だけを確認し、下の 1〜3 は行わない（最終行が無いため、2 と 3 は満たしようがない）。

```sh
wc -c ./audit.jsonl.fixed
```

出力が `0` であること。`0` でなければ書き戻さず、手順4をやり直すか調査へ回す。

**完全な行が 1 行以上残る場合**: 次の 3 点をすべて確認する。1 つでも満たさなければ書き戻さず、手順4をやり直すか調査へ回す。

1. `./audit.jsonl.fixed` の行数が `line_no - 1` であること。

```sh
wc -l ./audit.jsonl.fixed
```

2. 最終行が JSON として読めること（エラーなく終了すること。`jq` を使う）。

```sh
tail -n 1 ./audit.jsonl.fixed | jq -e . > /dev/null
```

3. 末尾が改行で終わっていること（出力が `\n`）。

```sh
tail -c 1 ./audit.jsonl.fixed | od -c
```

### 手順6: 書き戻し（同時更新を検出する）

**手順1で控えた世代番号**を `--if-generation-match` に指定して書き戻す。書き戻し直前に世代を取り直さない（取り直すと、手順1以降の別プロセスの更新が検出されず、上書きで失う）。手順1以降に別プロセスがこのオブジェクトを更新していた場合、この条件により書き戻しは失敗する。失敗した場合は手順1からやり直す。

```sh
gcloud storage cp ./audit.jsonl.fixed gs://<bucket>/audit/audit.jsonl --if-generation-match=<generation>
```

### 手順7: 取り除いた事実の記録

取り除いたバイト列（手順2で保存した `./removed-final-fragment.bin` を添付し、`xxd` の出力を本文に貼る）、取り除いた長さ（手順2の `wc -c` の値）、作業日時、作業者、判断の根拠（§2.2 のどの基準に該当したか）を、本障害の GitHub Issue に記録する。**監査ログへ自動で追記する仕組みは作らない**（手で Issue に残すことが正本の記録になる）。

### 手順8: 再起動と起動確認

Cloud Run は失敗したインスタンスを自動的に再試行する。`minScale` が 0 のサービスでは、次のリクエストでコールドスタートが走る。

```sh
curl -sS https://cs-support-mcp-235108918288.asia-northeast1.run.app/livez
```

`homesec-advisor` の場合は対応する service URL に置き換える。`200` が返り、§3.1 のログクエリで新しい `failed integrity check` エラーが出ていなければ復旧完了である。

## 5. 対象が複数あること

`homesec-advisor` は監査ログを 2 つ持つ（§3.2 の表）。それぞれ別オブジェクトであり、本手順は**それぞれ独立に**適用する（片方を復旧しても、もう片方が壊れていればプロセスは起動しない。両方について §2〜§4 を実行する）。

## 6. やってはいけないこと

- **完全な行の削除。** JSON として読め、連鎖の検証だけが失敗する行（§2.2 の「取り除いてはいけない」側）を削除しない。これは改竄・破損の隠蔽になる。
- **ファイルの作り直し。** ファイルを空にして新しいチェーンを始めない（`WormAuditLog::open` は既存ファイルが無ければ genesis から始めるが、これは過去の監査記録の抹消であり、復旧ではない）。
- **元ファイルを残さない作業。** 手順3（別名保存）を省略しない。
- **最終行かどうかを確認せずに取り除く。** §2.1 の行数確認を省略しない。
- **GCS バケット名・マウント設定を推測で使う。** §3.2 の `gcloud run services describe` で必ず確認する。
