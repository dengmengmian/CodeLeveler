# CodeLeveler Mobile Runtime Alignment Plan v1.0

**Status:** M8–M11 plus signed `fetch_attachment` are in; feature work is **FROZEN** at tag `mobile-beta-mvp` ([`MOBILE_FREEZE.md`](MOBILE_FREEZE.md)). Push (M12) is not. Agent workspace writes are not auto-registered as attachments.  
**Chinese:** [`MOBILE_RUNTIME_ALIGNMENT.zh-CN.md`](MOBILE_RUNTIME_ALIGNMENT.zh-CN.md)

M1–M7 made the phone *look* like a control client. Two holes remain: **steer a running turn** (`SteerCurrentTurn` is already allowed remotely; `Commands` does not send it) and **take artifacts home** (`attachment_added` is a filename row; no signed fetch RPC on the product surface).

Do not replace Flutter, the pairing stack, or the Runtime. Map `RuntimeEvent` — do not invent a second EventLog.

**Done in app:** M10 steer, M8 coverage, M9 B1–B2 cards/preview, M11a TaskHeader.  
**Not done:** host `fetch_attachment` (M9 B3), Task Detail route (M11b), push (M12, design only).

Keep ignoring token/progress noise. Never drop tool, approval, sub-agent, attachment, or completion facts into `_ignored` without a UI row.

Steering is `ClientCommand::SteerCurrentTurn`, not a new “intervention” bus. Artifacts are `AttachmentRef` projections; no public `download_url`. Task is a UI word for today’s 1:1 session.
