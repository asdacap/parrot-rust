// Actual installed Connect gRPC-Web client, unchanged generated TypeScript.
// This is Node fetch/Connect execution, not browser-engine execution.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { createRequire } = require('node:module');

async function main() {
  const [baseUrl, workspace, modules, output] = process.argv.slice(2);
  const installed = createRequire(path.join(modules, '../package.json'));
  const ts = installed('typescript');
  const { createClient, Code, ConnectError } = installed('@connectrpc/connect');
  const { createGrpcWebTransport } = installed('@connectrpc/connect-web');
  fs.mkdirSync(output, { recursive: true });
  fs.symlinkSync(modules, path.join(output, 'node_modules'), 'dir');
  const inputs = [];
  for (const name of ['parrot', 'web', 'process', 'queue']) {
    const input = path.join(workspace, `web/src/gen/${name}_pb.ts`);
    const source = fs.readFileSync(input);
    inputs.push({ path: `web/src/gen/${name}_pb.ts`, sha256: crypto.createHash('sha256').update(source).digest('hex') });
    const compiled = ts.transpileModule(source.toString(), {
      compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
      fileName: input,
      reportDiagnostics: true,
    });
    assert.equal(compiled.diagnostics?.length ?? 0, 0);
    fs.writeFileSync(path.join(output, `${name}_pb.js`), compiled.outputText);
  }
  const packages = ['typescript', '@bufbuild/protobuf', '@connectrpc/connect', '@connectrpc/connect-web'].map(name => {
    const bytes = fs.readFileSync(path.join(modules, name, 'package.json'));
    const entry = installed.resolve(name);
    return { name, version: JSON.parse(bytes).version, package_json_sha256: crypto.createHash('sha256').update(bytes).digest('hex'), module_entry: entry, module_entry_sha256: crypto.createHash('sha256').update(fs.readFileSync(entry)).digest('hex') };
  });
  const { Parrot } = require(path.join(output, 'parrot_pb.js'));
  const { ParrotWeb } = require(path.join(output, 'web_pb.js'));
  const transportEntry = path.join(path.dirname(installed.resolve('@connectrpc/connect-web')), 'grpc-web-transport.js');
  const transportModule = { path: transportEntry, sha256: crypto.createHash('sha256').update(fs.readFileSync(transportEntry)).digest('hex') };
  const progress = stage => fs.writeFileSync(path.join(output, 'progress.json'), JSON.stringify({ stage, inputs, packages, transportModule }, null, 2) + '\n');
  const deadline = () => AbortSignal.timeout(3000);
  progress('inputs_ready');
  const transport = createGrpcWebTransport({ baseUrl, useBinaryFormat: true });
  const parrot = createClient(Parrot, transport);
  const web = createClient(ParrotWeb, transport);
  const index = await fetch(baseUrl, { signal: deadline() });
  assert.match(index.headers.get('content-type'), /^text\/html/);
  const html = await index.text();
  assert.equal(await (await fetch(`${baseUrl}/s/synthetic-session`, { signal: deadline() })).text(), html);
  assert.match(html, /<script/);
  const scriptPath = html.match(/src="([^"]+\.js)"/)[1];
  const script = await fetch(new URL(scriptPath, baseUrl), { signal: deadline() });
  assert.match(script.headers.get('content-type'), /^text\/javascript/);
  assert.ok((await script.arrayBuffer()).byteLength > 1000);
  progress('assets_pass');
  const description = await web.describeHost({}, { signal: deadline() });
  assert.equal(description.workingDirectory, '/synthetic-browser-fixture');
  progress('web_unary_pass');
  const modes = await parrot.listModes({}, { signal: deadline() });
  assert.ok(modes.modes.some(mode => mode.id === 'build'));
  const session = await parrot.createSession({ model: 'local/model', mode: 'build', interactivePermissions: true }, { signal: deadline() });
  progress('session_created');
  assert.ok(session.id.startsWith('user-session-'));
  const listening = new AbortController();
  const events = parrot.listen({ userSessionId: session.id, replay: true }, { signal: AbortSignal.any([listening.signal, deadline()]) })[Symbol.asyncIterator]();
  const event = await events.next();
  assert.equal(event.done, false);
  // Source-shaped initial snapshots need not carry a durable event ID.
  assert.equal(event.value.$typeName, 'parrot.v1.Event');
  const firstNativePayload = event.value.payload.case;
  assert.ok(firstNativePayload, 'the first streamed snapshot must decode a real payload');
  listening.abort();
  await assert.rejects(events.next(), error => error instanceof ConnectError && error.code === Code.Canceled);
  progress('native_stream_abort_pass');
  const statusMessages = ['prompt failure % café', 'literal %', 'literal %41', 'unicode café π', 'ordinary message'];
  const observedErrors = [];
  for (const message of statusMessages) {
    let observedError = false;
    try {
      for await (const frame of web.runSlashCommand({ userSessionId: session.id, text: message === statusMessages[0] ? 'failure' : `failure:${message}` }, { signal: deadline() })) {
        assert.equal(frame.payload.case, 'started');
      }
    } catch (error) {
      assert.ok(error instanceof ConnectError);
      observedErrors.push({ expected: message, code: error.code, rawMessage: error.rawMessage, metadata: [...error.metadata], cause: String(error.cause) });
      fs.writeFileSync(path.join(output, 'observed-error.json'), JSON.stringify(observedErrors, null, 2) + '\n');
      assert.equal(error.code, Code.InvalidArgument);
      assert.equal(error.rawMessage, message);
      assert.equal(error.metadata.get('x-web-proof'), 'trailer');
      observedError = true;
    }
    assert.ok(observedError);
  }
  progress('stream_error_pass');
  const cancelling = new AbortController();
  const frames = web.runSlashCommand({ userSessionId: session.id, text: 'pending' }, { signal: AbortSignal.any([cancelling.signal, deadline()]) })[Symbol.asyncIterator]();
  const first = await frames.next();
  assert.equal(first.done, false);
  assert.equal(first.value.payload.case, 'started');
  cancelling.abort();
  await assert.rejects(frames.next(), error => error instanceof ConnectError && error.code === Code.Canceled);
  progress('web_stream_abort_pass');
  fs.writeFileSync(path.join(output, 'receipt.json'), JSON.stringify({ node: process.version, inputs, packages, transportModule, firstNativePayload, observedErrors, predicates: ['embedded_index_and_spa', 'genuine_javascript', 'web_unary', 'native_unary', 'actual_session_create', 'native_stream_before_end_and_abort', 'web_stream_error_message_and_trailer', 'web_stream_abort'], browser_engine: false }, null, 2) + '\n');
  console.log('Node Connect gRPC-Web: 8 predicates PASS; unchanged generated inputs; browser engine NOT_RUN');
}
main().catch(error => { console.error(error); process.exitCode = 1; });
