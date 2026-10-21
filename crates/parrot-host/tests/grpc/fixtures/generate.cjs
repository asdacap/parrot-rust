// Synthetic loopback-only TEST certificates. Requires only installed Node crypto.
// Run from this directory: node generate.cjs. Never install this root in system trust.
const fs = require('node:fs');
const crypto = require('node:crypto');
const path = require('node:path');
const der = (tag, ...parts) => {
  const value = Buffer.concat(parts);
  const length = value.length < 128 ? Buffer.from([value.length]) : (() => {
    let hex = value.length.toString(16); if (hex.length % 2) hex = '0' + hex;
    const bytes = Buffer.from(hex, 'hex'); return Buffer.concat([Buffer.from([0x80 | bytes.length]), bytes]);
  })();
  return Buffer.concat([Buffer.from([tag]), length, value]);
};
const seq = (...v) => der(0x30, ...v);
const oid = (value) => {
  const numbers = value.split('.').map(Number);
  const bytes = [40 * numbers[0] + numbers[1]];
  for (const number of numbers.slice(2)) {
    let n = number; const encoded = [n & 0x7f];
    while ((n = Math.floor(n / 128))) encoded.unshift(0x80 | (n & 0x7f));
    bytes.push(...encoded);
  }
  return der(0x06, Buffer.from(bytes));
};
const name = (cn) => seq(der(0x31, seq(oid('2.5.4.3'), der(0x0c, Buffer.from(cn)))));
const algorithm = seq(oid('1.2.840.113549.1.1.11'), der(0x05));
const extension = (id, value, critical = false) => seq(oid(id), ...(critical ? [der(0x01, Buffer.from([0xff]))] : []), der(0x04, value));
const pem = (label, bytes) => `-----BEGIN ${label}-----\n${bytes.toString('base64').match(/.{1,64}/g).join('\n')}\n-----END ${label}-----\n`;
const rootName = name('Parrot synthetic test root');
const root = crypto.generateKeyPairSync('rsa', {modulusLength: 2048});
const server = crypto.generateKeyPairSync('rsa', {modulusLength: 2048});
function certificate(key, issuerKey, subject, serial, extensions) {
  const tbs = seq(
    der(0xa0, der(0x02, Buffer.from([2]))), der(0x02, Buffer.from([serial])), algorithm,
    rootName,
    seq(der(0x17, Buffer.from('260101000000Z')), der(0x17, Buffer.from('300101000000Z'))),
    subject, key.publicKey.export({format:'der', type:'spki'}), der(0xa3, seq(...extensions)),
  );
  const signature = crypto.sign('sha256', tbs, issuerKey.privateKey);
  return seq(tbs, algorithm, der(0x03, Buffer.from([0]), signature));
}
const rootDer = certificate(root, root, rootName, 1, [
  extension('2.5.29.19', seq(der(0x01, Buffer.from([0xff]))), true),
  extension('2.5.29.15', der(0x03, Buffer.from([1, 0x06])), true),
]);
const serverDer = certificate(server, root, name('localhost'), 2, [
  extension('2.5.29.19', seq(), true),
  extension('2.5.29.15', der(0x03, Buffer.from([5, 0xa0])), true),
  extension('2.5.29.37', seq(oid('1.3.6.1.5.5.7.3.1'))),
  extension('2.5.29.17', seq(der(0x82, Buffer.from('localhost')), der(0x87, Buffer.from([127, 0, 0, 1])))),
]);
const rootCert = new crypto.X509Certificate(rootDer);
const serverCert = new crypto.X509Certificate(serverDer);
if (!rootCert.ca || serverCert.ca || !rootCert.verify(root.publicKey) || !serverCert.verify(root.publicKey) || !serverCert.checkPrivateKey(server.privateKey)) throw Error('synthetic chain validation failed');
for (const [file, value] of Object.entries({
  'root.pem': pem('CERTIFICATE', rootDer),
  'server.pem': pem('CERTIFICATE', serverDer),
  'server-key.pem': server.privateKey.export({format:'pem', type:'pkcs8'}),
})) fs.writeFileSync(path.join(__dirname, file), value);
console.log('Generated one synthetic root and localhost/127.0.0.1 server fixture; Node signature and key checks passed.');
