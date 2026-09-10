import { createHash } from 'node:crypto';
import * as path from 'node:path';
import type { ExtensionAPI, ExtensionContext } from '@oh-my-pi/pi-coding-agent';

interface Live {
  ctx: ExtensionContext;
  sessionId: string;
  cwd: string;
  root: string;
  timer: Timer | null;
  generation: number;
  draining: boolean;
}

let live: Live | null = null;
let generation = 0;
const launcher = path.join(import.meta.dirname, '..', 'bin', 'relay');
const identifier = /^[A-Za-z0-9._-]{1,128}$/;

export default function (pi: ExtensionAPI): void {
  const { Type } = pi.typebox;
  const parameters = Type.Object({
    action: Type.Union([
      Type.Literal('whoami'),
      Type.Literal('register'),
      Type.Literal('roster'),
      Type.Literal('discover'),
      Type.Literal('send'),
      Type.Literal('inbox'),
      Type.Literal('request'),
      Type.Literal('reply'),
      Type.Literal('wake'),
    ]),
    to: Type.Optional(Type.String()),
    name: Type.Optional(Type.String()),
    id: Type.Optional(Type.String()),
    status: Type.Optional(Type.Union([Type.Literal('completed'), Type.Literal('failed')])),
    text: Type.Optional(Type.String()),
  });
  type Params = typeof parameters.infer;

  function run(argv: string[], current: Live) {
    return pi.exec('env', [`RELAY_OMP_SESSIONS=${current.root}`, launcher, ...argv], {
      cwd: current.cwd,
      timeout: 15000,
    });
  }

  function argvFor(action: Params['action'], params: Params, sessionId: string): string[] {
    const { to, name, id, text, status = 'completed' } = params;
    switch (action) {
      case 'whoami':
        return [];
      case 'register':
        if (!name) throw new Error('register requires name');
        return ['register', '--id', sessionId, '--tool', 'omp', name];
      case 'roster':
        return ['list'];
      case 'discover':
        return ['discover'];
      case 'inbox':
        return ['inbox', '--hold', sessionId];
      case 'send':
      case 'request':
        if (!to || text === undefined) throw new Error(`${action} requires to and text`);
        return [action, '--from', sessionId, to, '--', text];
      case 'reply':
        if (!id || text === undefined) throw new Error('reply requires id and text');
        return ['reply', '--from', sessionId, '--status', status, id, '--', text];
      case 'wake':
        if (!to || text === undefined) throw new Error('wake requires to and text');
        return ['wake', to, '--', text];
      default: {
        const unknownAction: never = action;
        throw new Error(`Unknown relay action: ${unknownAction}`);
      }
    }
  }

  let doorbell: { generation: number; sentAt: number } | null = null;
  const chunkSize = 65_536;
  const tokenPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
  const sha256 = (text: string) => createHash('sha256').update(text).digest('hex');

  interface MailChunk {
    token: string;
    chunk: number;
    chunks: number;
    rendered_len: number;
    rendered_sha256: string;
    rows: string[];
    content: string;
  }

  function isChunk(value: unknown): value is MailChunk {
    return (
      typeof value === 'object' &&
      value !== null &&
      'token' in value &&
      typeof value.token === 'string' &&
      tokenPattern.test(value.token) &&
      'chunk' in value &&
      typeof value.chunk === 'number' &&
      Number.isInteger(value.chunk) &&
      value.chunk >= 0 &&
      'chunks' in value &&
      typeof value.chunks === 'number' &&
      Number.isInteger(value.chunks) &&
      value.chunks > value.chunk &&
      'rendered_len' in value &&
      typeof value.rendered_len === 'number' &&
      'rendered_sha256' in value &&
      typeof value.rendered_sha256 === 'string' &&
      'rows' in value &&
      Array.isArray(value.rows) &&
      value.rows.every((row) => typeof row === 'string') &&
      'content' in value &&
      typeof value.content === 'string' &&
      value.content.length <= chunkSize
    );
  }

  function pendingMail(ctx: ExtensionContext) {
    const groups = new Map<string, MailChunk[]>();
    for (const entry of ctx.sessionManager.getBranch()) {
      if (entry.type === 'custom' && entry.customType === 'session-relay.mail' && isChunk(entry.data)) {
        const group = groups.get(entry.data.token) ?? [];
        group.push(entry.data);
        groups.set(entry.data.token, group);
      }
      const details: unknown =
        entry.type === 'custom_message' && entry.customType === 'relay_mail'
          ? entry.details
          : entry.type === 'message' && entry.message.role === 'toolResult' && entry.message.toolName === 'relay'
            ? entry.message.details
            : undefined;
      if (typeof details === 'object' && details !== null && 'tokens' in details && Array.isArray(details.tokens)) {
        for (const token of details.tokens) if (typeof token === 'string') groups.delete(token);
      }
    }
    const complete = new Map<string, MailChunk[]>();
    for (const [token, group] of groups) {
      group.sort((a, b) => a.chunk - b.chunk);
      const first = group[0];
      if (
        !first ||
        group.length !== first.chunks ||
        group.some(
          (row, index) =>
            row.chunk !== index ||
            row.chunks !== first.chunks ||
            row.rendered_len !== first.rendered_len ||
            row.rendered_sha256 !== first.rendered_sha256,
        )
      )
        continue;
      const rendered = group.map((row) => row.content).join('');
      if (rendered.length === first.rendered_len && sha256(rendered) === first.rendered_sha256)
        complete.set(token, group);
    }
    return complete;
  }

  function pendingPayload(ctx: ExtensionContext) {
    const pending = pendingMail(ctx);
    return {
      content: [...pending.values()].flatMap((chunks) =>
        chunks.map((chunk) => ({ type: 'text' as const, text: chunk.content })),
      ),
      details: { tokens: [...pending.keys()] },
    };
  }

  function stillCurrent(current: Live): boolean {
    return live?.generation === current.generation && current.ctx.sessionManager.getSessionId() === current.sessionId;
  }

  async function finishHold(action: 'ack' | 'rollback', token: string, current: Live): Promise<void> {
    try {
      const result = await run([action, token], current);
      if (result.code !== 0) throw new Error(result.stderr || result.stdout || `exit ${result.code}`);
    } catch (error) {
      if (current.ctx.hasUI) {
        const reason = error instanceof Error ? error.message : String(error);
        current.ctx.ui.notify(`${action} failed: ${reason}`, 'warning');
      }
    }
  }

  async function commit(current: Live, token: string, content: string, rows: string[]): Promise<void> {
    if (!tokenPattern.test(token)) throw new Error('relay returned an invalid hold token');
    const chunks = Math.max(1, Math.ceil(content.length / chunkSize));
    const rendered_sha256 = sha256(content);
    try {
      if (!stillCurrent(current)) throw new Error('session identity changed during hold');
      // These synchronous appends bind the complete payload to this loaded session.
      for (let chunk = 0; chunk < chunks; chunk++) {
        pi.appendEntry('session-relay.mail', {
          token,
          chunk,
          chunks,
          rendered_len: content.length,
          rendered_sha256,
          rows,
          content: content.slice(chunk * chunkSize, (chunk + 1) * chunkSize),
        });
      }
      // The runtime manager exposes flush, although ReadonlySessionManager omits it.
      const manager: unknown = current.ctx.sessionManager;
      if (!manager || typeof manager !== 'object' || !('flush' in manager) || typeof manager.flush !== 'function') {
        throw new Error('session manager does not expose durable flush');
      }
      await manager.flush();
      const persisted = pendingMail(current.ctx).get(token)?.[0];
      if (
        !stillCurrent(current) ||
        persisted?.rendered_len !== content.length ||
        persisted.rendered_sha256 !== rendered_sha256
      ) {
        throw new Error('persisted relay mail failed verification');
      }
    } catch {
      try {
        await finishHold('rollback', token, current);
      } finally {
        if (!reattaching) await reconcile(current.ctx);
      }
      return;
    }
    // Ack failure is safe: the durable entry remains the delivery source of truth.
    await finishHold('ack', token, current);
  }

  async function drain(current: Live, inbox = false, initial = false): Promise<void> {
    const argv = inbox
      ? ['inbox', '--hold', current.sessionId]
      : [
          'hook',
          'omp',
          '--session',
          current.sessionId,
          '--cwd',
          current.cwd,
          ...(initial ? [] : ['--event', 'prompt']),
          '--hold',
        ];
    const result = await run(argv, current);
    if (result.code !== 0) throw new Error(result.stderr || result.stdout);
    if (!result.stdout) return;
    if (inbox) {
      const held: unknown = JSON.parse(result.stdout);
      if (!held || typeof held !== 'object' || !('token' in held))
        throw new Error('relay inbox returned no hold token');
      if (held.token === null) return;
      if (typeof held.token !== 'string' || !('messages' in held) || !Array.isArray(held.messages))
        throw new Error('relay inbox returned an invalid hold');
      const rows: string[] = [];
      for (const row of held.messages)
        if (row && typeof row === 'object' && 'id' in row && typeof row.id === 'string') rows.push(row.id);
      await commit(current, held.token, result.stdout, rows);
    } else {
      const newline = result.stdout.indexOf('\n');
      if (newline < 0) throw new Error('relay hook returned no held mail');
      await commit(current, result.stdout.slice(0, newline), result.stdout.slice(newline + 1), []);
    }
  }

  async function reconcileMail(ctx: ExtensionContext): Promise<void> {
    const current = await reconcile(ctx);
    if (!stillCurrent(current) || !ctx.isIdle() || doorbell) return;
    const pending = pendingMail(ctx);
    if (!pending.size) return;
    doorbell = { generation: current.generation, sentAt: Date.now() };
    pi.sendUserMessage(`[relay] ${pending.size} new message(s)`);
  }

  async function poll(current: Live): Promise<void> {
    if (live?.generation !== current.generation || current.draining) return;
    if (!stillCurrent(current)) {
      await reconcileMail(current.ctx);
      return;
    }
    current.draining = true;
    try {
      const peek = await run(['peek', current.sessionId], current);
      if (peek.code !== 0) throw new Error(peek.stderr || peek.stdout);
      const pending: unknown = JSON.parse(peek.stdout);
      if (!pending || typeof pending !== 'object' || !('count' in pending) || typeof pending.count !== 'number')
        throw new Error('relay peek returned no numeric count');
      if (pending.count > 0 && stillCurrent(current)) await drain(current);
    } finally {
      current.draining = false;
      if (doorbell?.generation === current.generation && current.ctx.isIdle() && Date.now() - doorbell.sentAt > 6000)
        doorbell = null;
      if (live?.generation === current.generation) await reconcileMail(current.ctx);
    }
  }

  async function attach(ctx: ExtensionContext): Promise<Live> {
    const manager: unknown = ctx.sessionManager;
    if (
      manager &&
      typeof manager === 'object' &&
      'ensureOnDisk' in manager &&
      typeof manager.ensureOnDisk === 'function'
    )
      await manager.ensureOnDisk();
    const file = ctx.sessionManager.getSessionFile();
    const current: Live = {
      ctx,
      sessionId: ctx.sessionManager.getSessionId(),
      cwd: ctx.cwd,
      root: path.dirname(file ? path.dirname(file) : ctx.sessionManager.getSessionDir()),
      timer: null,
      generation: ++generation,
      draining: true,
    };
    live = current;
    try {
      await drain(current, false, true);
    } finally {
      current.draining = false;
      if (live?.generation === current.generation) current.timer = ctx.setInterval(() => poll(current), 3000);
    }
    return current;
  }

  function detach(ctx: ExtensionContext): void {
    if (live?.timer !== null && live?.timer !== undefined) ctx.clearTimer(live.timer);
    doorbell = null;
    generation++;
    live = null;
  }

  let reattaching: Promise<Live> | null = null;
  function reconcile(ctx: ExtensionContext): Promise<Live> {
    if (live && live.sessionId === ctx.sessionManager.getSessionId()) return Promise.resolve(live);
    if (reattaching) return reattaching;
    detach(ctx);
    reattaching = attach(ctx).finally(() => {
      reattaching = null;
    });
    return reattaching;
  }

  pi.on('session_start', async (_event, ctx) => {
    await attach(ctx);
    await reconcileMail(ctx);
  });
  pi.on('before_agent_start', async (_event, ctx) => {
    doorbell = null;
    const current = await reconcile(ctx);
    if (!stillCurrent(current)) return;
    if (!current.draining) {
      current.draining = true;
      try {
        await drain(current);
      } finally {
        current.draining = false;
      }
    }
    const payload = pendingPayload(ctx);
    if (payload.details.tokens.length) return { message: { customType: 'relay_mail', ...payload, display: true } };
  });
  pi.on('agent_end', async (event, ctx) => {
    if (!event.willContinue) doorbell = null;
    await reconcileMail(ctx);
  });
  pi.on('session_branch', async (_event, ctx) => {
    doorbell = null;
    await reconcileMail(ctx);
  });
  pi.on('session_tree', async (_event, ctx) => {
    doorbell = null;
    await reconcileMail(ctx);
  });
  pi.on('session_switch', async (_event, ctx) => {
    detach(ctx);
    await attach(ctx);
    await reconcileMail(ctx);
  });
  pi.on('session_shutdown', (_event, ctx) => {
    detach(ctx);
  });

  pi.registerTool({
    name: 'relay',
    label: 'Relay',
    description: 'Cross-session mail. Your session identity is supplied automatically; reply defaults to completed.',
    parameters,
    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      try {
        for (const key of ['to', 'name', 'id'] as const) {
          const value = params[key];
          if (value !== undefined && !identifier.test(value)) {
            throw new Error(`${key} must match ${identifier.source}`);
          }
        }
        if (params.action === 'whoami') {
          return {
            content: [
              { type: 'text', text: JSON.stringify({ sessionId: ctx.sessionManager.getSessionId(), cwd: ctx.cwd }) },
            ],
          };
        }
        // The identity on argv must be the one the runtime reports now: a branch or fork
        // between reconcile and run would otherwise send or claim as the previous session.
        const current = await reconcile(ctx);
        if (!stillCurrent(current)) throw new Error('session identity changed during reconcile; retry');
        if (params.action === 'inbox') {
          if (current.draining) throw new Error('relay inbox drain already in progress');
          current.draining = true;
          try {
            await drain(current, true);
          } finally {
            current.draining = false;
          }
          return pendingPayload(ctx);
        }
        const result = await run(argvFor(params.action, params, current.sessionId), current);
        return {
          content: [{ type: 'text', text: result.stdout || result.stderr }],
          ...(result.code !== 0 ? { isError: true } : {}),
        };
      } catch (error) {
        return {
          content: [{ type: 'text', text: error instanceof Error ? error.message : String(error) }],
          isError: true,
        };
      }
    },
  });

  pi.registerCommand('relay', {
    description: 'Show the relay roster and pending mail count',
    handler: async (_args, ctx) => {
      const current = await reconcile(ctx);
      if (!stillCurrent(current)) throw new Error('session identity changed during reconcile; retry');
      const [roster, peek] = await Promise.all([run(['list'], current), run(['peek', current.sessionId], current)]);
      if (roster.code !== 0) throw new Error(roster.stderr || roster.stdout);
      if (peek.code !== 0) throw new Error(peek.stderr || peek.stdout);
      const pending: unknown = JSON.parse(peek.stdout);
      if (
        typeof pending !== 'object' ||
        pending === null ||
        !('count' in pending) ||
        typeof pending.count !== 'number'
      ) {
        throw new Error('relay peek returned no numeric count');
      }
      if (!stillCurrent(current)) return;
      const text = `${roster.stdout || roster.stderr}\nPending: ${pending.count}`;
      if (ctx.hasUI) ctx.ui.notify(text, 'info');
      else pi.sendMessage({ customType: 'relay_mail', content: text, display: true }, { triggerTurn: false });
    },
  });
  pi.registerMessageRenderer('relay_mail', (message) => {
    const content =
      typeof message.content === 'string'
        ? message.content
        : message.content
            .filter((block) => block.type === 'text')
            .map((block) => block.text)
            .join('');
    return new pi.pi.Text(content, 0, 0);
  });
}
