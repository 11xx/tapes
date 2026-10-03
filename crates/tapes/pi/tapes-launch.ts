/**
 * Records how each pi process opened a session, so tapes can tell a session
 * a person drove from one a program ran. pi writes no launch marker of its
 * own; this entry is that marker.
 *
 * Every session start appends one `custom` entry of type `tapes.launch` whose
 * `data.mode` is pi's run mode (`tui`, `print`, `json`, or `rpc`) and whose
 * `data.session` is the session it was written for. A fork copies its
 * parent's entries under a new id; the id tells those apart from the fork's
 * own. The entry stays out of the model's context. Install it with
 * `tapes install pi`.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.on("session_start", (_event, ctx) => {
		if (typeof ctx.mode !== "string") return;
		pi.appendEntry("tapes.launch", { mode: ctx.mode, session: ctx.sessionManager.getSessionId() });
	});
}
