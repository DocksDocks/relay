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
        return ['inbox', sessionId];
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

  // True while `current` is still the attached session and the runtime still reports its id.
  // Every await in a delivery path re-checks this: a branch or fork changes the id without
  // any event, and mail drained for the old id must never reach the new one.
  function stillCurrent(current: Live): boolean {
    return live?.generation === current.generation && current.ctx.sessionManager.getSessionId() === current.sessionId;
  }

  async function poll(current: Live): Promise<void> {
    if (live?.generation !== current.generation || current.draining) return;
    if (!stillCurrent(current)) {
      await reconcile(current.ctx);
      return;
    }
    current.draining = true;
    try {
      const peek = await run(['peek', current.sessionId], current);
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
      if (pending.count <= 0 || !stillCurrent(current)) return;
      const result = await run(
        ['hook', 'omp', '--session', current.sessionId, '--cwd', current.cwd, '--event', 'prompt'],
        current,
      );
      if (result.code !== 0) throw new Error(result.stderr || result.stdout);
      if (result.stdout && stillCurrent(current)) {
        pi.sendMessage(
          { customType: 'relay_mail', content: result.stdout, display: true },
          { deliverAs: 'followUp', triggerTurn: true },
        );
      }
    } finally {
      current.draining = false;
      if (!stillCurrent(current) && live?.generation === current.generation) await reconcile(current.ctx);
    }
  }

  async function attach(ctx: ExtensionContext): Promise<Live> {
    // A new omp session stays memory-only until its first assistant message. Relay birth
    // detection needs the session header on disk before the hook registers the id.
    const manager: unknown = ctx.sessionManager;
    if (
      manager &&
      typeof manager === 'object' &&
      'ensureOnDisk' in manager &&
      typeof manager.ensureOnDisk === 'function'
    ) {
      await manager.ensureOnDisk();
    }
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
      const result = await run(['hook', 'omp', '--session', current.sessionId, '--cwd', current.cwd], current);
      if (result.code !== 0) throw new Error(result.stderr || result.stdout);
      if (result.stdout && stillCurrent(current)) {
        pi.sendMessage(
          { customType: 'relay_mail', content: result.stdout, display: true },
          { deliverAs: 'followUp', triggerTurn: true },
        );
      }
    } finally {
      current.draining = false;
      // `ctx.setInterval` contains a rejected `poll`; identity drift during this hook is
      // reconciled by the first tick (awaiting `reconcile` here would wait on this attach).
      if (live?.generation === current.generation) current.timer = ctx.setInterval(() => poll(current), 3000);
    }
    return current;
  }

  function detach(ctx: ExtensionContext): void {
    if (live?.timer !== null && live?.timer !== undefined) ctx.clearTimer(live.timer);
    generation++;
    live = null;
  }

  // Branching or forking changes the session id without a switch event; every delivery and
  // tool path re-checks the runtime identity and reattaches when it moved. One reattach runs
  // at a time: concurrent callers share it instead of racing a second attach hook.
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
  });
  pi.on('before_agent_start', async (_event, ctx) => {
    const current = await reconcile(ctx);
    if (current.draining || !stillCurrent(current)) return;
    current.draining = true;
    try {
      const result = await run(
        ['hook', 'omp', '--session', current.sessionId, '--cwd', current.cwd, '--event', 'prompt'],
        current,
      );
      if (result.code !== 0) throw new Error(result.stderr || result.stdout);
      if (result.stdout && stillCurrent(current)) {
        return { message: { customType: 'relay_mail', content: result.stdout, display: true } };
      }
    } finally {
      current.draining = false;
      if (!stillCurrent(current) && live?.generation === current.generation) await reconcile(ctx);
    }
  });
  pi.on('session_switch', async (_event, ctx) => {
    detach(ctx);
    await attach(ctx);
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
            .join('\n');
    return new pi.pi.Text(content, 0, 0);
  });
}
