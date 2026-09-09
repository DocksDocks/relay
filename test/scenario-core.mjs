#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFixture, createScenarioCheck, runScenarioCli } from './selftest-fixture.mjs';

const UUID_V4_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const MESSAGE_V2_KEYS = [
  'body',
  'correlation_id',
  'created_at',
  'from_session_id',
  'id',
  'kind',
  'reply_to',
  'result_sha256',
  'schema',
  'terminal_status',
  'to_session_id',
];
const LEGACY_MCP_INPUT_SCHEMAS = {
  whoami: { type: 'object', properties: {}, additionalProperties: false },
  register: {
    type: 'object',
    properties: {
      name: { type: 'string', description: 'Friendly name to claim, e.g. "frontend" or "agent-A".' },
      id: {
        type: 'string',
        description: 'Override session id (defaults to this session, resolved from the project dir).',
      },
      dir: { type: 'string', description: 'Override project dir (defaults to the launch dir).' },
    },
    required: ['name'],
    additionalProperties: false,
  },
  roster: { type: 'object', properties: {}, additionalProperties: false },
  send: {
    type: 'object',
    properties: {
      to: { type: 'string', description: 'Recipient friendly name or session id (see roster).' },
      body: { type: 'string', description: 'Message text.' },
      from: {
        type: 'string',
        description:
          'Your own registered session id or name (see the identity line injected at session start). Pass it whenever this project dir may host more than one session — the dir-marker fallback mis-attributes the sender in shared dirs.',
      },
    },
    required: ['to', 'body'],
    additionalProperties: false,
  },
  inbox: {
    type: 'object',
    properties: {
      id: {
        type: 'string',
        description:
          "Your own registered session id or name (see the identity line injected at session start). Pass it whenever this project dir may host more than one session — the dir-marker fallback can drain another session's mailbox.",
      },
    },
    additionalProperties: false,
  },
  discover: {
    type: 'object',
    properties: {
      activeWithinMin: {
        type: 'number',
        description: 'Only sessions whose last activity is within this many minutes (default 60).',
      },
      tool: { type: 'string', enum: ['omp'], description: 'Restrict to one tool.' },
    },
    additionalProperties: false,
  },
};

function compactJcsObject(value) {
  return JSON.stringify(
    Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, value[key]]),
    ),
  );
}

function assertMessageV2(message, expected) {
  assert.deepEqual(Object.keys(message).sort(), MESSAGE_V2_KEYS);
  assert.equal(message.schema, 2);
  assert.match(message.id, UUID_V4_RE);
  assert.match(message.correlation_id, UUID_V4_RE);
  assert.match(message.from_session_id, UUID_V4_RE);
  assert.match(message.to_session_id, UUID_V4_RE);
  assert.match(message.created_at, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
  assert.equal(Buffer.byteLength(message.created_at), 24);
  for (const [key, value] of Object.entries(expected)) assert.deepEqual(message[key], value, key);
}

function assertToolMessage(response, expected) {
  assert.equal(response.result.isError, false);
  assert.deepEqual(
    response.result.content.map(({ type }) => type),
    ['text'],
  );
  const text = response.result.content[0].text;
  const message = JSON.parse(text);
  assertMessageV2(message, expected);
  assert.equal(text, compactJcsObject(message), 'MCP typed message text is canonical compact JCS');
  return message;
}

function assertToolDomainError(response, code) {
  assert.deepEqual(response.result, {
    content: [{ type: 'text', text: JSON.stringify({ code }) }],
    isError: true,
  });
}

function assertRpcInvalidParams(response) {
  assert.equal(response.error.code, -32602);
  assert.equal(response.result, undefined);
}

function assertCliDomainError(result, status, code) {
  assert.equal(result.status, status);
  assert.equal(result.stdout, '');
  assert.equal(result.stderr, `${code}\n`);
}

function assertCliOutcome(result, keys, expected) {
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stderr, '');
  const output = JSON.parse(result.stdout);
  assert.deepEqual(Object.keys(output), keys);
  assert.match(output.correlation_id, UUID_V4_RE);
  assert.match(output.message_id, UUID_V4_RE);
  assert.deepEqual(output, { ...output, ...expected });
  assert.equal(result.stdout, `${JSON.stringify(output)}\n`);
  return output;
}

function mcpSchemaShape(schema) {
  return {
    type: schema.type,
    properties: Object.fromEntries(
      Object.entries(schema.properties).map(([name, property]) => [
        name,
        { type: property.type, ...(property.enum === undefined ? {} : { enum: property.enum }) },
      ]),
    ),
    required: schema.required,
    additionalProperties: schema.additionalProperties,
  };
}

function assertMcpToolCatalog(tools) {
  const byName = Object.fromEntries(tools.map((tool) => [tool.name, tool]));
  assert.deepEqual(tools.map(({ name }) => name).sort(), [
    'discover',
    'inbox',
    'register',
    'reply',
    'request',
    'roster',
    'send',
    'whoami',
  ]);
  for (const [name, schema] of Object.entries(LEGACY_MCP_INPUT_SCHEMAS)) {
    assert.deepEqual(
      mcpSchemaShape(byName[name].inputSchema),
      mcpSchemaShape(schema),
      `${name} MCP input schema changed`,
    );
  }
  assert.deepEqual(mcpSchemaShape(byName.request.inputSchema), {
    type: 'object',
    properties: {
      to: { type: 'string' },
      body: { type: 'string' },
      from: { type: 'string' },
    },
    required: ['to', 'body'],
    additionalProperties: false,
  });
  assert.deepEqual(mcpSchemaShape(byName.reply.inputSchema), {
    type: 'object',
    properties: {
      correlation_id: { type: 'string' },
      status: { type: 'string', enum: ['completed', 'failed'] },
      body: { type: 'string' },
      from: { type: 'string' },
    },
    required: ['correlation_id', 'status', 'body'],
    additionalProperties: false,
  });
}

function assertCorrelatedCliMcpContracts({ HOME, relay, runHook, runBus, peek, tools }) {
  const requesterId = 'a1111111-1111-4111-8111-111111111111';
  const responderId = 'b2222222-2222-4222-8222-222222222222';
  const otherId = 'c3333333-3333-4333-8333-333333333333';
  const unknownCorrelation = 'f7777777-7777-4777-8777-777777777777';
  const requesterDir = path.join(HOME, 'protocol-requester');
  const responderDir = path.join(HOME, 'protocol-responder');
  const otherDir = path.join(HOME, 'protocol-other');
  for (const dir of [requesterDir, responderDir, otherDir]) fs.mkdirSync(dir);
  for (const [name, id, dir] of [
    ['protocol-a', requesterId, requesterDir],
    ['protocol-b', responderId, responderDir],
    ['protocol-c', otherId, otherDir],
  ]) {
    const registered = relay(['register', name, '--id', id, '--dir', dir]);
    assert.equal(registered.status, 0, registered.stderr);
  }
  for (const [sessionId, cwd] of [
    [requesterId, requesterDir],
    [responderId, responderDir],
    [otherId, otherDir],
  ]) {
    const hooked = runHook({ session_id: sessionId, cwd, hook_event_name: 'SessionStart', source: 'startup' });
    assert.equal(hooked.status, 0, hooked.stderr);
  }

  assertMcpToolCatalog(tools);

  const cliRequest = relay(['request', 'protocol-b', '--from', 'protocol-a', '--', 'cli request']);
  const requestOutcome = assertCliOutcome(cliRequest, ['correlation_id', 'message_id', 'outcome'], {
    outcome: 'enqueued',
  });
  const queuedRequest = peek('protocol-b');
  assert.equal(queuedRequest.count, 1);
  assert.equal(queuedRequest.messages[0].id, requestOutcome.message_id);
  assertMessageV2(queuedRequest.messages[0], {
    body: 'cli request',
    correlation_id: requestOutcome.correlation_id,
    from_session_id: requesterId,
    kind: 'request',
    reply_to: null,
    result_sha256: null,
    terminal_status: null,
    to_session_id: responderId,
  });

  const cliUnknown = relay([
    'reply',
    unknownCorrelation,
    '--from',
    'protocol-b',
    '--status',
    'completed',
    '--',
    'none',
  ]);
  assertCliDomainError(cliUnknown, 1, 'unknown_correlation');

  const cliUnauthorized = relay([
    'reply',
    requestOutcome.correlation_id,
    '--from',
    'protocol-c',
    '--status',
    'completed',
    '--',
    'forged',
  ]);
  assertCliDomainError(cliUnauthorized, 1, 'unauthorized_responder');
  assert.equal(peek('protocol-a').count, 0);

  const replyArgs = [
    'reply',
    requestOutcome.correlation_id,
    '--from',
    'protocol-b',
    '--status',
    'completed',
    '--',
    'cli reply',
  ];
  const cliReply = relay(replyArgs);
  const replyOutcome = assertCliOutcome(cliReply, ['correlation_id', 'message_id', 'outcome', 'status'], {
    correlation_id: requestOutcome.correlation_id,
    outcome: 'enqueued',
    status: 'completed',
  });
  const queuedReply = peek('protocol-a');
  assert.equal(queuedReply.count, 1);
  assert.equal(queuedReply.messages[0].id, replyOutcome.message_id);
  assertMessageV2(queuedReply.messages[0], {
    body: 'cli reply',
    correlation_id: requestOutcome.correlation_id,
    from_session_id: responderId,
    kind: 'terminal_reply',
    reply_to: requestOutcome.message_id,
    result_sha256: null,
    terminal_status: 'completed',
    to_session_id: requesterId,
  });

  const cliExactRetry = relay(replyArgs);
  assert.equal(cliExactRetry.status, 0, cliExactRetry.stderr);
  assert.equal(cliExactRetry.stdout, cliReply.stdout);
  assert.equal(cliExactRetry.stderr, '');
  assert.equal(peek('protocol-a').count, 1, 'exact retry must not enqueue a second terminal reply');

  const cliCompetitor = relay([
    'reply',
    requestOutcome.correlation_id,
    '--from',
    'protocol-b',
    '--status',
    'failed',
    '--',
    'changed reply',
  ]);
  assertCliDomainError(cliCompetitor, 2, 'correlation_conflict');
  assert.equal(peek('protocol-a').count, 1, 'competing claim must not enqueue');

  const failedRequestRun = relay(['request', 'protocol-b', '--from', 'protocol-a', 'failure request']);
  const failedRequest = assertCliOutcome(failedRequestRun, ['correlation_id', 'message_id', 'outcome'], {
    outcome: 'enqueued',
  });
  const failedReplyRun = relay([
    'reply',
    failedRequest.correlation_id,
    '--from',
    'protocol-b',
    '--status',
    'failed',
    'could not complete',
  ]);
  const failedReply = assertCliOutcome(failedReplyRun, ['correlation_id', 'message_id', 'outcome', 'status'], {
    correlation_id: failedRequest.correlation_id,
    outcome: 'enqueued',
    status: 'failed',
  });
  const failedEnvelope = peek('protocol-a').messages.find(({ id }) => id === failedReply.message_id);
  assertMessageV2(failedEnvelope, {
    body: 'could not complete',
    correlation_id: failedRequest.correlation_id,
    from_session_id: responderId,
    kind: 'terminal_reply',
    reply_to: failedRequest.message_id,
    result_sha256: null,
    terminal_status: 'failed',
    to_session_id: requesterId,
  });

  // Documented CLI defaults: omitted --from resolves the registered
  // current-project self identity via the cwd marker (same dir-marker
  // fallback as the MCP bus), and request --json emits the complete
  // canonical MessageV2 envelope instead of the three-key outcome.
  const cliSelfRequest = relay(['request', 'protocol-b', '--', 'self request'], { cwd: requesterDir });
  const selfRequestOutcome = assertCliOutcome(cliSelfRequest, ['correlation_id', 'message_id', 'outcome'], {
    outcome: 'enqueued',
  });
  const selfQueuedRequest = peek('protocol-b').messages.find(({ id }) => id === selfRequestOutcome.message_id);
  assertMessageV2(selfQueuedRequest, {
    body: 'self request',
    correlation_id: selfRequestOutcome.correlation_id,
    from_session_id: requesterId,
    kind: 'request',
    reply_to: null,
    result_sha256: null,
    terminal_status: null,
    to_session_id: responderId,
  });

  const cliSelfReply = relay(
    ['reply', selfRequestOutcome.correlation_id, '--status', 'completed', '--', 'self reply'],
    { cwd: responderDir },
  );
  const selfReplyOutcome = assertCliOutcome(cliSelfReply, ['correlation_id', 'message_id', 'outcome', 'status'], {
    correlation_id: selfRequestOutcome.correlation_id,
    outcome: 'enqueued',
    status: 'completed',
  });
  const selfQueuedReply = peek('protocol-a').messages.find(({ id }) => id === selfReplyOutcome.message_id);
  assertMessageV2(selfQueuedReply, {
    body: 'self reply',
    correlation_id: selfRequestOutcome.correlation_id,
    from_session_id: responderId,
    kind: 'terminal_reply',
    reply_to: selfRequestOutcome.message_id,
    result_sha256: null,
    terminal_status: 'completed',
    to_session_id: requesterId,
  });

  const cliJsonRequest = relay(['request', 'protocol-b', '--json', '--', 'canonical json request'], {
    cwd: requesterDir,
  });
  assert.equal(cliJsonRequest.status, 0, cliJsonRequest.stderr);
  assert.equal(cliJsonRequest.stderr, '');
  const jsonEnvelope = JSON.parse(cliJsonRequest.stdout);
  assertMessageV2(jsonEnvelope, {
    body: 'canonical json request',
    from_session_id: requesterId,
    kind: 'request',
    reply_to: null,
    result_sha256: null,
    terminal_status: null,
    to_session_id: responderId,
  });
  assert.equal(
    cliJsonRequest.stdout,
    `${compactJcsObject(jsonEnvelope)}\n`,
    'request --json emits the complete canonical MessageV2',
  );
  const queuedJsonEnvelope = peek('protocol-b').messages.find(({ id }) => id === jsonEnvelope.id);
  assert.deepEqual(queuedJsonEnvelope, jsonEnvelope, 'emitted --json envelope matches the queued request');

  const cliJsonExplicitFrom = relay([
    'request',
    'protocol-b',
    '--from',
    'protocol-a',
    '--json',
    '--',
    'explicit from json request',
  ]);
  assert.equal(cliJsonExplicitFrom.status, 0, cliJsonExplicitFrom.stderr);
  const explicitJsonEnvelope = JSON.parse(cliJsonExplicitFrom.stdout);
  assertMessageV2(explicitJsonEnvelope, {
    body: 'explicit from json request',
    from_session_id: requesterId,
    kind: 'request',
    to_session_id: responderId,
  });
  assert.equal(cliJsonExplicitFrom.stdout, `${compactJcsObject(explicitJsonEnvelope)}\n`);

  // A literal `--json` inside the message body must not flip the output mode.
  const cliBodyJson = relay(['request', 'protocol-b', '--from', 'protocol-a', '--', 'pass', '--json', 'literally']);
  const bodyJsonOutcome = assertCliOutcome(cliBodyJson, ['correlation_id', 'message_id', 'outcome'], {
    outcome: 'enqueued',
  });
  const bodyJsonEnvelope = peek('protocol-b').messages.find(({ id }) => id === bodyJsonOutcome.message_id);
  assert.equal(bodyJsonEnvelope.body, 'pass --json literally');

  // Invalid identities: an unknown explicit --from and a cwd without a
  // session marker both fail closed with protocol_store_error, enqueue nothing.
  assertCliDomainError(
    relay(['request', 'protocol-b', '--from', 'no-such-identity', '--', 'unknown sender']),
    1,
    'protocol_store_error',
  );
  const noMarkerDir = path.join(HOME, 'protocol-no-marker');
  fs.mkdirSync(noMarkerDir);
  const enqueuedBefore = peek('protocol-b').count;
  assertCliDomainError(
    relay(['request', 'protocol-b', '--', 'nobody home'], { cwd: noMarkerDir }),
    1,
    'protocol_store_error',
  );
  assertCliDomainError(
    relay(['reply', selfRequestOutcome.correlation_id, '--status', 'completed', '--', 'nobody home'], {
      cwd: noMarkerDir,
    }),
    1,
    'protocol_store_error',
  );
  assert.equal(peek('protocol-b').count, enqueuedBefore, 'failed identity resolution must not enqueue');

  const malformed = runBus(requesterDir, [
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
    {
      jsonrpc: '2.0',
      id: 2,
      method: 'tools/call',
      params: { name: 'request', arguments: { to: 'protocol-b' } },
    },
    {
      jsonrpc: '2.0',
      id: 3,
      method: 'tools/call',
      params: { name: 'request', arguments: { to: 'protocol-b', body: 'x', unexpected: true } },
    },
    {
      jsonrpc: '2.0',
      id: 4,
      method: 'tools/call',
      params: {
        name: 'reply',
        arguments: { correlation_id: unknownCorrelation, status: 'done', body: 'x', from: 'protocol-b' },
      },
    },
  ]);
  for (const id of [2, 3, 4]) assertRpcInvalidParams(malformed.get(id));

  const mcpRequestResponse = runBus(requesterDir, [
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
    {
      jsonrpc: '2.0',
      id: 2,
      method: 'tools/call',
      params: { name: 'request', arguments: { to: 'protocol-b', body: 'mcp request' } },
    },
  ]).get(2);
  const mcpRequest = assertToolMessage(mcpRequestResponse, {
    body: 'mcp request',
    from_session_id: requesterId,
    kind: 'request',
    reply_to: null,
    result_sha256: null,
    terminal_status: null,
    to_session_id: responderId,
  });

  const mcpReplyArgs = {
    correlation_id: mcpRequest.correlation_id,
    status: 'completed',
    body: 'mcp reply',
    from: 'protocol-b',
  };
  const responderCalls = runBus(responderDir, [
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
    {
      jsonrpc: '2.0',
      id: 2,
      method: 'tools/call',
      params: {
        name: 'reply',
        arguments: {
          correlation_id: unknownCorrelation,
          status: 'completed',
          body: 'none',
          from: 'protocol-b',
        },
      },
    },
    {
      jsonrpc: '2.0',
      id: 3,
      method: 'tools/call',
      params: {
        name: 'reply',
        arguments: { ...mcpReplyArgs, body: 'forged', from: 'protocol-c' },
      },
    },
    { jsonrpc: '2.0', id: 4, method: 'tools/call', params: { name: 'reply', arguments: mcpReplyArgs } },
    { jsonrpc: '2.0', id: 5, method: 'tools/call', params: { name: 'reply', arguments: mcpReplyArgs } },
    {
      jsonrpc: '2.0',
      id: 6,
      method: 'tools/call',
      params: { name: 'reply', arguments: { ...mcpReplyArgs, body: 'different' } },
    },
  ]);
  assertToolDomainError(responderCalls.get(2), 'unknown_correlation');
  assertToolDomainError(responderCalls.get(3), 'unauthorized_responder');
  const mcpReply = assertToolMessage(responderCalls.get(4), {
    body: 'mcp reply',
    correlation_id: mcpRequest.correlation_id,
    from_session_id: responderId,
    kind: 'terminal_reply',
    reply_to: mcpRequest.id,
    result_sha256: null,
    terminal_status: 'completed',
    to_session_id: requesterId,
  });
  assert.equal(responderCalls.get(5).result.isError, false);
  assert.equal(responderCalls.get(5).result.content[0].text, responderCalls.get(4).result.content[0].text);
  assert.equal(JSON.parse(responderCalls.get(5).result.content[0].text).id, mcpReply.id);
  assertToolDomainError(responderCalls.get(6), 'correlation_conflict');
  assert.equal(
    peek('protocol-a').messages.filter(({ correlation_id }) => correlation_id === mcpRequest.correlation_id).length,
    1,
  );

  const brokenHome = path.join(HOME, 'protocol-store-error-home');
  const brokenRequesterDir = path.join(brokenHome, 'requester');
  const brokenResponderDir = path.join(brokenHome, 'responder');
  const brokenRequesterId = 'd4444444-4444-4444-8444-444444444444';
  const brokenResponderId = 'e5555555-5555-4555-8555-555555555555';
  fs.mkdirSync(brokenRequesterDir, { recursive: true });
  fs.mkdirSync(brokenResponderDir);
  const brokenEnv = { AGENT_RELAY_HOME: brokenHome };
  assert.equal(
    relay(['register', 'broken-a', '--id', brokenRequesterId, '--dir', brokenRequesterDir], {
      env: brokenEnv,
    }).status,
    0,
  );
  assert.equal(
    relay(['register', 'broken-b', '--id', brokenResponderId, '--dir', brokenResponderDir], {
      env: brokenEnv,
    }).status,
    0,
  );
  fs.writeFileSync(path.join(brokenHome, 'protocol-v1'), 'not a directory');
  const cliStoreFailure = relay(['request', 'broken-b', '--from', 'broken-a', '--', 'cannot persist'], {
    env: brokenEnv,
  });
  assertCliDomainError(cliStoreFailure, 1, 'protocol_store_error');
  const mcpStoreFailure = runBus(
    brokenRequesterDir,
    [
      { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
      {
        jsonrpc: '2.0',
        id: 2,
        method: 'tools/call',
        params: { name: 'request', arguments: { to: 'broken-b', body: 'cannot persist', from: 'broken-a' } },
      },
    ],
    brokenEnv,
  );
  assertToolDomainError(mcpStoreFailure.get(2), 'protocol_store_error');
}

export const EXPECTED_LABELS = [
  '--version prints the exact Cargo package version',
  'hook seeds marker + registration for both sessions (exit 0)',
  'register CLI names both sessions',
  'initialize negotiates protocol + serverInfo',
  'bus catalog and request/reply preserve correlated delivery contracts',
  'whoami resolves this session from the cwd marker',
  'roster lists both registered sessions',
  'send to agent-B reports ok + correct recipient dir',
  "message landed in agent-B's mailbox tagged with the sender (peek is read-only)",
  'hook exits 0',
  'hook injects pending mail as omp context',
  'hook drained the inbox (no redelivery)',
  'send CLI queues to an explicit --id target',
  'inbox() returns then clears pending messages',
  'held mail rolls back for redelivery and ack commits delivery',
  'non-attach verbs still treat --exec as a value flag',
  'send to an unknown recipient returns isError',
  'registry pins explicit and default tools to omp',
  'AGENT_RELAY_HOME takes precedence over SESSION_RELAY_HOME',
  'wake --dry resumes omp with model and thinking before the prompt fence',
  'attach directly resumes omp under the resume lock without printing a command',
  'attach --exec inherits stdin/stdout/stderr and holds the resume lock',
  'attach strictly rejects extra operands, unknown flags, and exec after --',
  'attach refuses a missing stored dir before launch',
  'attach rejects an unresolved non-UUID id',
  'attach fails closed when the resume lock cannot be probed',
  'wake preserves child stdout and stderr bytes',
  'wake preserves no-trailing-newline stdout and child exit code',
];

export async function run({ bin, home, emit }) {
  const fixture = createFixture({ bin, home });
  const labels = [];
  let check;
  let scenarioResult;
  let hasPrimaryError = false;
  let primaryError;
  const {
    home: HOME,
    cargoVersion: CARGO_VERSION,
    relay,
    relayBytes,
    relayJSON,
    runHook,
    runBus,
    toolJSON,
    peek,
  } = fixture;

  try {
    check = createScenarioCheck({ emit, labels });
    const dirA = path.join(HOME, 'proj-a');
    const dirB = path.join(HOME, 'proj-b');
    fs.mkdirSync(dirA, { recursive: true });
    fs.mkdirSync(dirB, { recursive: true });
    const idA = '11111111-1111-1111-1111-111111111111';
    const idB = '22222222-2222-2222-2222-222222222222';

    check('--version prints the exact Cargo package version', () => {
      const result = relay(['--version']);
      assert.equal(result.status, 0, `relay --version exited ${result.status}: ${result.stderr}`);
      assert.equal(result.stdout, `session-relay ${CARGO_VERSION}\n`);
      assert.equal(result.stderr, '');
    });

    check('hook seeds marker + registration for both sessions (exit 0)', () => {
      assert.equal(
        runHook({ session_id: idA, cwd: dirA, hook_event_name: 'SessionStart', source: 'startup' }).status,
        0,
      );
      assert.equal(
        runHook({ session_id: idB, cwd: dirB, hook_event_name: 'SessionStart', source: 'startup' }).status,
        0,
      );
    });
    check('register CLI names both sessions', () => {
      assert.equal(relay(['register', 'agent-A', '--id', idA, '--dir', dirA]).status, 0);
      assert.equal(relay(['register', 'agent-B', '--id', idB, '--dir', dirB]).status, 0);
    });

    const reqs = [
      {
        jsonrpc: '2.0',
        id: 1,
        method: 'initialize',
        params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'selftest', version: '1' } },
      },
      { jsonrpc: '2.0', method: 'notifications/initialized' },
      { jsonrpc: '2.0', id: 2, method: 'tools/list' },
      { jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name: 'whoami', arguments: {} } },
      { jsonrpc: '2.0', id: 4, method: 'tools/call', params: { name: 'roster', arguments: {} } },
      {
        jsonrpc: '2.0',
        id: 5,
        method: 'tools/call',
        params: { name: 'send', arguments: { to: 'agent-B', body: 'hello from A' } },
      },
    ];
    const res = runBus(dirA, reqs);

    check('initialize negotiates protocol + serverInfo', () => {
      assert.equal(res.get(1).result.protocolVersion, '2025-06-18');
      assert.equal(res.get(1).result.serverInfo.name, 'session-relay-bus');
      assert.ok(res.get(1).result.capabilities.tools);
    });
    check('bus catalog and request/reply preserve correlated delivery contracts', () => {
      const names = res
        .get(2)
        .result.tools.filter(({ name }) => name !== 'request' && name !== 'reply')
        .map((tool) => tool.name)
        .sort();
      assert.deepEqual(names, ['discover', 'inbox', 'register', 'roster', 'send', 'whoami']);
      assertCorrelatedCliMcpContracts({
        HOME,
        relay,
        runHook,
        runBus,
        peek,
        tools: res.get(2).result.tools,
      });
    });
    check('whoami resolves this session from the cwd marker', () => {
      const me = toolJSON(res.get(3));
      assert.equal(me.registered, true);
      assert.equal(me.id, idA);
      assert.equal(me.name, 'agent-A');
    });
    check('roster lists both registered sessions', () => {
      const { agents } = toolJSON(res.get(4));
      assert.deepEqual(agents.map((agent) => agent.name).sort(), ['agent-A', 'agent-B']);
    });
    check('send to agent-B reports ok + correct recipient dir', () => {
      const result = toolJSON(res.get(5));
      assert.equal(result.ok, true);
      assert.equal(result.delivered_to, 'agent-B');
      assert.equal(result.recipient_dir, dirB);
    });
    check("message landed in agent-B's mailbox tagged with the sender (peek is read-only)", () => {
      const mail = peek('agent-B');
      assert.equal(mail.count, 1);
      assert.equal(mail.messages[0].body, 'hello from A');
      assert.equal(mail.messages[0].fromName, 'agent-A');
      assert.equal(peek('agent-B').count, 1);
    });

    const hookRun = runHook({ session_id: idB, cwd: dirB, hook_event_name: 'SessionStart', source: 'resume' });
    check('hook exits 0', () => assert.equal(hookRun.status, 0));
    check('hook injects pending mail as omp context', () => {
      assert.ok(hookRun.stdout.includes('hello from A'));
    });
    check('hook drained the inbox (no redelivery)', () => assert.equal(peek('agent-B').count, 0));

    check('send CLI queues to an explicit --id target', () => {
      assert.equal(relay(['send', '--id', idB, '--', 'second message']).status, 0);
    });
    const res2 = runBus(dirB, [
      { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18' } },
      { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'inbox', arguments: {} } },
    ]);
    check('inbox() returns then clears pending messages', () => {
      const box = toolJSON(res2.get(2));
      assert.equal(box.count, 1);
      assert.equal(box.messages[0].body, 'second message');
      assert.equal(peek('agent-B').count, 0);
    });
    check('held mail rolls back for redelivery and ack commits delivery', () => {
      assert.equal(relay(['send', 'agent-B', '--', 'held message']).status, 0);
      const held = relayJSON(['inbox', '--hold', '60', 'agent-B']);
      assert.deepEqual(
        held.messages.map(({ body }) => body),
        ['held message'],
      );
      assert.equal(relayJSON(['inbox', 'agent-B']).count, 0);
      assert.equal(relay(['rollback', held.token]).status, 0);
      const retried = relayJSON(['inbox', '--hold', '60', 'agent-B']);
      assert.deepEqual(retried.messages, held.messages);
      assert.equal(relay(['ack', retried.token]).status, 0);
      assert.equal(relayJSON(['inbox', 'agent-B']).count, 0);
    });
    check('non-attach verbs still treat --exec as a value flag', () => {
      const result = relay(['send', 'agent-B', '--exec', 'must-not-send']);
      assert.equal(result.status, 1);
      assert.match(result.stderr, /usage: relay send/);
      assert.equal(peek('agent-B').count, 0);
    });

    const res3 = runBus(dirA, [
      { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
      { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'send', arguments: { to: 'ghost', body: 'x' } } },
    ]);
    check('send to an unknown recipient returns isError', () => {
      assert.equal(res3.get(2).result.isError, true);
    });

    const dirC = path.join(HOME, 'proj-c');
    const idC = '33333333-3333-3333-3333-333333333333';
    fs.mkdirSync(dirC, { recursive: true });
    assert.equal(relay(['register', 'omp-C', '--id', idC, '--dir', dirC, '--tool', 'omp']).status, 0);
    check('registry pins explicit and default tools to omp', () => {
      const { agents } = toolJSON(
        runBus(dirA, [
          { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
          { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'roster', arguments: {} } },
        ]).get(2),
      );
      const byName = Object.fromEntries(agents.map((agent) => [agent.name, agent.tool]));
      assert.equal(byName['omp-C'], 'omp');
      assert.equal(byName['agent-A'], 'omp');
    });
    check('AGENT_RELAY_HOME takes precedence over SESSION_RELAY_HOME', () => {
      const alternateHome = path.join(HOME, 'alt-home');
      const precedenceId = '77777777-7777-7777-7777-777777777777';
      assert.equal(
        relay(['register', 'prec', '--id', precedenceId, '--dir', dirA], {
          env: { AGENT_RELAY_HOME: alternateHome },
        }).status,
        0,
      );
      const alternateRegistry = JSON.parse(fs.readFileSync(path.join(alternateHome, 'registry.json'), 'utf8'));
      assert.ok(alternateRegistry.agents[precedenceId], 'registered into the AGENT_RELAY_HOME store');
      const registry = JSON.parse(fs.readFileSync(path.join(HOME, 'registry.json'), 'utf8'));
      assert.ok(!registry.agents[precedenceId], 'legacy-alias store untouched');
    });
    check('wake --dry resumes omp with model and thinking before the prompt fence', () => {
      const dryRun = relayJSON([
        'wake',
        'omp-C',
        '--model',
        'test-model',
        '--effort',
        'high',
        '--dry',
        '--',
        '-prompt',
      ]);
      assert.equal(dryRun.tool, 'omp');
      assert.deepEqual(dryRun.args, [
        '-p',
        '--resume',
        idC,
        '--mode',
        'json',
        '--model',
        'test-model',
        '--thinking',
        'high',
        '--',
        '-prompt',
      ]);
      assert.equal(dryRun.cwd, dirC);
      assert.match(dryRun.cmd, /^omp /);
    });

    const stubDir = path.join(HOME, 'omp-stub');
    fs.mkdirSync(stubDir);
    const stub = path.join(stubDir, 'omp');
    fs.writeFileSync(
      stub,
      `#!/usr/bin/env node
const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
if (process.env.ATTACH_RECORD) {
  const competing = spawnSync(process.env.RELAY_BIN, ['attach', 'agent-A', '--exec'], {
    encoding: 'utf8', env: { ...process.env, ATTACH_RECORD: '' },
  });
  fs.writeFileSync(process.env.ATTACH_RECORD, JSON.stringify({
    argv: process.argv.slice(2), cwd: process.cwd(), stdin: fs.readFileSync(0, 'utf8'),
    competing: { status: competing.status, stderr: competing.stderr },
  }));
}
process.stdout.write(process.env.CHILD_STDOUT || '');
process.stderr.write(process.env.CHILD_STDERR || '');
process.exit(Number(process.env.CHILD_STATUS || 0));
`,
      { mode: 0o755 },
    );
    const childEnv = { PATH: `${stubDir}${path.delimiter}${process.env.PATH}`, RELAY_BIN: path.resolve(bin) };

    check('attach directly resumes omp under the resume lock without printing a command', () => {
      const record = path.join(HOME, 'attach-default.json');
      const result = relay(['attach', 'agent-A'], { env: { ...childEnv, ATTACH_RECORD: record } });
      assert.equal(result.status, 0, result.stderr);
      const observed = JSON.parse(fs.readFileSync(record, 'utf8'));
      assert.deepEqual(observed.argv, ['--resume', idA]);
      assert.equal(observed.cwd, dirA);
      assert.equal(observed.competing.status, 3);
      assert.match(observed.competing.stderr, /resume lock held/);
      assert.equal(result.stdout, '');
    });

    check('attach --exec inherits stdin/stdout/stderr and holds the resume lock', () => {
      const record = path.join(HOME, 'attach-exec.json');
      const result = relay(['attach', 'agent-A', '--exec'], {
        input: 'interactive-input',
        env: { ...childEnv, ATTACH_RECORD: record, CHILD_STDOUT: 'attach-out', CHILD_STDERR: 'attach-err' },
      });
      assert.equal(result.status, 0, result.stderr);
      const observed = JSON.parse(fs.readFileSync(record, 'utf8'));
      assert.deepEqual(observed.argv, ['--resume', idA]);
      assert.equal(observed.cwd, dirA);
      assert.equal(observed.stdin, 'interactive-input');
      assert.equal(observed.competing.status, 3);
      assert.match(observed.competing.stderr, /resume lock held/);
      assert.equal(result.stdout, 'attach-out');
      assert.match(result.stderr, /attach-err/);
      const again = relay(['attach', 'agent-A', '--exec'], { env: childEnv });
      assert.equal(again.status, 0, again.stderr);
    });

    check('attach strictly rejects extra operands, unknown flags, and exec after --', () => {
      for (const args of [
        ['attach', 'omp-C', 'extra'],
        ['attach', 'omp-C', '--bogus'],
        ['attach', 'omp-C', '--', '--exec'],
      ]) {
        const result = relay(args, { env: childEnv });
        assert.equal(result.status, 2);
        assert.match(result.stderr, /usage: relay attach/);
      }
    });

    check('attach refuses a missing stored dir before launch', () => {
      const missingId = '53535353-5353-4353-8353-535353535353';
      assert.equal(
        relay(['register', 'missing-attach', '--id', missingId, '--dir', path.join(HOME, 'missing-dir')]).status,
        0,
      );
      const result = relay(['attach', 'missing-attach', '--exec'], { env: childEnv });
      assert.equal(result.status, 1);
      assert.match(result.stderr, /stored dir does not exist/);
    });

    check('attach rejects an unresolved non-UUID id', () => {
      const result = relay(['attach', 'not-a-session-id']);
      assert.equal(result.status, 1);
      assert.match(result.stderr, /session UUID/);
    });

    check('attach fails closed when the resume lock cannot be probed', () => {
      const unknownId = '54545454-5454-4454-8454-545454545454';
      assert.equal(relay(['register', 'unknown-attach-lock', '--id', unknownId, '--dir', dirA]).status, 0);
      const lock = path.join(HOME, 'locks', `resume-${unknownId}.lock`);
      fs.mkdirSync(lock);
      try {
        const result = relay(['attach', 'unknown-attach-lock', '--exec'], { env: childEnv });
        assert.equal(result.status, 4);
        assert.match(result.stderr, /cannot verify resume lock state/);
      } finally {
        fs.rmdirSync(lock);
      }
    });

    check('wake preserves child stdout and stderr bytes', () => {
      const result = relayBytes(['wake', 'agent-A', '--model', 'test-model', '--', 'ping'], {
        env: { RELAY_WAKE_CMD_OMP: stub, CHILD_STDOUT: '{"message":"hello"}\n', CHILD_STDERR: 'child diagnostic\n' },
      });
      assert.equal(result.status, 0);
      assert.deepEqual(result.stdout, Buffer.from('{"message":"hello"}\n'));
      assert.deepEqual(result.stderr, Buffer.from('child diagnostic\n'));
    });
    check('wake preserves no-trailing-newline stdout and child exit code', () => {
      const result = relayBytes(['wake', 'agent-A', '--model', 'test-model', '--', 'ping'], {
        env: { RELAY_WAKE_CMD_OMP: stub, CHILD_STDOUT: 'not json', CHILD_STATUS: '7' },
      });
      assert.equal(result.status, 7);
      assert.deepEqual(result.stdout, Buffer.from('not json'));
      assert.deepEqual(result.stderr, Buffer.alloc(0));
    });

    assert.deepEqual(labels, EXPECTED_LABELS);
    scenarioResult = { count: labels.length, labels };
  } catch (error) {
    hasPrimaryError = true;
    primaryError = error;
  }

  let hasCleanupError = false;
  let cleanupError;
  try {
    await fixture.cleanup();
  } catch (error) {
    hasCleanupError = true;
    cleanupError = error;
  }

  const failures = [];
  if (hasPrimaryError) failures.push(primaryError);
  if (hasCleanupError) failures.push(cleanupError);
  if (failures.length === 1) throw failures[0];
  if (failures.length > 1) {
    const firstFailure = failures[0];
    const messages = failures.map((failure) => (failure instanceof Error ? failure.message : String(failure)));
    throw new AggregateError(failures, messages.join('\n'), { cause: firstFailure });
  }
  return scenarioResult;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await runScenarioCli({ scenario: 'core', run });
}
