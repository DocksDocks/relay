#!/usr/bin/env node
import assert from 'node:assert/strict';
import ext from '../plugin/extension/index.ts';

const events = [];
const tools = [];
const commands = [];
const renderers = [];
let execCalls = 0;

const pi = {
  on(name) {
    events.push(name);
  },
  registerTool(tool) {
    tools.push(tool.name);
  },
  registerCommand(name) {
    commands.push(name);
  },
  registerMessageRenderer(name) {
    renderers.push(name);
  },
  typebox: { Type: new Proxy({}, { get: () => () => ({}) }) },
  exec() {
    execCalls += 1;
    throw new Error('extension factory must not spawn a process');
  },
};

ext(pi);

assert.deepEqual(events, ['session_start', 'before_agent_start', 'session_switch', 'session_shutdown']);
assert.deepEqual(tools, ['relay']);
assert.deepEqual(commands, ['relay']);
assert.deepEqual(renderers, ['relay_mail']);
assert.equal(execCalls, 0, 'extension factory must not execute a process');

console.log(
  `PASS extension_smoke events=${events.join(',')} tools=${tools.join(',')} commands=${commands.join(',')} renderers=${renderers.join(',')}`,
);
