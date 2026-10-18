# Hearing-first handling for every escalation rule

| Item | Content |
| --- | --- |
| Purpose | Make every layer-1 escalation rule follow one flow on `/{project_id}/api/reply`: hear, answer what the materials can answer, and escalate only what stays unanswered or unresolved, carrying what was heard (Issue #109). Reposition KnownResolution (KR) as material in the answer step (replaces the KR layer of Issue #103). |
| Audience | Implementers, reviewers |
| Scope | Per-rule hearing contracts and outcomes, the layer-2 precedence change, the per-case handling state machine, the answer step and its materials, the resolution check, safety notices, the handling audit, the handoff summary, KR selection, the `rules.json` / `jev-questions.json` / schema changes and their rollout |
| Related documents | `specs/production-cs-mcp.md` (conversation layer; updated by this change), `2026-09-21-jev-shadow-design.md` (Jev client; hearing contract `product_and_symptom`), `2026-10-07-partial-answer-with-handoff-design.md` (partial answer; scope narrowed by §7.3), `2026-10-14-answer-after-escalation-design.md` (already-escalated cases; unchanged), `2026-10-15-knowledge-first-answering-design.md` (KR as primary material; §4.2 kept, §3.2/§3.3 replaced by §8) |

## 1. Principle and scope

A layer-1 rule match no longer escalates by itself. The case enters a **handling** for that rule:

1. **Hearing**: ask until the rule's hearing items are known (§4.3).
2. **Answer**: answer from the materials and ask whether it solved the problem (§5). Rules whose outcome is `handoff` skip this step.
3. **Resolution check**: on the next turn, close the handling if solved, answer a follow-up, or escalate (§6).

Escalation happens only when the materials cannot answer, the rule's outcome is `handoff`, the remedy did not solve the problem, the answer limit `max_answer_rounds` (§3) is reached, or a check in §4–§6 fails. Reaching the hearing limit `clarify_max_turns` does not escalate by itself: the handling continues with what was heard (§4.3). Every escalation carries a handoff summary (§7.1).

Unchanged:

- Layer 2 (prohibited domains `legal-privacy`, `security-guarantee`, `electrical-work`) escalates immediately. Only its precedence and the trigger of `electrical-work` change (§2.3).
- Layer 3 (no rule matched, manual insufficient) keeps the existing `missing`-based clarify.
- Cases already escalated keep `2026-10-14-answer-after-escalation-design.md`; no handling starts in them.
- `LexiconFallback` keeps its fail-closed escalation.
- Time-preference handling, the receipt number (受付番号), and the deterministic escalation block.
- MCP `evaluate_answerability`: no handling. Only what MCP shares with `evaluate` changes: the layer-2 precedence (§2.3), the KR input (§8), the deterministic rule order (§9.1), and `Escalate.hearing` becoming `None` for non-legacy rules (§4.2), which lifts the `HearingDeclared` partial-answer exclusion for `warranty-failure` wherever `evaluate` computes the partial answer.
- homesec advisor mode (`advisor/api.rs` has its own handler and reads schema `homesec`, which holds no escalation rules or KR records).
- homesec support mode (`advisor/cs_support.rs::run_support_turn`) calls `evaluate` on `support_schema = "urtect"` (`config.homesec.toml`), so it reads the urtect rules and KRs. It gets no handling: it passes the current message as `context_text`, runs the KR selection of §8 without the skip of §4.2 (it has no Jev client), passes `handling_flow: false` (§4.2), and keeps `decide_reply_action` and `partial_answer_enabled: false`. A non-legacy rule carries `Escalate.hearing = None` there, which changes nothing because the mode never calls Jev.

## 2. Rules

### 2.1 Per-rule definitions

| rule_id | `hearing` | `hearing_items_ja` | `after_hearing` | `safety_notice` |
| --- | --- | --- | --- | --- |
| security-incident | `incident_situation` | 「何が起きたか（侵入・不審な通知など）」「どのカメラ・場所か」「今も続いているか」 | `answer_or_handoff` | 「身の危険を感じる場合は、ただちに110番へ通報してください。」 |
| physical-damage | `damage_situation` | 「対象の製品」「状態（煙・異臭・発熱・破損など）」「今も続いているか」 | `answer_or_handoff` | 「煙・異臭・異常な発熱がある場合は、ただちに電源アダプタをコンセントから抜き、製品に触れずに離れてください。」 |
| warranty-failure | `product_and_symptom` | 「対象の製品（型番）」「具体的な症状（いつから・どんな状態か）」 | `answer_or_handoff` | – |
| contract-billing | `billing_matter` | 「お問い合わせの対象（請求額・支払い方法・プラン変更・解約など）」 | `answer_or_handoff` | – |
| construction-risk | `construction_plan` | 「設置しようとしている場所と取付方法」「対象の製品」 | `answer_or_handoff` | – |
| human-handoff | `handoff_matter` | 「ご相談になりたい内容」 | `answer_or_handoff` | – |
| initial-cost-quote | `site_details` | 「設置場所の種類（戸建て・店舗など）」「設置したい台数」 | `handoff` | – |

- `after_hearing = answer_or_handoff`: go to the answer step (§5). `handoff`: escalate right after hearing (§7), with the handled rule's `customer_ack` (§7.2).
- `hearing_items_ja` are the customer-facing labels of the items that must be known. They are passed to `clarify::draft_clarify_question` joined with 「、」 (§4.3) and, for non-legacy rules (§9.2), take the place of `JEV_HEARING_MISSING_TEXT` (`api.rs`). Legacy rules keep `JEV_HEARING_MISSING_TEXT` and the existing Jev hearing (§4.2 step 7); removing them after the legacy period is a separate Issue.
- `safety_notice` is prepended by §4.4. It is fixed text, never generated by the LLM.
- `binding` keeps one meaning: when several layer-1 rules match, a `mandatory` rule ranks above an `advisory` rule; within the same binding the rule with the smaller `order` (§9.1) ranks higher. The rank decides the winner of `decide` and the replacement of §4.2.

### 2.2 Jev questions

Each contract maps to one `noul` question in `server/data/urtect/jev-questions.json`. `product_and_symptom` keeps the existing key `has_enough_info`. New keys:

| Key | Type | instructions | true / criteria | false |
| --- | --- | --- | --- | --- |
| `hearing_incident_situation` | noul | 防犯上の出来事について、担当者が状況を把握できる情報が揃っているか | 何が起きたか、どのカメラ・場所か、今も続いているかがすべて分かる | いずれかが分からない |
| `hearing_damage_situation` | noul | 製品の破損・異常について、担当者が状況を把握できる情報が揃っているか | どの製品か、どんな状態か（煙・異臭・発熱・破損）、今も続いているかがすべて分かる | いずれかが分からない |
| `hearing_billing_matter` | noul | 契約・請求について、何の問い合わせかが分かるか | 問い合わせの対象（請求額・支払い方法・プラン変更・解約など）が分かる | 対象が分からない |
| `hearing_construction_plan` | noul | 設置・工事について、何をしようとしているかが分かるか | 設置場所と取付方法、対象の製品が分かる | いずれかが分からない |
| `hearing_handoff_matter` | noul | 担当者に何を相談したいかが分かるか | 相談したい内容が具体的に分かる | 内容が分からない |
| `hearing_site_details` | noul | 初期費用の見積もりに必要な情報が揃っているか | 設置場所の種類（戸建て・店舗など）と設置したい台数の両方が分かる | どちらかが分からない |
| `resolution_feedback` | choice | 顧客の最新の発話は、それまでの相談に対してどの反応か | `resolved`「問題が解決した・できたと述べている」, `unresolved`「解決しない・できない、または担当者と話したいと述べている」, `follow_up`「それまでの相談と同じ話題について質問・補足している」, `new_topic`「それまでの相談と関係のない別の話題を述べている」 | – |

`JevClient::evaluate` gains a `question_keys: &[&str]` parameter and sends only those questions (today it sends every question on every call). Each call in this document sends exactly one key. A key absent from the questions file makes `JevClient::evaluate` return an error naming the key, which the caller treats as a Jev failure; the startup check of §9.4 keeps every key used here present.

Jev receives customer utterances only (§4.1). The `resolution_feedback` criteria are therefore worded so that the latest utterance can be classified against the earlier customer utterances alone, without the answer that was given.

### 2.3 Layer-2 precedence

A prohibited domain must win over every layer-1 rule, so that no hearing starts on a prohibited matter. Two changes:

- `decide` evaluates `match_layer2` before `match_layer1_mandatory` (today the order is the reverse).
- `electrical-work` becomes pattern-only: `domain_signals: []`, `pattern` 「電気工事」「高所作業」 kept. Today its only signal `physical_construction_risk` is also the condition of `construction-risk`; with layer 2 first, every construction question would escalate immediately and `construction-risk` would never hear.

Effects:

- An `electrical-work` escalation has no handoff item (`handoff_items_for_decision` reads `domain_signals`), so it never gets the partial answer of §7.3; the customer receives the escalation reply with the ack only. Accepted: the matter is a prohibited domain, and the other layer-2 domains keep their partial answer.
- A construction question containing 「電気工事」 or 「高所作業」 escalates immediately as layer 2 (MCP and `/api/reply`). Other construction questions (「自分で設置していい？」, 「配線工事」) get the `construction-risk` hearing.
- MCP audit labels change for those questions from `construction-risk` (layer 1) to `electrical-work` (layer 2).
- `classify_stakes` no longer reaches High through `mandatory_domain_near` for construction questions without those patterns; they are Mid (threshold 0.8) through the hazard signal.
- `domain_signals` is already `#[serde(default)]`; an empty list is accepted by `ingest_rules` and the loader. A domain with neither `domain_signals` nor `pattern` could never match: the loader already rejects it, and `ingest_rules` gains the same rejection before writing anything (§9.1).
- In `data/rules.sample.json` (legacy project `sivira-cs-demo`), the mandatory rules `health-food-abnormal-ingestion` (`post_ingestion_symptom`) and `cosmetic-skin-reaction-continue` (`skin_irritation` + `continue_use_question`) are always taken by the domains `health-judgement` and `body-symptom`, whose `domain_signals` contain those signals. They still escalate immediately; only the reported id and the audit label change from the rule to the domain. PR 3 removes both rules from the file (§9.1).

## 3. Handling state (`CaseConvState`)

New attributes on the `support_case` node, read and written through the existing `conv_state_from_attrs` / `merge_conv_state_attributes`:

| Attribute | Type | Meaning |
| --- | --- | --- |
| `handling_rule_id` | `Option<String>` | the handled rule; `None` = no handling |
| `handling_contract` | `Option<HearingContract>` | the handled rule's contract, stored because the pre-check (§4.2) runs before `evaluate` loads the rules; updated when the loaded rule declares another contract (§4.2). Every other attribute of the handled rule (`binding`, `order`, `hearing_items_ja`, `after_hearing`, `safety_notice`, `customer_ack`) is read from the rules `evaluate` loaded in the turn, so a re-ingest of `rules.json` takes effect from the next turn |
| `handling_condition` | `String` (CSV of signals) | the handled rule's `condition`, stored for the same reason as `handling_contract`: the `new_topic` end (§4.2) adds its exclusions before `evaluate` loads the rules. Set at Start and Replacement; updated by the handled rule lookup (§4.2) when the loaded rule's `condition` differs |
| `handling_phase` | `Option<HandlingPhase>` | `Hearing` or `AwaitingResolution`, stored as `hearing` / `awaiting_resolution` |
| `answer_rounds` | `u32` (int) | answers given in this handling |
| `handling_transcript` | `String` | customer utterances of this handling, oldest first, separated by newlines; over 2,000 chars, whole utterances are dropped from the oldest side |
| `handling_answers` | `String` | the answer drafts given in this handling, oldest first, separated by a blank line; over 2,000 chars, whole drafts are dropped from the oldest side |
| `safety_notices_given` | `String` (CSV of rule_ids) | rules whose safety notice was already sent in this case; never cleared |
| `handoff_summary` | `Option<String>` | §7.1 |

`clarify_turns` (existing) counts hearing questions in the handling.

Limits (config `[api]`): `clarify_max_turns` (existing, 3) for hearing questions; `max_answer_rounds` (new, default 3) for answers.

Lifecycle:

- **Start** (§4.2 step 6): `handling_rule_id`, `handling_contract`, `handling_condition` set; phase `Hearing`; `clarify_turns = 0`; `answer_rounds = 0`; `handling_transcript` = the customer utterances of the request history (`eval_history`) followed by the message, selected as `build_jev_state` does today (at most `MAX_HISTORY_TURNS` utterances, 2,000 chars, oldest dropped whole, each utterance on one line), so the facts of earlier turns reach the first hearing check; the message alone when this turn ended another handling (`new_topic` or the handled rule lookup), so the ended topic is not carried over. On a product-switch turn `eval_history` is empty (§4.2), so the transcript is the message. The 2,000-char cap applies to the history part only, as in `build_jev_state` today; the current message is never trimmed, so a seeded transcript can exceed 2,000 chars and the Jev input of a Start equals the `build_jev_state` output; the cap applies as stated from the next append. Known limit: earlier unrelated utterances within the window (for example a topic closed by `resolved` a few turns earlier) are included, as they are today. `handling_answers` empty.
- **Replacement** (§4.2 step 3): rule, contract and condition replaced; phase `Hearing`; `clarify_turns = 0`; `answer_rounds = 0`; transcript and answers kept.
- **End**: resolved, `new_topic`, escalation, or a handled rule that is missing or legacy (§4.2 handled rule lookup). Clears `handling_rule_id`, `handling_contract`, `handling_condition`, `handling_phase`, `handling_transcript`, `handling_answers`, and sets `answer_rounds = 0` and `clarify_turns = 0`, so a later layer-3 clarify or handling starts with its full allowance. On `resolved` and `new_topic`, every signal of `handling_condition` is added to `excluded_signals` (§6.1).
- **Demotion** to out-of-scope (§4.2 step 0): the demotion write clears the same attributes and resets the same counters; it adds no exclusion beyond the signals the demotion already records, and writes no summary.
- A product switch does not pass through End: it starts a new case (§4.2), and the old case keeps its attributes.

Invariants:

- `handling_rule_id`, `handling_contract`, `handling_condition`, `handling_phase` are all set or all unset (an empty `handling_condition` is an invariant break, read as no handling).
- A handling never exists in an already-escalated case (`is_already_escalated()`).

Reading a stored state that breaks an invariant, or that holds an unknown `handling_phase` or `handling_contract` value: `conv_state_from_attrs` reads it as no handling (all handling attributes unset, counters as stored) and logs a warn naming `case_id` and the offending attribute; the next save of the case writes the cleared attributes. Today `conv_state_from_attrs` falls back silently on a parse failure; the new attributes get this warn.

Writing: the handling attributes are added to `merge_conv_state_attributes` and written by the existing `save_conv_state` upsert. `CaseConvState` gains a field `signals_to_exclude: Vec<Signal>` (not stored as its own attribute); `merge_conv_state_attributes` unions it into `excluded_signals` with the CSV union that `merge_out_of_scope_demotion_attributes` uses today (extracted into a shared function), so the End and its exclusion are one upsert. A failed upsert returns the existing `conv_state_save_failed` response, as every `save_conv_state` call in `reply_handler` does today. The pre-`evaluate` End on `new_topic` (§4.2) uses the same write, so `evaluate` of that turn already reads the new exclusion.

Concurrency: last writer wins, as for the existing conv state (the LINE adapter runs with `--max-instances=1`).

## 4. Turn processing on `/api/reply`

### 4.1 Inputs per process

Two texts exist in a turn with a handling:

- **current message**: the customer's message of this turn.
- **context text**: `handling_transcript` + newline + current message (2,000-char cap as in §3); equal to the current message without a handling. At a Start the context text is the seeded transcript (§3); `evaluate` of that turn has already used the current message for manual search and the KR check, because the handling did not exist yet.

| Process | Text |
| --- | --- |
| manual search | context text |
| KR applicability check (§8) | context text |
| answer-step draft user message (§5) | context text, plus the request history block (customer and assistant turns) as in `build_reply_user_message` today; the customer turns of the history block repeat in the context text, which is accepted so that the history block code path stays unchanged |
| Jev state (§4.3, §6) | context text, customer utterances only, in the format of `build_jev_state`; no assistant text |
| hearing question (§4.3) | context text and the contract's `hearing_items_ja` |
| handoff summary input (§7.1) | context text |
| signal extraction, `ng_near_hit`, layer-2 patterns, related cases, the case `question` attribute, audit question text | current message |

Signal extraction stays on the current message because accumulated signals already carry earlier turns, and `supplied_extraction` and the product switch work per message. `evaluate` gains a new input `context_text: &str`, used for manual search and the KR applicability check; its existing `question` keeps the processes of the last row. The conversation layer passes the context text to the other rows (answer step, Jev, hearing question, handoff summary).

### 4.2 Turn order

Existing steps before `evaluate` (allowlist gate, product switch, time preference) are unchanged. A product switch starts a new case in its initial state (no handling); the old case is not modified.

**Before `evaluate`**, the handling is read from the conv state that `already_escalated_before_evaluate` already loads for `eval_case_id`. On a product-switch turn `eval_case_id` is `None`, so the turn has no handling and no pre-check. When the case has a handling:

- `AwaitingResolution`: run the resolution check (§6) on the context text. On `new_topic`, persist the handling end (§3, including the exclusions) before `evaluate`, then process the turn as a turn without a handling. The exclusions are the signals of the stored `handling_condition`; when the rule was deleted or changed since the last turn, the stored condition is used as is (an exclusion of a signal no rule needs is harmless: `prune_excluded_signals` lifts it when the signal is observed again).
- `Hearing`: run the hearing check (§4.3) for the handled contract on the context text (the **pre-check**). When the pre-check result leads to Clarify (§4.3: incomplete below the limit, or a Jev failure with `clarify_turns == 0`), `evaluate` skips the KR applicability call, because that turn usually ends in a Clarify; every turn that still reaches the answer step runs the KR selection there (§5).

`evaluate` runs once per turn. `EvaluationOutcome` gains:

- `matched_rule_id: Option<String>`: the rule that won layer 1 in `decide`. `Escalate` gains a field `#[serde(skip)] rule_id: Option<String>`, set by `layer1_escalate` to the winning rule's id whatever the number of matched rules, so it stays out of the decision payload; `matched_rule_id` is copied from it (`None` for every other decision).
- `answer_threshold: f32`: the stakes threshold of the turn.
- `applicable_known_resolution: Option<KnownResolution>` (§8).
- `escalation_rules: Vec<EscalationRule>`: the rules `evaluate` loaded in this turn, in the order of §9.1. `/api/reply` reads every rule attribute it needs from this list: the winner by `matched_rule_id`, the handled rule by `handling_rule_id`.

`evaluate`'s own reply draft: `ReplyDraftPolicy::SkipWhenUnused` gains a field `handling_flow: bool`, true only from `/api/reply`; `advisor/cs_support.rs` and `harness/real_llm_smoke.rs` pass false, and MCP passes `ReplyDraftPolicy::Always`, which has no such field. `should_draft_reply` gains an argument `handling_owns_reply: bool`, which `evaluate` sets only when the policy is `SkipWhenUnused { handling_flow: true, .. }`, the decision is a layer-1 escalation whose `rule_id` names a non-legacy rule (§9.2), and the case is not already escalated. `SkipWhenUnused` already skips an escalation draft unless the partial answer applies; `handling_owns_reply` also suppresses that partial-answer draft, so `should_draft_reply` returns false, because the handling owns that reply; the answer step (§5) is a separate call. Steps 3–6 below never use a draft from `evaluate`. `decide` fills `Escalate.hearing` only for legacy rules (§9.2), through the existing advisory branch; for non-legacy rules it is `None`. This keeps `2026-10-14-answer-after-escalation-design.md` §3 intact: in an already-escalated case a layer-1 match carries no `hearing`, so the `HearingDeclared` exclusion does not fire and the partial answer applies as §3 describes. For `warranty-failure`, which today carries its contract there and so gets no partial answer, this newly allows one, as for the other rules.

**Handled rule lookup** (after `evaluate`, when the case has a handling and step 0 does not apply): the handled rule is the entry of `escalation_rules` whose `rule_id` is `handling_rule_id`.

- Absent, or legacy (§9.2): the handling ends (§3) without exclusions and without a summary; a warn names `case_id` and the rule id; the pre-check or resolution-check result is discarded; the turn continues as a turn without a handling.
- Its `condition` differs from `handling_condition`: the stored condition is updated.
- Its `handling_contract()` differs from the stored `handling_contract`: the stored contract is updated. In `Hearing`, when step 3 below does not apply, the pre-check result is discarded and a post-check for the new contract replaces it (when step 3 applies, its post-check for the new rule is the only one); counters and transcript are kept.

**After `evaluate`**, the first matching step applies:

0. Second-stage out-of-scope short circuit (`second_stage_short_circuit`, `api.rs`; it returns before `load_conv_state` today) → existing out-of-scope reply and `demote_case_to_out_of_scope`. When the case has a handling, the demotion write also clears it (§3 Demotion); the message is not appended, no summary is written, and the turn's pre-check or resolution-check result is discarded. A handling end already persisted on `new_topic` stays. A failed demotion write keeps today's behavior (error log, out-of-scope reply still sent), so the handling stays until a later end.
1. Already escalated, or `LexiconFallback` → existing behavior. A `LexiconFallback` escalation in a handling ends it and writes the summary (§7).
2. Layer-2 escalation → existing immediate escalation; ends any handling and writes the summary.
3. Handling exists, `matched_rule_id` names a non-legacy rule (§9.2), and it ranks higher than the handled rule (§2.1) → replacement (§3), then run the hearing check for the new contract on the context text (**post-check**) and apply step 5 with it. A replacement in `AwaitingResolution` discards the turn's resolution-check result.
4. Handling in `AwaitingResolution` → apply the resolution-check result (§6).
5. Handling in `Hearing` → apply the hearing result (§4.3): the pre-check result, or the post-check result when coming from step 3 or 6. Jev is not called again.
6. No handling and a layer-1 escalation whose `matched_rule_id` names a non-legacy rule → start a handling for `matched_rule_id` (§3), run the post-check on the context text (the transcript set at Start), and apply step 5's hearing result.
7. No handling and a layer-1 escalation of a legacy rule → existing behavior: `resolve_jev_has_enough_info` and `decide_jev_hearing_action` with `JEV_HEARING_MISSING_TEXT` for an advisory rule declaring `product_and_symptom`, otherwise `decide_reply_action`; its audit stays `record_jev_hearing_decision`.
8. Otherwise → existing behavior (Allowed answer, or layer-3 clarify).

Steps 4 and 5 apply whatever `decide` returned (Allowed, layer 1 including a legacy winner, or layer 3); only steps 1–3 override a running handling. Jev is called at most twice in a turn: the pre-check or resolution check, plus at most one post-check (start, replacement, or contract update; a contract update is not checked when a replacement follows).

When the KR applicability call was skipped, the turn runs without an applicable KR except in the answer step (§5). When the handled rule lookup ends the handling in such a turn, steps 7 and 8 use the result of `decide` without a KR as is: a legacy winner carries no `known_resolution_id`, a question a KR alone would answer is not answered from that KR, and an Allowed answer is drafted from the manual alone. This is accepted because it happens only when the rules were re-ingested during the case.

**Transcript**: whenever steps 3–6 apply, the stored `handling_transcript` becomes the turn's context text, so the current message is appended exactly once (at Start the result is the seeded transcript of §3). When the turn ends the handling, the context text is taken as the summary input (§7.1) before the attributes are cleared.

### 4.3 Hearing check

Query Jev with the contract's question (§2.2) on the context text. Complete when `noul ≥ [jev] enough_info_threshold` (0.5).

| Result | Action |
| --- | --- |
| complete | `after_hearing`: answer step (§5) or escalation (§7) |
| incomplete, `clarify_turns < clarify_max_turns` | Clarify (arguments below). Which items are unknown is left to that LLM call; Jev returns only complete or incomplete. `clarify_turns += 1` |
| incomplete, limit reached | as complete: `answer_or_handoff` goes to the answer step with what was heard, which escalates when the materials are insufficient (§5) |
| Jev disabled (`[jev] enabled = false`), error, timeout, missing key | `clarify_turns == 0` → Clarify once; otherwise as complete |

Clarify calls `clarify::draft_clarify_question` with:

- `question`: the context text;
- `missing`: the labels of the contract's `hearing_items_ja` joined with 「、」;
- `known_facts`: `build_known_facts(&lexicon, &outcome.accumulated_signals, eval_history)`, as today;
- `is_continuation`: as today.

The final-turn wording is appended after the egress gate by `clarify::append_final_turn_suffix(reply_text, is_final_clarify_turn(&conv, &config.api))`, as today.

The handling Clarify passes the clarify response gate (`gate_generated_text`, route `clarify`, fallback `FALLBACK_CLARIFY_TEXT`) before the final-turn suffix, as the layer-3 clarify does today.

`draft_clarify_question` gains an argument `missing_kind: MissingKind` (`Missing` | `HearingItems`). `build_clarify_prompt` puts `missing` in `<不足している情報>` for `Missing` (layer 3 and the legacy path, unchanged) and in a new tag `<確認したい項目>` for `HearingItems`; for `HearingItems` its system prompt gains the rule 「<確認したい項目> のうち、お客様の発話で既に述べられている項目は尋ねず、まだ分からない項目だけを尋ねてください。」.

For non-legacy rules this takes the place of the existing Jev hearing (`resolve_jev_has_enough_info`, `decide_jev_hearing_action`) and its fail-back to `decide_reply_action`. Legacy rules keep them (§4.2 step 7, §9.2).

### 4.4 Safety notices

On every reply of `/api/reply` (Clarify, answer, resolved reply, escalation), before sending:

1. From `escalation_rules`, collect the rules whose `condition` is non-empty and a subset of the signals `decide` used this turn (the layer-1 match of `rules.rs`: every signal of the condition present) and whose `safety_notice` is set, in `order` (§9.1).
2. Drop the rules already in `safety_notices_given`.
3. Prepend their notices, each followed by a blank line, and add their ids to `safety_notices_given`.

So a notice is sent once per case, on the first reply after its rule's signal appears, whatever phase the case is in: (a) at a handling start, (b) at a replacement, (c) when the signal appears while another rule is handled or after escalation.

Reply paths:

| Path | Safety notices |
| --- | --- |
| allowlist gate, time preference (reply returned before `evaluate`; no rules loaded) | none |
| step 0 out-of-scope reply (the turn's signals are discarded by the demotion) | none |
| steps 1–8 of §4.2 (already-escalated replies, `LexiconFallback`, layer 2, handling replies, legacy path, Allowed answer, layer-3 clarify) | yes |

A product-switch turn is not a reply path of its own: it continues into `evaluate` on the new case (§4.2) and takes the notices of the step it reaches; the new case starts with an empty `safety_notices_given`.

Persistence: when a reply adds ids to `safety_notices_given`, they are written with the conv-state save of that turn. On a path that makes no conv-state save today (for example an Allowed answer), a `save_conv_state` call is added for that turn, made only when the notice list changed. A failed save returns `conv_state_save_failed`, as every `save_conv_state` call in `reply_handler` does, and the notice is not sent.

### 4.5 Audit

Each handling decision of a turn (one per label in the table below) writes one audit event through a new `Harness::record_handling_decision`: a turn that ends one handling and starts another (an end on `new_topic` or by the handled rule lookup, followed by step 6) writes two events, `handling:end` first and then the label of the new handling; a replacement (step 3) is one event of the new handling with `handling_replaced_rule_id`. These events are written in addition to the event `evaluate` writes, in the way `record_jev_hearing_decision` does today. A failed write logs a warn with `request_id`, `case_id` and the error, and the reply continues (the `evaluate` event of the turn exists already).

| `decision` label | When |
| --- | --- |
| `handling:clarify` | §4.3 Clarify |
| `handling:answer` | §5 draft sent |
| `handling:resolved` | §6 `resolved` |
| `handling:escalate` | any escalation that ends a handling (§4.2 steps 1–2, §4.3 `handoff`, §5, §6) |
| `handling:end` | an end without escalation other than `resolved`: `new_topic`, handled rule lookup (§4.2), step 0 demotion |

`AuditDraft` gains additive fields, `None` on every other event, so `verify_chain` keeps verifying older events:

| Field | Content |
| --- | --- |
| `handling_rule_id: Option<String>` | the handled rule (the new rule after a replacement) |
| `handling_replaced_rule_id: Option<String>` | the replaced rule, on a replacement turn |
| `jev_contract: Option<String>` | the contract of the pre-check or post-check whose result was applied |
| `jev_has_enough_info: Option<f64>` (existing) | that check's `noul` value; `None` when Jev failed |
| `jev_resolution_choice: Option<String>` | the `resolution_feedback` choice, also when below `RESOLUTION_MIN_CONFIDENCE` |
| `jev_confidence: Option<f64>` | its confidence |
| `handling_reason: Option<String>` | for `handling:escalate`: `after_hearing_handoff`, `answer_insufficient` (`AnswerStepFailure::Insufficient`), `answer_failed:no_drafter`, `answer_failed:llm_error`, `answer_failed:egress_rejected`, `answer_failed:truncated`, `answer_failed:no_answer`, `answer_failed:out_of_scope_product` (the other variants; the error text of `LlmError` goes to the warn log, never to the audit event), `unresolved`, `answer_rounds_exhausted`, `resolution_check_failed`, `layer2`, `lexicon_fallback`; for `handling:end`: `new_topic`, `rule_missing`, `rule_legacy`, `demoted` |

The context text, drafts and the handoff summary are never written to audit events; the question text stays the current message (§4.1).

## 5. Answer step

The answer step is a Harness call made by the conversation layer after the hearing or resolution decision, not part of `evaluate`'s own draft:

```rust
pub async fn draft_handling_answer(
    &self,
    ctx: &RequestContext,
    materials: AnswerMaterials,   // KnownResolution(KnownResolution) | Manual(Vec<SectionHit>)
    context_text: &str,
    history: &[ReplyHistoryTurn],
    answer_threshold: f32,
    best_manual_score: Option<f32>,
) -> AnswerStepOutcome

pub enum AnswerStepOutcome {
    Drafted { draft: String },
    Failed { reason: AnswerStepFailure },
}

pub enum AnswerStepFailure { Insufficient, NoDrafter, LlmError(String), EgressRejected, Truncated, NoAnswer, OutOfScopeProduct }
```

A truncated draft is `Failed { reason: Truncated }`, never `Drafted`. The rules below define each outcome:

- Materials: the applicable KR alone when one exists (per `2026-10-15-knowledge-first-answering-design.md` §4.2: KR-only material); otherwise the turn's manual hits (from the context text).
- Sufficient when an applicable KR exists, or `evidence_sufficient(answer_threshold, best_manual_score)` holds.
- Sufficient → draft with `ReplyKind::Answer` (its prompt ends with the existing resolution question), passing the context text as the user message and the request history (`eval_history`) as the existing history block of `build_reply_user_message` (customer and assistant turns, its budget unchanged), so a `follow_up` answer can refer to the instructions already given. On a product-switch turn the history is empty (§4.2). The KR remains material the LLM composes from (steps, order, numbers and cautions preserved); no fidelity gate.
- On success: append the draft to `handling_answers`; `answer_rounds += 1`; phase `AwaitingResolution`.
- Insufficient, no drafter, LLM error, egress-gate rejection, a truncated draft, or a draft that is exactly `reply::NO_ANSWER_TOKEN` (rejected by `handoff_items::accept_draft` today) → escalate (§7).
- Product scope: the conversation layer passes a `Drafted` draft through `gate_customer_reply_draft` (`api.rs`, the answer-draft response gate of Issue #28 §3.5) before using it. A discarded draft is handled as `Failed { reason: OutOfScopeProduct }`: escalate (§7); the draft is not appended to `handling_answers` and `answer_rounds` is not incremented.
- When `evaluate` skipped the KR applicability call and the answer step runs in the same turn, the answer step runs the KR selection (§8) itself first. This covers every path: a replacement (step 3), a contract update (handled rule lookup), and a handling started at step 6 after the handled rule lookup ended the previous one, each with a complete post-check.

## 6. Resolution check

Query Jev `resolution_feedback` on the context text. The `choice` is adopted when it is one of the four values and its `confidence ≥ 0.5` (code constant `RESOLUTION_MIN_CONFIDENCE`); anything else is a failure.

| Value | Behavior |
| --- | --- |
| `resolved` | Reply 「解決してよかったです。ほかにも気になる点があれば、いつでもお送りください。」 (fixed); end the handling (§6.1); no escalation |
| `unresolved` | Escalate (§7) |
| `follow_up` | The transcript is extended (§4.2); when `answer_rounds < max_answer_rounds` run the answer step (§5) again, otherwise escalate |
| `new_topic` | Handled before `evaluate` (§4.2): end the handling (§6.1) and process the message as a turn without a handling |
| Jev disabled, error, timeout, failure | Escalate (§7) |

Configs with `[jev] enabled = false` (local and non-production) therefore always escalate at the resolution check.

### 6.1 Ending without escalation

On `resolved` and `new_topic`, every signal of the stored `handling_condition` is added to `excluded_signals`, so the accumulated signal does not start the same handling on the next turn. `prune_excluded_signals` already removes an exclusion when the signal is observed again in a later message, so a renewed mention starts a new handling. Accumulated signals of other rules are not excluded; such a rule starts its own handling on the next turn where it wins layer 1.

## 7. Escalation

### 7.1 Handoff summary

`handoff_summary` is written at the escalation that makes the case escalated (handling or not) and never overwritten afterwards. It is shown in the admin UI: `handoff_summary: Option<String>` is added to `ThreadDetail` (`server/src/admin.rs`, `admin-ui/src/app/core/models.ts`) and displayed in the thread view.

- Format (fixed headings): 「用件」「聞き取った内容」「案内した対処」「結果」.
- Input: the context text (without a handling: the customer utterances of the request history plus the current message, capped as in `build_jev_state`), the rule id, and `handling_answers`.
- Generated by the LLM synchronously before the conv-state write, with a timeout constant `HANDOFF_SUMMARY_TIMEOUT` (10 s).
- LLM failure or timeout → the summary is the raw input text under 「聞き取った内容」, and a warn is logged with `case_id` and the error. The escalation is never blocked by the summary.
- Never sent to the customer; never written to audit events.

### 7.2 Customer reply

The existing escalation reply (`build_escalation_reply_text`) is used, with the safety notices of §4.4. An escalation out of a handling (§4.3, §5, §6) uses the handled rule's `customer_ack` from `escalation_rules`, whatever the number of matched rules (today `layer1_escalate` sets it only when exactly one layer-1 rule matches). Every other escalation keeps `escalation_customer_ack(&outcome.decision)` as today.

### 7.3 Partial answer

The partial answer of `2026-10-07-partial-answer-with-handoff-design.md` §3.1 (layers 1 and 2 only; its `HearingDeclared` exclusion is kept) applies to:

- layer-2 escalations whose domain has `domain_signals` (not `electrical-work`, §2.3);
- layer-1 escalations of legacy rules (§9.2), as today;
- layer-1 escalations in already-escalated cases (`2026-10-14-answer-after-escalation-design.md` §3), as today.

An escalation out of a handling (§4.3, §5, §6) carries no partial answer: `evaluate` made no draft for that turn (§4.2), and the answer step has already given what the materials can answer or found them insufficient. `initial-cost-quote`, a mandatory rule that today can receive a partial answer at its first escalation, no longer does; it escalates after hearing with its `customer_ack`.

## 8. KR selection

The KR match inside `decide` keeps its position after layer 2 and layer-1 mandatory (order: layer 2 → layer-1 mandatory → KR / layer-1 advisory → layer 3) and its composites (KR + advisory → `Escalate.known_resolution_id`; KR alone → `Allowed{KnownResolution}`), so already-escalated cases and MCP keep their behavior. Only its input changes: instead of matching signals itself, `decide` receives the KR pre-selected by `evaluate`:

1. Candidates: KRs whose `signals` set is a non-empty subset of the decide signal set (accumulated before escalation, turn-only after, as `decide_signals_for_policy` already chooses), excluding `excluded_known_resolution_ids`; none when a hazard signal is present in the accumulated signals (as today, `stakes_input_for_turn`). Order: larger set, newer `verified_at`, smaller id. At most 3. A new `rules::known_resolution_candidates(resolutions, signals, limit) -> Vec<&KnownResolution>` returns them in that order; `rules::match_known_resolution` stays exactly as on `main` (a KR applies only when no signal is left over; otherwise `KrMatch::BlockedByAddedSignal`), together with `KrMatch::BlockedByAddedSignal`, `EscalateReason::UnknownAddedSignal`, the MCP `blocked_by_added_signal` output (`rmcp_server.rs`) and the tests `kr_added_signal_blocks_reuse` and `match_advisor_known_resolution_returns_none_when_blocked_by_added_signal`. It keeps serving the callers that have no applicability check, `search_known_resolutions` (`rmcp_server.rs`) and the homesec advisor (`advisor/materials.rs`). PR 1 does not carry over the local branch `feat/103-knowledge-first-answering` change that relaxed `match_known_resolution` and deleted those items (§12). The candidates of this section, which allow left-over signals, are used only inside `evaluate`, and always through the applicability check of step 2.
2. Applicability check: one LLM call receives the context text and each candidate's `applicability`, and returns the ids whose applicability matches the customer's situation. The first returned id in candidate order is the applicable KR.
3. LLM failure, unparsable output, or `LexiconFallback` → no applicable KR; warn with `request_id` and the error.
4. Skipped when §4.2 says so; then no applicable KR.

Layer-3 reason: `decide` still calls `match_known_resolution` on the decide signal set only to choose between `EscalateReason::UnknownAddedSignal` (`BlockedByAddedSignal`) and `InsufficientDirectness`, as today; it never applies a KR from that call.

The selected KR is exposed as `EvaluationOutcome.applicable_known_resolution` and used by the answer step (§5). MCP `evaluate_answerability` runs the same selection on its question.

## 9. Data, validation and rollout

### 9.1 `rules.json`

- `escalation_rules`: `hearing` and `hearing_items_ja` (non-empty array of non-empty strings) become required for every rule, mandatory included; `after_hearing` (`answer_or_handoff` | `handoff`) is required; `safety_notice` (non-empty string) is optional.
- `prohibited_domains`: `electrical-work` per §2.3.
- `ingest_rules` rejects a missing or unknown `hearing` / `after_hearing`, a missing or empty `hearing_items_ja`, before writing anything (same policy as `binding`). The existing rejection of `hearing` on a mandatory rule is removed, with its tests updated.
- `ingest_rules` rejects a prohibited domain whose `domain_signals` and `pattern` are both empty, before writing anything (§2.3).
- `ingest_rules` writes each escalation rule's array index as the int attribute `order`. Today `load_escalation_rules` keeps the order `query_nodes` returns, which is not defined; the loader now sorts by `order`, so the rank of §2.1 and the notice order of §4.4 are deterministic. Prohibited domains get no `order`: they all escalate immediately and only the reported `domain_id` could differ when two match, as today.
- `HearingContract` gains the six contracts of §2.1.
- `EscalationRule::hearing_contract()` (read by `decide` for `Escalate.hearing`) returns the contract only for legacy advisory rules, as today; a new `EscalationRule::handling_contract()` returns it for non-legacy rules (read by `/api/reply`, §4.2).
- `hearing_items_ja` is stored on the `EscalationRule` node newline-joined.
- `data/rules.sample.json` (the default `--rules-file` of `ingest_rules`, project `sivira-cs-demo`): its two escalation rules can no longer win from PR 2 (§2.3), so PR 3 deletes them and leaves `"escalation_rules": []`, which `ingest_rules` already accepts; the file then passes the required checks. Their nodes already written to `sivira-cs-demo` stay, because the backend has no delete; they load as legacy rules (§9.2) and are still preceded by layer 2.

### 9.2 Runtime loader

The loader stays backward compatible so that the service can run before `ingest-rules` writes the new attributes: a rule is a **legacy rule** when `after_hearing` is missing, empty or unknown, when `hearing` is missing or unknown, or when `hearing_items_ja` is missing or empty. A legacy rule keeps today's behavior (no handling; immediate or `missing`-based escalation, and the existing Jev hearing for an advisory rule declaring `product_and_symptom`), with a warn naming the rule and the missing attribute. A rule without `order` sorts after the ordered rules, by `rule_id`, with a warn. CI `auto-ingest` runs `ingest-rules` after the deploy because `rules.json` changes, which ends the legacy period.

### 9.3 Schema

- `EscalationRule` (`schema/cs-schema.yml`, `schema/cs-support.yml`): add `after_hearing`, `safety_notice`, `hearing_items_ja`, `order` (int).
- `support_case` (`schema/cs-schema.yml`, `schema/cs-support.yml`, `schema/homesec.yml`, kept identical; the nine keys are also added to the test list `written_support_case_attribute_keys` in `harness/knowledge.rs`, which checks that every written key is declared): add `handling_rule_id`, `handling_contract`, `handling_condition`, `handling_phase`, `answer_rounds` (int), `handling_transcript`, `handling_answers`, `safety_notices_given`, `handoff_summary`.
- `ingest_rules` registers the schema (`create_or_update_schema`) before writing rules, as today.

### 9.4 Jev

`jev-questions.json` gains the keys of §2.2. Startup fails when `[jev] enabled = true` and the file lacks the key of any contract in `HearingContract::ALL` or `resolution_feedback`. The checked set is static, because rules are loaded per request, not at startup. The check runs in `JevClient::build` (`jev.rs`, called by `JevClient::from_config`), right after the questions file is parsed, so `from_config` returns the error and startup fails as for an unreadable file today.

Customer utterances (context text, §4.1) are now sent to TypeSafe for every rule's hearing and for the resolution check, not only for `warranty-failure`. Assistant text is never sent. Turning `[jev] enabled` off stops all sending; hearing then asks once and proceeds, and the resolution check escalates.

## 10. Tests

| Target | Fixed content |
| --- | --- |
| Layer 2 | an `electrical-work` escalation has no handoff items and no partial answer; `decide` returns layer 2 for 「電気工事」 with `physical_construction_risk` present; a construction question without the patterns returns `construction-risk`; bundled `electrical-work` loads with no signals and matches by pattern; a domain with neither is rejected by `ingest_rules` before writing and by the loader |
| State machine (pure) | every step of §4.2 and every row of §4.3 and §6, including both limits (hearing limit with items unknown → answer step, not an escalation), replacement (rank higher replaces, lower or equal does not; counters reset, transcript kept), and the end conditions clearing the attributes and resetting `clarify_turns` and `answer_rounds`; a Hearing turn reuses the pre-check without a second Jev call; a replacement in `AwaitingResolution` discards the resolution-check result; the transcript gains the current message exactly once per turn (start, replacement, Hearing, `follow_up`) |
| Pre-check | Hearing pre-check false below the limit, and Jev failure with `clarify_turns == 0` → `evaluate` makes no KR applicability call; `new_topic` → the handling end is persisted before `evaluate`; a product-switch turn runs no pre-check |
| Stored state | a broken invariant, an unknown phase or contract, or a handling in an escalated case is read as no handling with a warn naming `case_id` and the attribute, and the next save clears it |
| Clarify prompt | the handling Clarify falls back to `FALLBACK_CLARIFY_TEXT` when it mentions an out-of-scope model; `HearingItems` puts the labels in `<確認したい項目>` with the do-not-ask-stated-items rule; `Missing` keeps `<不足している情報>` and today's prompt |
| Audit | each `handling:*` label and `handling_reason` value of §4.5 is written with its fields; an end followed by a start in one turn writes two events in that order; `LlmError` text is absent from the event; a failed write logs a warn and the reply is sent; events without a handling keep the new fields `None` and `verify_chain` passes over old and new events |
| Legacy winner | with no handling, a legacy layer-1 winner starts no handling and takes `resolve_jev_has_enough_info` / `decide_jev_hearing_action` (advisory `product_and_symptom`) or `decide_reply_action`, writing `record_jev_hearing_decision`; inside a handling, a legacy winner neither replaces it nor stops steps 4–5 |
| Draft ownership | `layer1_escalate` sets `Escalate.rule_id` to the winner also when several layer-1 rules match, and it is absent from the serialized payload; `should_draft_reply` is false when `handling_owns_reply` is true; `evaluate` sets `handling_owns_reply` only under `SkipWhenUnused { handling_flow: true, .. }`, so for a non-legacy layer-1 escalation MCP (`Always`) keeps its draft and partial answer and homesec support mode (`handling_flow: false`) keeps today's draft decision; `decide` leaves `Escalate.hearing` `None` for non-legacy rules and fills it for a legacy advisory `product_and_symptom` rule |
| Handled rule lookup | the handled rule absent from `escalation_rules` or legacy → the handling ends without exclusions or summary and the turn runs as without a handling; a changed contract → the post-check uses the new contract and the stored contract is updated; a changed `condition` → the stored `handling_condition` is updated and a later `new_topic` end excludes the updated signals |
| Out-of-scope demotion | a second-stage short circuit in a case with a handling clears the handling attributes in the demotion write, appends nothing, writes no summary |
| Writing | End and its `excluded_signals` union are one upsert; the union keeps existing exclusions; a failed upsert returns `conv_state_save_failed` |
| Exclusions | `resolved` and `new_topic` exclude every signal of the handled rule's `condition` and the next turn starts no handling; the `new_topic` end before `evaluate` excludes the stored `handling_condition`; a rule deleted or changed between turns still excludes the stored signals; a later message that re-mentions the signal starts a new handling |
| Inputs | a Start seeds the transcript with the bounded customer history; a Start right after an end in the same turn seeds the message only; the Jev input of a Start equals today's `build_jev_state` output; manual search, KR check, answer draft, hearing question and Jev receive the context text (Jev without assistant text); signal extraction, `ng_near_hit`, layer-2 patterns and the case `question` receive the current message |
| Answer step | `draft_handling_answer` returns every `AnswerStepFailure` except `OutOfScopeProduct`, which the conversation layer produces after `gate_customer_reply_draft` (§5); a draft mentioning an out-of-scope model → `OutOfScopeProduct` → escalation, nothing appended; a `follow_up` draft receives the history block containing the previous assistant answer; sufficient → Answer + `AwaitingResolution` + `handling_answers`; insufficient, LLM error, egress rejection, truncation, `reply::NO_ANSWER_TOKEN` → escalation; KR-only material when a KR applies; a turn whose KR applicability call was skipped runs the KR selection itself when it reaches the answer step, for a replacement, a contract update, and a start after the handled rule lookup ended the handling |
| Resolution check | each value; low confidence → escalation; Jev disabled → escalation; `follow_up` at `max_answer_rounds` → escalation |
| Safety notices | a product-switch turn whose message carries a notice rule's signals sends the notice on the new case; (a) at a handling start, (b) at a replacement, (c) while another rule is handled; never twice in a case; in `order`; a multi-signal `condition` gets its notice only when all its signals are present; none on the paths of §4.4 marked none; an Allowed answer that adds a notice saves `safety_notices_given`, and a failed save returns `conv_state_save_failed` without the notice |
| KR selection | `search_known_resolutions` still returns `blocked_by_added_signal` for a question with a left-over signal; the applicability check drops a non-matching candidate; LLM failure → no KR; hazard → no candidates; turn-only signals after escalation |
| Validation and loader | `ingest_rules` rejects missing `hearing` / `after_hearing` / `hearing_items_ja` and unknown values, accepts mandatory with `hearing`, writes `order`; startup fails when the key of any `HearingContract::ALL` contract or `resolution_feedback` is missing; the loader treats each legacy condition of §9.2 as legacy and sorts by `order`, rules without `order` last by `rule_id` |
| Handoff summary | written at the first escalation, not overwritten later; LLM failure and `HANDOFF_SUMMARY_TIMEOUT` expiry → raw text; absent from audit events and customer text; shown in `ThreadDetail` |
| Customer ack | an escalation out of a handling uses the handled rule's `customer_ack` when two layer-1 rules match the accumulated signals |
| Jev client | `JevClient::evaluate` with `question_keys` sends only those questions |
| Product switch | the new case starts without a handling; the old case is unchanged |
| Partial answer | an escalation out of a handling has no partial answer; a layer-2 escalation of a domain with signals and a legacy-rule escalation keep it |
| Regression | layer 3, already-escalated cases (a layer-1 match there gets the partial answer, `warranty-failure` included), `LexiconFallback`, time preference, MCP (except §2.3 and §8) |

Existing tests that fix the old behavior:

| Test | Action | PR (§12) |
| --- | --- | --- |
| `harness/decision.rs` | add `mandatory_and_layer2_together_is_layer2` (main has no test that pins layer 1 over layer 2): a mandatory layer-1 rule and a layer-2 domain matching together yield layer 2 | 2 |
| `harness/decision.rs` `mandatory_wins_over_a_matching_kr`, `layer2_wins_over_a_matching_kr`, `kr_with_advisory_escalates_with_the_kr_id`, `kr_alone_is_allowed_known_resolution`, `advisory_without_kr_keeps_missing_hearing_and_ack_rules` | rewrite: the KR is passed pre-selected (§8); the expected results are unchanged | 1 |
| `harness/decision.rs` `layer3_reuses_known_resolution`, `layer1_short_circuits_everything` | rewrite: the KR is passed pre-selected to `decide` (§8); the expected results are unchanged (`Allowed{KnownResolution}` with `kr1`; layer-1 escalation to `safety_team` despite a matching KR) | 1 |
| `harness/decision.rs` `hazard_blocks_kr` | move to the selection (§8): a hazard in the accumulated signals yields no candidates | 1 |
| `harness/decision.rs` `kr_matches_turn_signals_not_question_signals` | invert and rename to `kr_candidates_use_the_decide_signal_set`: before escalation candidates come from the accumulated signals, after escalation from the turn signals only (both cases asserted) | 1 |
| `harness/mod.rs` `decide_turn_does_not_apply_a_kr_split_across_turns` | invert and rename to `kr_split_across_turns_is_a_candidate_before_escalation`; keep an escalated-case variant asserting it is not a candidate | 1 |
| `harness/mod.rs` `decide_turn_skips_kr_when_a_previous_turn_had_a_hazard_signal`, `decide_turn_applies_kr_in_an_escalated_case_with_a_previous_advisory_signal`, `decide_turn_ignores_an_excluded_kr` | rewrite against the selection of §8 (pre-selected KR into `decide`); the expected results are unchanged | 1 |
| `harness/rules.rs` `kr_exact_match_applies`, `kr_added_signal_blocks_reuse`, `match_known_resolution_*`, `kr_more_specific_exception_rule_wins`, `kr_no_candidate_returns_none` | keep as on `main`, including `kr_added_signal_blocks_reuse` (PR 1 does not take the branch's relaxation of `match_known_resolution`, §12); `match_known_resolution` keeps its behavior (§8); `known_resolution_candidates` gets its own tests (order, limit 3, empty set) | 1 |
| `bin/ingest_rules.rs` `hearing_declared_on_a_mandatory_rule_is_rejected` | replace with `hearing_declared_on_a_mandatory_rule_is_accepted` | 3 |
| `bin/ingest_rules.rs` `bundled_urtect_rules_parse_and_only_warranty_failure_writes_a_hearing_attribute` | rewrite and rename to `bundled_urtect_rules_parse_and_every_rule_writes_its_hearing_attributes` | 3 |
| `bin/ingest_rules.rs` `a_misspelled_binding_can_not_slip_a_hearing_declaration_past_the_mandatory_guard` | delete: the mandatory guard is removed (§9.1); misspelled `binding` stays covered by `binding_matching_is_exact_so_case_variants_and_empty_are_rejected` | 3 |
| `api.rs` `decide_jev_hearing_action_*`, and `resolve_jev_has_enough_info_*` that do not read the bundled rules | keep: the legacy path stays (§4.2 step 7) | – |
| `api.rs` `resolve_jev_has_enough_info_calls_jev_for_the_bundled_warranty_failure_rule`, `resolve_jev_has_enough_info_never_calls_jev_for_the_bundled_contract_billing_rule`, `resolve_jev_has_enough_info_never_calls_jev_when_human_handoff_beats_warranty_failure` | rewrite on a legacy fixture (the bundled rules with `after_hearing` removed) and rename from `bundled` to `legacy`; add `bundled_warranty_failure_starts_a_handling` next to the Legacy winner row | 3 |
| `jev.rs` fixtures `valid_questions_path` / `write_questions_file` / `test_client` (only `is_emergency` today) and their users; `api.rs` `jev_questions_file` | the valid fixture gains every key of §2.2, so `JevClient::build` passes the new check; add `build_fails_when_a_contract_key_is_missing`, `build_fails_when_resolution_feedback_is_missing` and `evaluate_fails_for_a_key_absent_from_the_questions_file` | 2 |
| `jev.rs` `evaluate_sends_state_model_and_questions_in_request_body` | rewrite: the request body contains only the questions named by `question_keys` (§2.2) | 2 |
| `bin/ingest_rules.rs` `a_null_binding_is_rejected_like_an_omitted_one` | its inline fixture gains valid `hearing`, `hearing_items_ja` and `after_hearing`, so the error it asserts stays the `binding` rejection | 3 |
| `test_support.rs` `load_bundled_escalation_rules` (shared test helper) | extend: read `hearing_items_ja`, `after_hearing`, `safety_notice` and the array index as `order`, applying the same legacy rule as the runtime loader (§9.2), so that the bundled rules are non-legacy in every test once `rules.json` carries the values | 3 |
| `bin/verify_deploy.rs` `BundledRuleInput` / `bundled_rule_attrs` | extend: pass the same new attributes and `order` to `escalation_rule_from_attributes` | 3 |
| `bin/ingest_rules.rs` fixtures `rule_json`, `rule_json_with_customer_ack` | add valid `hearing`, `hearing_items_ja` and `after_hearing` by default, so every test built on them passes the required checks; the rejection tests remove one attribute at a time | 3 |
| `bin/ingest_rules.rs` `bundled_urtect_rules_round_trip_through_the_runtime_loader` | rewrite: every bundled rule loads non-legacy with its `handling_contract()`, and `hearing_contract()` is `None` for all | 3 |
| `bin/ingest_rules.rs` `rules_file_without_any_hearing_field_still_parses_and_writes_empty_declarations`, `rules_file_without_any_customer_ack_field_still_parses_and_writes_empty_declarations` | the first: replace with `a_rule_without_hearing_attributes_is_rejected` on an inline fixture; the second: move to an inline fixture with the new attributes and without `customer_ack`. Both read `rules.sample.json`, which has no escalation rule after PR 3 | 3 |
| `bin/ingest_rules.rs` `an_advisory_rule_may_declare_a_hearing` | keep, on the extended fixture | 3 |
| `bin/ingest_rules.rs` `bundled_and_sample_rules_files_build_every_node_with_the_binding_they_declare` | keep: for `rules.sample.json` it then checks the prohibited domains only | 3 |
| `harness/decision.rs` `bundled_only_warranty_failure_declares_the_product_and_symptom_hearing` | rewrite and rename to `bundled_rules_declare_no_legacy_hearing_contract`: `hearing_contract()` is `None` and `handling_contract()` is `Some` for every bundled rule | 3 |
| `harness/decision.rs` `bundled_warranty_failure_escalation_carries_the_product_and_symptom_hearing` | invert and rename to `bundled_warranty_failure_escalation_carries_no_hearing`; add `legacy_warranty_failure_escalation_carries_the_product_and_symptom_hearing` on a legacy fixture | 3 |
| `api.rs` `is_product_and_symptom_hearing_turn_true_for_the_bundled_warranty_failure_rule` | rewrite on the legacy fixture and rename from `bundled` to `legacy`; add `is_product_and_symptom_hearing_turn_false_for_the_bundled_warranty_failure_rule` | 3 |
| `harness/decision.rs` `bundled_human_handoff_rule_escalates_at_layer1_even_with_high_manual_score`, `bundled_human_handoff_rule_has_no_missing_with_low_manual_score`, `bundled_warranty_failure_and_human_handoff_conflict_resolves_to_mandatory`, `bundled_contract_billing_escalation_carries_no_hearing_although_it_is_advisory`, `bundled_customer_ack_applies_only_when_exactly_one_layer1_rule_matches` | keep: `decide` still returns these layer-1 escalations, and the handling of §4 starts from them in `/api/reply`; comments saying that a mandatory match is answered by an immediate escalation are reworded accordingly | 3 |
| `api.rs` `is_product_and_symptom_hearing_turn_false_for_the_mandatory_winner_of_a_warranty_failure_conflict`, `is_product_and_symptom_hearing_turn_false_for_the_bundled_contract_billing_rule`; `harness/handoff_items.rs` `handoff_items_for_decision_uses_only_domain_signals_when_advisory_and_layer2_both_match`; `harness/mod.rs` tests using `decide_with_bundled_rules_and_high_manual_score`; `harness/real_llm_smoke.rs` | keep: they do not depend on `Escalate.hearing` of a non-legacy rule or on the order of layer 2 and layer-1 mandatory. The comment above `is_product_and_symptom_hearing_turn_false_for_the_mandatory_winner_of_a_warranty_failure_conflict` that says a mandatory event escalates immediately is reworded to describe the legacy path | 3 |

Real-LLM verification (real Anthropic and Jev, production `search_manual` results), shown to the user before merge:

1. 「録画が何日も止まっています」: no retention KR is used; the reply hears or answers about recording, not storage retention.
2. warranty-failure: 「カメラが壊れたみたい」→ hearing → manual troubleshooting → 「直りました」→ closing reply, no escalation.
3. human-handoff: 「担当者と話したい」→ asks the matter → an answerable matter is answered.
4. physical-damage: the first reply starts with the safety notice and asks the state.
5. initial-cost-quote: asks site and count, then escalates with `customer_ack` and a handoff summary.
6. An `unresolved` reply escalates with a summary containing the heard items and the given remedy.
7. 「電気工事が必要ですか」: immediate layer-2 escalation, no hearing.
8. After an answer, a question on the same topic is classified `follow_up` and answered again; an unrelated question is classified `new_topic`, ends the handling, and is processed as a new turn.

## 11. Expected behavior and document updates

Hazard rules (`security-incident`, `physical-damage`, `construction-risk`) usually run at Mid or High stakes (thresholds 0.8 / 0.95), so after hearing they mostly escalate for insufficient material; the hearing still gives the operator the heard items. This is the intended outcome.

Documents to update in the implementing PRs:

- `specs/production-cs-mcp.md`: replace the "mandatory = immediate routing" statements (conversation layer, Issue #54 note) with a reference to this document.
- `CLAUDE.md` (the user edits it, or approves an edit): the `binding` warning (mandatory immediate escalation guarantee), the Jev paragraph (TypeSafe scope, §9.4), and the `rules.json` validation list (§9.1).
- `2026-10-15-knowledge-first-answering-design.md` §1, §3.2, §3.3; `2026-10-14-answer-after-escalation-design.md` references to the KR layer; `2026-10-07-partial-answer-with-handoff-design.md` §3.1; `2026-09-21-jev-shadow-design.md`; `2026-08-12-conversation-flow-v11-design.md`: replace the superseded statements with references to this document.

## 12. Delivery

Issue #109 is the parent. Child Issues, each one PR, in this order:

1. KR selection (§8); closes #103. PR 1 is reworked from `main`, porting from the local branch `feat/103-knowledge-first-answering` (PR #108) only what §8 needs. The branch relaxed `match_known_resolution` (test `match_known_resolution_applies_with_leftover_signals`) and deleted `KrMatch::BlockedByAddedSignal`, `EscalateReason::UnknownAddedSignal`, the test `kr_added_signal_blocks_reuse`, the MCP `blocked_by_added_signal` output and the advisor test `match_advisor_known_resolution_returns_none_when_blocked_by_added_signal`; PR 1 does not carry those changes over: all of them stay as on `main`, and the relaxed candidates are the separate function `rules::known_resolution_candidates` used only inside `evaluate` through the applicability check. Symbols that exist only on that branch and are introduced by PR 1 on `main`: `layer1_escalate`, `stakes_input_for_turn`, `match_layer1_mandatory`, `excluded_known_resolution_ids` and the PR 1 test names of §10.
2. Rules and data: layer-2 precedence (§2.3), rule `order`, rule schema, contracts, Jev questions, validation, loader, schema files (§2, §9).
3. Handling: state, turn order, hearing, safety notices, answer step, resolution check (§3–§6, §7.2, §7.3).
4. Handoff summary and admin UI display (§7.1).

No rule may become non-legacy (§9.2) before the handling exists, or the interim release would drop the existing hearing of `warranty-failure` without the new flow. Therefore PR 2 makes `ingest_rules` and the loader accept and validate `hearing_items_ja`, `after_hearing` and `safety_notice` when present, without requiring them, and leaves those values out of `rules.json`. PR 3 adds the values to `rules.json` and makes the fields required in `ingest_rules` (§9.1). PR 4 only adds the summary; until it merges, `handoff_summary` is not written.

PR 1 adds the `context_text` input of `evaluate`; `/api/reply` passes the current message, and nothing skips the KR applicability call. PR 3 passes the context text (§4.1) and adds the skip (§4.2). PR 2 adds the Jev keys and the startup check (§9.4). PR 3 removes the rejection of `hearing` on a mandatory rule (§9.1). PR 3 also adds the handling audit (§4.5) and empties `escalation_rules` in `data/rules.sample.json` (§9.1).

Out of scope: operator notification channels, multi-instance session storage, changes to layer 3.
