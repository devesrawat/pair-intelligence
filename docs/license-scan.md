# License scan (Task 1)

Run: 2026-10-03, against the pinned checkout `v2026.9.7` (`c074824a27c9`), read-only.

```bash
cd upstream/openclaw
pnpm licenses list --prod --json          # pnpm 12.5.1, Node 24.21.0; writes nothing to the checkout
pnpm why <pkg> -r --prod                  # reverse-dependency tracing, also read-only
```

Scope: production dependencies of every workspace package (973 distinct package names after pnpm's per-license grouping). Dev-only dependencies were not scanned. The raw JSON embeds absolute local paths and is not committed; re-run the command to regenerate it.

## Distribution

| License | Packages |
|---|---:|
| MIT | 720 |
| Apache-2.0 (incl. 3 spelled `apache-2.0`) | 110 |
| ISC | 59 |
| BSD-3-Clause / BSD-2-Clause | 41 / 10 |
| BlueOak-1.0.0, Unlicense, MIT-0, 0BSD, PSF-2.0, Artistic-2.0 | 15 |
| MPL-2.0 (file-level copyleft) | 5 |
| Dual / compound (MIT OR GPL-3.0-or-later, MPL-2.0 OR Apache-2.0, MIT OR EUPL-1.1+, MIT AND Zlib, WTFPL OR MIT) | 5 |
| **GPL / LGPL (strong copyleft)** | **7** |
| Unknown | 1 |

## Copyleft and unknown, with the dependency path

| Package | License | Pulled in by | In core runtime? |
|---|---|---|---|
| `libsignal` 6.0.0 | **GPL-3.0** | `baileys` -> `@openclaw/whatsapp`; `@openclaw/crabline` -> `@openclaw/qa-lab` | No: WhatsApp extension and QA lab only |
| `@audio/decode-aac` 1.5.0 | **GPL-2.0** | `@audio/decode` -> `audio-decode` -> `@openclaw/whatsapp` and `baileys` | No: WhatsApp only |
| `@audio/decode-ac3`, `-dts`, `-wma` | **GPL-2.0-or-later** | same chain | No: WhatsApp only |
| `@audio/decode-eac3` 1.0.0 | LGPL-2.1-or-later | same chain | No: WhatsApp only |
| `codec-parser` 2.5.0 | LGPL-3.0-or-later | `@audio/decode-flac/-mp4/-opus/-vorbis/-webm` -> same chain | No: WhatsApp only |
| `jszip` 3.10.2 | MIT OR GPL-3.0-or-later | dual-licensed; elect MIT | Elect MIT, no issue |
| `@novnc/novnc`, `@ubjs/core`, `@ubjs/node`, `mediabunny`, `web-push` | MPL-2.0 | various | File-level copyleft; unmodified use is fine |
| `dompurify` 3.4.15 | MPL-2.0 OR Apache-2.0 | UI | Elect Apache-2.0 |
| `@anthropic-ai/claude-agent-sdk` 0.3.274 | **Unknown** (proprietary, governed by Anthropic terms) | `@agentclientprotocol/claude-agent-acp` -> `@openclaw/acpx` | Bundled `acpx` plugin only |

## Findings

1. `libsignal` GPL-3.0 is now **confirmed locally** (the audit had it as "believed"). All GPL/LGPL packages sit behind `@openclaw/whatsapp` (and `libsignal` additionally behind `@openclaw/qa-lab`). No other workspace package reaches them in `pnpm why`. They are not dependencies of the `openclaw` core package.
2. Divergence from `upstream-audit.md`: `mpg123-decoder` resolves to **MIT** here, not LGPL-2.1. The LGPL-2.1 item is `@audio/decode-eac3`; the LGPL-3.0 item is `codec-parser`.
3. `@anthropic-ai/claude-agent-sdk` has no OSS license. It is reachable only through the bundled `acpx` plugin. PAIR must not ship or enable `acpx` (this also matches `provider-billing.md`: no subscription-backed Claude routes).
4. Actions for PAIR distribution:
   - Do not bundle or enable `@openclaw/whatsapp` or `@openclaw/qa-lab` in any PAIR artifact. If PAIR ships a pruned runtime, prove it with `pnpm why libsignal -r --prod` returning nothing.
   - Disable `acpx` (`plugins.deny` or `plugins.entries.acpx.enabled=false`).
   - Elect MIT for `jszip`, Apache-2.0 for `dompurify`.
   - PAIR's own adapter plugin has no runtime dependencies; it imports the host SDK only.
5. Limits: this is a metadata scan (`license` fields), not a source audit. Nested vendored code and native binaries were not inspected. The 722-advisory re-check remains open.
