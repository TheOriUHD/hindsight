// The limits and input checks, kept free of Cloudflare APIs so they can be tested on their own.

export const MAX_CLIP_BYTES = 60_000_000;
export const MAX_THUMB_BYTES = 300_000;
export const TOTAL_CAP_BYTES = 500_000_000_000;
// 500 GB over 14 days is about 36 GB a day; this keeps a burst from filling it at once.
export const DAILY_CAP_BYTES = 40_000_000_000;
export const UPLOADS_PER_DAY = 20;
export const KEEP_DAYS = 14;
// Reports from different people that hide a clip until it's been reviewed.
export const HIDE_AFTER_REPORTS = 3;
// Requests are shared with the account's other Workers, whose allowance is 10M
// a month, so the clips Worker stops serving well before that.
export const MONTHLY_REQUEST_CAP = 8_000_000;
// Counts one request in this many, in steps of this many, to spare database writes.
export const COUNT_EVERY = 32;

export const EMOJI = ["🔥", "😂", "😮", "👏", "💀"];
export const REPORT_REASONS = ["Illegal content", "Copyright", "Harassment or hate", "Sexual or violent content", "Personal information", "Other"];

const ID_CHARS = "23456789abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";

export function newId(random) {
  return [...random].map((b) => ID_CHARS[b % ID_CHARS.length]).join("");
}

export const validId = (id) => /^[2-9a-zA-Z]{10}$/.test(id);

/**
 * Walks the top-level MP4 boxes: they must start with "ftyp", include "moov"
 * and "mdat", and add up to exactly the file. Junk with an MP4-looking start
 * fails here.
 */
export function isMp4(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const seen = new Set();
  let at = 0;
  while (at < bytes.length) {
    if (at + 8 > bytes.length) return false;
    let size = view.getUint32(at);
    const type = String.fromCharCode(bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]);
    if (!/^[a-z0-9 ]{4}$/i.test(type)) return false;
    if (seen.size === 0 && type !== "ftyp") return false;
    if (size === 1) {
      if (at + 16 > bytes.length) return false;
      size = Number(view.getBigUint64(at + 8));
    } else if (size === 0) {
      size = bytes.length - at;
    }
    if (size < 8 || at + size > bytes.length) return false;
    seen.add(type);
    at += size;
  }
  return seen.has("moov") && seen.has("mdat");
}

export function isJpeg(bytes) {
  return bytes.length > 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff;
}

export function cleanText(value, max) {
  return String(value ?? "")
    .replace(/[\u0000-\u001f\u007f]/g, " ")
    .trim()
    .slice(0, max);
}

export function headerText(value, max) {
  try {
    return cleanText(decodeURIComponent(value ?? ""), max);
  } catch {
    return "";
  }
}

export const number = (value, min, max, fallback) => {
  const n = Number(value);
  return Number.isFinite(n) && n >= min && n <= max ? n : fallback;
};

/** bytes=a-b, bytes=a- or bytes=-n, resolved against `size`. Null means send everything. */
export function parseRange(header, size) {
  const m = /^bytes=(\d*)-(\d*)$/.exec(header ?? "");
  if (!m || (m[1] === "" && m[2] === "")) return null;
  let start, end;
  if (m[1] === "") {
    const n = Number(m[2]);
    start = Math.max(0, size - n);
    end = size - 1;
  } else {
    start = Number(m[1]);
    end = m[2] === "" ? size - 1 : Math.min(Number(m[2]), size - 1);
  }
  return start > end || start >= size ? null : { start, end };
}

export const esc = (v) => String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
