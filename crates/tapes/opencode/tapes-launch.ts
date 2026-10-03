/**
 * Records process launch evidence in session metadata, outside model context.
 * The `/cli/tui/worker` entrypoint path identifies TUI mode; the `run`
 * command argument identifies run mode. These internal-layout signals were
 * verified against OpenCode 1.18.33. A layout tapes does not recognize leaves
 * the session without a marker.
 */
export default async function ({ client }) {
	const entrypoint = process.argv[1]?.replaceAll("\\", "/") ?? "";
	const mode = /\/cli\/tui\/worker\.[jt]s$/.test(entrypoint)
		? "tui"
		: process.argv[2] === "run" ? "run" : undefined;
	if (!mode) return {};

	const pending = new Map<string, Promise<void>>();
	async function record(session: string) {
		if (pending.has(session)) return pending.get(session);
		const writing = (async () => {
			const response = await client.session.get({ path: { id: session } });
			if (response.error || !response.data) { pending.delete(session); return; }
			const metadata = response.data.metadata ?? {};
			const previous = metadata["tapes.launch"];
			const modes = previous?.session === session && Array.isArray(previous.modes)
				? previous.modes.filter((value) => value === "tui" || value === "run") : [];
			const result = await client.session.update({
				path: { id: session },
				body: { metadata: { ...metadata,
					"tapes.launch": { session, modes: [...new Set([...modes, mode])] },
				} },
			});
			if (result.error) pending.delete(session);
		})();
		pending.set(session, writing);
		try { await writing; } catch { pending.delete(session); }
	}
	return {
		event: async ({ event }) => {
			if (event.type === "session.created") await record(event.properties.info.id);
		},
		"chat.message": async ({ sessionID }) => { await record(sessionID); },
	};
}
