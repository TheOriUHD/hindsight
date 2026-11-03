import assert from "node:assert/strict";
import { test } from "node:test";

import { cleanText, esc, headerText, isJpeg, isMp4, newId, number, parseRange, validId } from "../src/rules.js";

test("new ids are valid and carry no confusing characters", () => {
  const id = newId(new Uint8Array([0, 1, 2, 3, 54, 55, 100, 200, 254, 255]));
  assert.equal(id.length, 10);
  assert.ok(validId(id));
  assert.ok(!/[01lIoO]/.test(id));
});

test("ids can't carry a path", () => {
  for (const bad of ["../../etc", "abc", "abcdefghijk", "abc/efghij", ""]) assert.ok(!validId(bad));
});

const box = (type, body = 0) => {
  const b = new Uint8Array(8 + body);
  new DataView(b.buffer).setUint32(0, 8 + body);
  b.set(new TextEncoder().encode(type), 4);
  return b;
};
const join = (...parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) (out.set(p, at), (at += p.length));
  return out;
};

test("only well-formed MP4 files pass", () => {
  assert.ok(isMp4(join(box("ftyp", 16), box("moov", 100), box("mdat", 1000))));
  assert.ok(isMp4(join(box("ftyp", 16), box("mdat", 1000), box("moov", 100))));
  // An MP4-looking start followed by junk, the case that got through before.
  assert.ok(!isMp4(join(box("ftyp", 16), new Uint8Array(3000))));
  assert.ok(!isMp4(join(box("ftyp", 16), box("mdat", 1000))));
  assert.ok(!isMp4(join(box("moov", 100), box("ftyp", 16), box("mdat", 10))));
  const truncated = join(box("ftyp", 16), box("moov", 100), box("mdat", 1000)).slice(0, 500);
  assert.ok(!isMp4(truncated));
  assert.ok(!isMp4(new TextEncoder().encode("<html><script>alert(1)</script></html>")));
});

test("real clips pass", async () => {
  const { readFileSync, existsSync } = await import("node:fs");
  const file = process.env.HINDSIGHT_TEST_MP4;
  if (!file || !existsSync(file)) return;
  assert.ok(isMp4(new Uint8Array(readFileSync(file))));
});

test("only JPEG thumbnails pass", () => {
  assert.ok(isJpeg(new Uint8Array([0xff, 0xd8, 0xff, 0xe0])));
  assert.ok(!isJpeg(box("ftyp", 4)));
});

test("ranges", () => {
  assert.deepEqual(parseRange("bytes=0-99", 1000), { start: 0, end: 99 });
  assert.deepEqual(parseRange("bytes=900-", 1000), { start: 900, end: 999 });
  assert.deepEqual(parseRange("bytes=-100", 1000), { start: 900, end: 999 });
  assert.deepEqual(parseRange("bytes=900-5000", 1000), { start: 900, end: 999 });
  assert.equal(parseRange("bytes=2000-", 1000), null);
  assert.equal(parseRange("bytes=-", 1000), null);
  assert.equal(parseRange("nonsense", 1000), null);
  assert.equal(parseRange(null, 1000), null);
});

test("text from uploads is cleaned and escaped", () => {
  assert.equal(cleanText(" a\u0000b\nc ", 10), "a b c");
  assert.equal(cleanText("x".repeat(50), 5), "xxxxx");
  assert.equal(headerText("Double%20kill%20%F0%9F%94%A5", 100), "Double kill 🔥");
  assert.equal(headerText("%E0%A4%A", 100), "");
  assert.equal(esc(`"><script>'&`), "&quot;&gt;&lt;script&gt;&#39;&amp;");
});

test("numbers outside the range fall back", () => {
  assert.equal(number("1920", 16, 7680, 1280), 1920);
  assert.equal(number("99999", 16, 7680, 1280), 1280);
  assert.equal(number("nope", 16, 7680, 1280), 1280);
});
