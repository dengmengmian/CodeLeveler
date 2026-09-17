/** Presentation-only split of clarification option strings. Protocol stays string[]. */

export function splitChoiceOption(option: string): { ordinal: string | null; body: string } {
  const trimmed = option.trim();
  const m = /^([A-Za-z])[.)、：:]\s*([\s\S]*)$/.exec(trimmed);
  if (m) return { ordinal: m[1].toUpperCase(), body: m[2].trim() || trimmed };
  return { ordinal: null, body: trimmed };
}

export function choiceOrdinal(index: number): string {
  return String.fromCharCode(65 + (index % 26));
}
