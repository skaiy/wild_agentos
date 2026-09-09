# WAO Core Design: Multimodal (Image+Text) — **Reuse Existing Capabilities First**

| Item | Content |
|---|---|
| Doc version | 0.2.0 (revision: align with existing design + landed contracts) |
| Audience | Open-source line (coordination / deploy / test / security) |
| Trigger | StructCapture demo line (business repo only consumes the contract) |
| Parent design | [docs/13-DESIGN_DETAIL.md](./13-DESIGN_DETAIL.md) / [zh](./13-DESIGN_DETAIL.zh.md) (**authoritative — do not invent a parallel stack**) |
| Related | Sibling business-layer design in `wild-struct-capture`; isolation `17-isolation-contract`; tools `05-tool-system`; ingest `16-knowledge-ingest-import-graph` |
| Chinese | [19-multimodal-reuse-existing-capabilities.zh.md](./19-multimodal-reuse-existing-capabilities.zh.md) |
| Demo snapshot | WAO `core:v0.6.1` · OIDC · Agent `structcapture-organizer` · gateway temporarily text MiniMax |

---

## 0. Hard principles for the open-source line (pin this)

> **Do not build a separate “multimodal middleware” for StructCapture.**  
> First **wire / configure / document / close gaps** on capabilities that already exist; any gap is a minimal delta written back into DESIGN_DETAIL (or the matching numbered doc), not a second semantics stack in the business repo or a side path.

| Principle | Meaning |
|---|---|
| **Reuse first** | Orchestration, memory, JSON-LD, 5W2H, skill graph, tools, gateway, isolation contract — all follow models already defined in `13-DESIGN_DETAIL` |
| **Align to existing contracts** | Agent chat already accepts `images[]`; with images, resolve `model_mounts.vision` → `chat`; business clients use that shape — do not invent new field names |
| **Config before code** | Demo blockers are usually “vision slot not mounted to a VL model / docs unclear / error semantics unclear”, not a missing pipeline |
| **Business-agnostic** | Domain prompts, HITL, enum dictionaries stay in Capture; Core stays generic |
| **No silent image drop** | If the vision slot falls back to a non-VL model or the upstream rejects images, the failure must be **observable** (error or explicit degrade policy). Never accept `images` and answer as pure text with no signal |

Business-side VL bypass is a **temporary demo fallback** only; the correct path is stock WAO above.

---

## 1. Mapping onto `13-DESIGN_DETAIL`: how existing capabilities carry image+text structuring

Multimodal here is an **input-shape extension on the existing architecture**, not a new subsystem.

| DESIGN_DETAIL capability | Use in multimodal structuring (reuse) | Avoid |
|---|---|---|
| **§1 Generalized PDCA + complexity levels** | “Organize shots → structured fields” is usually **L0/L1** (single turn / single PDCA). Do not default to full L3+ multi-agent. SA picks mode via 5W2H | Hard-coding “always run a full PDCA team” for vision |
| **§2 Five-layer memory** | Image evidence enters L0/L2 as **@id / IRI**; L1 keeps summary + pointers. Do not dump full base64 into the context window | A parallel “image cache service” that bypasses memory layers |
| **§3 JSON-LD semantic bus** | Each shot / extracted field as a typed `@id`/`@type` node; Framing controls projection depth; conflicts trace via named graphs | A business-only non-RDF evidence store as the “official” path |
| **§4 5W2H** | **Where** = image/URL evidence; **What/Why** = structuring goal and success criteria; CA can audit image↔text alignment per dimension | A separate “vision meta” schema unrelated to 5W2H |
| **§5 Skill graph** | Vision/OCR/VL completion as **AtomicSkill or MCP wrapper**; text extraction via `AlternativeLink` fallback; pitfalls as KnowledgeFragment | Hard-coding StructCapture packaging OCR inside Core |
| **§6 Proactive perception** | CycleTimeout / QualityDegradation cover slow VL and bad extracts; reuse dedup window | A separate vision alert bus |
| **§7 Tools + MCP** | Large vision outputs go through result routing / micro-tools; external VL via **existing LLMClient / MCP**, not a private protocol | Business clients bypassing SyscallGate / isolation |
| **§8–§9 Checkpoints / work queue** | Long vision jobs may be async + checkpoint; bulk evidence import may use workers | Forcing the sync demo path onto a heavy queue (not required) |
| **§10 Templates + JSON Schema “one round-trip, dual harvest”** | Structured fields: **Schema validate → JSON-LD → blackboard**; same think/content/summary pattern | Free-text JSON with no validation |
| **§11 Gateway / LLMClient** | Always through gateway; with images select the vision slot | A second “outside-WAO gateway” treated as the primary path |

**Takeaway for planning:** schedule as “**connect / configure / harden / document existing chat+vision+isolation+memory**”, not “design a multimodal subsystem from scratch”.

---

## 2. Landed code contract (consume as-is; do not reinvent)

From current mainline (`src/api/http/chat.rs` and related). Business layer already assumes this.

### 2.1 Agent Chat request

```http
POST /api/v1/agents/{agent_id}/chat
Authorization: Bearer <OIDC with verified isolation claims>
Content-Type: application/json

{
  "message": "required text",
  "images": ["data:image/jpeg;base64,...", "https://..."]
}
```

- Empty/omitted `images` = text-only (today’s behavior).  
- With images: `ChatContent::Parts` (text part + `ChatContent::image`).  
- **Same path** checks isolation claims; missing claims → `verified isolation claims required for chat` (multimodal must not bypass).

### 2.2 Model mounts (already implemented)

With images, resolve in order: `model_mounts["vision"]` → `model_mounts["chat"]` → legacy `agent.model` → `gateway.default_model()`.  
Without images: `chat` only.

**Demo gap (config, not architecture):** `structcapture-organizer` often has **no real VL on the vision mount** (or points at a text model); gateway may also be text MiniMax → “images are accepted but vision quality is poor”. Prefer **register model resources + mounts** before changing Core.

### 2.3 Links back to DESIGN_DETAIL

| Existing behavior | Align with |
|---|---|
| Generic chat; no default EV-repair system/RAG | §11 business-agnostic; BizAgent isolation |
| Claims-scoped agents | `17-isolation-contract` |
| gateway `chat_with_model` | §11 LLMClient / Gateway |
| (Later) persist extract results | §2–§3 memory + JSON-LD; §10 Schema |

---

## 3. Goals and non-goals (revised)

### 3.1 Goals

- **W1** **Document and freeze** the existing `images` + vision/chat mounts contract (OpenAPI / examples / limits / errors).  
- **W2** Demo Agent **correctly mounts a VL**; text and vision models coexist (config surface).  
- **W3** Distinguishable failures: VL unset, upstream reject, 413, timeout — business can fall back; **default: no silent image drop**.  
- **W4** Document how evidence ties into memory / JSON-LD / 5W2H / skill graph (IRI, not only one-shot base64).  
- **W5** OIDC/isolation on the same path as imaged requests (mostly done; add tests).

### 3.2 Non-goals

- No StructCapture-domain OCR / enums / HITL inside WAO.  
- No parallel “Vision Service” replacing gateway.  
- Vision not required on every Agent.  
- No commercial VL SDK wired into Core as a bypass.

---

## 4. Gap list: minimal deltas on top of what exists

### P0 — Demo correct path (config + contract hardening)

| ID | Work | Kind | Acceptance |
|---|---|---|---|
| P0-1 | Formal docs: `images`, mounts `vision`/`chat`, limits, error codes; link DESIGN_DETAIL §7/§11 | Docs | Copy-pasteable curl; matches code |
| P0-2 | Demo/sample: add VL under `models.resources`; Agent `model_mounts.vision` points to it | **Config** | 1-image chat references visible text/objects |
| P0-3 | When vision mount missing and resolve falls back to a text model: **observable** policy | Small change / policy | Tests prove no fake “vision success” |
| P0-4 | Payload limits + 413 / business code (configurable) | Harden | Oversize is testable |
| P0-5 | OIDC + isolation + images: positive/negative CI | Tests | Missing claims still 401 |

**Decided fallback policy (open-source line):** default **soft degrade + warning** (`degraded` / `warning: vision_mount_unavailable`); set `AGENTOS_VISION_FALLBACK=error` for hard 4xx. **Silent drop remains forbidden.**

### P1 — Align memory & skills (still reuse DESIGN_DETAIL)

| ID | Work | Reuse |
|---|---|---|
| P1-1 | Optional: persist image evidence as `mem:`/`exec:` nodes in L2/L0; L1 IRI only | §2 §3 |
| P1-2 | Audit: `image_count`, bytes, model id; **never log base64** | §6 / security |
| P1-3 | https image URLs: SSRF allowlist; prefer pre-signed URLs | §7 network-tool class constraints |
| P1-4 | Structured output via Schema + JSON-LD (dual harvest) | §10 |
| P1-5 | Optional AtomicSkill/MCP `vision.describe` for PDCA Do; direct chat `images` remains | §5 §7 |

### P2 — Enhancements

| ID | Work |
|---|---|
| P2-1 | Explicit multi-image refs in message (`[image:n]`) |
| P2-2 | Streaming chat + multimodal (if text streaming already exists) |
| P2-3 | Sample CycleTimeout tuning for VL latency |
| P2-4 | Content-safety hooks (doc placeholder OK) |

---

## 5. Collaboration with the business layer

```
Business (StructCapture)                    Stock WAO (open-source)
────────────────────────                    ───────────────────────
compress / max shots / flags / HITL / enums
POST message + images[]  ─────────────►  existing chat contract
                         ◄─────────────  reply / distinguishable errors
temp VL bypass  ◄── only while P0 is open
```

**Conditions to turn off business VL bypass:**

1. P0-1…P0-5 done (docs + demo Agent vision mount + observable policy + tests)  
2. Demo VPS smoke: imaged organize beats text-only  
3. Deploy samples; test checklist; security confirms no image bodies in logs  

Target release for this P0 slice: **v0.6.2** (hotfix; not mixed into v0.7).

---

## 6. Suggested milestone language

Avoid: “new multimodal subsystem / new Vision API”.  
Prefer: “**Agent chat multimodal wiring acceptance** (existing `images` + `model_mounts.vision`) + DESIGN_DETAIL cross-links + demo config”.

| Slice | Content |
|---|---|
| Hotfix / small release (v0.6.2) | P0: docs, VL mount, silent-drop governance, 413, CI |
| Next small release | P1: evidence nodes, audit, SSRF, Schema→JSON-LD |
| Later | P2 |

---

## 7. Ops notes — config first

| Item | Suggestion |
|---|---|
| Model resources | Register VL in `models.resources` |
| Agent | `model_mounts.chat` = text; `model_mounts.vision` = VL |
| Rollback | Remove vision mount or point at text = immediate text path; business clears `INCLUDE_IMAGES` |
| Health | Optional vision probe (tiny fixture image) |
| Quota | Confirm pool/billing before switching VL |

---

## 8. Test notes

1. No `images`: text regression.  
2. Images + vision mounted to VL: packaging fixture; reply cites visible brand/spec cues.  
3. Images but vision unset: assert **observable** policy (not silent fake success).  
4. Oversized payload.  
5. No OIDC / missing isolation claims.  
6. StructCapture organize E2E stays in the business repo; Core provides staging notes.

---

## 9. Security

1. Same auth strength as text (existing claims gate; do not weaken for images).  
2. Logs must not contain data URLs / raw images.  
3. SSRF protection for https image fetch.  
4. Cross-tenant named graphs / vector namespaces still via isolation mint (`16`/`17`).  
5. No HS256 shared-secret “demo shortcut”.

---

## 10. Explicitly not Core’s job

| Item | Owner |
|---|---|
| home-inventory fields, enums, HITL UI, ASR UX | StructCapture / business line |
| Customer Basic Auth, Caddy, kaiy.ai | Demo ops / business VPS |
| Which commercial VL | Joint advice; lands as **`models.resources` config** |

---

## 11. One-page summary

> Per [13-DESIGN_DETAIL](./13-DESIGN_DETAIL.md): multimodal is **not** a new mid-tier — attach image evidence to existing PDCA / memory / JSON-LD / 5W2H / skills / tools / Gateway.  
> Code already has `POST .../chat` `images[]` and `model_mounts.vision→chat`. Prioritize **mounting VL, freezing contract + error semantics, CI**; forbid silent image drop.  
> StructCapture only consumes the contract; domain + HITL stay in the business repo. VL bypass is temporary. Target **v0.6.2** for P0.

---

## 12. Open questions (product/ops)

1. ~~Soft vs hard when vision falls back to text?~~ **Decided:** soft + warning by default; `AGENTOS_VISION_FALLBACK=error` for hard 4xx.  
2. Official defaults for `max_images` / `max_bytes`?  
3. Must P1 force L2 @id persistence, or is stateless chat enough for now?  
4. ~~0.6.x vs 0.7.0?~~ **Decided:** **v0.6.2** hotfix slice.

---

## 13. Revision history

| Date | Version | Notes |
|---|---|---|
| 2026-09-09 | 0.1.0 | Initial capability list (zh) |
| 2026-09-09 | 0.2.0 | zh: force DESIGN_DETAIL reuse; document landed `images`/vision mounts |
| 2026-09-09 | 0.2.0 | **EN pair** for PR #215; record v0.6.2 + soft-degrade+warning decision |
