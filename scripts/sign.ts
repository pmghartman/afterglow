// sign: FILE signed as minisign does (prehashed Ed25519) into FILE.minisig, with the
// key in UPDATE_SIGNING_KEY in whatever form phosphorus keeps it: the key file, base64
// of it, or its key line alone (the comment line is optional). No minisign binary, and
// no password: a password-protected key is refused. Usage: sign FILE [TRUSTED COMMENT]
import { createPrivateKey, sign } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";

const COMMENT = "untrusted comment:";
const die = (msg: string): never => {
  console.error("sign:", msg);
  process.exit(1);
};
const blake2b512 = (...parts: Uint8Array[]) => {
  const h = new Bun.CryptoHasher("blake2b512");
  for (const p of parts) h.update(p);
  return Buffer.from(h.digest());
};
const blake2b256 = (...parts: Uint8Array[]) => {
  const h = new Bun.CryptoHasher("blake2b256");
  for (const p of parts) h.update(p);
  return Buffer.from(h.digest());
};

// the key line: from the file itself, base64 of the file, or the line alone
function keyLine(s: string): Buffer {
  s = s.trim();
  if (!s.startsWith(COMMENT)) {
    const b = Buffer.from(s, "base64");
    if (b.toString().startsWith(COMMENT)) s = b.toString().trim();
  }
  const lines = s.replaceAll("\r\n", "\n").split("\n");
  return Buffer.from((lines.length > 1 ? lines[1]! : lines[0]!).trim(), "base64");
}

const [file, comment] = process.argv.slice(2);
if (!file) die("usage: sign FILE [TRUSTED COMMENT]");
const b = keyLine(process.env.UPDATE_SIGNING_KEY ?? "");
if (b.length !== 158 || b.subarray(0, 2).toString() !== "Ed" || b.subarray(4, 6).toString() !== "B2")
  die("UPDATE_SIGNING_KEY isn't a minisign ed25519 secret key");
if (b[2] !== 0 || b[3] !== 0) die("UPDATE_SIGNING_KEY is password protected; make an unencrypted one");
const body = b.subarray(54);
if (!blake2b256(b.subarray(0, 2), body.subarray(0, 72)).equals(body.subarray(72, 104)))
  die("UPDATE_SIGNING_KEY is damaged");
const id = body.subarray(0, 8);
const priv = body.subarray(8, 72);
const key = createPrivateKey({
  key: {
    kty: "OKP",
    crv: "Ed25519",
    d: priv.subarray(0, 32).toString("base64url"),
    x: priv.subarray(32).toString("base64url"),
  },
  format: "jwk",
});
const sig = sign(null, blake2b512(readFileSync(file)), key);
const trusted = comment ?? `timestamp:${Math.floor(Date.now() / 1000)}`;
const global = sign(null, Buffer.concat([sig, Buffer.from(trusted)]), key);
writeFileSync(
  `${file}.minisig`,
  `untrusted comment: signature from afterglow secret key\n${Buffer.concat([Buffer.from("ED"), id, sig]).toString("base64")}\ntrusted comment: ${trusted}\n${global.toString("base64")}\n`,
);
console.log("signed", file);
