#!/usr/bin/env node
// session-start.mjs — SessionStart hook. Two jobs, run on every start/resume:
//   1. Register this session: write the cwd->id marker (so the MCP bus can
//      resolve "me") and upsert {id, dir} into the registry.
//   2. Drain this session's inbox and inject any pending messages as
//      additionalContext, so a woken/resumed session sees its mail immediately.
// Never blocks the session: any error is logged to stderr and we exit 0.
import * as store from '../lib/store.mjs';

let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (c) => { input += c; });
process.stdin.on('end', () => {
  try {
    const ev = JSON.parse(input || '{}');
    const id = ev.session_id;
    const dir = ev.cwd || process.env.CLAUDE_PROJECT_DIR || process.cwd();
    if (id) {
      store.setMarker(dir, id);
      store.register({ id, dir });
      const msgs = store.drain(id);
      if (msgs.length) {
        const lines = msgs
          .map((m) => `- from ${m.fromName || m.from || 'unknown'} (${m.ts}): ${m.body}`)
          .join('\n');
        const additionalContext = `📬 session-relay: ${msgs.length} message(s) delivered to this session:\n${lines}\n\nTo reply, use the session-relay skill and send to the sender.`;
        process.stdout.write(JSON.stringify({
          hookSpecificOutput: { hookEventName: 'SessionStart', additionalContext },
        }));
      }
    }
  } catch (e) {
    process.stderr.write(`[session-relay/hook] ${e?.message || e}\n`);
  }
  process.exit(0);
});
