// Pure helpers for the Desktop credential review. The detector, masking and use
// scope come from the host (`secret_scan`); this module only turns a reviewed
// draft into captures and decides which drafts are safe to persist.

/** Every distinct captured reference token in a transcript text. */
export function secretReferences(text) {
  const found = [];
  const pattern = /\[secret:(sec_[0-9a-f]{32})\]/g;
  for (const match of String(text ?? '').matchAll(pattern)) {
    if (!found.includes(match[1])) found.push(match[1]);
  }
  return found;
}

/**
 * Replace each finding span with a unique placeholder and return the matching
 * captures. The host reports UTF-16 offsets because the renderer slices UTF-16
 * strings; byte offsets never cross this boundary.
 */
export function captureDraft(content, findings, newId) {
  const spans = [...(findings ?? [])].sort((left, right) => left.start_utf16 - right.start_utf16);
  let previousEnd = 0;
  for (const finding of spans) {
    const start = finding.start_utf16;
    const end = finding.end_utf16;
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end)
      || start < previousEnd || end <= start || end > content.length) {
      throw new Error('凭据扫描结果与消息不匹配，请重新检查');
    }
    previousEnd = end;
  }
  let text = content;
  const captures = [];
  for (const finding of [...spans].reverse()) {
    const value = content.slice(finding.start_utf16, finding.end_utf16);
    const placeholder = `{{secret-capture:${newId()}}}`;
    text = text.slice(0, finding.start_utf16) + placeholder + text.slice(finding.end_utf16);
    captures.push({
      placeholder,
      value,
      ...(finding.key ? { key_hint: finding.key } : {}),
    });
  }
  return { content: text, captures };
}

/**
 * Drafts that may be written to local storage: a value is persisted only when
 * it is byte-identical to a text the host scan reported without findings.
 */
export function persistableDrafts(drafts, clean) {
  const out = {};
  for (const [key, value] of drafts) {
    if (value && clean.get(key) === value) out[key] = value;
  }
  return out;
}
